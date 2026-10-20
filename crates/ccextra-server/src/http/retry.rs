/// Retry-After 头解析:仅支持秒数形式(HTTP-date 忽略,交客户端兜底)
pub fn parse_retry_after(headers: &axum::http::HeaderMap) -> Option<std::time::Duration> {
    let raw = headers
        .get(axum::http::header::RETRY_AFTER)?
        .to_str()
        .ok()?;
    let secs = raw.trim().parse::<u64>().ok()?;
    Some(std::time::Duration::from_secs(secs))
}
