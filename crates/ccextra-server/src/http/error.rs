use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use ccextra_core::convert::ConvertError;
use ccextra_core::route::RouteError;
use serde_json::{Value, json};

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
            StatusCode::UNSUPPORTED_MEDIA_TYPE => "invalid_request_error",
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
            RouteError::AliasConflict(_)
            | RouteError::EmptyBaseUrl(_)
            | RouteError::EmptyKey(_)
            | RouteError::OfficialNonEmptyKey(_)
            | RouteError::InvalidOfficialBaseUrl(..)
            | RouteError::OfficialMetadataNotAllowed(_)
            | RouteError::OfficialAliasMismatch(..)
            | RouteError::InvalidWildcardModel(..)
            | RouteError::InvalidProxy(..) => Self::new(err),
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
    serde_json::to_vec(&anthropic_error_value(body, status)).unwrap_or_default()
}

/// Gemini / Antigravity 上游错误 → anthropic 错误形状;`invalid_request_error` 且文案为
/// Google 输入 token 超限时加 `prompt is too long: ` 前缀,让 Claude Code 识别上下文溢出
/// 并自动压缩(对齐 CPA `ff9339fe` normalizeClaudePromptTooLongMessage)。
pub fn to_google_anthropic_error(body: &[u8], status: StatusCode) -> Vec<u8> {
    let mut out = anthropic_error_value(body, status);
    if out["error"]["type"] == "invalid_request_error"
        && let Some(prefixed) = out["error"]["message"]
            .as_str()
            .and_then(prompt_too_long_message)
    {
        out["error"]["message"] = Value::String(prefixed);
    }
    serde_json::to_vec(&out).unwrap_or_default()
}

/// 已带 `prompt is too long` 前缀(大小写不敏感)的不重复加;不匹配返回 None
fn prompt_too_long_message(message: &str) -> Option<String> {
    let lower = message.to_lowercase();
    if lower.starts_with("prompt is too long") {
        return None;
    }
    is_google_input_token_exceeded(&lower).then(|| format!("prompt is too long: {message}"))
}

/// 对齐 CPA isGoogleInputTokenExceededMessage:`input token count` 后(可选括号计数)
/// 紧跟 `exceeds the maximum`;`output token count exceeds…; input token count: N` 不算
fn is_google_input_token_exceeded(lower: &str) -> bool {
    const PHRASE: &str = "input token count";
    let mut remaining = lower;
    while let Some(idx) = remaining.find(PHRASE) {
        let after = &remaining[idx + PHRASE.len()..];
        let mut rest = after.trim_start();
        if rest.starts_with('(')
            && let Some(end) = rest.find(')')
        {
            rest = rest[end + 1..].trim_start();
        }
        if rest.starts_with("exceeds the maximum") {
            return true;
        }
        remaining = after;
    }
    false
}

fn anthropic_error_value(body: &[u8], status: StatusCode) -> Value {
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
    out
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
    if let Some(Value::String(s)) = &msg
        && let Ok(inner) = serde_json::from_str::<Value>(s)
    {
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
        assert!(
            v["error"]["message"]
                .as_str()
                .unwrap()
                .contains("上游请求超时")
        );
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

    const GOOGLE_MSG: &str = "The input token count exceeds the maximum number of tokens allowed (1048576); input token count: 1050377.";

    fn google_error(code: u16, message: &str, status: &str) -> Vec<u8> {
        serde_json::to_vec(&json!({"error": {"code": code, "message": message, "status": status}}))
            .unwrap()
    }

    fn google_out(body: &[u8], status: StatusCode) -> Value {
        serde_json::from_slice(&to_google_anthropic_error(body, status)).unwrap()
    }

    /// 对齐 CPA ff9339fe:Google 400 输入 token 超限加 `prompt is too long: ` 前缀
    #[test]
    fn google_input_token_exceeded_gets_prompt_too_long_prefix() {
        let out = google_out(
            &google_error(400, GOOGLE_MSG, "INVALID_ARGUMENT"),
            StatusCode::BAD_REQUEST,
        );
        assert_eq!(out["error"]["type"], "invalid_request_error");
        assert_eq!(
            out["error"]["message"],
            format!("prompt is too long: {GOOGLE_MSG}")
        );
    }

    #[test]
    fn google_parenthesized_token_count_variant_gets_prefix() {
        let msg = "The input token count (1050377) exceeds the maximum number of tokens allowed (1048576).";
        let out = google_out(
            &google_error(400, msg, "INVALID_ARGUMENT"),
            StatusCode::BAD_REQUEST,
        );
        assert_eq!(
            out["error"]["message"],
            format!("prompt is too long: {msg}")
        );
    }

    #[test]
    fn already_prefixed_prompt_too_long_is_not_doubled() {
        let msg = format!("Prompt is too long: {GOOGLE_MSG}");
        let body = serde_json::to_vec(
            &json!({"type": "error", "error": {"type": "invalid_request_error", "message": msg}}),
        )
        .unwrap();
        let out = google_out(&body, StatusCode::BAD_REQUEST);
        assert_eq!(out["error"]["message"], msg);
    }

    #[test]
    fn google_token_exceeded_non_invalid_request_is_untouched() {
        let out = google_out(
            &google_error(500, GOOGLE_MSG, "INTERNAL"),
            StatusCode::INTERNAL_SERVER_ERROR,
        );
        assert_eq!(out["error"]["type"], "api_error");
        assert_eq!(out["error"]["message"], GOOGLE_MSG);
    }

    #[test]
    fn output_token_limit_mentioning_input_count_is_untouched() {
        let msg = "Requested output token count exceeds the maximum; input token count: 100.";
        let out = google_out(
            &google_error(400, msg, "INVALID_ARGUMENT"),
            StatusCode::BAD_REQUEST,
        );
        assert_eq!(out["error"]["type"], "invalid_request_error");
        assert_eq!(out["error"]["message"], msg);
    }

    /// 非 Google 车道(to_anthropic_error)不改写,只作用于 Gemini / Antigravity
    #[test]
    fn non_google_error_conversion_keeps_message() {
        let body = google_error(400, GOOGLE_MSG, "INVALID_ARGUMENT");
        let out: Value =
            serde_json::from_slice(&to_anthropic_error(&body, StatusCode::BAD_REQUEST)).unwrap();
        assert_eq!(out["error"]["message"], GOOGLE_MSG);
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
