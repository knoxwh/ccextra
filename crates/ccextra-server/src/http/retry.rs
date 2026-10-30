/// Retry-After 头解析:仅支持秒数形式(HTTP-date 忽略,交客户端兜底)
pub fn parse_retry_after(headers: &axum::http::HeaderMap) -> Option<std::time::Duration> {
    let raw = headers
        .get(axum::http::header::RETRY_AFTER)?
        .to_str()
        .ok()?;
    let secs = raw.trim().parse::<u64>().ok()?;
    Some(std::time::Duration::from_secs(secs))
}

/// Google 429 错误 body 中的重试等待(Antigravity 无 Retry-After 头时用)。
///
/// 移植 CPA `helps.ParseRetryDelay`,顺序一致:
/// 1. `error.details[]` 的 `google.rpc.RetryInfo.retryDelay`(解析失败即放弃,不再看后续来源)
/// 2. `google.rpc.ErrorInfo.metadata.quotaResetDelay`
/// 3. `error.message` 的 `after Ns`,再试小写后的 `after 1h2m3s`(须 > 0)
pub fn parse_google_retry_delay(body: &[u8]) -> Option<std::time::Duration> {
    use once_cell::sync::Lazy;
    static AFTER_SECS: Lazy<regex::Regex> =
        Lazy::new(|| regex::Regex::new(r"after\s+(\d+)s\.?").expect("valid regex"));
    static AFTER_HUMAN: Lazy<regex::Regex> = Lazy::new(|| {
        regex::Regex::new(r"after\s+((?:\d+h)?(?:\d+m)?(?:\d+s)?)\.?").expect("valid regex")
    });

    let v: serde_json::Value = serde_json::from_slice(body).ok()?;
    if let Some(details) = v.pointer("/error/details").and_then(|d| d.as_array()) {
        let detail_str = |d: &serde_json::Value, ty: &str, ptr: &str| -> Option<String> {
            (d.get("@type")?.as_str()? == ty)
                .then(|| d.pointer(ptr)?.as_str().map(str::to_owned))
                .flatten()
                .filter(|s| !s.is_empty())
        };
        if let Some(raw) = details
            .iter()
            .find_map(|d| detail_str(d, "type.googleapis.com/google.rpc.RetryInfo", "/retryDelay"))
        {
            return parse_go_duration(&raw);
        }
        if let Some(d) = details.iter().find_map(|d| {
            detail_str(
                d,
                "type.googleapis.com/google.rpc.ErrorInfo",
                "/metadata/quotaResetDelay",
            )
            .and_then(|raw| parse_go_duration(&raw))
        }) {
            return Some(d);
        }
    }
    let message = v.pointer("/error/message")?.as_str()?;
    if let Some(secs) = AFTER_SECS
        .captures(message)
        .and_then(|c| c[1].parse::<u64>().ok())
    {
        return Some(std::time::Duration::from_secs(secs));
    }
    AFTER_HUMAN
        .captures(&message.to_lowercase())
        .and_then(|c| parse_go_duration(&c[1]))
        .filter(|d| !d.is_zero())
}

/// Retry-After 头只能写整秒:向上取整且至少 1s(对齐 CPA `safeRetryAfterHeader`)
pub fn retry_after_header_secs(delay: std::time::Duration) -> std::time::Duration {
    let secs = delay.as_secs() + u64::from(delay.subsec_nanos() > 0);
    std::time::Duration::from_secs(secs.max(1))
}

/// Go `time.ParseDuration` 子集:`[+]?(数字[.数字]单位)+` 或 `0`;单位 ns/us/µs/μs/ms/s/m/h。
/// 负值返回 None(Duration 无负数)。
fn parse_go_duration(raw: &str) -> Option<std::time::Duration> {
    let s = raw.strip_prefix('+').unwrap_or(raw);
    if s == "0" {
        return Some(std::time::Duration::ZERO);
    }
    if s.is_empty() || s.starts_with('-') {
        return None;
    }
    let mut rest = s;
    let mut total = 0f64;
    while !rest.is_empty() {
        let num_len = rest
            .find(|c: char| !(c.is_ascii_digit() || c == '.'))
            .unwrap_or(rest.len());
        let num = &rest[..num_len];
        if num.is_empty() || num == "." {
            return None;
        }
        let value: f64 = num.parse().ok()?;
        rest = &rest[num_len..];
        let unit_len = rest
            .find(|c: char| c.is_ascii_digit() || c == '.')
            .unwrap_or(rest.len());
        let unit_secs = match &rest[..unit_len] {
            "ns" => 1e-9,
            "us" | "\u{b5}s" | "\u{3bc}s" => 1e-6,
            "ms" => 1e-3,
            "s" => 1.0,
            "m" => 60.0,
            "h" => 3600.0,
            _ => return None,
        };
        total += value * unit_secs;
        rest = &rest[unit_len..];
    }
    std::time::Duration::try_from_secs_f64(total).ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    fn retry_info(delay: &str) -> Vec<u8> {
        serde_json::json!({"error": {"code": 429, "message": "quota", "details": [
            {"@type": "type.googleapis.com/google.rpc.ErrorInfo", "metadata": {"quotaResetDelay": "9s"}},
            {"@type": "type.googleapis.com/google.rpc.RetryInfo", "retryDelay": delay}
        ]}})
        .to_string()
        .into_bytes()
    }

    #[test]
    fn retry_info_first_then_header_rounding() {
        let d = parse_google_retry_delay(&retry_info("30s")).unwrap();
        assert_eq!(d, Duration::from_secs(30));
        assert_eq!(retry_after_header_secs(d), Duration::from_secs(30));
        let d = parse_google_retry_delay(&retry_info("0.479417207s")).unwrap();
        assert_eq!(retry_after_header_secs(d), Duration::from_secs(1));
        let d = parse_google_retry_delay(&retry_info("3600s")).unwrap();
        assert_eq!(retry_after_header_secs(d), Duration::from_secs(3600));
        assert_eq!(
            retry_after_header_secs(Duration::from_millis(1500)),
            Duration::from_secs(2)
        );
        assert_eq!(
            retry_after_header_secs(Duration::ZERO),
            Duration::from_secs(1)
        );
    }

    #[test]
    fn bad_retry_info_stops_without_fallback() {
        // CPA:RetryInfo 解析失败直接返回错误,不看 quotaResetDelay / message
        assert_eq!(parse_google_retry_delay(&retry_info("soon")), None);
    }

    #[test]
    fn quota_reset_delay_and_message_fallbacks() {
        let body = br#"{"error":{"message":"x","details":[{"@type":"type.googleapis.com/google.rpc.ErrorInfo","metadata":{"quotaResetDelay":"1h2m3s"}}]}}"#;
        assert_eq!(
            parse_google_retry_delay(body),
            Some(Duration::from_secs(3723))
        );
        let body = br#"{"error":{"message":"Your quota will reset after 30s."}}"#;
        assert_eq!(
            parse_google_retry_delay(body),
            Some(Duration::from_secs(30))
        );
        // CPA 首个正则不要求 > 0:`after 0s` 返回 0,写头时再抬到 1
        let body = br#"{"error":{"message":"try after 0s"}}"#;
        assert_eq!(parse_google_retry_delay(body), Some(Duration::ZERO));
        let body = br#"{"error":{"message":"Quota resets AFTER 2M5S."}}"#;
        assert_eq!(
            parse_google_retry_delay(body),
            Some(Duration::from_secs(125))
        );
    }

    #[test]
    fn no_delay_yields_none() {
        for body in [
            &br#"{"error":{"message":"rate limited"}}"#[..],
            br#"{"error":{"message":"try after soon"}}"#,
            br#"<html>429</html>"#,
            br#"{"error":{"details":[{"@type":"type.googleapis.com/google.rpc.ErrorInfo","metadata":{"quotaResetDelay":"bad"}}]}}"#,
        ] {
            assert_eq!(parse_google_retry_delay(body), None, "{body:?}");
        }
    }

    #[test]
    fn go_duration_subset() {
        assert_eq!(parse_go_duration("0"), Some(Duration::ZERO));
        assert_eq!(parse_go_duration("1.5h"), Some(Duration::from_secs(5400)));
        assert_eq!(parse_go_duration("250ms"), Some(Duration::from_millis(250)));
        assert_eq!(parse_go_duration("2us"), Some(Duration::from_micros(2)));
        assert_eq!(parse_go_duration("-1s"), None);
        assert_eq!(parse_go_duration("10"), None);
        assert_eq!(parse_go_duration("s"), None);
    }
}
