// Anthropic → Antigravity 协议转换
// Antigravity 使用 Gemini 格式，但包裹在 request 对象中

use serde_json::Value;
use std::collections::HashMap;

use super::gemini::{convert_to_gemini_with, SchemaFlavor};

/// 转换 Anthropic 请求体为 Antigravity 格式
///
/// Antigravity 格式(对齐 CLIProxyAPI geminiToAntigravity)：
/// ```json
/// {
///   "project": "<project_id>",
///   "model": "gemini-3.7-flash-medium",
///   "userAgent": "antigravity",
///   "requestType": "agent",
///   "requestId": "agent-<hex>",
///   "request": {
///     "contents": [...],
///     "sessionId": "-<int64>"
///   }
/// }
/// ```
///
/// 返回 (Antigravity 请求体, 短名→原名映射)
pub fn convert_to_antigravity(
    body: &Value,
    upstream_model: &str,
    project_id: Option<&str>,
) -> (Value, HashMap<String, String>) {
    // 1. 先转换为 Gemini 格式(Antigravity 用 VALIDATED schema 语义)
    let (gemini_body, short_to_original) =
        convert_to_gemini_with(body, upstream_model, SchemaFlavor::Antigravity);

    // 2. 包裹为 Antigravity 格式(对齐 CLIProxyAPI geminiToAntigravity)
    let mut antigravity = serde_json::json!({
        "model": upstream_model,
        "userAgent": "antigravity",
        "requestType": "agent",
        "requestId": format!("agent-{}", random_hex_id()),
        "request": {}
    });
    // 对齐 CPA:project 为空时不写该键
    if let Some(pid) = project_id {
        if !pid.is_empty() {
            antigravity["project"] = Value::String(pid.to_string());
        }
    }

    // 3. 将 Gemini 字段移到 request 中（除了 model）
    if let Some(gemini_obj) = gemini_body.as_object() {
        let mut request_obj = serde_json::Map::new();

        for (key, value) in gemini_obj {
            if key != "model" {
                request_obj.insert(key.clone(), value.clone());
            }
        }

        // 对齐 CLIProxyAPI:删除 safetySettings
        request_obj.remove("safetySettings");

        // 对齐 CPA executor:claude 模型强制 VALIDATED 模式
        if upstream_model.contains("claude") {
            request_obj
                .entry("toolConfig")
                .or_insert_with(|| serde_json::json!({}))
                .as_object_mut()
                .map(|tc| {
                    tc.entry("functionCallingConfig")
                        .or_insert_with(|| serde_json::json!({}))
                        .as_object_mut()
                        .map(|fcc| {
                            fcc.insert("mode".to_string(), Value::String("VALIDATED".into()))
                        })
                });
        }

        // 对齐 CPA executor:maxOutputTokens 按 registry max_completion_tokens 封顶
        if let Some(cap) = antigravity_max_completion_tokens(upstream_model) {
            if let Some(Value::Object(gc)) = request_obj.get_mut("generationConfig") {
                if let Some(v) = gc.get("maxOutputTokens").and_then(|v| v.as_i64()) {
                    if v > cap {
                        gc.insert("maxOutputTokens".to_string(), Value::from(cap));
                    }
                }
            }
        }

        // 对齐 CLIProxyAPI:非 claude 模型删除 maxOutputTokens(上游自管理)
        if !upstream_model.contains("claude") {
            if let Some(Value::Object(gc)) = request_obj.get_mut("generationConfig") {
                gc.remove("maxOutputTokens");
            }
        }

        // 稳定 sessionId:取首条 user 文本的 sha256 前 8 字节(对齐 generateStableSessionID)
        let session_id = stable_session_id(&request_obj);
        request_obj.insert("sessionId".to_string(), Value::String(session_id));

        antigravity["request"] = Value::Object(request_obj);
    }

    (antigravity, short_to_original)
}

/// 对齐 CPA registry models.json antigravity 节 max_completion_tokens
/// 未收录模型返回 None(不封顶)
fn antigravity_max_completion_tokens(model: &str) -> Option<i64> {
    Some(match model {
        "claude-opus-4-6-thinking" | "claude-sonnet-4-6" => 64000,
        "gemini-3.6-flash-high" | "gemini-3.7-flash-high" | "gemini-3-flash" => 65536,
        "gemini-pro-agent" | "gemini-3.1-pro-low" | "gemini-3.1-flash-lite" => 65535,
        "gpt-oss-120b-medium" => 32768,
        _ => return None,
    })
}

/// 对齐 CLIProxyAPI generateStableSessionID:
/// 首条 user 消息首段文本 sha256 前 8 字节转 int64(掩符号位),前缀 "-"
fn stable_session_id(request_obj: &serde_json::Map<String, Value>) -> String {
    if let Some(Value::Array(contents)) = request_obj.get("contents") {
        for content in contents {
            if content.get("role").and_then(|r| r.as_str()) != Some("user") {
                continue;
            }
            let text = content
                .get("parts")
                .and_then(|p| p.get(0))
                .and_then(|p| p.get("text"))
                .and_then(|t| t.as_str())
                .unwrap_or("");
            if text.is_empty() {
                continue;
            }
            // sha256 前 8 字节大端转 i64 并掩符号位
            let digest = {
                use sha2::Digest;
                let mut h = sha2::Sha256::new();
                h.update(text.as_bytes());
                h.finalize()
            };
            let v = i64::from_be_bytes(digest[..8].try_into().unwrap()) & 0x7FFF_FFFF_FFFF_FFFF;
            return format!("-{v}");
        }
    }
    // 兜底:随机(对 CLIProxyAPI generateSessionID 的随机分支)
    let v = (random_u64() & 0x7FFF_FFFF_FFFF_FFFF) as i64;
    format!("-{v}")
}

/// 无 uuid 依赖的随机源:时间戳+pid 进 sha256,取 8 字节
fn random_u64() -> u64 {
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    h.update(now.to_be_bytes());
    h.update(std::process::id().to_be_bytes());
    h.update(std::thread::current().name().unwrap_or("").as_bytes());
    let d = h.finalize();
    u64::from_be_bytes(d[..8].try_into().unwrap())
}

fn random_hex_id() -> String {
    format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        random_u64() as u32,
        random_u64() as u16,
        random_u64() as u16,
        random_u64() as u16,
        random_u64() & 0xFFFF_FFFF_FFFF
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn base_body() -> Value {
        json!({
            "model": "claude-x", "max_tokens": 128000,
            "messages": [{"role": "user", "content": "hi"}]
        })
    }

    #[test]
    fn test_filters_claude_identity_in_antigravity() {
        let body = json!({
            "model": "m", "max_tokens": 100,
            "system": [
                {"type": "text", "text": "You are a Claude agent, built on Anthropic's Claude Agent SDK."},
                {"type": "text", "text": "Do the task."}
            ],
            "messages": [{"role": "user", "content": "hi"}]
        });
        let (out, _) = convert_to_antigravity(&body, "gemini-3.7-flash-medium", None);
        let parts = out["request"]["systemInstruction"]["parts"]
            .as_array()
            .unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0]["text"], "Do the task.");
    }

    #[test]
    fn test_claude_model_sets_validated_mode() {
        let (out, _) = convert_to_antigravity(&base_body(), "claude-opus-4-6-thinking", None);
        assert_eq!(
            out["request"]["toolConfig"]["functionCallingConfig"]["mode"],
            "VALIDATED"
        );
    }

    #[test]
    fn test_non_claude_no_validated_and_max_output_deleted() {
        let (out, _) = convert_to_antigravity(&base_body(), "gemini-3-flash", None);
        assert!(out["request"].get("toolConfig").is_none());
        assert!(out["request"]["generationConfig"]
            .get("maxOutputTokens")
            .is_none());
    }

    #[test]
    fn test_claude_max_output_capped() {
        let (out, _) = convert_to_antigravity(&base_body(), "claude-sonnet-4-6", None);
        assert_eq!(out["request"]["generationConfig"]["maxOutputTokens"], 64000);
    }

    #[test]
    fn test_signed_thinking_preserved_unsigned_dropped() {
        let body = json!({
            "model": "m", "max_tokens": 100,
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "t1", "signature": "C4x2-valid-claude-sig"},
                    {"type": "thinking", "thinking": "t2"},
                    {"type": "text", "text": "answer"}
                ]}
            ]
        });
        let (out, _) = convert_to_antigravity(&body, "claude-sonnet-4-6", None);
        let parts = out["request"]["contents"][1]["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["thought"], true);
        assert_eq!(parts[0]["thoughtSignature"], "C4x2-valid-claude-sig");
        assert_eq!(parts[1]["text"], "answer");
    }

    #[test]
    fn test_web_search_tool_stripped() {
        let body = json!({
            "model": "m", "max_tokens": 100,
            "tools": [
                {"type": "web_search_20250305", "name": "web_search", "max_uses": 3},
                {"name": "Read", "input_schema": {"type": "object"}}
            ],
            "messages": [{"role": "user", "content": "hi"}]
        });
        let (out, _) = convert_to_antigravity(&body, "claude-sonnet-4-6", None);
        let decls = out["request"]["tools"][0]["functionDeclarations"]
            .as_array()
            .unwrap();
        assert_eq!(decls.len(), 1);
        assert_eq!(decls[0]["name"], "Read");
    }

    #[test]
    fn test_antigravity_schema_adds_placeholder() {
        let body = json!({
            "model": "m", "max_tokens": 100,
            "tools": [{"name": "NoArgs", "input_schema": {"type": "object", "properties": {}}}],
            "messages": [{"role": "user", "content": "hi"}]
        });
        let (out, _) = convert_to_antigravity(&body, "claude-sonnet-4-6", None);
        let schema = &out["request"]["tools"][0]["functionDeclarations"][0]["parametersJsonSchema"];
        assert_eq!(schema["required"], json!(["reason"]));
    }

    #[test]
    fn test_antigravity_optional_only_properties_gets_underscore_placeholder() {
        // 对齐 CPA cleanNestedSchema:全可选 properties 套一层后补 `_`
        let body = json!({
            "model": "m", "max_tokens": 100,
            "tools": [{
                "name": "Flag",
                "input_schema": {
                    "type": "object",
                    "properties": {"flag": {"type": "string"}}
                }
            }],
            "messages": [{"role": "user", "content": "hi"}]
        });
        let (out, _) = convert_to_antigravity(&body, "claude-sonnet-4-6", None);
        let schema = &out["request"]["tools"][0]["functionDeclarations"][0]["parametersJsonSchema"];
        assert_eq!(schema["required"], json!(["_"]));
        assert_eq!(schema["properties"]["_"]["type"], "boolean");
        assert_eq!(schema["properties"]["flag"]["type"], "string");
    }
}
