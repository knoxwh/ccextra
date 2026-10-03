mod input;
pub mod proto;
mod request;
mod schema;

pub use request::{build_run_request, conversation_id, CursorRunRequest};
pub use schema::decode_mcp_args;

#[derive(Debug, thiserror::Error)]
pub enum CursorConvertError {
    #[error("Cursor 请求无效: {0}")]
    Invalid(String),
    #[error("Cursor 暂不支持: {0}")]
    Unsupported(String),
    #[error(transparent)]
    Json(#[from] serde_json::Error),
}
