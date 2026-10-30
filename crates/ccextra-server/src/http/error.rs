use axum::{
    http::StatusCode,
    response::{IntoResponse, Response},
    Json,
};
use ccextra_core::convert::ConvertError;
use ccextra_core::route::RouteError;
use serde_json::{json, Value};

#[derive(Debug)]
pub struct AppError {
    pub status: StatusCode,
    pub err: anyhow::Error,
}

impl AppError {
    pub fn new(err: impl Into<anyhow::Error>) -> Self {
        let err = err.into();
        // 等响应头 SEND_TIMEOUT:对齐 CPA incomplete/timeout → 408 + timeout_error
        if is_upstream_send_timeout(&err) {
            return Self {
                status: StatusCode::REQUEST_TIMEOUT,
                err,
            };
        }
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            err,
        }
    }

    pub fn unauthorized(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            err: anyhow::anyhow!(msg.into()),
        }
    }

    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            err: anyhow::anyhow!(msg.into()),
        }
    }

    /// 指定状态码构造(如上游 body 超限 502 / 停顿 504,保留上游状态)
    pub fn with_status(status: StatusCode, msg: impl Into<String>) -> Self {
        Self {
            status,
            err: anyhow::anyhow!(msg.into()),
        }
    }

    pub fn not_found(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            err: anyhow::anyhow!(msg.into()),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let error_type = match self.status {
            StatusCode::BAD_REQUEST => "invalid_request_error",
            StatusCode::UNAUTHORIZED => "authentication_error",
            StatusCode::NOT_FOUND => "not_found_error",
            StatusCode::UNPROCESSABLE_ENTITY => "invalid_request_error",
            // 对齐 CPA claudeErrorTypeFromStatus:408/504 → timeout_error
            StatusCode::REQUEST_TIMEOUT | StatusCode::GATEWAY_TIMEOUT => "timeout_error",
            _ => "api_error",
        };
        let body = json!({
            "type": "error",
            "error": {
                "type": error_type,
                "message": self.err.to_string()
            }
        });
        (self.status, Json(body)).into_response()
    }
}

impl From<RouteError> for AppError {
    fn from(err: RouteError) -> Self {
        match err {
            RouteError::ModelNotFound(_) => Self::not_found(err.to_string()),
            RouteError::AliasConflict(_) => Self::new(err),
            RouteError::EmptyBaseUrl(_) | RouteError::InvalidProxy(..) => Self::new(err),
        }
    }
}

impl From<anyhow::Error> for AppError {
    fn from(err: anyhow::Error) -> Self {
        Self::new(err)
    }
}

impl From<ConvertError> for AppError {
    fn from(err: ConvertError) -> Self {
        Self::new(err)
    }
}

impl From<reqwest::Error> for AppError {
    fn from(err: reqwest::Error) -> Self {
        Self::new(err)
    }
}

/// 等响应头 SEND_TIMEOUT 文案(见 upstream::send_with_timeout*)
fn is_upstream_send_timeout(err: &anyhow::Error) -> bool {
    err.chain().any(|e| e.to_string().contains("上游请求超时"))
}

/// 上游错误 body → anthropic 错误形状
/// `{"type":"error","error":{"type":...,"message":...},"request_id"?}`
///
/// 兼容两类上游错误结构:
/// 1. OpenAI 标准 `{"error":{"type":...,"message":...}}`
/// 2. 阿里云百炼 `{"code":"Throttling.RateQuota","message":"{\"error\":{...}}"}`——
///    code/message 平铺在顶层,且 message 是嵌套 JSON 字符串,真实错误藏在里面。
///    提取不到 error 时不丢真实信息,用顶层 code/message 兜底。
///
/// Anthropic 官方 type 原样保留;无法识别时按上游 `status` 兜底
/// (对齐 CPA `claudeErrorTypeFromStatus`)。顶层字符串 `request_id` 保留(官方错误形状)。
pub fn to_anthropic_error(body: &[u8], status: StatusCode) -> Vec<u8> {
    let (raw_type, raw_message) = extract_upstream_error(body);
    let err_type = match raw_type.to_lowercase().as_str() {
        t @ ("invalid_request_error"
        | "authentication_error"
        | "billing_error"
        | "permission_error"
        | "not_found_error"
        | "conflict_error"
        | "request_too_large"
        | "rate_limit_error"
        | "timeout_error"
        | "api_error"
        | "overloaded_error") => t.to_string(),
        "rate_limit" | "requests" | "tokens" => "rate_limit_error".to_string(),
        // 阿里云百炼/OpenAI 的裸类型名(EngineOverloadedError 等)归入语义相近的错误
        t if t.contains("overload") => "overloaded_error".to_string(),
        t if t.contains("rate") || t.contains("quota") => "rate_limit_error".to_string(),
        t if t.contains("auth") || t.contains("apikey") || t.contains("forbidden") => {
            "authentication_error".to_string()
        }
        _ => error_type_from_status(status).to_string(),
    };
    let message = if raw_message.is_empty() {
        "upstream error".to_string()
    } else {
        raw_message
    };
    let mut out = json!({
        "type": "error",
        "error": {"type": err_type, "message": message}
    });
    if let Some(id) = serde_json::from_slice::<Value>(body)
        .ok()
        .and_then(|v| v.get("request_id")?.as_str().map(str::to_owned))
    {
        out["request_id"] = Value::String(id);
    }
    serde_json::to_vec(&out).unwrap_or_default()
}

/// 无可识别 type 时按 HTTP 状态选 Anthropic error type(对齐 CPA `claudeErrorTypeFromStatus`)
fn error_type_from_status(status: StatusCode) -> &'static str {
    match status.as_u16() {
        401 => "authentication_error",
        402 => "billing_error",
        403 => "permission_error",
        404 => "not_found_error",
        409 => "conflict_error",
        413 => "request_too_large",
        429 => "rate_limit_error",
        408 | 504 => "timeout_error",
        529 => "overloaded_error",
        s if s >= 500 => "api_error",
        _ => "invalid_request_error",
    }
}

/// 从上游错误 body 提取 (type, message)。优先标准 `error` 对象,其次
/// 顶层 `code`/`message`(百炼风格),message 为字符串时尝试二次解析嵌套 JSON。
pub fn extract_upstream_error(body: &[u8]) -> (String, String) {
    let Ok(v) = serde_json::from_slice::<Value>(body) else {
        return (
            String::new(),
            String::from_utf8_lossy(body).trim().to_string(),
        );
    };

    // OpenAI 标准结构
    if let Some(err) = v.get("error") {
        let t = err
            .get("type")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let m = err
            .get("message")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        if !t.is_empty() || !m.is_empty() {
            return (t, m);
        }
    }

    // 阿里云百炼风格:顶层 code/message
    let code = v
        .get("code")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let msg = v.get("message").cloned();
    // message 可能是嵌套 JSON 字符串,真实错误对象藏在里面
    if let Some(Value::String(s)) = &msg {
        if let Ok(inner) = serde_json::from_str::<Value>(s) {
            if let Some(err) = inner.get("error") {
                let t = err
                    .get("type")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                let m = err
                    .get("message")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                if !t.is_empty() || !m.is_empty() {
                    return (if t.is_empty() { code } else { t }, m);
                }
            }
            // 二次解析的对象里没有 error,直接取其 message(若有)
            if let Some(m) = inner.get("message").and_then(|x| x.as_str()) {
                return (code, m.to_string());
            }
        }
    }
    match msg {
        // message 是普通字符串(二次解析失败或非嵌套)
        Some(Value::String(s)) => (code, s),
        _ => (code, String::new()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::response::IntoResponse;

    async fn body_json(resp: axum::response::Response) -> Value {
        let bytes = to_bytes(resp.into_body(), 1024 * 1024).await.unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    #[tokio::test]
    async fn send_timeout_maps_to_408_timeout_error() {
        let err = AppError::new(anyhow::anyhow!("上游请求超时 (60s)"));
        let resp = err.into_response();
        assert_eq!(resp.status(), StatusCode::REQUEST_TIMEOUT);
        let v = body_json(resp).await;
        assert_eq!(v["type"], "error");
        assert_eq!(v["error"]["type"], "timeout_error");
        assert!(v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("上游请求超时"));
    }

    #[tokio::test]
    async fn gateway_timeout_maps_to_timeout_error() {
        let err = AppError::with_status(
            StatusCode::GATEWAY_TIMEOUT,
            "上游响应读取停顿: idle timeout",
        );
        let resp = err.into_response();
        assert_eq!(resp.status(), StatusCode::GATEWAY_TIMEOUT);
        let v = body_json(resp).await;
        assert_eq!(v["error"]["type"], "timeout_error");
    }

    #[tokio::test]
    async fn plain_500_stays_api_error() {
        let err = AppError::new(anyhow::anyhow!("构造响应失败: boom"));
        let resp = err.into_response();
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
        let v = body_json(resp).await;
        assert_eq!(v["error"]["type"], "api_error");
    }
}
