// Anthropic → OpenAI responses 转换
//
// 参考 CPA internal/translator/codex/claude/codex_claude_request.go (586 行),
// 但避开两个已知坑:
// 1. system 写顶层 template.instructions(不写 input[0] as developer)——保持
//    缓存前缀稳定,避免轮次间工具集变化导致映射漂移
// 2. 工具名保留原样,不截断、不加 _1 后缀
//
// 主要映射:
// - system → template.instructions
// - messages → input[]:text→input_text/output_text, thinking→reasoning,
//   image→input_image, tool_use→function_call, tool_result→function_call_output
// - tools → template.tools(保留原名)
// - thinking.budget_tokens → reasoning.effort

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

    // System → template.instructions(避开 CPA 的 input[0] 坑)
    if let Some(system) = body.get("system") {
        let text = match system {
            Value::String(s) => s.clone(),
            Value::Array(blocks) => blocks
                .iter()
                .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
                .collect::<Vec<_>>()
                .join("\n\n"),
            _ => return Err(ConvertError::InvalidType("system".into())),
        };
        openai["template"]["instructions"] = json!(text);
    }

    // thinking → reasoning.effort
    if let Some(effort) = thinking_to_effort(body.get("thinking")) {
        openai["reasoning"] = json!({"effort": effort});
    }

    // Messages → input[]
    if let Some(messages) = body.get("messages").and_then(|v| v.as_array()) {
        for msg in messages {
            let role = msg
                .get("role")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ConvertError::MissingField("role".into()))?;
            let content = msg.get("content").cloned().unwrap_or(json!(""));

            // 字符串内容
            if let Some(s) = content.as_str() {
                let item_type = if role == "assistant" { "output_text" } else { "input_text" };
                openai["input"].as_array_mut().unwrap().push(json!({
                    "role": role, "type": item_type, "text": s
                }));
                continue;
            }

            let Some(parts) = content.as_array() else {
                continue;
            };

            let mut content_items: Vec<Value> = Vec::new();
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
                        // assistant 的 thinking → 独立 reasoning item(加密内容透传)
                        if role == "assistant" {
                            if let Some(sig) = part.get("signature").and_then(|v| v.as_str()) {
                                openai["input"].as_array_mut().unwrap().push(json!({
                                    "type": "reasoning",
                                    "summary": [],
                                    "content": null,
                                    "encrypted_content": sig
                                }));
                            } else if let Some(t) = part.get("thinking").and_then(|v| v.as_str()) {
                                // 无签名时退化为输出文本
                                content_items.push(json!({"type": "output_text", "text": t}));
                            }
                        }
                    }
                    "image" => {
                        if let Some(url) = image_to_url(part) {
                            openai["input"].as_array_mut().unwrap().push(json!({
                                "type": "input_image",
                                "image_url": url
                            }));
                        }
                    }
                    "tool_use" => {
                        let id = part.get("id").and_then(|v| v.as_str()).unwrap_or("");
                        let name = part.get("name").and_then(|v| v.as_str()).unwrap_or("");
                        let args = part
                            .get("input")
                            .map(|i| i.to_string())
                            .unwrap_or_else(|| "{}".to_string());
                        openai["input"].as_array_mut().unwrap().push(json!({
                            "type": "function_call",
                            "call_id": id,
                            "name": name,
                            "arguments": args
                        }));
                    }
                    "tool_result" => {
                        let call_id = part
                            .get("tool_use_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let output = tool_result_output(part.get("content").cloned().unwrap_or(json!("")));
                        openai["input"].as_array_mut().unwrap().push(json!({
                            "type": "function_call_output",
                            "call_id": call_id,
                            "output": output
                        }));
                    }
                    _ => {}
                }
            }

            if !content_items.is_empty() {
                openai["input"].as_array_mut().unwrap().push(json!({
                    "role": role, "content": content_items
                }));
            }
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
                fn_obj["parameters"] = schema.clone();
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

    // 其他参数
    for key in &["max_output_tokens", "temperature", "top_p"] {
        if let Some(val) = body.get(*key) {
            openai[key] = val.clone();
        }
    }

    *body = openai;
    Ok(())
}

/// thinking.budget_tokens → reasoning.effort(与 CPA 一致)
fn thinking_to_effort(thinking: Option<&Value>) -> Option<&'static str> {
    let thinking = thinking?;
    if !thinking.is_object() {
        return None;
    }
    let ty = thinking.get("type").and_then(|v| v.as_str())?;
    let budget = match ty {
        "enabled" | "adaptive" => thinking
            .get("budget_tokens")
            .and_then(|v| v.as_i64())
            .unwrap_or(-1),
        "disabled" => 0,
        _ => return None,
    };
    Some(budget_to_effort(budget))
}

fn budget_to_effort(budget: i64) -> &'static str {
    match budget {
        b if b < -1 => "auto",
        -1 => "auto",
        0 => "none",
        b if b <= 512 => "minimal",
        b if b <= 1024 => "low",
        b if b <= 8192 => "medium",
        b if b <= 24576 => "high",
        _ => "xhigh",
    }
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
        assert_eq!(body["input"][0]["type"], "input_text");
        assert_eq!(body["input"][0]["text"], "hi");
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
        assert_eq!(body["input"][0]["type"], "input_image");
        assert_eq!(body["input"][0]["image_url"], "https://example.com/image.png");
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