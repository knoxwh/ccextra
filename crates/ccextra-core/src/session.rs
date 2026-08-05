// 会话身份派生:从 messages[0] 内容哈希生成 session_key
//
// 复用 tklite conversation_discriminator 逻辑:
// - 排除 model 字段(切模型不换桶)
// - 对 messages[0].content 做 SHA-256
// - 输出 hex 字符串

use serde_json::Value;
use sha2::{Digest, Sha256};

/// 从 anthropic messages body 派生 session_key
///
/// 输入:完整请求体 JSON
/// 输出:32 字节哈希的 hex 字符串(64 字符)
pub fn derive_session_key(body: &Value) -> String {
    let messages = match body.get("messages").and_then(|v| v.as_array()) {
        Some(arr) if !arr.is_empty() => arr,
        _ => return "anonymous".to_string(), // 空 messages 或无效结构
    };

    let first_message = &messages[0];

    // 只哈希 content,排除 role(固定 user)
    let content = match first_message.get("content") {
        Some(c) => c,
        None => return "anonymous".to_string(),
    };

    // 规范化 JSON(排除空格差异)
    let canonical = serde_json::to_string(content).unwrap_or_default();

    let mut hasher = Sha256::new();
    hasher.update(canonical.as_bytes());
    let hash = hasher.finalize();

    hex::encode(hash)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_derive_session_key_simple() {
        let body = json!({
            "model": "claude-opus-5",
            "messages": [
                {"role": "user", "content": "hello"}
            ]
        });

        let key1 = derive_session_key(&body);
        assert_eq!(key1.len(), 64); // SHA-256 hex = 64 字符

        // 相同 content 应得到相同 key
        let key2 = derive_session_key(&body);
        assert_eq!(key1, key2);
    }

    #[test]
    fn test_session_key_ignores_model() {
        let body1 = json!({
            "model": "model-a",
            "messages": [{"role": "user", "content": "test"}]
        });

        let body2 = json!({
            "model": "model-b",
            "messages": [{"role": "user", "content": "test"}]
        });

        // model 不同但 messages[0] 相同 → session_key 相同
        assert_eq!(derive_session_key(&body1), derive_session_key(&body2));
    }

    #[test]
    fn test_session_key_changes_with_content() {
        let body1 = json!({
            "messages": [{"role": "user", "content": "message1"}]
        });

        let body2 = json!({
            "messages": [{"role": "user", "content": "message2"}]
        });

        // content 不同 → session_key 不同
        assert_ne!(derive_session_key(&body1), derive_session_key(&body2));
    }

    #[test]
    fn test_empty_messages() {
        let body = json!({"messages": []});
        assert_eq!(derive_session_key(&body), "anonymous");
    }

    #[test]
    fn test_missing_content() {
        let body = json!({
            "messages": [{"role": "user"}]
        });
        assert_eq!(derive_session_key(&body), "anonymous");
    }
}
