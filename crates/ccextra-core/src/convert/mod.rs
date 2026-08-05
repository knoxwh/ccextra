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
