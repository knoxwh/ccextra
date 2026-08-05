// prompt_cache_key 派生与注入(对齐 CPA applyPromptCacheKey claude 分支)
//
// CPA 参考:
// - internal/runtime/executor/openai_compat_executor.go applyPromptCacheKey
// - internal/runtime/executor/helps/claude_code_session.go ClaudeCodePromptCache
//
// 语义:
// - provider 级开关(默认 false,对应 CPA support-prompt-cache-key)
// - 仅 openai_chat / openai_responses 注入(claude 协议无此字段)
// - body 已有非空 prompt_cache_key 时不覆盖
// - 无 Claude Code 会话 ID(头或 metadata.user_id)时不注入
// - identity 前缀保留 "cli-proxy-api:codex:claude-code",与 CPA 同桶,
//   切换代理不碎缓存

use http::HeaderMap;
use serde_json::Value;

/// Claude Code agent 头(CPA ClaudeCodeAgentHeader)
pub const CLAUDE_CODE_AGENT_HEADER: &str = "x-claude-code-agent-id";
/// 根 agent 哨兵(CPA ClaudeCodeMainAgentID)
pub const CLAUDE_CODE_MAIN_AGENT_ID: &str = "main";

/// 提取 Claude Code agent ID(对齐 CPA ExtractClaudeCodeAgentID):头优先,缺省 "main"
pub fn extract_claude_code_agent(headers: &HeaderMap) -> String {
    headers
        .get(CLAUDE_CODE_AGENT_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| CLAUDE_CODE_MAIN_AGENT_ID.to_string())
}

/// 派生确定性 prompt_cache_key(对齐 CPA ClaudeCodePromptCache):
/// UUIDv5(NameSpaceOID, "cli-proxy-api:codex:claude-code" \0 model \0 "claude:<session>:agent:<agent>")
///
/// model 或 session_id 为空时返回 None(CPA 同语义)
pub fn claude_code_prompt_cache_key(
    model: &str,
    session_id: &str,
    agent_id: &str,
) -> Option<String> {
    let model = model.trim();
    if model.is_empty() || session_id.is_empty() {
        return None;
    }
    let scope = format!("claude:{session_id}:agent:{agent_id}");
    let identity = ["cli-proxy-api:codex:claude-code", model, scope.as_str()].join("\0");
    Some(uuid::Uuid::new_v5(&uuid::Uuid::NAMESPACE_OID, identity.as_bytes()).to_string())
}

/// 向转换后的 openai body 注入 prompt_cache_key
///
/// 返回 true 表示注入了新 key;false 表示跳过(已有 key / 无会话 / 无 model)。
/// session_id 必须在转换前提取(转换后 metadata 被丢弃),由调用方传入。
pub fn inject_prompt_cache_key(
    body: &mut Value,
    headers: &HeaderMap,
    session_id: Option<&str>,
) -> bool {
    // 已有非空 key 不覆盖(CPA:入站/原始/转换后任一存在即保留)
    if let Some(existing) = body.get("prompt_cache_key").and_then(|v| v.as_str()) {
        if !existing.trim().is_empty() {
            return false;
        }
    }
    let Some(session_id) = session_id else {
        return false;
    };
    let model = body
        .get("model")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let agent = extract_claude_code_agent(headers);
    match claude_code_prompt_cache_key(&model, session_id, &agent) {
        Some(key) => {
            body["prompt_cache_key"] = Value::String(key);
            true
        }
        None => false,
    }
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
    fn test_agent_header_default_main() {
        assert_eq!(extract_claude_code_agent(&HeaderMap::new()), "main");
    }

    #[test]
    fn test_agent_header_override() {
        let h = headers_with("x-claude-code-agent-id", "sub-agent-1");
        assert_eq!(extract_claude_code_agent(&h), "sub-agent-1");
    }

    #[test]
    fn test_agent_header_blank_falls_back() {
        let h = headers_with("x-claude-code-agent-id", "   ");
        assert_eq!(extract_claude_code_agent(&h), "main");
    }

    // 向量由 python3 uuid.uuid5(NAMESPACE_OID, identity) 交叉验证,
    // 与 CPA uuid.NewSHA1(uuid.NameSpaceOID, ...) 同算法
    #[test]
    fn test_known_vector_main_agent() {
        let key = claude_code_prompt_cache_key("gpt-5.6-terra", "sess-abc-123", "main").unwrap();
        assert_eq!(key, "d54a6c52-9dbd-5444-9cd7-4ccff3f8df0d");
    }

    #[test]
    fn test_known_vector_sub_agent() {
        let key = claude_code_prompt_cache_key("gpt-5.6-terra", "sess-abc-123", "sub-1").unwrap();
        assert_eq!(key, "6dedb235-7b15-588e-bb24-0ef2719a9b12");
    }

    #[test]
    fn test_deterministic_and_distinct() {
        let a = claude_code_prompt_cache_key("m1", "s1", "main").unwrap();
        assert_eq!(a, claude_code_prompt_cache_key("m1", "s1", "main").unwrap());
        assert_ne!(a, claude_code_prompt_cache_key("m2", "s1", "main").unwrap());
        assert_ne!(a, claude_code_prompt_cache_key("m1", "s2", "main").unwrap());
    }

    #[test]
    fn test_empty_model_or_session_none() {
        assert!(claude_code_prompt_cache_key("", "s1", "main").is_none());
        assert!(claude_code_prompt_cache_key("  ", "s1", "main").is_none());
        assert!(claude_code_prompt_cache_key("m1", "", "main").is_none());
    }

    #[test]
    fn test_inject_sets_key() {
        let mut body = json!({"model": "gpt-5.6-terra"});
        let h = headers_with("x-claude-code-session-id", "sess-abc-123");
        assert!(inject_prompt_cache_key(&mut body, &h, Some("sess-abc-123")));
        assert_eq!(
            body["prompt_cache_key"],
            "d54a6c52-9dbd-5444-9cd7-4ccff3f8df0d"
        );
    }

    #[test]
    fn test_inject_preserves_existing() {
        let mut body = json!({"model": "m", "prompt_cache_key": "user-key"});
        let h = headers_with("x-claude-code-session-id", "s");
        assert!(!inject_prompt_cache_key(&mut body, &h, Some("s")));
        assert_eq!(body["prompt_cache_key"], "user-key");
    }

    #[test]
    fn test_inject_no_session_skips() {
        let mut body = json!({"model": "m"});
        let h = HeaderMap::new();
        assert!(!inject_prompt_cache_key(&mut body, &h, None));
        assert!(body.get("prompt_cache_key").is_none());
    }

    #[test]
    fn test_inject_no_model_skips() {
        let mut body = json!({});
        let h = HeaderMap::new();
        assert!(!inject_prompt_cache_key(&mut body, &h, Some("s")));
    }
}
