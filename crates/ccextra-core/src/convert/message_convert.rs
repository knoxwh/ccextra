// Anthropic messages → Gemini contents 转换

use serde_json::{json, Value};
use std::collections::HashMap;

/// 转换 Anthropic messages 为 Gemini contents
///
/// 注意：system 消息已在调用方独立处理，这里只处理 user/assistant 消息
pub fn convert_messages(
    messages: &[Value],
    original_to_claude_id: &HashMap<String, String>,
) -> Vec<Value> {
    let mut contents = Vec::new();

    for msg in messages {
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");

        match role {
            "user" => {
                let parts = convert_content_to_parts(&msg["content"], original_to_claude_id);
                if !parts.is_empty() {
                    contents.push(json!({
                        "role": "user",
                        "parts": parts
                    }));
                }
            }
            "assistant" => {
                let parts = convert_content_to_parts(&msg["content"], original_to_claude_id);
                if !parts.is_empty() {
                    contents.push(json!({
                        "role": "model",
                        "parts": parts
                    }));
                }
            }
            _ => {}
        }
    }

    contents
}

/// 转换 content 为 Gemini parts
fn convert_content_to_parts(
    content: &Value,
    original_to_claude_id: &HashMap<String, String>,
) -> Vec<Value> {
    let mut parts = Vec::new();

    match content {
        Value::String(text) => {
            parts.push(json!({
                "text": text
            }));
        }
        Value::Array(blocks) => {
            for block in blocks {
                if let Some(part) = convert_block_to_part(block, original_to_claude_id) {
                    parts.push(part);
                }
            }
        }
        _ => {}
    }

    parts
}

/// 转换单个 content block 为 Gemini part
fn convert_block_to_part(
    block: &Value,
    original_to_claude_id: &HashMap<String, String>,
) -> Option<Value> {
    let block_type = block.get("type")?.as_str()?;

    match block_type {
        "text" => {
            let text = block.get("text")?.as_str()?;
            Some(json!({ "text": text }))
        }
        "thinking" => {
            // Thinking 块转为带前缀的文本
            let thinking = block.get("thinking")?.as_str()?;
            Some(json!({
                "text": format!("[Thinking]\n{}", thinking)
            }))
        }
        "tool_use" => {
            // Tool use 转为 functionCall
            let name = block.get("name")?.as_str()?;
            let claude_id = original_to_claude_id.get(name)?;
            let default_input = json!({});
            let input = block.get("input").unwrap_or(&default_input);

            Some(json!({
                "functionCall": {
                    "name": claude_id,
                    "args": input
                }
            }))
        }
        "tool_result" => {
            // Tool result 转为 functionResponse
            let tool_use_id = block.get("tool_use_id")?.as_str()?;
            let default_content = json!([]);
            let content = block.get("content").unwrap_or(&default_content);

            // 提取文本内容
            let text = if let Some(s) = content.as_str() {
                s.to_string()
            } else if let Some(arr) = content.as_array() {
                arr.iter()
                    .filter_map(|b| b.get("text")?.as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            } else {
                "".to_string()
            };

            Some(json!({
                "functionResponse": {
                    "name": tool_use_id,
                    "response": {
                        "result": text
                    }
                }
            }))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_convert_messages_basic() {
        let messages = vec![
            json!({
                "role": "user",
                "content": "Hello"
            }),
            json!({
                "role": "assistant",
                "content": "Hi there"
            }),
        ];

        let tool_map = HashMap::new();
        let contents = convert_messages(&messages, &tool_map);

        assert_eq!(contents.len(), 2);
        assert_eq!(contents[0]["role"], "user");
        assert_eq!(contents[0]["parts"][0]["text"], "Hello");
        assert_eq!(contents[1]["role"], "model");
        assert_eq!(contents[1]["parts"][0]["text"], "Hi there");
    }

    #[test]
    fn test_convert_messages_with_thinking() {
        let messages = vec![json!({
            "role": "assistant",
            "content": [
                {
                    "type": "thinking",
                    "thinking": "Let me think..."
                },
                {
                    "type": "text",
                    "text": "Here's my answer"
                }
            ]
        })];

        let tool_map = HashMap::new();
        let contents = convert_messages(&messages, &tool_map);

        assert_eq!(contents.len(), 1);
        let parts = contents[0]["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert!(parts[0]["text"].as_str().unwrap().starts_with("[Thinking]"));
        assert_eq!(parts[1]["text"], "Here's my answer");
    }

    #[test]
    fn test_convert_messages_with_tool_use() {
        let mut tool_map = HashMap::new();
        tool_map.insert("Read".to_string(), "cpa_gemini_abc123".to_string());

        let messages = vec![json!({
            "role": "assistant",
            "content": [
                {
                    "type": "text",
                    "text": "Let me read that"
                },
                {
                    "type": "tool_use",
                    "id": "call_1",
                    "name": "Read",
                    "input": {
                        "path": "/file.txt"
                    }
                }
            ]
        })];

        let contents = convert_messages(&messages, &tool_map);

        assert_eq!(contents.len(), 1);
        let parts = contents[0]["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[1]["functionCall"]["name"], "cpa_gemini_abc123");
        assert_eq!(parts[1]["functionCall"]["args"]["path"], "/file.txt");
    }

    #[test]
    fn test_convert_messages_with_tool_result() {
        let messages = vec![json!({
            "role": "user",
            "content": [
                {
                    "type": "tool_result",
                    "tool_use_id": "call_1",
                    "content": "File contents here"
                }
            ]
        })];

        let tool_map = HashMap::new();
        let contents = convert_messages(&messages, &tool_map);

        assert_eq!(contents.len(), 1);
        let parts = contents[0]["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0]["functionResponse"]["name"], "call_1");
        assert_eq!(
            parts[0]["functionResponse"]["response"]["result"],
            "File contents here"
        );
    }

    #[test]
    fn test_convert_messages_ignores_system() {
        let messages = vec![
            json!({
                "role": "system",
                "content": "You are a helpful assistant"
            }),
            json!({
                "role": "user",
                "content": "Hello"
            }),
        ];

        let tool_map = HashMap::new();
        let contents = convert_messages(&messages, &tool_map);

        // system 消息被忽略，只有 user 消息
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0]["role"], "user");
    }
}
