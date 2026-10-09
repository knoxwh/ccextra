use super::CursorConvertError;
use serde_json::Value;

/// 历史伪造模式末条纯 tool_result 时的续接尾巴(生产验证措辞)
const CONTINUATION_TAIL: &str = "The above is the previous conversation context including tool call results.\nContinue your response based on this context.\n\nContinue from the conversation above.";

pub(super) fn text(content: &Value) -> Result<String, CursorConvertError> {
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
                    Some(
                        "tool_use" | "tool_result" | "thinking" | "redacted_thinking" | "image",
                    ) => {}
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

/// 图片输入(对齐 Plus extractImages:仅 base64 数据,远程 URL 不支持)
pub(super) struct CursorImage {
    pub mime_type: String,
    pub data: String,
}

/// UserText 与 root blob system 的拆分结果(对齐 Plus:system 不进 UserText)
pub(super) struct UserInput {
    pub text: String,
    pub system: String,
    pub images: Vec<CursorImage>,
}

/// 提取 content 数组顶层的 image 块(对齐 Plus:仅扫 user 消息,tool_result 内图片不收)
fn extract_images(content: &Value) -> Vec<CursorImage> {
    let parts = match content.as_array() {
        Some(parts) => parts,
        None => return Vec::new(),
    };
    parts
        .iter()
        .filter(|part| part.get("type").and_then(Value::as_str) == Some("image"))
        .filter_map(|part| {
            let source = part.get("source")?;
            if source.get("type").and_then(Value::as_str) != Some("base64") {
                return None;
            }
            let data = source.get("data").and_then(Value::as_str)?;
            if data.is_empty() {
                return None;
            }
            let mime_type = source
                .get("media_type")
                .or_else(|| source.get("mime_type"))
                .and_then(Value::as_str)
                .unwrap_or("application/octet-stream")
                .to_string();
            Some(CursorImage {
                mime_type,
                data: data.to_string(),
            })
        })
        .collect()
}

pub(super) fn user_text(body: &Value, checkpoint: bool) -> Result<UserInput, CursorConvertError> {
    // 对齐 Plus:顶层 system 与 messages 内 system 消息合并为 root blob 的 system prompt,
    // system 不进 UserText、不算对话轮;无 system 时兜底默认提示词
    let mut system_parts: Vec<String> = Vec::new();
    if let Some(system) = body.get("system").map(text).transpose()? {
        if !system.is_empty() {
            system_parts.push(system);
        }
    }
    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| CursorConvertError::Invalid("缺少 messages 数组".into()))?;
    for message in messages {
        if message.get("role").and_then(Value::as_str) == Some("system") {
            let line = text(message.get("content").unwrap_or(&Value::Null))?;
            if !line.is_empty() {
                system_parts.push(line);
            }
        }
    }
    let system = if system_parts.is_empty() {
        "You are a helpful assistant.".to_string()
    } else {
        system_parts.join("\n")
    };
    let mut last_text = String::new();
    let mut images = Vec::new();
    // 只取最后一条非 system 消息的文本:checkpoint 模式现状;无 checkpoint 的
    // 多轮会话走历史伪造(history::fabricate 编 root prompt blob),历史不再
    // 拼进 UserText。Claude Code 会在 user 后追加 system reminder,末条常是
    // system,不能让它挤掉真正的用户输入
    let last_conversation = messages
        .iter()
        .rposition(|message| message.get("role").and_then(Value::as_str) != Some("system"));
    for (index, message) in messages.iter().enumerate() {
        let role = message
            .get("role")
            .and_then(Value::as_str)
            .ok_or_else(|| CursorConvertError::Invalid("message 缺少 role".into()))?;
        if role == "system" {
            continue;
        }
        if !matches!(role, "user" | "assistant") {
            return Err(CursorConvertError::Invalid(format!("未知角色 {role}")));
        }
        let content = message.get("content").unwrap_or(&Value::Null);
        let line = text(content)?;
        if role == "user" {
            // 对齐 Plus:每条 user 消息覆盖,最终保留最后一条的图片
            images = extract_images(content);
        }
        if Some(index) == last_conversation {
            last_text = line;
        }
    }
    // 单条对话消息且无工具结果 = 首轮流对话:直接取原文,不拼续接尾巴。
    // system 已并入 root blob,不算对话轮(Plus 同场景无尾巴)。
    let conversation: Vec<&Value> = messages
        .iter()
        .filter(|message| message.get("role").and_then(Value::as_str) != Some("system"))
        .collect();
    let single_turn_without_results = conversation.len() == 1
        && !conversation[0]
            .get("content")
            .and_then(Value::as_array)
            .is_some_and(|parts| {
                parts
                    .iter()
                    .any(|part| part.get("type").and_then(Value::as_str) == Some("tool_result"))
            });
    let mut result = if !single_turn_without_results && !checkpoint && last_text.is_empty() {
        // 历史伪造模式末条纯 tool_result:历史已编入 root prompt blob,
        // UserText 用续接尾巴引导模型继续
        CONTINUATION_TAIL.to_string()
    } else {
        last_text
    };
    result.push_str(&output_constraints(body)?);
    Ok(UserInput {
        text: result,
        system,
        images,
    })
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
