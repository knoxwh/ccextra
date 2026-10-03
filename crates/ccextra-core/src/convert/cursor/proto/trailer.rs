use base64::{engine::general_purpose::STANDARD, Engine};
use serde::Deserialize;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectError {
    pub code: String,
    pub message: String,
    /// details 里提取的可读文本(base64 解码后扫描可打印段)
    pub details: Vec<String>,
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
    details: Option<Vec<ErrorDetail>>,
}

#[derive(Deserialize)]
struct ErrorDetail {
    /// base64 编码的序列化 protobuf 字节
    value: Option<String>,
}

/// 从 ErrorDetail value 提取可读文本:扫描连续可打印 ASCII 段(≥6 字符)
///
/// proto codec 下 value 是序列化 protobuf,字符串字段常可直接读出;
/// 对齐 cursoride2api 的 base64 解码做法,但按段扫描而非整段当 UTF-8
fn readable_runs(bytes: &[u8]) -> Vec<String> {
    let mut runs = Vec::new();
    let mut current: Vec<u8> = Vec::new();
    for &byte in bytes {
        if (0x20..0x7f).contains(&byte) {
            current.push(byte);
        } else {
            if current.len() >= 6 {
                runs.push(String::from_utf8_lossy(&current).into_owned());
            }
            current.clear();
        }
    }
    if current.len() >= 6 {
        runs.push(String::from_utf8_lossy(&current).into_owned());
    }
    runs
}

pub fn parse_connect_end_stream(data: &[u8]) -> Result<Option<ConnectError>, TrailerError> {
    if data.is_empty() {
        return Ok(None);
    }
    let trailer: Trailer<'_> = serde_json::from_slice(data)?;
    let Some(error) = trailer.error else {
        return Ok(None);
    };
    let mut details = Vec::new();
    for detail in error.details.unwrap_or_default() {
        let Some(value) = detail.value else {
            continue;
        };
        if let Ok(bytes) = STANDARD.decode(value.as_bytes()) {
            details.extend(readable_runs(&bytes));
        }
    }
    Ok(Some(ConnectError {
        code: error.code.as_deref().unwrap_or("unknown").to_string(),
        message: error
            .message
            .as_deref()
            .unwrap_or("Unknown error")
            .to_string(),
        details,
    }))
}
