// Gemini 工具名清洗:64字符限制,仅字母数字下划线

use std::collections::HashSet;

/// 清洗工具名以符合 Gemini 限制:
/// - 最长64字符
/// - 仅 [a-zA-Z0-9_]
/// - 冲突时追加 _2, _3 等
pub fn sanitize_tool_name(name: &str, used: &mut HashSet<String>) -> String {
    // 替换非法字符为下划线
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_alphanumeric() || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect();

    // 截断到64字符
    let mut result = if cleaned.len() > 64 {
        cleaned.chars().take(64).collect()
    } else {
        cleaned
    };

    // 冲突解决:追加 _2, _3...
    if used.contains(&result) {
        // 为后缀预留空间:_999 需要4字符,所以基础名最多60字符
        let base = if result.len() > 60 {
            result.chars().take(60).collect::<String>()
        } else {
            result.clone()
        };

        for suffix in 2..1000 {
            let candidate = format!("{}_{}", base, suffix);
            if candidate.len() <= 64 && !used.contains(&candidate) {
                result = candidate;
                break;
            }
        }
    }

    used.insert(result.clone());
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_tool_name_simple() {
        let mut used = HashSet::new();
        let result = sanitize_tool_name("Read", &mut used);
        assert_eq!(result, "Read");
        assert!(used.contains("Read"));
    }

    #[test]
    fn test_sanitize_tool_name_special_chars() {
        let mut used = HashSet::new();
        let result = sanitize_tool_name("mcp__context7__query-docs", &mut used);
        assert_eq!(result, "mcp__context7__query_docs");
    }

    #[test]
    fn test_sanitize_tool_name_truncate() {
        let mut used = HashSet::new();
        let long_name = "a".repeat(100);
        let result = sanitize_tool_name(&long_name, &mut used);
        assert_eq!(result.len(), 64);
        assert_eq!(result, "a".repeat(64));
    }

    #[test]
    fn test_sanitize_tool_name_conflict_resolution() {
        let mut used = HashSet::new();
        let first = sanitize_tool_name("Read", &mut used);
        let second = sanitize_tool_name("Read", &mut used);
        let third = sanitize_tool_name("Read", &mut used);

        assert_eq!(first, "Read");
        assert_eq!(second, "Read_2");
        assert_eq!(third, "Read_3");
    }

    #[test]
    fn test_sanitize_tool_name_conflict_long() {
        let mut used = HashSet::new();
        let long_name = "a".repeat(64);
        let first = sanitize_tool_name(&long_name, &mut used);
        let second = sanitize_tool_name(&long_name, &mut used);

        assert_eq!(first.len(), 64);
        assert_eq!(first, "a".repeat(64));
        // 第二个会截断基础名到60字符后加 _2
        assert!(second.len() <= 64);
        assert!(second.starts_with(&"a".repeat(60)));
        assert!(second.ends_with("_2"));
        assert_ne!(first, second);
    }
}
