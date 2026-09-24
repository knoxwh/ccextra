// 协议转换:三条独立 body-to-body 转换
//
// - passthrough: claude → claude 只改 model
// - to_openai_chat: anthropic → openai chat
// - to_openai_responses: anthropic → openai responses
// - to_gemini: anthropic → gemini
// - to_antigravity: anthropic → antigravity

use thiserror::Error;

pub mod antigravity;
pub mod antigravity_tools;
pub mod cursor;
pub mod fix_json;
pub mod gemini;
pub mod gemini_response;
pub mod gemini_schema;
pub mod message_convert;
pub mod passthrough;
pub mod reasoning_replay;
pub mod shorten;
pub mod signature;
pub mod to_openai_chat;
pub mod to_openai_responses;
pub mod tool_id;
pub mod tool_sanitize;

pub use antigravity::{convert_to_antigravity, convert_to_antigravity_with};
pub use antigravity_tools::{
    antigravity_tool_name_to_upstream, antigravity_upstream_tool_name_to_client,
};
pub use fix_json::fix_json_quotes;
pub use gemini::{convert_to_gemini, convert_to_gemini_with_registry};
pub use gemini_response::{
    convert_gemini_response, convert_gemini_stream_chunk, finalize_gemini_stream,
    force_finalize_gemini_stream, GeminiStreamState,
};
pub use gemini_schema::{
    clean_json_schema_for_antigravity, clean_json_schema_for_gemini,
    clean_nested_schema_for_antigravity,
};
pub use passthrough::{clamp_passthrough_effort, convert_passthrough, sanitize_passthrough_prompt};
pub use reasoning_replay::{
    append_replay_turn, build_replay_turn, compute_input_prefix_fingerprint,
    input_prefix_fingerprint, insert_replay_turns, REPLAY_TURN_TYPE,
};
pub use shorten::build_reverse_map;
pub use shorten::build_short_name_map;
pub use signature::{
    format_claude_signature_value, is_valid_gpt_reasoning_signature,
    is_valid_grok_encrypted_content, model_group,
};
pub use to_openai_chat::{convert_to_openai_chat, convert_to_openai_chat_with};
pub use to_openai_responses::{
    convert_to_openai_responses, convert_to_openai_responses_with, is_thinking_signature_invalid,
    sanitize_gpt_reasoning_items, trim_encrypted_reasoning_items,
};

/// Claude Code 每请求注入 system 的计费+prompt 指纹块前缀(内容逐请求变化)。
/// 转换到 openai 侧必须剥离,否则上游缓存前缀每次请求全 miss。
const CLAUDE_CODE_ATTRIBUTION_PREFIX: &str = "x-anthropic-billing-header:";

/// Claude Code subagent / CLI 注入的固定身份声明。非 Claude 上游剥离防拦截/人设冲突。
pub const CLAUDE_AGENT_SDK_IDENTITY: &str =
    "You are a Claude agent, built on Anthropic's Claude Agent SDK.";
pub const CLAUDE_CODE_CLI_IDENTITY: &str =
    "You are Claude Code, Anthropic's official CLI for Claude.";

/// 是否为 Claude Code 计费归属文本(前导空白后以前缀开头)
pub fn is_attribution_text(text: &str) -> bool {
    text.trim_start()
        .starts_with(CLAUDE_CODE_ATTRIBUTION_PREFIX)
}

/// 剥离文本前导的计费归属行(含 CR/LF/CRLF 行尾),保留其余内容。
/// 对齐 sub2api be4a4990:归属行与后续指令同块时只删行,不丢整块;
/// 纯归属文本返回空串。非前导出现的归属字面量不动。
pub fn strip_attribution_line(text: &str) -> &str {
    let trimmed = text.trim_start();
    if !trimmed.starts_with(CLAUDE_CODE_ATTRIBUTION_PREFIX) {
        return text;
    }
    let rest = match trimmed.find(['\r', '\n']) {
        // 无行尾:整段都是归属行
        None => return "",
        Some(end) => &trimmed[end..],
    };
    // 消费行尾分隔符(CRLF 算一个)
    let rest = rest
        .strip_prefix("\r\n")
        .or_else(|| rest.strip_prefix('\r'))
        .or_else(|| rest.strip_prefix('\n'))
        .unwrap_or(rest);
    rest
}

/// 是否为 Claude 官方固定身份声明句
pub fn is_claude_identity_text(text: &str) -> bool {
    let t = text.trim();
    t == CLAUDE_AGENT_SDK_IDENTITY || t == CLAUDE_CODE_CLI_IDENTITY
}

/// 是否为目标上游需忽略的系统提示文本(空白、计费头或非 Claude 目标的身份声明)
pub fn is_ignorable_system_text(text: &str, upstream_model: &str) -> bool {
    let t = text.trim();
    t.is_empty()
        || is_attribution_text(t)
        || (!upstream_model.to_lowercase().contains("claude") && is_claude_identity_text(t))
}

/// 是否为 Claude 服务端工具(web_search 系列)。此类工具在 chat 转换时
/// 直接丢弃(无 Chat Completions 等价,对齐 anthropicToolsToChatTools);
/// responses 转换时映射为 {"type":"web_search"}。
pub fn is_web_search_tool_type(tool_type: &str) -> bool {
    matches!(tool_type, "web_search_20250305" | "web_search_20260209")
}

/// type:object 节点递归补 properties:{}(部分 OpenAI 兼容上游要求 object schema 必须带 properties)。
pub fn normalize_object_schema_properties(schema: serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match schema {
        Value::Object(mut map) => {
            let is_object_type = map
                .get("type")
                .and_then(|t| t.as_str())
                .is_some_and(|t| t == "object");
            if is_object_type && !map.contains_key("properties") {
                map.insert("properties".into(), serde_json::json!({}));
            }
            // 对齐 sub2api 1d640c40e:required:null 无效，删键而非替换成 []。
            if map.get("required").is_some_and(Value::is_null) {
                map.shift_remove("required");
            }
            // 对齐 CPA e56abd56:剥离含 \p{...}/\P{...} 的 pattern
            // (Python re 编译报 bad escape \p,上游 schema 校验失败)
            if let Some(Value::String(p)) = map.get("pattern") {
                if has_unsupported_unicode_property_escape(p) {
                    map.shift_remove("pattern");
                }
            }
            // 对齐 CPA 37ce368c:patternProperties 正则键含 \p{...} 时整键删除
            if let Some(Value::Object(pat_props)) = map.get_mut("patternProperties") {
                let bad_keys: Vec<String> = pat_props
                    .keys()
                    .filter(|k| has_unsupported_unicode_property_escape(k))
                    .cloned()
                    .collect();
                for k in bad_keys {
                    pat_props.shift_remove(&k);
                }
            }
            // 仅沿 schema 关键字递归,避免误删用户数据中的 pattern 键
            for map_key in SCHEMA_MAP_KEYWORDS {
                if let Some(Value::Object(sub_map)) = map.get_mut(*map_key) {
                    for v in sub_map.values_mut() {
                        let taken = std::mem::take(v);
                        *v = normalize_object_schema_properties(taken);
                    }
                }
            }
            for val_key in SCHEMA_VALUE_KEYWORDS {
                if let Some(val) = map.get_mut(*val_key) {
                    match val {
                        Value::Object(_) => {
                            let taken = std::mem::take(val);
                            *val = normalize_object_schema_properties(taken);
                        }
                        Value::Array(arr) => {
                            for item in arr.iter_mut() {
                                let taken = std::mem::take(item);
                                *item = normalize_object_schema_properties(taken);
                            }
                        }
                        _ => {}
                    }
                }
            }
            Value::Object(map)
        }
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(normalize_object_schema_properties)
                .collect(),
        ),
        other => other,
    }
}

/// 对齐 CPA util.HasUnsupportedUnicodePropertyEscape:检测未转义的
/// \p{...}/\P{...}(Python re 编译失败)与八进制 NUL 转义 \0(严格上游
/// 校验报 "is not a 'regex'",等价 \x00 拼写可接受)。跳过被反斜杠转义的字符。
pub fn has_unsupported_unicode_property_escape(pattern: &str) -> bool {
    let b = pattern.as_bytes();
    let mut i = 0;
    while i < b.len() {
        if b[i] != b'\\' {
            i += 1;
            continue;
        }
        if i + 2 < b.len() && (b[i + 1] == b'p' || b[i + 1] == b'P') && b[i + 2] == b'{' {
            return true;
        }
        // 对齐 CPA 320100ec:八进制 NUL 转义 \0 同样剥离
        if i + 1 < b.len() && b[i + 1] == b'0' {
            return true;
        }
        i += 2; // 跳过反斜杠与其后一个字符
    }
    false
}

/// 对齐 CPA util.SchemaMapKeywords:值为 subschema 映射的关键字
pub const SCHEMA_MAP_KEYWORDS: &[&str] = &[
    "properties",
    "$defs",
    "definitions",
    "patternProperties",
    "dependentSchemas",
    "dependencies",
];

/// 对齐 CPA util.SchemaValueKeywords:值为单个 subschema 或数组的关键字
pub const SCHEMA_VALUE_KEYWORDS: &[&str] = &[
    "items",
    "prefixItems",
    "contains",
    "additionalProperties",
    "propertyNames",
    "unevaluatedProperties",
    "unevaluatedItems",
    "additionalItems",
    "contentSchema",
    "anyOf",
    "oneOf",
    "allOf",
    "not",
    "if",
    "then",
    "else",
];

#[derive(Debug, Error)]
pub enum ConvertError {
    #[error("JSON 解析错误: {0}")]
    JsonError(#[from] serde_json::Error),

    #[error("缺少必需字段: {0}")]
    MissingField(String),

    #[error("字段类型错误: {0}")]
    InvalidType(String),
}

pub type Result<T> = std::result::Result<T, ConvertError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_strip_attribution_line() {
        // 对齐 sub2api be4a4990 测试矩阵:仅剥前导归属行
        const ATTR: &str = "x-anthropic-billing-header: cc_version=2.1.271.4bf;";
        assert_eq!(strip_attribution_line(ATTR), "");
        assert_eq!(strip_attribution_line(&format!("{ATTR}\n")), "");
        assert_eq!(strip_attribution_line(&format!(" \t\n{ATTR}")), "");
        assert_eq!(
            strip_attribution_line(&format!("{ATTR}\nKeep these instructions.")),
            "Keep these instructions."
        );
        // CRLF 算一个行尾;CR 单独也算
        assert_eq!(
            strip_attribution_line(&format!("{ATTR}\r\n  Keep indentation.")),
            "  Keep indentation."
        );
        assert_eq!(
            strip_attribution_line(&format!("{ATTR}\rKeep these instructions.")),
            "Keep these instructions."
        );
        // 非前导出现的归属字面量不动
        let mid = format!("Explain this metadata: {ATTR}");
        assert_eq!(strip_attribution_line(&mid), mid);
        let later = format!("Example:\n{ATTR}");
        assert_eq!(strip_attribution_line(&later), later);
        // 无冒号前缀不匹配;相邻字段名不算
        assert_eq!(
            strip_attribution_line("x-anthropic-billing-header keep"),
            "x-anthropic-billing-header keep"
        );
        assert_eq!(
            strip_attribution_line("x-anthropic-billing-header-extra: keep"),
            "x-anthropic-billing-header-extra: keep"
        );
    }

    #[test]
    fn test_has_unsupported_unicode_property_escape() {
        // 对齐 CPA e56abd56 测试矩阵
        assert!(has_unsupported_unicode_property_escape(r"\p{L}"));
        assert!(has_unsupported_unicode_property_escape(r"\P{L}"));
        assert!(has_unsupported_unicode_property_escape("a\\p{Han}b"));
        // 双反斜杠后跟 p{ 是字面量,不算
        assert!(!has_unsupported_unicode_property_escape(r"\\p{L}"));
        // 无花括号的 \p 不算
        assert!(!has_unsupported_unicode_property_escape(r"\pL"));
        assert!(!has_unsupported_unicode_property_escape(r"^\d{4}-\d{2}$"));
        // 对齐 CPA 320100ec:八进制 NUL \0 算,\x00 拼写与转义反斜杠不算
        assert!(has_unsupported_unicode_property_escape("^[^\\0]*$"));
        assert!(!has_unsupported_unicode_property_escape("^[^\\x00]*$"));
        assert!(!has_unsupported_unicode_property_escape(r"\\0"));
    }

    #[test]
    fn test_normalize_object_schema_properties_strips_bad_patterns() {
        // \p{...} pattern 被剥离,普通 pattern 保留
        let schema: serde_json::Value = serde_json::from_str(
            r#"{
                "type": "object",
                "properties": {
                    "a": {"type": "string", "pattern": "\\p{Han}+"},
                    "b": {"type": "string", "pattern": "^\\d+$"}
                },
                "anyOf": [{"type": "string", "pattern": "\\P{L}"}]
            }"#,
        )
        .unwrap();
        let out = normalize_object_schema_properties(schema);
        assert!(out["properties"]["a"].get("pattern").is_none());
        assert_eq!(out["properties"]["b"]["pattern"], r"^\d+$");
        assert!(out["anyOf"][0].get("pattern").is_none());
    }

    #[test]
    fn test_normalize_object_schema_properties_strips_octal_nul_pattern() {
        // 对齐 CPA 320100ec / TestNormalizeCodexToolSchemas_StripsOctalNULPatternEscape:
        // \0 pattern 剥离,\x00 拼写与普通 pattern 保留
        let schema: serde_json::Value = serde_json::from_str(
            r#"{
                "type": "object",
                "properties": {
                    "file_paths": {
                        "type": "array",
                        "items": {"type": "string", "pattern": "^[^\\0]*$", "minLength": 1}
                    },
                    "asset_id": {"type": "string", "pattern": "^[0-9a-f]{32}$"},
                    "hex_nul": {"type": "string", "pattern": "^[^\\x00]*$"}
                }
            }"#,
        )
        .unwrap();
        let out = normalize_object_schema_properties(schema);
        assert!(out["properties"]["file_paths"]["items"]
            .get("pattern")
            .is_none());
        assert_eq!(out["properties"]["file_paths"]["items"]["minLength"], 1);
        assert_eq!(out["properties"]["asset_id"]["pattern"], "^[0-9a-f]{32}$");
        assert_eq!(out["properties"]["hex_nul"]["pattern"], "^[^\\x00]*$");
    }

    #[test]
    fn test_normalize_object_schema_properties_strips_bad_pattern_property_keys() {
        // 对齐 CPA 37ce368c:patternProperties 正则键含 \p{...} 时整键删除
        let schema: serde_json::Value = serde_json::from_str(
            r#"{
                "type": "object",
                "patternProperties": {
                    "^\\p{Han}$": {"type": "string"},
                    "^[a-z]+$": {"type": "string"}
                }
            }"#,
        )
        .unwrap();
        let out = normalize_object_schema_properties(schema);
        let props = out["patternProperties"].as_object().unwrap();
        assert_eq!(props.len(), 1, "只应保留合法键: {props:?}");
        assert!(props.contains_key("^[a-z]+$"));
    }

    #[test]
    fn test_normalize_object_schema_properties_preserves_user_data() {
        // schema-aware:仅沿 schema 关键字递归,非关键字下的 pattern 键保留
        let schema: serde_json::Value = serde_json::from_str(
            r#"{
                "type": "object",
                "properties": {
                    "cfg": {
                        "type": "object",
                        "description": "use \\p{L} to match letters",
                        "default": {"pattern": "\\p{L}"}
                    }
                }
            }"#,
        )
        .unwrap();
        let out = normalize_object_schema_properties(schema);
        assert_eq!(out["properties"]["cfg"]["default"]["pattern"], r"\p{L}");
        // properties 缺省补空 object 仍生效
        let bare = normalize_object_schema_properties(
            serde_json::json!({"type": "object", "pattern": "\\p{L}"}),
        );
        assert!(bare.get("pattern").is_none());
        assert!(bare["properties"].is_object());
    }
}
