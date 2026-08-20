// Anthropic messages → Gemini contents 转换(对齐 CLIProxyAPI convertClaudeRequestToGemini)

use serde_json::{json, Value};
use std::collections::HashMap;

use super::is_attribution_text;
use super::tool_id::tool_name_from_claude_tool_use_id;
use super::tool_sanitize::sanitize_function_name;

/// functionCall 附加的思考签名哨兵(对齐 CPA geminiClaudeThoughtSignature)
const THOUGHT_SIGNATURE_SENTINEL: &str = "skip_thought_signature_validator";

/// 转换 Anthropic messages 为 Gemini contents
///
/// original_to_short: 原始工具名 → 上游清洗短名(本轮 tools 声明)
/// antigravity: Antigravity 语义(保留带非空 signature 的 thinking 块,
/// 工具图嵌进 functionResponse.parts);Gemini 直连保持兄弟 inline_data
pub fn convert_messages(
    messages: &[Value],
    original_to_short: &HashMap<String, String>,
    antigravity: bool,
) -> Vec<Value> {
    let mut contents = Vec::new();

    for msg in messages {
        let role = msg.get("role").and_then(|r| r.as_str()).unwrap_or("");

        match role {
            "user" => {
                let parts =
                    convert_content_to_parts(&msg["content"], original_to_short, antigravity);
                if !parts.is_empty() {
                    contents.push(json!({ "role": "user", "parts": parts }));
                }
            }
            "assistant" => {
                let parts =
                    convert_content_to_parts(&msg["content"], original_to_short, antigravity);
                if !parts.is_empty() {
                    contents.push(json!({ "role": "model", "parts": parts }));
                }
            }
            // system 消息 → user 角色的 system-reminder 文本(对齐 ClaudeMessageSystemReminderText)
            "system" => {
                if let Some(reminder) = system_reminder_text(&msg["content"]) {
                    contents.push(json!({
                        "role": "user",
                        "parts": [{ "text": reminder }]
                    }));
                }
            }
            _ => {}
        }
    }

    // 剥离尾部带未应答 functionCall 的 model 回合(对齐 CPA)
    if let Some(last) = contents.last() {
        let is_dangling_model_turn = last.get("role").and_then(|r| r.as_str()) == Some("model")
            && last
                .get("parts")
                .and_then(|p| p.as_array())
                .is_some_and(|parts| parts.iter().any(|p| p.get("functionCall").is_some()));
        if is_dangling_model_turn {
            contents.pop();
        }
    }

    contents
}

/// 提取 system 消息文本并包成 <system-reminder>...</system-reminder>
/// 空文本与 attribution 文本跳过;无有效内容返回 None
fn system_reminder_text(content: &Value) -> Option<String> {
    let mut parts: Vec<&str> = Vec::new();
    match content {
        Value::String(s) => {
            if !s.is_empty() && !is_attribution_text(s) {
                parts.push(s);
            }
        }
        Value::Array(arr) => {
            for item in arr {
                if item.get("type").and_then(|t| t.as_str()) != Some("text") {
                    continue;
                }
                if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                    if !text.is_empty() && !is_attribution_text(text) {
                        parts.push(text);
                    }
                }
            }
        }
        _ => return None,
    }
    let joined = parts.join("\n");
    if joined.trim().is_empty() {
        return None;
    }
    Some(format!("<system-reminder>\n{}\n</system-reminder>", joined))
}

/// 转换 content 为 Gemini parts
fn convert_content_to_parts(
    content: &Value,
    original_to_short: &HashMap<String, String>,
    antigravity: bool,
) -> Vec<Value> {
    let mut parts = Vec::new();

    match content {
        Value::String(text) => {
            if !text.is_empty() {
                parts.push(json!({ "text": text }));
            }
        }
        Value::Array(blocks) => {
            for block in blocks {
                append_block_parts(&mut parts, block, original_to_short, antigravity);
            }
        }
        _ => {}
    }

    parts
}

/// 单个 content block → 0..n 个 Gemini parts(tool_result 可能追加图片 part)
fn append_block_parts(
    parts: &mut Vec<Value>,
    block: &Value,
    original_to_short: &HashMap<String, String>,
    antigravity: bool,
) {
    let Some(block_type) = block.get("type").and_then(|t| t.as_str()) else {
        return;
    };

    match block_type {
        "text" => {
            // 空文本 part 跳过(对齐 CPA:规避 Gemini required oneof field 'data' 报错)
            if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                if !text.is_empty() {
                    parts.push(json!({ "text": text }));
                }
            }
        }
        // thinking 块默认丢弃(Gemini 非 compat 路径);Antigravity 保留带签名块
        // (对齐 CPA antigravity 翻译器,空签名块跳过不转文本)
        "thinking" => {
            if !antigravity {
                return;
            }
            let text = block.get("thinking").and_then(|t| t.as_str()).unwrap_or("");
            let signature = block
                .get("signature")
                .and_then(|s| s.as_str())
                .unwrap_or("");
            if text.is_empty() || signature.is_empty() {
                return;
            }
            parts.push(json!({
                "thought": true,
                "text": text,
                "thoughtSignature": signature
            }));
        }
        "tool_use" => {
            // name 经清洗后必须匹配本轮 tools 声明(短名),否则上游无法配对
            let Some(name) = block.get("name").and_then(|n| n.as_str()) else {
                return;
            };
            let short = original_to_short
                .get(name)
                .cloned()
                .unwrap_or_else(|| sanitize_function_name(name));
            // args 归一化(对齐 CPA:null 补 {};JSON 字符串先 parse;非对象兜底 {})
            let raw_input = block.get("input").cloned().unwrap_or_else(|| json!({}));
            let input = match raw_input {
                Value::Null => json!({}),
                Value::String(s) => serde_json::from_str(&s)
                    .ok()
                    .filter(|v: &Value| v.is_object())
                    .unwrap_or_else(|| json!({})),
                v if v.is_object() => v,
                _ => json!({}),
            };
            parts.push(json!({
                "thoughtSignature": THOUGHT_SIGNATURE_SENTINEL,
                "functionCall": {
                    "name": short,
                    "args": input
                }
            }));
        }
        "tool_result" => {
            let Some(tool_use_id) = block.get("tool_use_id").and_then(|i| i.as_str()) else {
                return;
            };
            // 从 id 反解工具名(对齐 toolNameFromClaudeToolUseID),保证与
            // 模型发出的 functionCall.name 一致;反解失败兜底用 id 本身
            let mut func_name = tool_name_from_claude_tool_use_id(tool_use_id);
            if func_name.is_empty() {
                func_name = tool_use_id.to_string();
            }
            let func_name = original_to_short
                .get(func_name.as_str())
                .cloned()
                .unwrap_or_else(|| sanitize_function_name(&func_name));

            let (result, images) = convert_tool_result_content(block.get("content"));
            // 对齐 CPA fixCLIToolResponse 归一化:Antigravity 把工具图嵌进
            // functionResponse.parts(inlineData 驼峰 + 显式 mimeType,缺省
            // image/png——Cloud Code Assist 忽略无 mimeType 的 inlineData),
            // Gemini 直连保持 sibling inline_data
            if antigravity && !images.is_empty() {
                let image_parts: Vec<Value> = images
                    .into_iter()
                    .map(|(mime_type, data)| {
                        let mime_type = if mime_type.is_empty() {
                            "image/png".to_string()
                        } else {
                            mime_type
                        };
                        json!({
                            "inlineData": { "mimeType": mime_type, "data": data }
                        })
                    })
                    .collect();
                parts.push(json!({
                    "functionResponse": {
                        "name": func_name,
                        "response": { "result": result },
                        "parts": image_parts
                    }
                }));
            } else {
                parts.push(json!({
                    "functionResponse": {
                        "name": func_name,
                        "response": { "result": result }
                    }
                }));
                for (mime_type, data) in images {
                    parts.push(json!({
                        "inline_data": { "mime_type": mime_type, "data": data }
                    }));
                }
            }
        }
        "image" => {
            // base64 图片 → inline_data(对齐 CPA)
            let source = block.get("source").cloned().unwrap_or_else(|| json!({}));
            if source.get("type").and_then(|t| t.as_str()) != Some("base64") {
                return;
            }
            let mime = source
                .get("media_type")
                .and_then(|m| m.as_str())
                .unwrap_or("");
            let data = source.get("data").and_then(|d| d.as_str()).unwrap_or("");
            if mime.is_empty() || data.is_empty() {
                return;
            }
            parts.push(json!({
                "inline_data": { "mime_type": mime, "data": data }
            }));
        }
        _ => {}
    }
}

/// tool_result content 归一化(对齐 CPA ConvertClaudeToolResultContent):
/// - 字符串 → 原样
/// - 单个非图块 → 该块 raw JSON
/// - 多个非图块 → raw JSON 数组
/// - base64 图片块 → 拆出为 inline_data parts
/// - 对象 → raw JSON
/// - 缺失/空 → 空串
fn convert_tool_result_content(content: Option<&Value>) -> (Value, Vec<(String, String)>) {
    let Some(content) = content else {
        return (json!(""), Vec::new());
    };

    match content {
        Value::String(s) => (json!(s), Vec::new()),
        Value::Array(arr) => {
            let mut images = Vec::new();
            let mut non_image: Vec<&Value> = Vec::new();
            for block in arr {
                if let Some(img) = base64_image_data(block) {
                    images.push(img);
                } else {
                    non_image.push(block);
                }
            }
            let result = match non_image.len() {
                0 => json!(""),
                1 => non_image[0].clone(),
                _ => json!(non_image),
            };
            (result, images)
        }
        Value::Object(_) => {
            if let Some(img) = base64_image_data(content) {
                return (json!(""), vec![img]);
            }
            (content.clone(), Vec::new())
        }
        other => (other.clone(), Vec::new()),
    }
}

/// 提取 base64 图片块的 (mime_type, data);非 base64 图片或无数据返回 None
fn base64_image_data(block: &Value) -> Option<(String, String)> {
    if block.get("type").and_then(|t| t.as_str()) != Some("image") {
        return None;
    }
    if block.pointer("/source/type").and_then(|t| t.as_str()) != Some("base64") {
        return None;
    }
    let data = block.pointer("/source/data").and_then(|d| d.as_str())?;
    if data.is_empty() {
        return None;
    }
    let mime = block
        .pointer("/source/media_type")
        .and_then(|m| m.as_str())
        .unwrap_or("");
    Some((mime.to_string(), data.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_convert_messages_basic() {
        let messages = vec![
            json!({"role": "user", "content": "Hello"}),
            json!({"role": "assistant", "content": "Hi there"}),
        ];
        let map = HashMap::new();
        let contents = convert_messages(&messages, &map, false);

        assert_eq!(contents.len(), 2);
        assert_eq!(contents[0]["role"], "user");
        assert_eq!(contents[0]["parts"][0]["text"], "Hello");
        assert_eq!(contents[1]["role"], "model");
        assert_eq!(contents[1]["parts"][0]["text"], "Hi there");
    }

    #[test]
    fn test_convert_messages_empty_text_skipped() {
        let messages = vec![json!({
            "role": "user",
            "content": [{"type": "text", "text": ""}, {"type": "text", "text": "hi"}]
        })];
        let contents = convert_messages(&messages, &HashMap::new(), false);
        let parts = contents[0]["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0]["text"], "hi");
    }

    #[test]
    fn test_convert_messages_thinking_dropped() {
        let messages = vec![json!({
            "role": "assistant",
            "content": [
                {"type": "thinking", "thinking": "Let me think..."},
                {"type": "text", "text": "answer"}
            ]
        })];
        let contents = convert_messages(&messages, &HashMap::new(), false);
        let parts = contents[0]["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0]["text"], "answer");
    }

    #[test]
    fn test_convert_messages_system_reminder() {
        let messages = vec![
            json!({"role": "system", "content": [{"type": "text", "text": "permissions note"}]}),
            json!({"role": "system", "content": "x-anthropic-billing-header: abc"}),
        ];
        let contents = convert_messages(&messages, &HashMap::new(), false);
        // attribution 文本的 system 消息被整体跳过
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0]["role"], "user");
        let text = contents[0]["parts"][0]["text"].as_str().unwrap();
        assert!(text.starts_with("<system-reminder>"));
        assert!(text.contains("permissions note"));
    }

    #[test]
    fn test_convert_messages_tool_use_uses_short_name() {
        let mut map = HashMap::new();
        map.insert("Read".to_string(), "Read".to_string());

        let messages = vec![
            json!({
                "role": "assistant",
                "content": [{
                    "type": "tool_use",
                    "id": "Read-3",
                    "name": "Read",
                    "input": {"path": "/file.txt"}
                }]
            }),
            // 尾部追加 user 回合,避免触发未应答 functionCall 剥离
            json!({
                "role": "user",
                "content": [{"type": "tool_result", "tool_use_id": "Read-3", "content": "ok"}]
            }),
        ];
        let contents = convert_messages(&messages, &map, false);
        let parts = contents[0]["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 1);
        // 对齐 CPA:name 用声明短名,附加签名哨兵
        assert_eq!(parts[0]["functionCall"]["name"], "Read");
        assert_eq!(
            parts[0]["thoughtSignature"],
            "skip_thought_signature_validator"
        );
        assert_eq!(parts[0]["functionCall"]["args"]["path"], "/file.txt");
    }

    #[test]
    fn test_convert_messages_tool_result_derives_name_from_id() {
        let messages = vec![json!({
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": "mcp__x__query-docs-12",
                "content": "File contents here"
            }]
        })];
        let contents = convert_messages(&messages, &HashMap::new(), false);
        let parts = contents[0]["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0]["functionResponse"]["name"], "mcp__x__query-docs");
        assert_eq!(
            parts[0]["functionResponse"]["response"]["result"],
            "File contents here"
        );
    }

    #[test]
    fn test_convert_messages_tool_result_with_image() {
        let messages = vec![json!({
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": "Read-1",
                "content": [
                    {"type": "text", "text": "see image"},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}}
                ]
            }]
        })];
        let contents = convert_messages(&messages, &HashMap::new(), false);
        let parts = contents[0]["parts"].as_array().unwrap();
        // functionResponse + 拆出的 inline_data 图片
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["functionResponse"]["name"], "Read");
        // 对齐 CPA:单个非图块 → 该块 raw JSON(非数组)
        assert_eq!(
            parts[0]["functionResponse"]["response"]["result"],
            json!({"type": "text", "text": "see image"})
        );
        assert_eq!(parts[1]["inline_data"]["mime_type"], "image/png");
        assert_eq!(parts[1]["inline_data"]["data"], "AAAA");
    }

    #[test]
    fn test_convert_messages_antigravity_nests_tool_images() {
        let messages = vec![json!({
            "role": "user",
            "content": [{
                "type": "tool_result",
                "tool_use_id": "Read-1",
                "content": [
                    {"type": "text", "text": "see image"},
                    {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}}
                ]
            }]
        })];
        let contents = convert_messages(&messages, &HashMap::new(), true);
        let parts = contents[0]["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 1);
        let fr = &parts[0]["functionResponse"];
        assert_eq!(fr["parts"].as_array().unwrap().len(), 1);
        assert_eq!(fr["parts"][0]["inlineData"]["mimeType"], "image/png");
        assert_eq!(fr["parts"][0]["inlineData"]["data"], "AAAA");
    }

    #[test]
    fn test_convert_messages_image_block() {
        let messages = vec![json!({
            "role": "user",
            "content": [
                {"type": "image", "source": {"type": "base64", "media_type": "image/jpeg", "data": "BBBB"}},
                {"type": "text", "text": "what is this"}
            ]
        })];
        let contents = convert_messages(&messages, &HashMap::new(), false);
        let parts = contents[0]["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["inline_data"]["mime_type"], "image/jpeg");
        assert_eq!(parts[1]["text"], "what is this");
    }

    #[test]
    fn test_convert_messages_strips_dangling_model_function_call() {
        let messages = vec![
            json!({"role": "user", "content": "hi"}),
            json!({"role": "assistant", "content": [{
                "type": "tool_use", "id": "Read-1", "name": "Read", "input": {}
            }]}),
        ];
        let mut map = HashMap::new();
        map.insert("Read".to_string(), "Read".to_string());
        let contents = convert_messages(&messages, &map, false);
        // 尾部未应答 functionCall 的 model 回合被剥离
        assert_eq!(contents.len(), 1);
        assert_eq!(contents[0]["role"], "user");
    }
}
