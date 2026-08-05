// Anthropic → OpenAI responses 转换
//
// 参考 CPA internal/translator/codex/claude/codex_claude_request.go (586 行),
// 但避开两个已知坑:
// 1. system 写顶层 template.instructions(不写 input[0] as developer)——保持
//    缓存前缀稳定,避免轮次间工具集变化导致映射漂移
// 2. 工具名保留原样,不截断、不加 _1 后缀
//
// 主要映射:
// - system → template.instructions(剥离计费归属块)
// - messages → input[]:text→input_text/output_text, thinking(带签名)→reasoning,
//   image→message 内 input_image, tool_use→function_call, tool_result→function_call_output
//   (遇 thinking/tool_use/tool_result 先 flush 文本 message,对齐 CPA flushMessage 顺序)
// - tools → template.tools(保留原名)
// - thinking.budget_tokens → reasoning.effort
// - max_tokens → max_output_tokens;store=false;include=["reasoning.encrypted_content"];
//   parallel_tool_calls(对齐 CPA codex 路径)

use serde_json::{json, Value};

use super::{ConvertError, Result};

/// Anthropic messages → OpenAI responses
pub fn convert_to_openai_responses(body: &mut Value, upstream_model: &str) -> Result<()> {
    let mut openai = json!({
        "model": upstream_model,
        "template": {
            "instructions": "",
            "tools": []
        },
        "input": [],
        "stream": body.get("stream").unwrap_or(&json!(true)).clone(),
    });

    // System → template.instructions(避开 CPA 的 input[0] 坑;
    // 剥离计费归属块与空白块,对齐 CPA appendSystemText)
    if let Some(system) = body.get("system") {
        let text = match system {
            Value::String(s) => s.clone(),
            Value::Array(blocks) => blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .filter(|t| !t.trim().is_empty() && !super::is_attribution_text(t))
                .collect::<Vec<_>>()
                .join("\n\n"),
            _ => return Err(ConvertError::InvalidType("system".into())),
        };
        let text = if super::is_attribution_text(&text) { String::new() } else { text };
        openai["template"]["instructions"] = json!(text);
    }

    // thinking → reasoning.effort(忠实 CPA thinking 映射)
    if let Some(effort) = body
        .get("thinking")
        .and_then(|t| crate::thinking::resolve_effort(t, &crate::thinking::DEFAULT_SUPPORTED))
    {
        openai["reasoning"] = json!({"effort": effort});
    }

    // Messages → input[](对齐 CPA codex_claude_request.go 的 flush 语义:
    // 文本累积在 message 里,遇 thinking/tool_use/tool_result 先 flush,
    // 保证 message 不越过 function_call/function_call_output)
    if let Some(messages) = body.get("messages").and_then(|v| v.as_array()) {
        for msg in messages {
            let role = msg
                .get("role")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ConvertError::MissingField("role".into()))?;
            // 空内容(缺失/null):丢弃消息(对齐 CPA 与 chat 侧语义)
            let Some(content) = msg.get("content").cloned() else {
                continue;
            };
            if content.is_null() {
                continue;
            }

            // 字符串内容(包 message 包装,兼容仅认 message 类型的上游)
            if let Some(s) = content.as_str() {
                let item_type = if role == "assistant" { "output_text" } else { "input_text" };
                // 空字符串 → content:[] 保留消息(与 chat 侧一致);
                // assistant 空 content 是 thinking-only/tool 轮的正常信号,不能丢
                let items = if s.is_empty() {
                    Vec::new()
                } else {
                    vec![json!({"type": item_type, "text": s})]
                };
                openai["input"].as_array_mut().unwrap().push(json!({
                    "type": "message", "role": role, "content": items
                }));
                continue;
            }

            let Some(parts) = content.as_array() else {
                continue;
            };

            let mut content_items: Vec<Value> = Vec::new();
            let mut out_items: Vec<Value> = Vec::new();
            for part in parts {
                let ptype = part.get("type").and_then(|v| v.as_str()).unwrap_or("");
                match ptype {
                    "text" => {
                        if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                            let item_type = if role == "assistant" { "output_text" } else { "input_text" };
                            content_items.push(json!({"type": item_type, "text": t}));
                        }
                    }
                    "thinking" => {
                        // assistant 带签名 thinking → 独立 reasoning item(先 flush 保序)。
                        // 无签名思考丢弃:对齐 CPA,不退化为纯文本(避免污染会话内容)。
                        // CPA 另校验签名须 GPT 兼容(sigcompat),此处从简:有签名即透传。
                        if role == "assistant" {
                            let signature = part
                                .get("signature")
                                .and_then(|v| v.as_str())
                                .filter(|s| !s.trim().is_empty());
                            if let Some(sig) = signature {
                                flush_message(role, &mut content_items, &mut out_items);
                                out_items.push(json!({
                                    "type": "reasoning",
                                    "summary": [],
                                    "content": null,
                                    "encrypted_content": sig
                                }));
                            }
                        }
                    }
                    "image" => {
                        // input_image 只能是 message 的 content part,顶层 item 非法
                        if let Some(url) = image_to_url(part) {
                            content_items.push(json!({
                                "type": "input_image",
                                "image_url": url
                            }));
                        }
                    }
                    "tool_use" => {
                        flush_message(role, &mut content_items, &mut out_items);
                        let id = part.get("id").and_then(|v| v.as_str()).unwrap_or("");
                        let name = part.get("name").and_then(|v| v.as_str()).unwrap_or("");
                        let args = part
                            .get("input")
                            .map(|i| i.to_string())
                            .unwrap_or_else(|| "{}".to_string());
                        out_items.push(json!({
                            "type": "function_call",
                            "call_id": shorten_call_id(id),
                            "name": name,
                            "arguments": args
                        }));
                    }
                    "tool_result" => {
                        flush_message(role, &mut content_items, &mut out_items);
                        let call_id = part
                            .get("tool_use_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let output = tool_result_output(part.get("content").cloned().unwrap_or(json!("")));
                        out_items.push(json!({
                            "type": "function_call_output",
                            "call_id": shorten_call_id(call_id),
                            "output": output
                        }));
                    }
                    _ => {}
                }
            }
            flush_message(role, &mut content_items, &mut out_items);
            openai["input"].as_array_mut().unwrap().extend(out_items);
        }
    }

    // Tools → template.tools(保留原名,不截断)
    if let Some(tools) = body.get("tools").and_then(|v| v.as_array()) {
        let mut openai_tools = Vec::new();
        for tool in tools {
            let name = tool.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let description = tool
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let mut fn_obj = json!({
                "type": "function",
                "name": name,
                "description": description,
                "parameters": {"type": "object", "properties": {}}
            });
            if let Some(schema) = tool.get("input_schema") {
                fn_obj["parameters"] = super::normalize_object_schema_properties(schema.clone());
            }
            openai_tools.push(fn_obj);
        }
        openai["template"]["tools"] = json!(openai_tools);
    }

    // tool_choice(简单映射)
    if let Some(tc) = body.get("tool_choice") {
        if let Some(choice) = convert_tool_choice(tc) {
            openai["tool_choice"] = choice;
        }
    }

    // anthropic max_tokens → responses max_output_tokens
    if let Some(val) = body.get("max_tokens") {
        openai["max_output_tokens"] = val.clone();
    }

    // 对齐 CPA codex 路径:store=false + 回传加密思考 + 并行工具调用
    openai["store"] = json!(false);
    openai["include"] = json!(["reasoning.encrypted_content"]);
    let disable_parallel = body
        .get("tool_choice")
        .and_then(|tc| tc.get("disable_parallel_tool_use"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    openai["parallel_tool_calls"] = json!(!disable_parallel);

    *body = openai;
    Ok(())
}

/// 把累积的文本 content 刷成 message item(CPA flushMessage 语义)
fn flush_message(role: &str, content_items: &mut Vec<Value>, out_items: &mut Vec<Value>) {
    if !content_items.is_empty() {
        out_items.push(json!({
            "type": "message", "role": role, "content": std::mem::take(content_items)
        }));
    }
}

/// call_id 超 64 字符确定性截短(对齐 CPA shortenCodexCallIDIfNeeded:
/// 前缀 + "_" + SHA-256 前 8 字节 hex)
fn shorten_call_id(id: &str) -> String {
    const LIMIT: usize = 64;
    if id.len() <= LIMIT {
        return id.to_string();
    }
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(id.as_bytes());
    let suffix = format!("_{}", hex::encode(&digest[..8]));
    let mut prefix_len = LIMIT.saturating_sub(suffix.len());
    while prefix_len > 0 && !id.is_char_boundary(prefix_len) {
        prefix_len -= 1;
    }
    format!("{}{}", &id[..prefix_len], suffix)
}

/// image block → data URL
fn image_to_url(part: &Value) -> Option<String> {
    let source = part.get("source")?;
    let media_type = source.get("media_type").and_then(|v| v.as_str()).unwrap_or("image/png");
    match source.get("type").and_then(|v| v.as_str()) {
        Some("base64") => {
            let data = source
                .get("data")
                .or_else(|| source.get("base64"))
                .and_then(|v| v.as_str())?;
            Some(format!("data:{media_type};base64,{data}"))
        }
        Some("url") => source.get("url").and_then(|v| v.as_str()).map(|s| s.to_string()),
        _ => None,
    }
}

/// tool_result content → 字符串或数组
fn tool_result_output(content: Value) -> Value {
    match &content {
        Value::Array(items) => {
            let mut out = Vec::new();
            for item in items {
                match item.get("type").and_then(|v| v.as_str()) {
                    Some("text") => {
                        if let Some(t) = item.get("text").and_then(|v| v.as_str()) {
                            out.push(json!({"type": "input_text", "text": t}));
                        }
                    }
                    Some("image") => {
                        if let Some(url) = image_to_url(item) {
                            out.push(json!({"type": "input_image", "image_url": url}));
                        }
                    }
                    _ => {}
                }
            }
            if out.is_empty() {
                Value::String(content.to_string())
            } else {
                Value::Array(out)
            }
        }
        Value::String(s) => Value::String(s.clone()),
        other => other.clone(),
    }
}

/// tool_choice 映射
fn convert_tool_choice(tc: &Value) -> Option<Value> {
    let ty = tc.get("type").and_then(|v| v.as_str())?;
    match ty {
        "auto" => Some(json!("auto")),
        "any" => Some(json!("required")),
        "none" => Some(json!("none")),
        "tool" => {
            let name = tc.get("name").and_then(|v| v.as_str()).unwrap_or("");
            Some(json!({"type": "function", "name": name}))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_system_goes_to_instructions() {
        let mut body = json!({
            "model": "test",
            "system": "You are helpful",
            "messages": [{"role": "user", "content": "hi"}]
        });
        convert_to_openai_responses(&mut body, "gpt-5").unwrap();
        assert_eq!(body["template"]["instructions"], "You are helpful");
        assert_eq!(body["input"][0]["type"], "message");
        assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(body["input"][0]["content"][0]["text"], "hi");
    }

    #[test]
    fn test_tool_name_preserved() {
        let mut body = json!({
            "model": "test",
            "messages": [],
            "tools": [{
                "name": "very_long_tool_name_that_exceeds_sixty_four_characters_in_total_length",
                "description": "test",
                "input_schema": {"type": "object"}
            }]
        });
        convert_to_openai_responses(&mut body, "gpt-5").unwrap();
        let name = body["template"]["tools"][0]["name"].as_str().unwrap();
        assert_eq!(name, "very_long_tool_name_that_exceeds_sixty_four_characters_in_total_length");
        assert!(!name.ends_with("_1"));
    }

    #[test]
    fn test_tool_use_and_result() {
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "get_weather", "input": {"city": "beijing"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": "sunny"}
                ]}
            ]
        });
        convert_to_openai_responses(&mut body, "gpt-5").unwrap();
        assert_eq!(body["input"][0]["type"], "function_call");
        assert_eq!(body["input"][0]["call_id"], "t1");
        assert_eq!(body["input"][0]["name"], "get_weather");
        assert_eq!(body["input"][1]["type"], "function_call_output");
        assert_eq!(body["input"][1]["output"], "sunny");
    }

    #[test]
    fn test_thinking_effort() {
        let mut body = json!({
            "model": "test",
            "thinking": {"type": "enabled", "budget_tokens": 20000},
            "messages": []
        });
        convert_to_openai_responses(&mut body, "gpt-5").unwrap();
        assert_eq!(body["reasoning"]["effort"], "high");
    }

    #[test]
    fn test_image_block() {
        let mut body = json!({
            "model": "test",
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "image",
                    "source": {"type": "url", "url": "https://example.com/image.png"}
                }]
            }]
        });
        convert_to_openai_responses(&mut body, "gpt-5").unwrap();
        // input_image 必须在 message 的 content 内,顶层 item 非法
        assert_eq!(body["input"][0]["type"], "message");
        assert_eq!(body["input"][0]["role"], "user");
        assert_eq!(body["input"][0]["content"][0]["type"], "input_image");
        assert_eq!(body["input"][0]["content"][0]["image_url"], "https://example.com/image.png");
    }

    #[test]
    fn test_max_tokens_maps_to_max_output_tokens() {
        let mut body = json!({
            "model": "test",
            "messages": [],
            "max_tokens": 8192
        });
        convert_to_openai_responses(&mut body, "gpt-5").unwrap();
        assert_eq!(body["max_output_tokens"], 8192);
    }

    #[test]
    fn test_message_flushed_before_function_call() {
        // assistant 同一条消息含 text + tool_use:message 必须先于 function_call
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "text", "text": "let me check"},
                    {"type": "tool_use", "id": "t1", "name": "search", "input": {}}
                ]}
            ]
        });
        convert_to_openai_responses(&mut body, "gpt-5").unwrap();
        assert_eq!(body["input"][0]["type"], "message");
        assert_eq!(body["input"][0]["content"][0]["text"], "let me check");
        assert_eq!(body["input"][1]["type"], "function_call");
        assert_eq!(body["input"][1]["call_id"], "t1");
    }

    #[test]
    fn test_thinking_signed_emits_reasoning_unsigned_dropped() {
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "unsigned text"},
                    {"type": "thinking", "thinking": "signed text", "signature": "sig-abc"},
                    {"type": "text", "text": "answer"}
                ]}
            ]
        });
        convert_to_openai_responses(&mut body, "gpt-5").unwrap();
        // 无签名思考丢弃(不退化为文本);带签名 → reasoning item;文本 message 在最后
        assert_eq!(body["input"][0]["type"], "reasoning");
        assert_eq!(body["input"][0]["encrypted_content"], "sig-abc");
        assert_eq!(body["input"][1]["type"], "message");
        assert_eq!(body["input"][1]["content"][0]["text"], "answer");
    }

    #[test]
    fn test_long_call_id_shortened_deterministically() {
        let long_id = "toolu_".to_string() + &"x".repeat(80);
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": long_id.clone(), "name": "f", "input": {}}
                ]}
            ]
        });
        convert_to_openai_responses(&mut body, "gpt-5").unwrap();
        let call_id = body["input"][0]["call_id"].as_str().unwrap();
        assert!(call_id.len() <= 64);
        // 确定性:同输入同输出
        assert_eq!(call_id, shorten_call_id(&long_id));
    }

    #[test]
    fn test_codex_fields_present() {
        let mut body = json!({
            "model": "test",
            "messages": []
        });
        convert_to_openai_responses(&mut body, "gpt-5").unwrap();
        assert_eq!(body["store"], false);
        assert_eq!(body["include"][0], "reasoning.encrypted_content");
        assert_eq!(body["parallel_tool_calls"], true);
    }

    #[test]
    fn test_parallel_tool_calls_disabled() {
        let mut body = json!({
            "model": "test",
            "messages": [],
            "tool_choice": {"type": "auto", "disable_parallel_tool_use": true}
        });
        convert_to_openai_responses(&mut body, "gpt-5").unwrap();
        assert_eq!(body["parallel_tool_calls"], false);
    }

    #[test]
    fn test_system_attribution_stripped() {
        let mut body = json!({
            "model": "test",
            "system": [
                {"type": "text", "text": "x-anthropic-billing-header: fp=abc123"},
                {"type": "text", "text": "Real instructions"}
            ],
            "messages": []
        });
        convert_to_openai_responses(&mut body, "gpt-5").unwrap();
        let instructions = body["template"]["instructions"].as_str().unwrap();
        assert!(!instructions.contains("billing-header"));
        assert!(instructions.contains("Real instructions"));
    }

    #[test]
    fn test_empty_messages() {
        let mut body = json!({
            "model": "test",
            "messages": []
        });
        convert_to_openai_responses(&mut body, "gpt-5").unwrap();
        assert_eq!(body["input"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn test_null_and_missing_content_messages_dropped() {
        // null/missing content → 丢弃消息(对齐 CPA 与 chat 侧)
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "user", "content": null},
                {"role": "user"},
                {"role": "user", "content": "keep"}
            ]
        });
        convert_to_openai_responses(&mut body, "gpt-5").unwrap();
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 1, "null/missing 消息应丢弃,只留 keep");
        assert_eq!(input[0]["content"][0]["text"], "keep");
    }

    #[test]
    fn test_empty_string_content_keeps_message_with_empty_array() {
        // 空字符串 → content:[] 保留消息(与 chat 侧一致;assistant 空 content 是正常信号)
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "assistant", "content": ""},
                {"role": "user", "content": "hi"}
            ]
        });
        convert_to_openai_responses(&mut body, "gpt-5").unwrap();
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 2, "空字符串消息应保留(不丢弃)");
        assert_eq!(input[0]["role"], "assistant");
        assert_eq!(input[0]["content"].as_array().unwrap().len(), 0, "空字符串 → 空数组");
    }

    #[test]
    fn test_empty_tools() {
        let mut body = json!({
            "model": "test",
            "messages": [],
            "tools": []
        });
        convert_to_openai_responses(&mut body, "gpt-5").unwrap();
        assert_eq!(body["template"]["tools"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn test_tool_choice_mappings() {
        let mut body = json!({
            "model": "test",
            "messages": [],
            "tool_choice": {"type": "any"}
        });
        convert_to_openai_responses(&mut body, "gpt-5").unwrap();
        assert_eq!(body["tool_choice"], "required");

        let mut body2 = json!({
            "model": "test",
            "messages": [],
            "tool_choice": {"type": "tool", "name": "search"}
        });
        convert_to_openai_responses(&mut body2, "gpt-5").unwrap();
        assert_eq!(body2["tool_choice"]["type"], "function");
        assert_eq!(body2["tool_choice"]["name"], "search");
    }

    #[test]
    fn test_system_array_form() {
        let mut body = json!({
            "model": "test",
            "system": [
                {"type": "text", "text": "Instruction 1"},
                {"type": "text", "text": "Instruction 2"}
            ],
            "messages": []
        });
        convert_to_openai_responses(&mut body, "gpt-5").unwrap();
        let instructions = body["template"]["instructions"].as_str().unwrap();
        assert!(instructions.contains("Instruction 1"));
        assert!(instructions.contains("Instruction 2"));
    }
}