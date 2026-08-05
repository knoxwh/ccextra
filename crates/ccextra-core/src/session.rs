// 会话身份派生(头优先,user_id 兜底)
//
// 优先级:
// 1. 请求头 X-Claude-Code-Session-Id(Claude Code 原生发送,整会话稳定,
//    压缩/续接/reminder 注入均不影响)
// 2. metadata.user_id 尾部 `_session_<uuid>`(或 JSON 形态的 session_id)
//
// 背景:messages[0] 会被 Claude Code 每请求注入 system-reminder、
// 上下文压缩后整体替换,不是稳定身份,不采用。

use http::HeaderMap;
use serde_json::Value;

/// Claude Code 会话头(小写形式,HeaderMap 查找大小写不敏感)
pub const CLAUDE_CODE_SESSION_HEADER: &str = "x-claude-code-session-id";

/// 提取 Claude Code 会话 ID(头优先,user_id 兜底)
pub fn extract_claude_code_session(headers: &HeaderMap, body: &Value) -> Option<String> {
    if let Some(v) = headers
        .get(CLAUDE_CODE_SESSION_HEADER)
        .and_then(|v| v.to_str().ok())
    {
        let v = v.trim();
        if !v.is_empty() {
            return Some(v.to_string());
        }
    }
    let user_id = body
        .get("metadata")
        .and_then(|m| m.get("user_id"))
        .and_then(|u| u.as_str())?;
    session_id_from_user_id(user_id)
}

/// user_id 形态:
/// - "..._session_<hex-uuid>" 尾缀
/// - JSON 字符串 {"session_id": "..."}
fn session_id_from_user_id(user_id: &str) -> Option<String> {
    const MARKER: &str = "_session_";
    if let Some(idx) = user_id.rfind(MARKER) {
        let suffix = &user_id[idx + MARKER.len()..];
        if !suffix.is_empty() && suffix.chars().all(|c| c.is_ascii_hexdigit() || c == '-') {
            return Some(suffix.to_string());
        }
    }
    if user_id.starts_with('{') {
        if let Ok(v) = serde_json::from_str::<Value>(user_id) {
            let sid = v
                .get("session_id")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            if !sid.is_empty() {
                return Some(sid);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn headers_with(name: &str, value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            name.parse::<http::header::HeaderName>().unwrap(),
            value.parse().unwrap(),
        );
        h
    }

    #[test]
    fn test_header_takes_priority() {
        let headers = headers_with("x-claude-code-session-id", "sess-abc-123");
        let body = json!({
            "metadata": {"user_id": "user_x_session_deadbeef-dead"},
            "messages": [{"role": "user", "content": "hello"}]
        });
        assert_eq!(
            extract_claude_code_session(&headers, &body).as_deref(),
            Some("sess-abc-123")
        );
    }

    #[test]
    fn test_header_case_insensitive_and_trimmed() {
        let headers = headers_with("X-Claude-Code-Session-Id", "  sess-x  ");
        let body = json!({"messages": []});
        assert_eq!(
            extract_claude_code_session(&headers, &body).as_deref(),
            Some("sess-x")
        );
    }

    #[test]
    fn test_user_id_session_suffix() {
        let headers = HeaderMap::new();
        let body = json!({
            "metadata": {"user_id": "user_abc_account_def_session_0123abcd-ef01-2345-6789-abcdef012345"},
            "messages": []
        });
        assert_eq!(
            extract_claude_code_session(&headers, &body).as_deref(),
            Some("0123abcd-ef01-2345-6789-abcdef012345")
        );
    }

    #[test]
    fn test_user_id_json_form() {
        let headers = HeaderMap::new();
        let body = json!({
            "metadata": {"user_id": "{\"session_id\": \"json-sess-1\"}"},
            "messages": []
        });
        assert_eq!(
            extract_claude_code_session(&headers, &body).as_deref(),
            Some("json-sess-1")
        );
    }

    #[test]
    fn test_no_session_returns_none() {
        let headers = HeaderMap::new();
        let body = json!({"messages": [{"role": "user", "content": "hi"}]});
        assert_eq!(extract_claude_code_session(&headers, &body), None);
    }
}
