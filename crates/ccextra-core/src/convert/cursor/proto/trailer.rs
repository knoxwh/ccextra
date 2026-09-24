use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectError {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Error)]
pub enum TrailerError {
    #[error("invalid Connect end-stream trailer: {0}")]
    Json(#[from] serde_json::Error),
}

#[derive(Deserialize)]
struct Trailer<'a> {
    #[serde(borrow)]
    error: Option<ErrorBody<'a>>,
}

#[derive(Deserialize)]
struct ErrorBody<'a> {
    #[serde(borrow)]
    code: Option<std::borrow::Cow<'a, str>>,
    #[serde(borrow)]
    message: Option<std::borrow::Cow<'a, str>>,
}

pub fn parse_connect_end_stream(data: &[u8]) -> Result<Option<ConnectError>, TrailerError> {
    if data.is_empty() {
        return Ok(None);
    }
    let trailer: Trailer<'_> = serde_json::from_slice(data)?;
    let Some(error) = trailer.error else {
        return Ok(None);
    };
    Ok(Some(ConnectError {
        code: error.code.as_deref().unwrap_or("unknown").to_string(),
        message: error
            .message
            .as_deref()
            .unwrap_or("Unknown error")
            .to_string(),
    }))
}
