// 工具名缩短(对齐 CPA codex/claude/codex_claude_request.go)
//
// OpenAI Responses API 要求工具名 ≤64 字符。超长 Claude 工具名(尤其 mcp__ 前缀)
// 需确定性缩短 + 冲突时唯一 `_N` 后缀,保证同一请求内名字不重复。
// 响应侧用反向映射还原原名(对齐 buildReverseMapFromClaudeOriginalShortToOriginal)。

use std::collections::HashMap;

/// 名长上限(OpenAI Responses 限制,对齐 CPA const limit = 64)
const NAME_LIMIT: usize = 64;

/// mcp__ 前缀工具名缩短:保留 mcp__ + 末段 __ 后的名字(对齐 CPA shortenNameIfNeeded)
pub fn shorten_name_if_needed(name: &str) -> String {
    if name.len() <= NAME_LIMIT {
        return name.to_string();
    }
    if let Some(rest) = name.strip_prefix("mcp__") {
        if let Some(idx) = rest.rfind("__") {
            let cand = format!("mcp__{}", &rest[idx + 2..]);
            if cand.len() > NAME_LIMIT {
                return cand[..NAME_LIMIT].to_string();
            }
            return cand;
        }
    }
    name[..NAME_LIMIT].to_string()
}

/// 候选名冲突时追加 `_N` 后缀直到唯一(对齐 CPA makeUnique)
fn make_unique(cand: String, used: &std::collections::HashSet<String>) -> String {
    if !used.contains(&cand) {
        return cand;
    }
    let base = cand;
    for i in 1.. {
        let suffix = format!("_{i}");
        let allowed = NAME_LIMIT.saturating_sub(suffix.len());
        let mut tmp: String = base.chars().take(allowed).collect();
        tmp.push_str(&suffix);
        if !used.contains(&tmp) {
            return tmp;
        }
    }
    unreachable!("迭代必有唯一候选")
}

/// 构建 original→short 映射,保证同名唯一(对齐 CPA buildShortNameMap)
pub fn build_short_name_map(names: &[String]) -> HashMap<String, String> {
    let mut used = std::collections::HashSet::new();
    let mut m = HashMap::new();
    for n in names {
        let cand = shorten_name_if_needed(n);
        let uniq = make_unique(cand, &used);
        used.insert(uniq.clone());
        m.insert(n.clone(), uniq);
    }
    m
}

/// 反向映射 short→original,响应侧还原工具名(对齐 CPA buildReverseMap...)
pub fn build_reverse_map(short_map: &HashMap<String, String>) -> HashMap<String, String> {
    short_map
        .iter()
        .map(|(orig, short)| (short.clone(), orig.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_short_name_under_limit_preserved() {
        let names = vec!["get_weather".to_string()];
        let m = build_short_name_map(&names);
        assert_eq!(m["get_weather"], "get_weather");
    }

    #[test]
    fn test_short_name_over_limit_truncated() {
        let long = "a".repeat(80);
        let names = vec![long.clone()];
        let m = build_short_name_map(&names);
        assert_eq!(m[&long].len(), 64);
    }

    #[test]
    fn test_mcp_shortening_keeps_tail() {
        let long = format!("mcp__{}", "x".repeat(100));
        let names = vec![long.clone()];
        let m = build_short_name_map(&names);
        let short = &m[&long];
        assert_eq!(short.len(), 64);
        assert!(short.starts_with("mcp__"));
    }

    #[test]
    fn test_collision_gets_unique_suffix() {
        // 两个长名截断到相同 64 前缀 → 第二个加 _N
        let a = "a".repeat(64) + "X";
        let b = "a".repeat(64) + "Y";
        let names = vec![a.clone(), b.clone()];
        let m = build_short_name_map(&names);
        let sa = &m[&a];
        let sb = &m[&b];
        assert_ne!(sa, sb);
        assert!(sb.ends_with("_1"));
    }

    #[test]
    fn test_reverse_map_roundtrip() {
        let names = vec!["tool_a".to_string(), "tool_b".to_string()];
        let short_map = build_short_name_map(&names);
        let rev = build_reverse_map(&short_map);
        for (orig, short) in &short_map {
            assert_eq!(rev[short], *orig);
        }
    }
}