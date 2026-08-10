// prompt_cache_key 注入
//
// 语义:
// - provider 级开关(默认 false)
// - 仅 openai_chat / openai_responses 注入(claude 协议无此字段)
// - body 已有非空 prompt_cache_key 时不覆盖
// - 无 Claude Code 会话 ID(头或 metadata.user_id)时不注入
// - key = session_id 裸值,与 codex CLI 0.147 的生成策略一致
//   (codex: prompt_cache_key = session_id,无 model/agent 维度)

use serde_json::Value;

/// 生成与 codex 一致的 prompt_cache_key:session_id 裸值。
/// 对齐 codex 0.147 client.rs prompt_cache_key():override 缺省时返回 session_id。
///
/// session_id 为空时返回 None
pub fn claude_code_prompt_cache_key(session_id: &str) -> Option<String> {
    let session_id = session_id.trim();
    if session_id.is_empty() {
        return None;
    }
    Some(session_id.to_string())
}

/// 向转换后的 openai body 注入 prompt_cache_key
///
/// 返回 true 表示注入了新 key;false 表示跳过(已有 key / 无会话)。
/// session_id 必须在转换前提取(转换后 metadata 被丢弃),由调用方传入。
pub fn inject_prompt_cache_key(body: &mut Value, session_id: Option<&str>) -> bool {
    // 已有非空 key 不覆盖(入站/原始/转换后任一存在即保留)
    if let Some(existing) = body.get("prompt_cache_key").and_then(|v| v.as_str()) {
        if !existing.trim().is_empty() {
            return false;
        }
    }
    let Some(session_id) = session_id else {
        return false;
    };
    match claude_code_prompt_cache_key(session_id) {
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

    #[test]
    fn test_key_equals_session_id() {
        assert_eq!(
            claude_code_prompt_cache_key("sess-abc-123").unwrap(),
            "sess-abc-123"
        );
    }

    #[test]
    fn test_key_trimmed() {
        assert_eq!(
            claude_code_prompt_cache_key("  sess-abc  ").unwrap(),
            "sess-abc"
        );
    }

    #[test]
    fn test_empty_session_none() {
        assert!(claude_code_prompt_cache_key("").is_none());
        assert!(claude_code_prompt_cache_key("  ").is_none());
    }

    #[test]
    fn test_inject_sets_session_id() {
        let mut body = json!({"model": "gpt-5.6-terra"});
        assert!(inject_prompt_cache_key(&mut body, Some("sess-abc-123")));
        assert_eq!(body["prompt_cache_key"], "sess-abc-123");
    }

    #[test]
    fn test_inject_preserves_existing() {
        let mut body = json!({"model": "m", "prompt_cache_key": "user-key"});
        assert!(!inject_prompt_cache_key(&mut body, Some("s")));
        assert_eq!(body["prompt_cache_key"], "user-key");
    }

    #[test]
    fn test_inject_no_session_skips() {
        let mut body = json!({"model": "m"});
        assert!(!inject_prompt_cache_key(&mut body, None));
        assert!(body.get("prompt_cache_key").is_none());
    }
}
