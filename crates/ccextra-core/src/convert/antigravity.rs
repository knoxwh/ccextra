// Anthropic → Antigravity 协议转换
// Antigravity 使用 Gemini 格式，但包裹在 request 对象中

use serde_json::Value;
use std::collections::HashMap;

use super::gemini::{SchemaFlavor, convert_to_gemini_with_registry};
use super::gemini_schema::clean_json_schema_for_antigravity_response;

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
    if let Some(pid) = project_id
        && !pid.is_empty()
    {
        antigravity["project"] = Value::String(pid.to_string());
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
        if let Some(cap) = crate::thinking::max_completion_tokens(upstream_model, registry)
            && let Some(Value::Object(gc)) = request_obj.get_mut("generationConfig")
            && let Some(v) = gc.get("maxOutputTokens").and_then(|v| v.as_i64())
            && v > cap
        {
            gc.insert("maxOutputTokens".to_string(), Value::from(cap));
            // 对齐 CPA 16bcf5a0:Claude 要求 thinkingBudget < maxOutputTokens,封顶后预算 ≥ 上限则降为上限-1
            if upstream_model.contains("claude")
                && let Some(Value::Object(tc)) = gc.get_mut("thinkingConfig")
                && tc
                    .get("thinkingBudget")
                    .and_then(|b| b.as_i64())
                    .is_some_and(|b| b >= cap)
            {
                tc.insert("thinkingBudget".to_string(), Value::from(cap - 1));
            }
        }

        // 对齐 CLIProxyAPI:非 claude 模型删除 maxOutputTokens(上游自管理)
        if !upstream_model.contains("claude")
            && let Some(Value::Object(gc)) = request_obj.get_mut("generationConfig")
        {
            gc.remove("maxOutputTokens");
        }

        // 对齐 CPA dc21a426:归一化 responseJsonSchema/response_json_schema → responseSchema
        normalize_generation_config_response_schema(&mut request_obj);
        clean_generation_config_response_schema(&mut request_obj);

        // 对齐 CPA 38067a29:systemInstruction 合并单段、functionResponse result → output
        normalize_antigravity_request_format(&mut request_obj);

        request_obj.insert("sessionId".to_string(), Value::String(derived_session_id));

        antigravity["request"] = Value::Object(request_obj);
    }

    (antigravity, short_to_original)
}

/// 官方 v1internal 外层信封字段序(agy 二进制 proto 序;缺的键跳过,未列出的键保持原序排后)。
const ENVELOPE_KEY_ORDER: &[&str] = &[
    "project",
    "requestId",
    "request",
    "model",
    "userPromptId",
    "userAgent",
    "requestType",
    "enabledCreditTypes",
];

/// 内层 aiplatform GenerateContentRequest 字段序;toolConfig 紧跟 tools(同官方 proto,
/// CPA b8e3bc66 未给它排位、落在最后)。
const REQUEST_KEY_ORDER: &[&str] = &[
    "contents",
    "systemInstruction",
    "tools",
    "toolConfig",
    "labels",
    "safetySettings",
    "generationConfig",
    "sessionId",
];

/// 按官方 proto 字段序紧凑序列化 Antigravity 请求体(对齐 CPA b8e3bc66 的外层信封与
/// `request` 两层,更深层不动)。只重排键、不克隆值;序列化时排序,OAuth 后补的 `project`
/// 同样排到最前。非对象 body 原样序列化。
pub fn serialize_antigravity_body(body: &Value) -> serde_json::Result<Vec<u8>> {
    match body.as_object() {
        Some(envelope) => serde_json::to_vec(&OrderedObject {
            object: envelope,
            order: ENVELOPE_KEY_ORDER,
            request_order: Some(REQUEST_KEY_ORDER),
        }),
        None => serde_json::to_vec(body),
    }
}

struct OrderedObject<'a> {
    object: &'a serde_json::Map<String, Value>,
    order: &'static [&'static str],
    /// 仅外层:`request` 子对象按此序
    request_order: Option<&'static [&'static str]>,
}

impl serde::Serialize for OrderedObject<'_> {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeMap;
        let mut map = serializer.serialize_map(Some(self.object.len()))?;
        let ranked = self
            .order
            .iter()
            .filter_map(|key| self.object.get_key_value(*key));
        let rest = self
            .object
            .iter()
            .filter(|(key, _)| !self.order.contains(&key.as_str()));
        for (key, value) in ranked.chain(rest) {
            match (self.request_order, key.as_str(), value.as_object()) {
                (Some(order), "request", Some(inner)) => map.serialize_entry(
                    key,
                    &OrderedObject {
                        object: inner,
                        order,
                        request_order: None,
                    },
                )?,
                _ => map.serialize_entry(key, value)?,
            }
        }
        map.end()
    }
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

/// 对齐 CPA sanitizeAntigravityGenerationSchemas:generationConfig 响应 schema 按
/// CleanJSONSchemaForAntigravityResponse 清洗(只动对象形态的 schema)
fn clean_generation_config_response_schema(request_obj: &mut serde_json::Map<String, Value>) {
    for container in ["generationConfig", "generation_config"] {
        let Some(Value::Object(gc)) = request_obj.get_mut(container) else {
            continue;
        };
        for key in ["responseSchema", "response_schema"] {
            if let Some(schema) = gc.get_mut(key).filter(|s| s.is_object()) {
                *schema = clean_json_schema_for_antigravity_response(schema);
            }
        }
    }
}

/// 对齐 CPA 38067a29 NormalizeAntigravityRequestDoc(官方 Antigravity 线格式)
fn normalize_antigravity_request_format(request_obj: &mut serde_json::Map<String, Value>) {
    normalize_antigravity_system_instruction(request_obj);
    normalize_antigravity_function_response_results(request_obj);
}

/// systemInstruction 置 `role: user`;parts 全为字符串 `text` 时,非空文本以 `\n\n` 合并成
/// 单个 part,全空则删除 systemInstruction;含非文本 part 或 parts 为空时只置 role
fn normalize_antigravity_system_instruction(request_obj: &mut serde_json::Map<String, Value>) {
    let Some(Value::Object(instruction)) = request_obj.get_mut("systemInstruction") else {
        return;
    };
    instruction.insert("role".to_string(), Value::String("user".to_string()));
    let Some(parts) = instruction
        .get("parts")
        .and_then(Value::as_array)
        .filter(|parts| !parts.is_empty())
    else {
        return;
    };
    let mut texts = Vec::with_capacity(parts.len());
    for part in parts {
        let Some(text) = part.get("text").and_then(Value::as_str) else {
            return;
        };
        if !text.is_empty() {
            texts.push(text);
        }
    }
    if texts.is_empty() {
        request_obj.remove("systemInstruction");
        return;
    }
    let merged = texts.join("\n\n");
    instruction.insert("parts".to_string(), serde_json::json!([{ "text": merged }]));
}

/// `functionResponse.response` 只有一个字符串 `result` 时改名为 `output`(兼容蛇形键)
fn normalize_antigravity_function_response_results(
    request_obj: &mut serde_json::Map<String, Value>,
) {
    let Some(Value::Array(contents)) = request_obj.get_mut("contents") else {
        return;
    };
    for content in contents {
        let Some(Value::Array(parts)) = content.get_mut("parts") else {
            continue;
        };
        for part in parts {
            for field in ["functionResponse", "function_response"] {
                let Some(Value::Object(response)) =
                    part.get_mut(field).and_then(|f| f.get_mut("response"))
                else {
                    continue;
                };
                if response.len() == 1
                    && response.get("result").is_some_and(Value::is_string)
                    && let Some(result) = response.remove("result")
                {
                    response.insert("output".to_string(), result);
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
        .map_or(0, |d| d.as_nanos());
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
        .map_or(0, |d| d.as_millis());
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
        .map_or(0, |contents| {
            contents
                .iter()
                .filter(|c| c.get("role").and_then(|r| r.as_str()) == Some("user"))
                .filter(|c| {
                    c.get("parts")
                        .and_then(|p| p.as_array())
                        .is_some_and(|parts| parts.iter().any(|p| p.get("text").is_some()))
                })
                .count()
        });
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
        assert!(
            out["request"]["generationConfig"]
                .get("maxOutputTokens")
                .is_none()
        );
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
    fn test_claude_thinking_budget_reclamped_after_output_cap() {
        // 对齐 CPA 16bcf5a0
        let reg = |id: &str| {
            vec![crate::thinking::ModelCapability {
                id: id.into(),
                reasoning_levels: vec![],
                force_effort: None,
                max_completion_tokens: Some(64000),
            }]
        };
        let body = |budget: i64| {
            json!({
                "model": "claude-x", "max_tokens": 128000,
                "thinking": {"type": "enabled", "budget_tokens": budget},
                "messages": [{"role": "user", "content": "hi"}]
            })
        };
        let gc = |out: &Value| out["request"]["generationConfig"].clone();
        let reg_c = reg("claude-sonnet-4-6");
        let (out, _) =
            convert_to_antigravity_with(&body(100000), "claude-sonnet-4-6", None, &reg_c, None);
        assert_eq!(gc(&out)["maxOutputTokens"], 64000);
        assert_eq!(gc(&out)["thinkingConfig"]["thinkingBudget"], 63999);
        // 预算本已低于上限:不动
        let (out, _) =
            convert_to_antigravity_with(&body(32000), "claude-sonnet-4-6", None, &reg_c, None);
        assert_eq!(gc(&out)["thinkingConfig"]["thinkingBudget"], 32000);
        // 未封顶(无注册表):不动
        let (out, _) = convert_to_antigravity(&body(100000), "claude-sonnet-4-6", None);
        assert_eq!(gc(&out)["thinkingConfig"]["thinkingBudget"], 100000);
        // 非 Claude 模型:不动
        let reg_g = reg("gemini-3.1-pro");
        let (out, _) =
            convert_to_antigravity_with(&body(100000), "gemini-3.1-pro", None, &reg_g, None);
        assert_eq!(gc(&out)["thinkingConfig"]["thinkingBudget"], 100000);
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
        assert!(
            parts[0]["thoughtSignature"]
                .as_str()
                .unwrap()
                .starts_with('R')
        );
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

    /// 对齐 CPA 32842348/ac415c08:AG output_config.format → request.generationConfig.responseSchema
    /// (清洗后保留 additionalProperties:false 与非 boolean enum)
    #[test]
    fn test_output_config_json_schema_maps_to_antigravity_response_schema() {
        let body = json!({
            "model": "m", "max_tokens": 256,
            "output_config": {"format": {"type": "json_schema", "schema": {
                "$schema": "http://json-schema.org/draft-07/schema#",
                "type": "object",
                "properties": {"city": {"type": "string", "enum": ["Tokyo", "Paris"]}},
                "required": ["city"],
                "additionalProperties": false
            }}},
            "messages": [{"role": "user", "content": "Return Tokyo."}]
        });
        let (out, _) = convert_to_antigravity(&body, "gemini-3.8-flash-high", None);
        let gc = &out["request"]["generationConfig"];
        assert_eq!(gc["responseMimeType"], "application/json");
        assert!(gc.get("responseJsonSchema").is_none());
        let schema = &gc["responseSchema"];
        assert_eq!(schema["properties"]["city"]["type"], "string");
        assert_eq!(
            schema["properties"]["city"]["enum"],
            json!(["Tokyo", "Paris"])
        );
        assert_eq!(schema["additionalProperties"], json!(false));
        assert!(schema.get("$schema").is_none());
    }

    /// AG Claude 模型保守不映射 output_config.format(CPA 5-5 走 system 结构化输出、其余原生下发,均未跟)
    #[test]
    fn test_output_config_json_schema_skipped_for_antigravity_claude() {
        let mut body = base_body();
        body["output_config"] =
            json!({"format": {"type": "json_schema", "schema": {"type": "object"}}});
        let (out, _) = convert_to_antigravity(&body, "claude-opus-4-6-thinking", None);
        let gc = &out["request"]["generationConfig"];
        for key in ["responseMimeType", "responseSchema", "responseJsonSchema"] {
            assert!(gc.get(key).is_none(), "{key}");
        }
    }

    /// 对齐 CPA 38067a29:多段文本 system 合并为单 part(`\n\n` 拼接),role=user
    #[test]
    fn test_system_instruction_text_parts_merged_into_one() {
        let body = json!({
            "model": "m", "max_tokens": 100,
            "system": [
                {"type": "text", "text": "First rule."},
                {"type": "text", "text": "Second rule."}
            ],
            "messages": [{"role": "user", "content": "hi"}]
        });
        let (out, _) = convert_to_antigravity(&body, "gemini-3.8-flash-high", None);
        let si = &out["request"]["systemInstruction"];
        assert_eq!(si["role"], "user");
        assert_eq!(
            si["parts"],
            json!([{"text": "First rule.\n\nSecond rule."}])
        );
    }

    #[test]
    fn test_normalize_system_instruction_edge_cases() {
        // 全空文本 → 删除
        let mut req = serde_json::Map::new();
        req.insert(
            "systemInstruction".into(),
            json!({"parts": [{"text": ""}, {"text": ""}]}),
        );
        normalize_antigravity_system_instruction(&mut req);
        assert!(req.get("systemInstruction").is_none());

        // 空文本跳过,其余合并
        let mut req = serde_json::Map::new();
        req.insert(
            "systemInstruction".into(),
            json!({"role": "system", "parts": [{"text": "a"}, {"text": ""}, {"text": "b"}]}),
        );
        normalize_antigravity_system_instruction(&mut req);
        assert_eq!(
            req["systemInstruction"],
            json!({"role": "user", "parts": [{"text": "a\n\nb"}]})
        );

        // 含非文本 part:只置 role,不合并
        let mixed = json!([{"text": "a"}, {"inlineData": {"mimeType": "image/png", "data": "x"}}]);
        let mut req = serde_json::Map::new();
        req.insert("systemInstruction".into(), json!({"parts": mixed.clone()}));
        normalize_antigravity_system_instruction(&mut req);
        assert_eq!(req["systemInstruction"]["role"], "user");
        assert_eq!(req["systemInstruction"]["parts"], mixed);

        // parts 为空:保留,只置 role
        let mut req = serde_json::Map::new();
        req.insert("systemInstruction".into(), json!({"parts": []}));
        normalize_antigravity_system_instruction(&mut req);
        assert_eq!(
            req["systemInstruction"],
            json!({"parts": [], "role": "user"})
        );
    }

    /// 对齐 CPA 38067a29:functionResponse.response 仅有字符串 result → output
    #[test]
    fn test_function_response_single_string_result_renamed_to_output() {
        let body = json!({
            "model": "m", "max_tokens": 100,
            "messages": [
                {"role": "user", "content": "read"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "toolu_a", "name": "Read", "input": {"file_path": "/a"}},
                    {"type": "tool_use", "id": "toolu_b", "name": "Bash", "input": {"command": "false"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_a", "content": "A"},
                    {"type": "tool_result", "tool_use_id": "toolu_b", "content": "exit 1", "is_error": true}
                ]}
            ]
        });
        let (out, _) = convert_to_antigravity(&body, "gemini-3.8-flash-high", None);
        let responses: Vec<&Value> = out["request"]["contents"]
            .as_array()
            .unwrap()
            .iter()
            .flat_map(|c| c["parts"].as_array().unwrap())
            .filter_map(|p| p.get("functionResponse"))
            .map(|fr| &fr["response"])
            .collect();
        assert_eq!(responses.len(), 2);
        assert_eq!(*responses[0], json!({"output": "A"}));
        // error 键不改
        assert_eq!(*responses[1], json!({"error": "exit 1"}));
    }

    #[test]
    fn test_normalize_function_response_only_exact_single_string_result() {
        let mut req = serde_json::Map::new();
        req.insert(
            "contents".into(),
            json!([{"role": "user", "parts": [
                {"function_response": {"name": "a", "response": {"result": "x"}}},
                {"functionResponse": {"name": "b", "response": {"result": {"k": 1}}}},
                {"functionResponse": {"name": "c", "response": {"result": "x", "extra": 1}}}
            ]}]),
        );
        normalize_antigravity_function_response_results(&mut req);
        let parts = &req["contents"][0]["parts"];
        assert_eq!(
            parts[0]["function_response"]["response"],
            json!({"output": "x"})
        );
        assert_eq!(
            parts[1]["functionResponse"]["response"],
            json!({"result": {"k": 1}})
        );
        assert_eq!(
            parts[2]["functionResponse"]["response"],
            json!({"result": "x", "extra": 1})
        );
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
        // 内存键序不变(线上键序由 serialize_antigravity_body 决定)
        let keys: Vec<_> = out1.as_object().unwrap().keys().cloned().collect();
        assert_eq!(
            keys,
            ["model", "userAgent", "requestType", "requestId", "request"]
        );
    }

    fn wire_keys(value: &Value) -> Vec<String> {
        value.as_object().unwrap().keys().cloned().collect()
    }

    /// 对齐 CPA b8e3bc66(部分)+ 官方 v1internal proto:外层信封与 request 两层按官方字段序,
    /// toolConfig 紧跟 tools;值不变、紧凑无空白;OAuth 后补的 project 也排最前。
    #[test]
    fn test_serialize_antigravity_body_uses_official_key_order() {
        let req = json!({
            "system": "sys",
            "max_tokens": 64,
            "tools": [{"name": "Read", "input_schema": {"type": "object", "properties": {"p": {"type": "string"}}}}],
            "messages": [{"role": "user", "content": "hi"}]
        });
        let (out, _) = convert_to_antigravity_with(
            &req,
            "claude-sonnet-4-6",
            Some("proj-1"),
            &[],
            Some("s-1"),
        );
        let bytes = serialize_antigravity_body(&out).unwrap();
        assert!(!bytes.contains(&b'\n'));
        let wire: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(wire, out);
        assert_eq!(
            wire_keys(&wire),
            [
                "project",
                "requestId",
                "request",
                "model",
                "userAgent",
                "requestType"
            ]
        );
        assert_eq!(
            wire_keys(&wire["request"]),
            [
                "contents",
                "systemInstruction",
                "tools",
                "toolConfig",
                "generationConfig",
                "sessionId"
            ]
        );

        let (mut late, _) =
            convert_to_antigravity_with(&req, "gemini-3-pro-preview", None, &[], Some("s-1"));
        late["project"] = json!("proj-late");
        late["extraTopLevel"] = json!(1);
        let wire: Value =
            serde_json::from_slice(&serialize_antigravity_body(&late).unwrap()).unwrap();
        assert_eq!(
            wire_keys(&wire),
            [
                "project",
                "requestId",
                "request",
                "model",
                "userAgent",
                "requestType",
                "extraTopLevel"
            ]
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
