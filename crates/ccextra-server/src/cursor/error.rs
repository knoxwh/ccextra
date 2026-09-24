use axum::http::{HeaderMap, HeaderValue, StatusCode};
use ccextra_core::convert::cursor::proto::ConnectError;
use std::fmt;

#[derive(Debug)]
pub struct CursorFailure {
    pub status: StatusCode,
    pub message: String,
    pub retry_after: Option<HeaderValue>,
    pub progressed: bool,
}

impl CursorFailure {
    pub fn from_connect(error: ConnectError) -> Self {
        let code = error.code.to_ascii_lowercase();
        let status = match code.as_str() {
            "unauthenticated" => StatusCode::UNAUTHORIZED,
            "permission_denied" => StatusCode::FORBIDDEN,
            "resource_exhausted" => StatusCode::TOO_MANY_REQUESTS,
            "unavailable" => StatusCode::SERVICE_UNAVAILABLE,
            "internal" => StatusCode::INTERNAL_SERVER_ERROR,
            "invalid_argument" => StatusCode::BAD_REQUEST,
            _ if error.message.to_ascii_lowercase().contains("rate limit")
                || error.message.to_ascii_lowercase().contains("quota") =>
            {
                StatusCode::TOO_MANY_REQUESTS
            }
            _ => StatusCode::BAD_GATEWAY,
        };
        Self {
            status,
            message: format!("Cursor Connect error {}: {}", error.code, error.message),
            retry_after: None,
            progressed: false,
        }
    }

    pub fn from_http(status: StatusCode, headers: &HeaderMap) -> Self {
        Self {
            status,
            message: format!("Cursor 上游返回 HTTP {}", status.as_u16()),
            retry_after: headers.get("retry-after").cloned(),
            progressed: false,
        }
    }

    pub fn from_transport(error: impl fmt::Display) -> Self {
        let message = error.to_string();
        let status = if message.to_ascii_lowercase().contains("timeout") {
            StatusCode::GATEWAY_TIMEOUT
        } else {
            StatusCode::BAD_GATEWAY
        };
        Self {
            status,
            message: format!("Cursor 流中断: {message}"),
            retry_after: None,
            progressed: false,
        }
    }

    pub fn retryable(&self) -> bool {
        self.status.is_server_error()
    }
}

impl fmt::Display for CursorFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.message.fmt(f)
    }
}

impl std::error::Error for CursorFailure {}
