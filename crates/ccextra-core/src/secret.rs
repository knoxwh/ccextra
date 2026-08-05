// 入口 secret 辅助(纯逻辑,无 IO)
// 参考 CPA internal/.../looksLikeBcrypt

/// 是否为 bcrypt 哈希,用于区分"明文 key"与"已哈希 key"
pub fn looks_like_bcrypt(s: &str) -> bool {
    s.starts_with("$2a$") || s.starts_with("$2b$") || s.starts_with("$2y$")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_looks_like_bcrypt() {
        assert!(looks_like_bcrypt("$2a$12$abcdefghijklmnopqrstuv"));
        assert!(looks_like_bcrypt("$2b$12$abcdefghijklmnopqrstuv"));
        assert!(looks_like_bcrypt("$2y$12$abcdefghijklmnopqrstuv"));
        assert!(!looks_like_bcrypt("sk-plain-123"));
        assert!(!looks_like_bcrypt(""));
        assert!(!looks_like_bcrypt("$2x$12$abcdefghijklmnopqrstuv"));
    }
}