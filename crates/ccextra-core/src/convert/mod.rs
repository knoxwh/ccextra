// 协议转换:三条独立 body-to-body 转换
//
// - passthrough: claude → claude 只改 model
// - to_openai_chat: anthropic → openai chat
// - to_openai_responses: anthropic → openai responses
// - to_gemini: anthropic → gemini
// - to_antigravity: anthropic → antigravity

use thiserror::Error;

pub mod antigravity;
pub mod fix_json;
pub mod gemini;
pub mod gemini_response;
pub mod gemini_schema;
pub mod message_convert;
pub mod passthrough;
pub mod reasoning_replay;
pub mod shorten;
pub mod to_openai_chat;
pub mod to_openai_responses;
pub mod tool_id;
pub mod tool_sanitize;

pub use antigravity::convert_to_antigravity;
pub use fix_json::fix_json_quotes;
pub use gemini::convert_to_gemini;
pub use gemini_response::{
    convert_gemini_response, convert_gemini_stream_chunk, finalize_gemini_stream,
    force_finalize_gemini_stream, GeminiStreamState,
};
pub use gemini_schema::{
    clean_json_schema_for_antigravity, clean_json_schema_for_gemini,
    clean_nested_schema_for_antigravity,
};
pub use passthrough::convert_passthrough;
pub use reasoning_replay::{
    append_replay_turn, build_replay_turn, extract_replay_items, filter_replay_items,
    insert_replay_items, insert_replay_turns, REPLAY_TURN_TYPE,
};
pub use shorten::build_reverse_map;
pub use shorten::build_short_name_map;
pub use to_openai_chat::convert_to_openai_chat;
pub use to_openai_responses::{
    convert_to_openai_responses, is_thinking_signature_invalid, sanitize_gpt_reasoning_items,
    trim_encrypted_reasoning_items,
};

/// Claude Code 每请求注入 system 的计费+prompt 指纹块前缀(内容逐请求变化)。
/// 转换到 openai 侧必须剥离,否则上游缓存前缀每次请求全 miss。
const CLAUDE_CODE_ATTRIBUTION_PREFIX: &str = "x-anthropic-billing-header:";

/// 是否为 Claude Code 计费归属文本(前导空白后以前缀开头)
pub fn is_attribution_text(text: &str) -> bool {
    text.trim_start()
        .starts_with(CLAUDE_CODE_ATTRIBUTION_PREFIX)
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
            for (_, v) in map.iter_mut() {
                let taken = std::mem::take(v);
                *v = normalize_object_schema_properties(taken);
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
