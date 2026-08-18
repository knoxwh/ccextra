// Claude 侧工具 ID 方案(对齐 CLIProxyAPI
// translator/gemini/claude/gemini_claude_response.go 的 {name}-{counter}
// 生成与反解;sanitize 规则取自 util/claude_tool_id.go)
//
// 流式/非流式响应侧统一生成 `{name}-{counter}` 形式(经 sanitize),
// 请求侧可从该 id 反解工具名(去掉最后一个 "-" 段),保证
// functionResponse.name 与模型发出的 functionCall.name 一致。

use std::sync::atomic::{AtomicU64, Ordering};

static FALLBACK_COUNTER: AtomicU64 = AtomicU64::new(0);

/// 清洗 id 使其符合 Claude tool_use.id 正则 ^[a-zA-Z0-9_-]+$
/// (对齐 SanitizeClaudeToolID:非法字符换 "_",空结果给兜底)
pub fn sanitize_claude_tool_id(id: &str) -> String {
    let cleaned: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if cleaned.is_empty() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        return format!(
            "toolu_{}_{}",
            nanos,
            FALLBACK_COUNTER.fetch_add(1, Ordering::SeqCst)
        );
    }
    cleaned
}

/// 生成 Claude 侧 tool_use id:`{name}-{counter}`(对齐 CPA 流/非流响应侧)
pub fn claude_tool_id_for(name: &str, counter: u64) -> String {
    sanitize_claude_tool_id(&format!("{}-{}", name, counter))
}

/// 从 tool_use_id 反解工具名(对齐 CPA toolNameFromClaudeToolUseID):
/// 按 "-" 分段,去掉最后一段后重新拼接;不足两段返回空
pub fn tool_name_from_claude_tool_use_id(tool_use_id: &str) -> String {
    let parts: Vec<&str> = tool_use_id.split('-').collect();
    if parts.len() <= 1 {
        return String::new();
    }
    parts[..parts.len() - 1].join("-")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_claude_tool_id_keeps_valid() {
        assert_eq!(sanitize_claude_tool_id("Read-3"), "Read-3");
        assert_eq!(sanitize_claude_tool_id("toolu_abc"), "toolu_abc");
    }

    #[test]
    fn test_sanitize_claude_tool_id_replaces_invalid() {
        assert_eq!(sanitize_claude_tool_id("a.b:c d"), "a_b_c_d");
        assert!(!sanitize_claude_tool_id("").is_empty());
    }

    #[test]
    fn test_claude_tool_id_for() {
        assert_eq!(claude_tool_id_for("Read", 3), "Read-3");
        assert_eq!(
            claude_tool_id_for("mcp__x__query-docs", 12),
            "mcp__x__query-docs-12"
        );
    }

    #[test]
    fn test_tool_name_from_claude_tool_use_id() {
        assert_eq!(tool_name_from_claude_tool_use_id("Read-3"), "Read");
        assert_eq!(
            tool_name_from_claude_tool_use_id("mcp__x__query-docs-12"),
            "mcp__x__query-docs"
        );
        assert_eq!(tool_name_from_claude_tool_use_id("nodash"), "");
    }
}
