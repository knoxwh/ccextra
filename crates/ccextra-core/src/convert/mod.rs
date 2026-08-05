// 协议转换:三条独立 body-to-body 转换
//
// - passthrough: claude → claude 只改 model
// - to_openai_chat: anthropic → openai chat
// - to_openai_responses: anthropic → openai responses

use thiserror::Error;

pub mod passthrough;
pub mod to_openai_chat;
pub mod to_openai_responses;

pub use passthrough::convert_passthrough;
pub use to_openai_chat::convert_to_openai_chat;
pub use to_openai_responses::convert_to_openai_responses;

/// Claude Code 每请求注入 system 的计费+prompt 指纹块前缀(内容逐请求变化)。
/// 转换到 openai 侧必须剥离,否则上游缓存前缀每次请求全 miss。
/// 对齐 CPA util.IsClaudeCodeAttributionSystemText。
const CLAUDE_CODE_ATTRIBUTION_PREFIX: &str = "x-anthropic-billing-header:";

/// 是否为 Claude Code 计费归属文本(前导空白后以前缀开头)
pub fn is_attribution_text(text: &str) -> bool {
    text.trim_start().starts_with(CLAUDE_CODE_ATTRIBUTION_PREFIX)
}

/// type:object 节点递归补 properties:{}(对齐 CPA normalizeObjectSchemaProperties)。
/// 部分 OpenAI 兼容上游要求 object schema 必须带 properties。
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
            for (_, v) in map.iter_mut() {
                let taken = std::mem::take(v);
                *v = normalize_object_schema_properties(taken);
            }
            Value::Object(map)
        }
        Value::Array(items) => {
            Value::Array(items.into_iter().map(normalize_object_schema_properties).collect())
        }
        other => other,
    }
}

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
