use crate::claude::constants::OAUTH_BETA;
use axum::http::{header, HeaderMap, HeaderName, HeaderValue};

/// Claude Code / CPA 对齐的 Accept-Encoding(`protocol: claude` 直通)。
///
/// 与 CLI 2.1.296 MITM 金样及 CPA `defaultAcceptEncoding` 一致。
pub const CLAUDE_ACCEPT_ENCODING: &str = "gzip, deflate, br, zstd";

/// Claude 原生直通(`protocol: claude`,入站已是 Anthropic messages)的最小出站头。
///
/// 只剥身份 / Host / CL / hop-by-hop / 误带的 chatgpt-account-id / CT:
/// Stainless、`anthropic-*`、入站 UA/Accept/AE 尽量原样。
/// AE 缺省时补 [`CLAUDE_ACCEPT_ENCODING`](对齐 CLI/CPA);reqwest/wreq 已开
/// gzip/br/deflate/zstd 自动解压,与广告能力一致。
/// CT 仍剥,由 upstream 统一写单条 `application/json`。
///
/// 协议转换出站不是 Anthropic 头组装,不走本函数。
pub fn claude_passthrough_headers(headers: &HeaderMap) -> HeaderMap {
    let connection_header_names: Vec<String> = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_ascii_lowercase)
        .collect();
    let mut out = HeaderMap::new();
    for (name, value) in headers.iter() {
        if !is_claude_passthrough_header_excluded(name.as_str(), &connection_header_names) {
            out.append(name.clone(), value.clone());
        }
    }
    ensure_claude_accept_encoding(&mut out);
    out
}

pub fn is_claude_passthrough_header_excluded(
    name: &str,
    connection_header_names: &[String],
) -> bool {
    let name = name.to_ascii_lowercase();
    connection_header_names.iter().any(|item| item == &name)
        || matches!(
            name.as_str(),
            "authorization"
                | "x-api-key"
                | "chatgpt-account-id"
                | "host"
                | "content-length"
                // upstream 统一声明 application/json,避免双 CT
                | "content-type"
                // AE 直通保留;缺省由 ensure_claude_accept_encoding 补齐
                | "connection"
                | "keep-alive"
                | "proxy-connection"
                | "proxy-authenticate"
                | "proxy-authorization"
                | "te"
                | "trailer"
                | "transfer-encoding"
                | "upgrade"
                | "http2-settings"
        )
}

/// 缺 Accept-Encoding 时补 CLI/CPA 金样;已有则不动(透传入站值)。
pub fn ensure_claude_accept_encoding(headers: &mut HeaderMap) {
    if headers.contains_key(header::ACCEPT_ENCODING) {
        return;
    }
    headers.insert(
        header::ACCEPT_ENCODING,
        HeaderValue::from_static(CLAUDE_ACCEPT_ENCODING),
    );
}

pub fn claude_inbound_user_agent(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
}

/// 订阅 OAuth (`sk-ant-oat*`) 出站时:入站 anthropic-beta 缺 oauth beta 则追加。
/// 不改写已有 beta 列表顺序以外的内容;已含则不动。
pub fn ensure_oauth_beta(headers: &mut HeaderMap) {
    let beta_name = HeaderName::from_static("anthropic-beta");
    let mut already = false;
    let mut first: Option<String> = None;
    for value in headers.get_all(&beta_name) {
        if let Ok(s) = value.to_str() {
            if first.is_none() {
                first = Some(s.to_string());
            }
            if beta_list_contains(s, OAUTH_BETA) {
                already = true;
                break;
            }
        }
    }
    if already {
        return;
    }
    match first {
        Some(existing) => {
            let merged = if existing.trim().is_empty() {
                OAUTH_BETA.to_string()
            } else {
                format!("{existing},{OAUTH_BETA}")
            };
            headers.remove(&beta_name);
            if let Ok(v) = HeaderValue::from_str(&merged) {
                headers.insert(beta_name, v);
            }
        }
        None => {
            if let Ok(v) = HeaderValue::from_str(OAUTH_BETA) {
                headers.insert(beta_name, v);
            }
        }
    }
}

fn beta_list_contains(list: &str, needle: &str) -> bool {
    list.split(',')
        .map(str::trim)
        .any(|part| part.eq_ignore_ascii_case(needle))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ensure_oauth_beta_appends_when_missing() {
        let mut map = HeaderMap::new();
        map.insert(
            HeaderName::from_static("anthropic-beta"),
            HeaderValue::from_static("claude-code-20250219"),
        );
        ensure_oauth_beta(&mut map);
        let got = map.get("anthropic-beta").unwrap().to_str().unwrap();
        assert_eq!(got, "claude-code-20250219,oauth-2025-04-20");
    }

    #[test]
    fn test_ensure_oauth_beta_noop_when_present() {
        let mut map = HeaderMap::new();
        map.insert(
            HeaderName::from_static("anthropic-beta"),
            HeaderValue::from_static("claude-code-20250219,oauth-2025-04-20"),
        );
        ensure_oauth_beta(&mut map);
        let got = map.get("anthropic-beta").unwrap().to_str().unwrap();
        assert_eq!(got, "claude-code-20250219,oauth-2025-04-20");
    }

    #[test]
    fn test_ensure_oauth_beta_inserts_when_absent() {
        let mut map = HeaderMap::new();
        ensure_oauth_beta(&mut map);
        let got = map.get("anthropic-beta").unwrap().to_str().unwrap();
        assert_eq!(got, "oauth-2025-04-20");
    }

    #[test]
    fn test_passthrough_keeps_ua_stainless_and_ae() {
        let mut headers = HeaderMap::new();
        for (k, v) in [
            ("authorization", "Bearer inbound"),
            ("x-api-key", "inbound-key"),
            ("host", "inbound.example"),
            ("content-length", "99"),
            ("content-type", "application/json; charset=utf-8"),
            ("accept-encoding", "gzip, deflate, br, zstd"),
            ("user-agent", "claude-cli/2.1.296 (external, sdk-cli)"),
            ("accept", "application/json"),
            ("anthropic-version", "2023-06-01"),
            ("x-stainless-os", "MacOS"),
            ("x-app", "cli"),
            ("connection", "keep-alive, x-hop"),
            ("x-hop", "drop-me"),
        ] {
            headers.append(
                HeaderName::from_bytes(k.as_bytes()).unwrap(),
                HeaderValue::from_str(v).unwrap(),
            );
        }
        let out = claude_passthrough_headers(&headers);
        assert_eq!(
            out.get("user-agent").and_then(|v| v.to_str().ok()),
            Some("claude-cli/2.1.296 (external, sdk-cli)")
        );
        assert_eq!(
            out.get("accept").and_then(|v| v.to_str().ok()),
            Some("application/json")
        );
        assert_eq!(
            out.get("anthropic-version").and_then(|v| v.to_str().ok()),
            Some("2023-06-01")
        );
        assert_eq!(
            out.get("x-stainless-os").and_then(|v| v.to_str().ok()),
            Some("MacOS")
        );
        assert_eq!(out.get("x-app").and_then(|v| v.to_str().ok()), Some("cli"));
        assert_eq!(
            out.get("accept-encoding").and_then(|v| v.to_str().ok()),
            Some("gzip, deflate, br, zstd")
        );
        for excluded in [
            "authorization",
            "x-api-key",
            "host",
            "content-length",
            "content-type",
            "connection",
            "x-hop",
        ] {
            assert!(!out.contains_key(excluded), "{excluded}");
        }
    }

    #[test]
    fn test_passthrough_fills_default_ae_when_missing() {
        let out = claude_passthrough_headers(&HeaderMap::new());
        assert_eq!(
            out.get("accept-encoding").and_then(|v| v.to_str().ok()),
            Some(CLAUDE_ACCEPT_ENCODING)
        );
    }
}
