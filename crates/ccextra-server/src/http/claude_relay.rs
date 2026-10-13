use crate::claude::constants::OAUTH_BETA;
use axum::http::{header, HeaderMap, HeaderName, HeaderValue};

/// 构建 Claude 中转请求头:保留入站头,排除认证、代理重建及连接管理头。
pub fn claude_relay_headers(headers: &HeaderMap) -> HeaderMap {
    let connection_header_names: Vec<String> = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_ascii_lowercase)
        .collect();
    let mut relay_headers = HeaderMap::new();
    for (name, value) in headers.iter() {
        if !is_claude_relay_header_excluded(name.as_str(), &connection_header_names) {
            relay_headers.append(name.clone(), value.clone());
        }
    }
    relay_headers
}

pub fn is_claude_relay_header_excluded(name: &str, connection_header_names: &[String]) -> bool {
    let name = name.to_ascii_lowercase();
    connection_header_names.iter().any(|item| item == &name)
        || matches!(
            name.as_str(),
            "authorization"
                | "x-api-key"
                // ccextra 出站订阅身份头,入站同名头不得透传或触发禁 redirect
                | "chatgpt-account-id"
                | "user-agent"
                | "host"
                | "content-length"
                // upstream.rs 统一声明 application/json;reqwest .header 为 append,
                // 透传入站 Content-Type 会与之叠成双头
                | "content-type"
                // reqwest 未启用解压 feature:转发客户端 accept-encoding 会让上游
                // 返回压缩 body,错误路径按原始字节解析会得到乱码,故剥掉
                | "accept-encoding"
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
}
