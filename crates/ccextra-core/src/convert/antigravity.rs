// Anthropic → Antigravity 协议转换
// Antigravity 使用 Gemini 格式，但包裹在 request 对象中

use serde_json::Value;
use std::collections::HashMap;

use super::gemini::{convert_to_gemini_with_registry, SchemaFlavor};

/// 转换 Anthropic 请求体为 Antigravity 格式
///
/// Antigravity 格式(对齐 CLIProxyAPI geminiToAntigravity)：
/// ```json
/// {
///   "project": "<project_id>",
///   "model": "gemini-3.8-flash-medium",
///   "userAgent": "antigravity",
///   "requestType": "agent",
///   "requestId": "agent/<cascade-uuid>/<unix-ms>/<trajectory-uuid>/<step>",
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
    convert_to_antigravity_with(body, upstream_model, project_id, &[], None)
}

/// 带 reasoning 注册表与稳定客户端会话的 Antigravity 转换
pub fn convert_to_antigravity_with(
    body: &Value,
    upstream_model: &str,
    project_id: Option<&str>,
    registry: &[crate::thinking::ModelCapability],
    session_id: Option<&str>,
) -> (Value, HashMap<String, String>) {
    // 1. 先转换为 Gemini 格式(Antigravity 用 VALIDATED schema 语义)
    let (gemini_body, short_to_original) =
        convert_to_gemini_with_registry(body, upstream_model, SchemaFlavor::Antigravity, registry);

    // 对齐 CPA DerivedAntigravitySessionID:客户端显式 session 优先
    // sha256("cli-proxy-api:antigravity:derived-session\0" + id) 转负数
    let empty = serde_json::Map::new();
    let gemini_obj_ref = gemini_body.as_object().unwrap_or(&empty);
    let derived_session_id = if let Some(sid) = session_id.filter(|s| !s.trim().is_empty()) {
        derived_antigravity_session_id(sid)
    } else {
        stable_session_id(gemini_obj_ref)
    };
    // 对齐官方 2.31.2 language_server requestId 形态;两个 UUID 由 sessionId 稳定派生
    let request_id = official_request_id(&derived_session_id, user_turn_count(gemini_obj_ref));

    // 2. 包裹为 Antigravity 格式(对齐 CLIProxyAPI geminiToAntigravity)
    let mut antigravity = serde_json::json!({
        "model": upstream_model,
        "userAgent": "antigravity",
        "requestType": "agent",
        "requestId": request_id,
        "request": {}
    });
    // 对齐 CPA:project 为空时不写该键
    if let Some(pid) = project_id {
        if !pid.is_empty() {
            antigravity["project"] = Value::String(pid.to_string());
        }
    }

    // 3. 将 Gemini 字段移到 request 中（除了 model）
    // gemini_body 此后不再使用,直接 move 键值,免全量深克隆
    if let Value::Object(gemini_obj) = gemini_body {
        let mut request_obj = serde_json::Map::new();

        for (key, value) in gemini_obj {
            if key != "model" {
                request_obj.insert(key, value);
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
        if let Some(cap) = crate::thinking::max_completion_tokens(upstream_model, registry) {
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

        // 对齐 CPA dc21a426:归一化 responseJsonSchema/response_json_schema → responseSchema
        normalize_generation_config_response_schema(&mut request_obj);

        request_obj.insert("sessionId".to_string(), Value::String(derived_session_id));

        antigravity["request"] = Value::Object(request_obj);
    }

    (antigravity, short_to_original)
}

/// 对齐 CPA dc21a426:归一化 generationConfig.responseJsonSchema → responseSchema
/// 处理 generationConfig 和 generation_config 两种命名，以及 camelCase/snake_case schema 键
fn normalize_generation_config_response_schema(request_obj: &mut serde_json::Map<String, Value>) {
    for container in ["generationConfig", "generation_config"] {
        let Some(Value::Object(gc)) = request_obj.get_mut(container) else {
            continue;
        };

        let has_response_schema = gc.contains_key("responseSchema");

        for schema_key in ["responseJsonSchema", "response_json_schema"] {
            if let Some(schema) = gc.remove(schema_key) {
                // 只在 responseSchema 不存在时移动
                if !has_response_schema {
                    gc.insert("responseSchema".to_string(), schema);
                }
            }
        }
    }
}

/// 对齐 CLIProxyAPI DerivedAntigravitySessionID:
/// 将客户端派生会话 id 映射为 Antigravity 负十进制格式:
/// sha256("cli-proxy-api:antigravity:derived-session\0" + id) 前 8 字节转 int64(掩符号位),前缀 "-"
pub fn derived_antigravity_session_id(derived_id: &str) -> String {
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    h.update(b"cli-proxy-api:antigravity:derived-session\0");
    h.update(derived_id.as_bytes());
    let digest = h.finalize();
    let v = i64::from_be_bytes(digest[..8].try_into().unwrap()) & 0x7FFF_FFFF_FFFF_FFFF;
    format!("-{v}")
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

/// 官方 language_server 形态:`agent/<cascade>/<unix-ms>/<trajectory>/<step>`。
/// cascade / trajectory 由 seed 经不同命名空间稳定派生(同 seed 同值,两者不同)。
fn official_request_id(seed: &str, step: usize) -> String {
    let millis = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!(
        "agent/{}/{millis}/{}/{step}",
        derived_uuid(b"ccextra:antigravity:cascade-id\0", seed),
        derived_uuid(b"ccextra:antigravity:trajectory-id\0", seed),
    )
}

/// sha256(namespace + seed) 前 16 字节,置 v4 版本/变体位,格式化为 UUID
fn derived_uuid(namespace: &[u8], seed: &str) -> String {
    use sha2::Digest;
    let mut h = sha2::Sha256::new();
    h.update(namespace);
    h.update(seed.as_bytes());
    let d = h.finalize();
    let mut b: [u8; 16] = d[..16].try_into().unwrap();
    b[6] = (b[6] & 0x0F) | 0x40;
    b[8] = (b[8] & 0x3F) | 0x80;
    let hex: String = b.iter().map(|x| format!("{x:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &hex[..8],
        &hex[8..12],
        &hex[12..16],
        &hex[16..20],
        &hex[20..]
    )
}

/// 步数从简:含文本的 user 轮次数(工具结果轮不计),至少 1
fn user_turn_count(request_obj: &serde_json::Map<String, Value>) -> usize {
    let n = request_obj
        .get("contents")
        .and_then(|c| c.as_array())
        .map(|contents| {
            contents
                .iter()
                .filter(|c| c.get("role").and_then(|r| r.as_str()) == Some("user"))
                .filter(|c| {
                    c.get("parts")
                        .and_then(|p| p.as_array())
                        .is_some_and(|parts| parts.iter().any(|p| p.get("text").is_some()))
                })
                .count()
        })
        .unwrap_or(0);
    n.max(1)
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
    fn test_neutralizes_claude_identity_in_antigravity() {
        let body = json!({
            "model": "m", "max_tokens": 100,
            "system": [
                {"type": "text", "text": "You are a Claude agent, built on Anthropic's Claude Agent SDK. Do the task."}
            ],
            "messages": [{"role": "user", "content": "Does Claude Code support claude-sonnet-4-6?"}]
        });
        let (out, _) = convert_to_antigravity(&body, "gemini-3.8-flash-medium", None);
        let parts = out["request"]["systemInstruction"]["parts"]
            .as_array()
            .unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0]["text"], "You are an AI agent. Do the task.");
        assert_eq!(
            out["request"]["contents"][0]["parts"][0]["text"],
            "Does Claude Code support claude-sonnet-4-6?"
        );
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
    fn test_claude_max_output_capped_from_registry() {
        let reg = vec![crate::thinking::ModelCapability {
            id: "claude-sonnet-4-6".into(),
            reasoning_levels: vec![],
            force_effort: None,
            max_completion_tokens: Some(64000),
        }];
        let (out, _) =
            convert_to_antigravity_with(&base_body(), "claude-sonnet-4-6", None, &reg, None);
        assert_eq!(out["request"]["generationConfig"]["maxOutputTokens"], 64000);
    }

    #[test]
    fn test_claude_max_output_uncapped_without_registry() {
        let (out, _) = convert_to_antigravity(&base_body(), "claude-sonnet-4-6", None);
        assert_eq!(
            out["request"]["generationConfig"]["maxOutputTokens"],
            128000
        );
    }

    #[test]
    fn test_signed_thinking_preserved_unsigned_dropped() {
        let valid_claude_sig = crate::convert::signature::fixtures::claude_native_default();
        let body = json!({
            "model": "m", "max_tokens": 100,
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "t1", "signature": valid_claude_sig},
                    {"type": "thinking", "thinking": "t2"},
                    {"type": "text", "text": "answer"}
                ]}
            ]
        });
        let (out, _) = convert_to_antigravity(&body, "claude-sonnet-4-6", None);
        let parts = out["request"]["contents"][1]["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 2);
        assert_eq!(parts[0]["thought"], true);
        assert!(parts[0]["thoughtSignature"]
            .as_str()
            .unwrap()
            .starts_with('R'));
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

    #[test]
    fn test_flash_model_skips_validated_placeholder() {
        // 对齐 CPA requirePlaceholder=false:gemini-3.8-flash 不加 reason/_
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
        let (out, _) = convert_to_antigravity(&body, "gemini-3.8-flash-high", None);
        let schema = &out["request"]["tools"][0]["functionDeclarations"][0]["parametersJsonSchema"];
        assert!(schema.get("required").is_none(), "{schema}");
        assert!(schema["properties"].get("_").is_none());
    }

    #[test]
    fn test_normalize_response_json_schema_key_variants() {
        for (container, schema_key, schema, property, expected_type) in [
            (
                "generationConfig",
                "responseJsonSchema",
                json!({"type": "object", "properties": {"message": {"type": "string"}}, "required": ["message"]}),
                "message",
                "string",
            ),
            (
                "generationConfig",
                "response_json_schema",
                json!({"type": "object", "properties": {"data": {"type": "string"}}}),
                "data",
                "string",
            ),
            (
                "generation_config",
                "responseJsonSchema",
                json!({"type": "object", "properties": {"x": {"type": "number"}}}),
                "x",
                "number",
            ),
        ] {
            let mut request_obj = serde_json::Map::new();
            request_obj.insert(container.to_string(), json!({schema_key: schema}));
            normalize_generation_config_response_schema(&mut request_obj);
            let gc = request_obj[container].as_object().unwrap();
            assert!(gc.get(schema_key).is_none(), "{container}.{schema_key}");
            assert_eq!(
                gc["responseSchema"]["properties"][property]["type"],
                expected_type
            );
        }
    }

    #[test]
    fn test_normalize_preserves_existing_response_schema() {
        let mut request_obj = serde_json::Map::new();
        let mut gc = serde_json::Map::new();
        gc.insert(
            "responseSchema".to_string(),
            json!({"type": "object", "properties": {"name": {"type": "string"}}}),
        );
        gc.insert(
            "responseJsonSchema".to_string(),
            json!({"type": "object", "properties": {"stale": {"type": "string"}}}),
        );
        request_obj.insert("generationConfig".to_string(), Value::Object(gc));

        normalize_generation_config_response_schema(&mut request_obj);

        let gc = request_obj["generationConfig"].as_object().unwrap();
        // responseJsonSchema 应被删除
        assert!(gc.get("responseJsonSchema").is_none());
        // responseSchema 保持原值（name 属性，不是 stale）
        assert_eq!(gc["responseSchema"]["properties"]["name"]["type"], "string");
        assert!(gc["responseSchema"]["properties"].get("stale").is_none());
    }

    #[test]
    fn test_derived_antigravity_session_id() {
        let sid1 = derived_antigravity_session_id("sess-123");
        let sid2 = derived_antigravity_session_id("sess-123");
        let sid3 = derived_antigravity_session_id("sess-456");
        assert_eq!(sid1, sid2);
        assert_ne!(sid1, sid3);
        assert!(sid1.starts_with('-'));
        let n: i64 = sid1.parse().expect("valid i64");
        assert!(n < 0);
    }

    /// 解析 `agent/<uuid>/<ms>/<uuid>/<step>`,返回 (cascade, ms, traj, step)
    fn parse_request_id(id: &str) -> (String, u128, String, usize) {
        fn is_uuid(s: &str) -> bool {
            let groups: Vec<_> = s.split('-').collect();
            groups.iter().map(|g| g.len()).collect::<Vec<_>>() == [8, 4, 4, 4, 12]
                && groups.iter().all(|g| {
                    g.chars()
                        .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase())
                })
        }
        let parts: Vec<_> = id.split('/').collect();
        assert_eq!(parts.len(), 5, "{id}");
        assert_eq!(parts[0], "agent", "{id}");
        assert!(is_uuid(parts[1]) && is_uuid(parts[3]), "{id}");
        let step: usize = parts[4].parse().unwrap();
        assert!(step >= 1, "{id}");
        (
            parts[1].to_string(),
            parts[2].parse().unwrap(),
            parts[3].to_string(),
            step,
        )
    }

    #[test]
    fn test_request_id_official_shape_and_stable_per_session() {
        let req1 = json!({
            "messages": [{"role": "user", "content": "hello"}]
        });
        let req2 = json!({
            "messages": [
                {"role": "user", "content": "hello"},
                {"role": "assistant", "content": "hi"},
                {"role": "user", "content": "again"}
            ]
        });
        let (out1, _) =
            convert_to_antigravity_with(&req1, "gemini-2.5-pro", None, &[], Some("s-1"));
        let (out2, _) =
            convert_to_antigravity_with(&req2, "gemini-2.5-pro", None, &[], Some("s-1"));
        let (out3, _) =
            convert_to_antigravity_with(&req1, "gemini-2.5-pro", None, &[], Some("s-2"));
        let (c1, ms1, t1, step1) = parse_request_id(out1["requestId"].as_str().unwrap());
        let (c2, _, t2, step2) = parse_request_id(out2["requestId"].as_str().unwrap());
        let (c3, _, t3, _) = parse_request_id(out3["requestId"].as_str().unwrap());
        assert_eq!((c1.as_str(), t1.as_str()), (c2.as_str(), t2.as_str()));
        assert_ne!(c1, t1);
        assert_ne!(c1, c3);
        assert_ne!(t1, t3);
        assert_eq!(step1, 1);
        assert_eq!(step2, 2);
        assert!(ms1 > 1_700_000_000_000);
        // 键顺序不变:requestId 仍在 request 之前
        let keys: Vec<_> = out1.as_object().unwrap().keys().cloned().collect();
        assert_eq!(
            keys,
            ["model", "userAgent", "requestType", "requestId", "request"]
        );
    }

    #[test]
    fn test_request_id_without_session_is_valid_shape() {
        let (out, _) = convert_to_antigravity(&json!({"messages": []}), "gemini-2.5-pro", None);
        let (_, _, _, step) = parse_request_id(out["requestId"].as_str().unwrap());
        assert_eq!(step, 1);
        let req = json!({"messages": [{"role": "user", "content": "same text"}]});
        let (a, _) = convert_to_antigravity(&req, "gemini-2.5-pro", None);
        let (b, _) = convert_to_antigravity(&req, "gemini-2.5-pro", None);
        let (ca, _, ta, _) = parse_request_id(a["requestId"].as_str().unwrap());
        let (cb, _, tb, _) = parse_request_id(b["requestId"].as_str().unwrap());
        assert_eq!((ca, ta), (cb, tb));
    }

    #[test]
    fn test_convert_to_antigravity_with_stable_session_id() {
        let req1 = json!({
            "contents": [{"role": "user", "parts": [{"text": "hello turn 1"}]}]
        });
        let req2 = json!({
            "contents": [{"role": "user", "parts": [{"text": "hello turn 2 completely different"}]}]
        });

        let (out1, _) = convert_to_antigravity_with(
            &req1,
            "gemini-2.5-pro",
            Some("test-project"),
            &[],
            Some("sess-cc-123"),
        );
        let (out2, _) = convert_to_antigravity_with(
            &req2,
            "gemini-2.5-pro",
            Some("test-project"),
            &[],
            Some("sess-cc-123"),
        );

        assert_eq!(out1["request"]["sessionId"], out2["request"]["sessionId"]);
        assert_eq!(
            out1["request"]["sessionId"].as_str().unwrap(),
            derived_antigravity_session_id("sess-cc-123")
        );
    }
}
