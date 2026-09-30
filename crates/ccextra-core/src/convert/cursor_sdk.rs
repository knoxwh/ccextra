// Cursor SDK sidecar 请求转换:anthropic body → /run body
//
// 纯逻辑无 IO。输出字段:model / modelParams / systemPrompt / workspaceDir /
// messages / tools。apiKey 由 server 侧 client 注入,不进 converter。
// 历史 thinking 块剥离(对齐 sidecar stripThinkingBlocks);图片块拦截
// 返回 UnsupportedImage(D4:v1 不支持图片);system 复用 passthrough 的
// 非 Claude 清洗(剥计费归属指纹/身份声明/触发块)。

use std::path::Path;

use serde_json::{json, Map, Value};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CursorSdkConvertError {
    /// v1 不支持图片输入(D4),收到 image 块直接报错
    #[error("unsupported image block in messages")]
    UnsupportedImage,

    /// messages 缺失或不是非空数组
    #[error("messages must be a non-empty array")]
    InvalidMessages,
}

/// 转换为 sidecar /run body
///
/// 输入:归一化后的 anthropic body(含 system / messages / tools)
/// 输出:sidecar /run 请求体(不含 apiKey)
pub fn convert_to_cursor_sdk(
    body: Value,
    upstream_model: &str,
    workspace_dir: &Path,
) -> Result<Value, CursorSdkConvertError> {
    let mut body = body;
    // system 清洗:Cursor 非 Claude 上游,剥归属指纹/身份声明/触发块
    super::passthrough::sanitize_passthrough_prompt(&mut body, upstream_model);
    let system_prompt = flatten_system(body.get("system"));

    let messages = body
        .get("messages")
        .and_then(|m| m.as_array())
        .cloned()
        .ok_or(CursorSdkConvertError::InvalidMessages)?;
    if messages.is_empty() {
        return Err(CursorSdkConvertError::InvalidMessages);
    }
    let messages = strip_thinking_and_guard_images(messages)?;

    // tools 原样透传(Anthropic 形状);缺省输出空数组,与 sidecar 缺省语义一致
    let tools = body
        .get("tools")
        .and_then(|t| t.as_array())
        .cloned()
        .unwrap_or_default();

    Ok(json!({
        "model": upstream_model,
        // v1 固定空参数:入站无标准 modelParams 字段,不从 metadata 解析
        "modelParams": [],
        "systemPrompt": system_prompt,
        "workspaceDir": workspace_dir.to_string_lossy(),
        "messages": messages,
        "tools": tools,
    }))
}

/// 顶层 system(字符串或 text 块数组)压平为单个字符串
///
/// 块数组用空行连接;清洗后无 system 输出空串(sidecar 恒收 systemPrompt)
fn flatten_system(system: Option<&Value>) -> String {
    match system {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n\n"),
        _ => String::new(),
    }
}

/// 递归剥 thinking 块;遇到 image 块返回 UnsupportedImage
///
/// 对齐 sidecar stripThinkingBlocks 的递归语义(含 tool_result 嵌套内容)
fn strip_thinking_and_guard_images(
    messages: Vec<Value>,
) -> Result<Vec<Value>, CursorSdkConvertError> {
    let mut out = Vec::with_capacity(messages.len());
    for message in messages {
        out.push(strip_value(message)?);
    }
    Ok(out)
}

fn strip_value(value: Value) -> Result<Value, CursorSdkConvertError> {
    match value {
        Value::Array(items) => {
            let mut kept = Vec::with_capacity(items.len());
            for item in items {
                let block_type = item.get("type").and_then(|t| t.as_str());
                if block_type == Some("thinking") {
                    continue;
                }
                if block_type == Some("image") {
                    return Err(CursorSdkConvertError::UnsupportedImage);
                }
                kept.push(strip_value(item)?);
            }
            Ok(Value::Array(kept))
        }
        Value::Object(map) => {
            let mut out = Map::with_capacity(map.len());
            for (key, child) in map {
                out.insert(key, strip_value(child)?);
            }
            Ok(Value::Object(out))
        }
        other => Ok(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn workspace() -> std::path::PathBuf {
        std::path::PathBuf::from("/tmp/workspace")
    }

    #[test]
    fn keeps_tools_and_message_order_without_images() {
        let body = json!({
            "model": "composer-2.5",
            "system": "You are helpful.",
            "messages": [
                { "role": "user", "content": [{ "type": "text", "text": "hi" }] },
                { "role": "assistant", "content": [{ "type": "text", "text": "hello" }] },
                { "role": "user", "content": [{ "type": "text", "text": "go on" }] }
            ],
            "tools": [
                { "name": "Bash", "description": "run shell", "input_schema": { "type": "object" } }
            ]
        });
        let out = convert_to_cursor_sdk(body, "composer-2.5", &workspace()).unwrap();
        assert_eq!(out["model"], "composer-2.5");
        assert_eq!(out["systemPrompt"], "You are helpful.");
        assert_eq!(out["workspaceDir"], "/tmp/workspace");
        let messages = out["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["content"][0]["text"], "hi");
        assert_eq!(messages[2]["content"][0]["text"], "go on");
        assert_eq!(out["tools"].as_array().unwrap().len(), 1);
        assert_eq!(out["tools"][0]["name"], "Bash");
    }

    #[test]
    fn strips_history_thinking_blocks() {
        let body = json!({
            "model": "auto",
            "messages": [
                { "role": "user", "content": "hi" },
                { "role": "assistant", "content": [
                    { "type": "thinking", "thinking": "internal" },
                    { "type": "text", "text": "answer" }
                ] },
                { "role": "user", "content": [
                    { "type": "tool_result", "tool_use_id": "t1", "content": [
                        { "type": "thinking", "thinking": "nested" },
                        { "type": "text", "text": "result" }
                    ] }
                ] }
            ]
        });
        let out = convert_to_cursor_sdk(body, "auto", &workspace()).unwrap();
        let messages = out["messages"].as_array().unwrap();
        let assistant = messages[1]["content"].as_array().unwrap();
        assert_eq!(assistant.len(), 1);
        assert_eq!(assistant[0]["type"], "text");
        let tool_result = messages[2]["content"][0]["content"].as_array().unwrap();
        assert_eq!(tool_result.len(), 1);
        assert_eq!(tool_result[0]["text"], "result");
    }

    #[test]
    fn sanitizes_top_level_and_message_system() {
        let body = json!({
            "model": "auto",
            "system": [
                { "type": "text", "text": "x-anthropic-billing-header: abc" },
                { "type": "text", "text": "You are Claude Code, Anthropic's official CLI for Claude." },
                { "type": "text", "text": "Real instructions." }
            ],
            "messages": [
                { "role": "system", "content": "x-anthropic-billing-header: xyz" },
                { "role": "user", "content": "hi" }
            ]
        });
        let out = convert_to_cursor_sdk(body, "auto", &workspace()).unwrap();
        // 归属指纹与身份声明块被剥,只留真实指令
        assert_eq!(out["systemPrompt"], "Real instructions.");
        let messages = out["messages"].as_array().unwrap();
        // 消息内 system 清洗后整条丢弃
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "user");
    }

    #[test]
    fn image_block_returns_unsupported_image() {
        let body = json!({
            "model": "auto",
            "messages": [
                { "role": "user", "content": [
                    { "type": "image", "source": { "type": "base64" } }
                ] }
            ]
        });
        let err = convert_to_cursor_sdk(body, "auto", &workspace()).unwrap_err();
        assert!(matches!(err, CursorSdkConvertError::UnsupportedImage));
    }

    #[test]
    fn image_inside_tool_result_also_rejected() {
        let body = json!({
            "model": "auto",
            "messages": [
                { "role": "user", "content": [
                    { "type": "tool_result", "tool_use_id": "t1", "content": [
                        { "type": "image", "source": { "type": "base64" } }
                    ] }
                ] }
            ]
        });
        let err = convert_to_cursor_sdk(body, "auto", &workspace()).unwrap_err();
        assert!(matches!(err, CursorSdkConvertError::UnsupportedImage));
    }

    #[test]
    fn missing_or_empty_messages_is_invalid() {
        let body = json!({ "model": "auto", "messages": [] });
        let err = convert_to_cursor_sdk(body, "auto", &workspace()).unwrap_err();
        assert!(matches!(err, CursorSdkConvertError::InvalidMessages));

        let body = json!({ "model": "auto" });
        let err = convert_to_cursor_sdk(body, "auto", &workspace()).unwrap_err();
        assert!(matches!(err, CursorSdkConvertError::InvalidMessages));
    }

    #[test]
    fn emits_empty_model_params_and_empty_tools_consistently() {
        let body = json!({
            "model": "auto",
            "messages": [{ "role": "user", "content": "hi" }]
        });
        let out = convert_to_cursor_sdk(body, "auto", &workspace()).unwrap();
        assert_eq!(out["modelParams"], json!([]));
        assert_eq!(out["tools"], json!([]));
        // temperature / top_p / stop_sequences / thinking 不进输出
        for dropped in ["temperature", "top_p", "stop_sequences", "thinking"] {
            assert!(out.get(dropped).is_none());
        }
    }

    #[test]
    fn conversion_is_idempotent_for_same_input() {
        let body = json!({
            "model": "auto",
            "system": "Keep this.",
            "messages": [
                { "role": "user", "content": "hi" },
                { "role": "assistant", "content": [
                    { "type": "thinking", "thinking": "x" },
                    { "type": "text", "text": "answer" }
                ] }
            ],
            "tools": [{ "name": "Bash", "input_schema": { "type": "object" } }]
        });
        let first = convert_to_cursor_sdk(body.clone(), "auto", &workspace()).unwrap();
        let second = convert_to_cursor_sdk(body, "auto", &workspace()).unwrap();
        assert_eq!(first, second);
    }
}
