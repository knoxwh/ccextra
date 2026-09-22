use axum::http::{header, HeaderMap};

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
