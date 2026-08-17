// Claude 侧工具 ID 生成:使用 SHA256 哈希避免名称冲突

use sha2::{Digest, Sha256};

/// 生成 Claude 侧工具 ID
///
/// 格式: cpa_gemini_{hash}
/// hash = sha256(原始工具名)[0:16] 十六进制
pub fn generate_claude_tool_id(original_name: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(original_name.as_bytes());
    let hash = hasher.finalize();

    // 取前16字节转十六进制
    let hex = hex::encode(&hash[..16]);
    format!("cpa_gemini_{}", hex)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_claude_tool_id() {
        let id1 = generate_claude_tool_id("Read");
        let id2 = generate_claude_tool_id("Write");

        assert!(id1.starts_with("cpa_gemini_"));
        assert!(id2.starts_with("cpa_gemini_"));
        assert_ne!(id1, id2);

        // 长度: cpa_gemini_ (11) + 32字符十六进制 = 43
        assert_eq!(id1.len(), 43);
    }

    #[test]
    fn test_generate_claude_tool_id_deterministic() {
        let id1 = generate_claude_tool_id("mcp__context7__query-docs");
        let id2 = generate_claude_tool_id("mcp__context7__query-docs");
        assert_eq!(id1, id2);
    }

    #[test]
    fn test_generate_claude_tool_id_collision_resistant() {
        let id1 = generate_claude_tool_id("tool_a");
        let id2 = generate_claude_tool_id("tool_b");
        let id3 = generate_claude_tool_id("a_tool");

        assert_ne!(id1, id2);
        assert_ne!(id1, id3);
        assert_ne!(id2, id3);
    }
}
