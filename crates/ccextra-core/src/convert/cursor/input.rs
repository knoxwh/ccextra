use super::CursorConvertError;
use serde_json::{json, Value};

fn text(content: &Value) -> Result<String, CursorConvertError> {
    match content {
        Value::Null => Ok(String::new()),
        Value::String(text) => Ok(text.clone()),
        Value::Array(parts) => {
            let mut out = String::new();
            for part in parts {
                match part.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        out.push_str(part.get("text").and_then(Value::as_str).unwrap_or(""))
                    }
                    Some("tool_use" | "tool_result" | "thinking" | "redacted_thinking") => {}
                    Some(other) => {
                        return Err(CursorConvertError::Unsupported(format!("输入块 {other}")))
                    }
                    None => {
                        return Err(CursorConvertError::Invalid("content 数组缺少 type".into()))
                    }
                }
            }
            Ok(out)
        }
        _ => Err(CursorConvertError::Invalid(
            "content 必须是文本或块数组".into(),
        )),
    }
}

fn entry(out: &mut String, role: &str, content: &str) {
    if !content.is_empty() {
        out.push_str(role);
        out.push_str(": ");
        out.push_str(content);
        out.push_str("\n\n");
    }
}

pub(super) fn user_text(body: &Value, checkpoint: bool) -> Result<String, CursorConvertError> {
    let system = body
        .get("system")
        .map(text)
        .transpose()?
        .unwrap_or_default();
    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| CursorConvertError::Invalid("缺少 messages 数组".into()))?;
    let mut transcript = String::new();
    if !checkpoint {
        entry(&mut transcript, "SYSTEM", &system);
    }
    for (index, message) in messages.iter().enumerate() {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .ok_or_else(|| CursorConvertError::Invalid("message 缺少 role".into()))?;
        if !matches!(role, "user" | "assistant") {
            return Err(CursorConvertError::Invalid(format!("未知角色 {role}")));
        }
        let content = message.get("content").unwrap_or(&Value::Null);
        let line = text(content)?;
        if !checkpoint || index + 1 == messages.len() {
            entry(
                &mut transcript,
                if role == "assistant" {
                    "ASSISTANT"
                } else {
                    "USER"
                },
                &line,
            );
        }
        if checkpoint {
            continue;
        }
        if let Some(parts) = content.as_array() {
            for part in parts {
                match part.get("type").and_then(Value::as_str) {
                    Some("tool_use") => entry(&mut transcript, "ASSISTANT_TOOL_CALL", &json!({
                        "id": part.get("id"), "name": part.get("name"), "arguments": part.get("input")
                    }).to_string()),
                    Some("tool_result") => entry(&mut transcript, "TOOL_RESULT", &json!({
                        "tool_call_id": part.get("tool_use_id"), "content": part.get("content"),
                        "is_error": part.get("is_error")
                    }).to_string()),
                    _ => {}
                }
            }
        }
    }
    // 单条消息且无工具结果 = 首轮流对话:直接取原文,不拼续接尾巴。
    // system 已并入 transcript,不能作为判别条件(Plus 同场景无尾巴)。
    let single_turn_without_results = messages.len() == 1
        && !messages[0]
            .get("content")
            .and_then(Value::as_array)
            .is_some_and(|parts| {
                parts
                    .iter()
                    .any(|part| part.get("type").and_then(Value::as_str) == Some("tool_result"))
            });
    let mut result = if checkpoint || single_turn_without_results {
        transcript
            .trim_end()
            .strip_prefix("USER: ")
            .unwrap_or(transcript.trim_end())
            .to_string()
    } else {
        format!("{transcript}The above is the previous conversation context including tool call results.\nContinue your response based on this context.\n\nContinue from the conversation above.")
    };
    result.push_str(&output_constraints(body)?);
    Ok(result)
}

fn output_constraints(body: &Value) -> Result<String, CursorConvertError> {
    let mut lines = Vec::new();
    if let Some(tokens) = body
        .get("max_tokens")
        .and_then(Value::as_u64)
        .filter(|&n| n > 0)
    {
        lines.push(format!(
            "Keep the answer within about {tokens} output tokens."
        ));
    }
    if let Some(stop) = body.get("stop_sequences") {
        let stops = stop
            .as_array()
            .ok_or_else(|| CursorConvertError::Invalid("stop_sequences 必须是数组".into()))?;
        let stops: Vec<&str> = stops
            .iter()
            .filter_map(Value::as_str)
            .filter(|s| !s.is_empty())
            .collect();
        if !stops.is_empty() {
            lines.push(format!(
                "Stop before any of these sequences: {}",
                stops.join(", ")
            ));
        }
    }
    if let Some(format) = body.get("response_format") {
        match format.get("type").and_then(Value::as_str) {
            Some("json_object") => lines.push(
                "Return a single valid JSON object and no surrounding prose or code fences.".into(),
            ),
            Some("json_schema") => {
                let schema = format
                    .pointer("/json_schema/schema")
                    .or_else(|| format.get("schema"))
                    .unwrap_or(format);
                lines.push(format!(
                    "Return only valid JSON (no prose or code fences) matching this schema: {}",
                    schema
                ));
            }
            _ => {}
        }
    }
    if lines.is_empty() {
        return Ok(String::new());
    }
    let constraint = format!("\n\nOUTPUT CONSTRAINTS:\n- {}", lines.join("\n- "));
    if constraint.len() > 4096 {
        return Err(CursorConvertError::Unsupported(
            "输出约束超过 4096 字节".into(),
        ));
    }
    Ok(constraint)
}
