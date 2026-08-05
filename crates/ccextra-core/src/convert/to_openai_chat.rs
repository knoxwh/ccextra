// Anthropic → OpenAI chat/completions 转换
//
// 参考 CPA internal/translator/openai/claude/openai_claude_request.go (500 行)。
// 请求侧采用 CPA 完整字段映射(ai-gateway 的 Standard 中间层有损:cache_control/
// image 丢失,故不采用)。
//
// 主要映射:
// - system → messages[0] {role: system}
// - thinking.budget_tokens → reasoning_effort
// - content 块逐项转换:thinking→reasoning_content, image→data URL,
//   tool_use→tool_calls, tool_result→role=tool(tool_result 先发保相邻)
// - tools input_schema → parameters
// - tool_choice 映射
// - stop_sequences → stop

use serde_json::{json, Value};

use super::{ConvertError, Result};

/// Anthropic messages → OpenAI chat/completions
pub fn convert_to_openai_chat(body: &mut Value, upstream_model: &str) -> Result<()> {
    let mut openai = json!({
        "model": upstream_model,
        "messages": [],
        "stream": body.get("stream").unwrap_or(&json!(true)).clone(),
    });

    // System → messages[0]
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
        if !text.is_empty() {
            openai["messages"]
                .as_array_mut()
                .unwrap()
                .push(json!({"role": "system", "content": text}));
        }
    }

    // thinking → reasoning_effort
    if let Some(effort) = thinking_to_effort(body.get("thinking")) {
        openai["reasoning_effort"] = json!(effort);
    }

    // Messages 逐条转换
    if let Some(messages) = body.get("messages").and_then(|v| v.as_array()) {
        for msg in messages {
            let role = msg
                .get("role")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ConvertError::MissingField("role".into()))?;
            let content = msg.get("content").cloned().unwrap_or(json!(""));

            // 单条消息转换结果(可能展开成多条:tool_result 在前)
            let converted = convert_message(role, &content)?;
            for m in converted {
                openai["messages"].as_array_mut().unwrap().push(m);
            }
        }
    }

    // Tools: input_schema → parameters
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
                "function": {
                    "name": name,
                    "description": description,
                    "parameters": {"type": "object", "properties": {}}
                }
            });
            if let Some(schema) = tool.get("input_schema") {
                fn_obj["function"]["parameters"] = schema.clone();
            }
            openai_tools.push(fn_obj);
        }
        openai["tools"] = json!(openai_tools);
    }

    // tool_choice 映射
    if let Some(tc) = body.get("tool_choice") {
        if let Some(choice) = convert_tool_choice(tc) {
            openai["tool_choice"] = choice;
        }
    }

    // stop_sequences → stop
    if let Some(seqs) = body.get("stop_sequences").and_then(|v| v.as_array()) {
        let stops: Vec<&str> = seqs.iter().filter_map(|s| s.as_str()).collect();
        if !stops.is_empty() {
            openai["stop"] = json!(stops);
        }
    }

    // 其他参数透传
    for key in &["max_tokens", "max_output_tokens", "temperature", "top_p"] {
        if let Some(val) = body.get(*key) {
            openai[key] = val.clone();
        }
    }

    *body = openai;
    Ok(())
}

/// thinking.budget_tokens → reasoning_effort level
fn thinking_to_effort(thinking: Option<&Value>) -> Option<&'static str> {
    let thinking = thinking?;
    if !thinking.is_object() {
        return None;
    }
    let ty = thinking.get("type").and_then(|v| v.as_str())?;
    let budget = match ty {
        "enabled" => thinking
            .get("budget_tokens")
            .and_then(|v| v.as_i64())
            .unwrap_or(-1),
        "disabled" => 0,
        "adaptive" => thinking
            .get("budget_tokens")
            .and_then(|v| v.as_i64())
            .unwrap_or(-1),
        _ => return None,
    };
    Some(budget_to_effort(budget))
}

/// 预算 → 级别(与 CPA ConvertBudgetToLevel 一致)
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

/// 转换单条消息,返回一个或多个 openai 消息(tool_result 展开在前)
fn convert_message(role: &str, content: &Value) -> Result<Vec<Value>> {
    // 字符串内容
    if let Some(s) = content.as_str() {
        return Ok(vec![json!({"role": role, "content": s})]);
    }

    // 空内容
    if content.is_null() {
        return Ok(vec![json!({"role": role, "content": ""})]);
    }

    let Some(parts) = content.as_array() else {
        return Err(ConvertError::InvalidType("content".into()));
    };

    let mut content_items: Vec<Value> = Vec::new();
    let mut reasoning_parts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    let mut tool_results: Vec<Value> = Vec::new();

    for part in parts {
        let ptype = part.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match ptype {
            "thinking" => {
                // 仅 assistant 映射(防注入);非 assistant 忽略
                if role == "assistant" {
                    if let Some(t) = part.get("thinking").and_then(|v| v.as_str()) {
                        if !t.trim().is_empty() {
                            reasoning_parts.push(t.to_string());
                        }
                    }
                }
            }
            "redacted_thinking" => { /* 显式忽略 */ }
            "text" => {
                if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                    content_items.push(json!({"type": "text", "text": t}));
                }
            }
            "image" => {
                if let Some(url) = image_to_url(part) {
                    content_items.push(json!({
                        "type": "image_url",
                        "image_url": {"url": url}
                    }));
                }
            }
            "tool_use" => {
                // 仅 assistant 允许
                if role == "assistant" {
                    let id = part.get("id").and_then(|v| v.as_str()).unwrap_or("");
                    let name = part.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    let args = part
                        .get("input")
                        .map(|i| i.to_string())
                        .unwrap_or_else(|| "{}".to_string());
                    tool_calls.push(json!({
                        "id": id,
                        "type": "function",
                        "function": {"name": name, "arguments": args}
                    }));
                }
            }
            "tool_result" => {
                let tool_call_id = part
                    .get("tool_use_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let content_val = part.get("content").cloned().unwrap_or(json!(""));
                tool_results.push(json!({
                    "role": "tool",
                    "tool_call_id": tool_call_id,
                    "content": content_val
                }));
            }
            _ => {}
        }
    }

    let mut out = Vec::new();
    // tool_result 先发(紧跟上一轮 assistant 的 tool_calls)
    out.extend(tool_results);

    // assistant:单消息合并 content + reasoning_content + tool_calls
    if role == "assistant" {
        let has_content = !content_items.is_empty();
        let has_reasoning = !reasoning_parts.is_empty();
        let has_tool_calls = !tool_calls.is_empty();
        if has_content || has_reasoning || has_tool_calls {
            let mut msg = json!({"role": "assistant"});
            if has_content {
                msg["content"] = json!(content_items);
            } else {
                msg["content"] = json!("");
            }
            if has_reasoning {
                msg["reasoning_content"] = json!(reasoning_parts.join("\n\n"));
            }
            if has_tool_calls {
                msg["tool_calls"] = json!(tool_calls);
            }
            out.push(msg);
        }
    } else if !content_items.is_empty() {
        // 非 assistant(通常 user):纯 content
        out.push(json!({"role": role, "content": content_items}));
    } else if role == "user" && content.is_null() {
        out.push(json!({"role": role, "content": ""}));
    }

    Ok(out)
}

/// image block → data URL(base64)或 url
fn image_to_url(part: &Value) -> Option<String> {
    let source = part.get("source")?;
    let media_type = source.get("media_type").and_then(|v| v.as_str()).unwrap_or("image/png");
    match source.get("type").and_then(|v| v.as_str()) {
        Some("base64") => {
            let data = source.get("data").and_then(|v| v.as_str())?;
            Some(format!("data:{media_type};base64,{data}"))
        }
        Some("url") => source.get("url").and_then(|v| v.as_str()).map(|s| s.to_string()),
        _ => None,
    }
}

/// tool_choice 映射:auto→auto, any→required, tool→specific function
fn convert_tool_choice(tc: &Value) -> Option<Value> {
    let ty = tc.get("type").and_then(|v| v.as_str())?;
    match ty {
        "auto" => Some(json!("auto")),
        "any" => Some(json!("required")),
        "none" => Some(json!("none")),
        "tool" => {
            let name = tc
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            Some(json!({"type": "function", "function": {"name": name}}))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_conversion() {
        let mut body = json!({
            "model": "evol-opus-5",
            "messages": [{"role": "user", "content": "hello"}],
            "max_tokens": 1024
        });
        convert_to_openai_chat(&mut body, "gpt-4").unwrap();
        assert_eq!(body["model"], "gpt-4");
        assert_eq!(body["messages"][0]["role"], "user");
    }

    #[test]
    fn test_system_string() {
        let mut body = json!({
            "model": "test",
            "system": "You are helpful",
            "messages": [{"role": "user", "content": "hi"}]
        });
        convert_to_openai_chat(&mut body, "gpt").unwrap();
        assert_eq!(body["messages"][0]["role"], "system");
        assert_eq!(body["messages"][0]["content"], "You are helpful");
    }

    #[test]
    fn test_thinking_budget_to_effort() {
        let mut body = json!({
            "model": "test",
            "thinking": {"type": "enabled", "budget_tokens": 20000},
            "messages": []
        });
        convert_to_openai_chat(&mut body, "gpt").unwrap();
        assert_eq!(body["reasoning_effort"], "high");
    }

    #[test]
    fn test_tool_use_and_result() {
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "user", "content": "weather please"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "get_weather", "input": {"city": "beijing"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": "sunny"}
                ]}
            ]
        });
        convert_to_openai_chat(&mut body, "gpt").unwrap();
        let msgs = body["messages"].as_array().unwrap();
        // [user, assistant(tool_calls), tool] —— tool_result 紧跟 assistant tool_calls
        assert_eq!(msgs[1]["role"], "assistant");
        assert_eq!(msgs[1]["tool_calls"][0]["function"]["name"], "get_weather");
        assert_eq!(msgs[2]["role"], "tool");
        assert_eq!(msgs[2]["tool_call_id"], "t1");
    }

    #[test]
    fn test_image_base64() {
        let mut body = json!({
            "model": "test",
            "messages": [{"role": "user", "content": [
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}}
            ]}]
        });
        convert_to_openai_chat(&mut body, "gpt").unwrap();
        let c = &body["messages"][0]["content"][0];
        assert_eq!(c["type"], "image_url");
        assert_eq!(c["image_url"]["url"], "data:image/png;base64,AAAA");
    }

    #[test]
    fn test_tools_schema() {
        let mut body = json!({
            "model": "test",
            "messages": [],
            "tools": [{
                "name": "t",
                "description": "tool",
                "input_schema": {"type": "object", "properties": {"a": {"type": "string"}}}
            }]
        });
        convert_to_openai_chat(&mut body, "gpt").unwrap();
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(body["tools"][0]["function"]["parameters"]["properties"]["a"]["type"], "string");
    }

    #[test]
    fn test_system_array_with_empty_blocks() {
        let mut body = json!({
            "model": "test",
            "system": [
                {"type": "text", "text": "You are helpful"},
                {"type": "text", "text": ""},
                {"type": "text", "text": "Answer concisely"}
            ],
            "messages": []
        });
        convert_to_openai_chat(&mut body, "gpt").unwrap();
        // 空块也应处理,用 \n\n 连接
        let system_text = body["messages"][0]["content"].as_str().unwrap();
        assert!(system_text.contains("You are helpful"));
        assert!(system_text.contains("Answer concisely"));
    }

    #[test]
    fn test_redacted_thinking_ignored() {
        let mut body = json!({
            "model": "test",
            "messages": [{
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "answer"},
                    {"type": "redacted_thinking"}
                ]
            }]
        });
        convert_to_openai_chat(&mut body, "gpt").unwrap();
        // redacted_thinking 应被显式忽略
        let msg = &body["messages"][0];
        assert_eq!(msg["content"][0]["text"], "answer");
        assert_eq!(msg["content"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn test_empty_messages_array() {
        let mut body = json!({
            "model": "test",
            "messages": []
        });
        convert_to_openai_chat(&mut body, "gpt").unwrap();
        assert_eq!(body["messages"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn test_empty_tools_array() {
        let mut body = json!({
            "model": "test",
            "messages": [],
            "tools": []
        });
        convert_to_openai_chat(&mut body, "gpt").unwrap();
        assert_eq!(body["tools"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn test_tool_choice_mappings() {
        let mut body = json!({
            "model": "test",
            "messages": [],
            "tool_choice": {"type": "auto"}
        });
        convert_to_openai_chat(&mut body, "gpt").unwrap();
        assert_eq!(body["tool_choice"], "auto");

        let mut body2 = json!({
            "model": "test",
            "messages": [],
            "tool_choice": {"type": "any"}
        });
        convert_to_openai_chat(&mut body2, "gpt").unwrap();
        assert_eq!(body2["tool_choice"], "required");

        let mut body3 = json!({
            "model": "test",
            "messages": [],
            "tool_choice": {"type": "tool", "name": "search"}
        });
        convert_to_openai_chat(&mut body3, "gpt").unwrap();
        assert_eq!(body3["tool_choice"]["type"], "function");
        assert_eq!(body3["tool_choice"]["function"]["name"], "search");
    }
}