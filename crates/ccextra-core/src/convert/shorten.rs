// 工具名缩短(OpenAI Responses 工具名 ≤64 字符)
//
// 超长 Claude 工具名(尤其 mcp__ 前缀)需确定性缩短 + 冲突时唯一 `_N` 后缀,
// 保证同一请求内名字不重复。响应侧用反向映射还原原名。

use std::collections::HashMap;
use std::sync::OnceLock;

use regex::Regex;

/// 名长上限(OpenAI Responses 限制)
const NAME_LIMIT: usize = 64;

/// 网关 MCP 工具 id `[mcp_]<server 词>-<hex 实例 id...>_<tool>`
/// (对齐 CPA 08437a67 gatewayToolNamePattern)
fn gateway_tool_name_pattern() -> &'static Regex {
    static PATTERN: OnceLock<Regex> = OnceLock::new();
    PATTERN.get_or_init(|| {
        Regex::new(r"^(?:mcp_)?([a-z][a-z0-9-]*?)-[0-9a-f]{6,}(?:-[0-9a-f]+)*_(.+)$")
            .expect("gateway tool name pattern")
    })
}

/// mcp__ 前缀工具名缩短:保留 mcp__ + 末段 __ 后的名字;仍超长且末段是网关
/// MCP 工具 id 时剥掉中间 hex 实例 id,保留 server 词 + 工具名(对齐 CPA 08437a67,
/// 避免同一长名 server 的工具都截成同一前缀)
/// 对齐 CPA capResponsesChatToolName:截断后去除前导分隔符(部分严格上游拒绝非字母数字开头)
pub fn shorten_name_if_needed(name: &str) -> String {
    if name.len() <= NAME_LIMIT {
        return name.to_string();
    }
    let truncated = if let Some(rest) = name.strip_prefix("mcp__") {
        if let Some(idx) = rest.rfind("__") {
            let tail = &rest[idx + 2..];
            let cand = format!("mcp__{tail}");
            if cand.len() <= NAME_LIMIT {
                cand
            } else if let Some(readable) = gateway_tool_name_pattern()
                .captures(tail)
                .map(|c| format!("mcp__{}_{}", &c[1], &c[2]))
                .filter(|r| r.len() <= NAME_LIMIT)
            {
                readable
            } else {
                truncate_to_limit(&cand)
            }
        } else {
            truncate_to_limit(name)
        }
    } else {
        truncate_to_limit(name)
    };
    // 去除前导 _ 和 - (对齐 CPA strings.TrimLeft(truncated, "_-"))
    let trimmed = truncated.trim_start_matches(&['_', '-'][..]);
    if !trimmed.is_empty() {
        trimmed.to_string()
    } else {
        truncated
    }
}

/// 截到 NAME_LIMIT 字节;落在多字节字符中间时回退到前一个字符边界(避免切片 panic)
fn truncate_to_limit(s: &str) -> String {
    let mut cut = NAME_LIMIT.min(s.len());
    while !s.is_char_boundary(cut) {
        cut -= 1;
    }
    s[..cut].to_string()
}

/// 候选名冲突时追加 `_N` 后缀直到唯一
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

/// 构建 original→short 映射,保证同名唯一
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

/// 反向映射 short→original,响应侧还原工具名
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
    fn test_trim_leading_separators() {
        for (name, expected) in [
            ("_".to_string() + &"tool_name_".repeat(10), None),
            ("--tool-name-".repeat(10), None),
            ("_-_-".to_string() + &"x".repeat(70), Some("x".repeat(60))),
        ] {
            let short = shorten_name_if_needed(&name);
            assert!(!short.starts_with(['_', '-']), "{name}");
            assert!(short.len() <= 64, "{name}");
            if let Some(expected) = expected {
                assert_eq!(short, expected);
            }
        }
    }

    #[test]
    fn test_all_separators_fallback() {
        // 截断后全是分隔符 → 保留原截断(不清空)
        let name = "_".repeat(80);
        let names = vec![name.clone()];
        let m = build_short_name_map(&names);
        let short = &m[&name];
        assert_eq!(short.len(), 64);
        assert_eq!(short, &"_".repeat(64));
    }

    const GATEWAY_PREFIX: &str =
        "mcp__gateway__mcp_github-12345678-9abc-def0-1234-56789abcdef0-12345678_";

    /// 对齐 CPA 08437a67:超长网关 MCP 名剥 hex 实例 id,保留 server 词 + 工具名
    #[test]
    fn test_gateway_tool_names_stay_readable_and_distinct() {
        let names: Vec<String> = [
            format!("{GATEWAY_PREFIX}add-comment-to-pending-review"),
            format!("{GATEWAY_PREFIX}create-branch"),
            format!("{GATEWAY_PREFIX}get_file_contents"),
            "mcp__gateway__mcp_playwright-browser-67892a0b_browser_click".to_string(),
            "Bash".to_string(),
        ]
        .into();
        let m = build_short_name_map(&names);
        for (orig, want) in names.iter().zip([
            "mcp__github_add-comment-to-pending-review",
            "mcp__github_create-branch",
            "mcp__github_get_file_contents",
            "mcp__gateway__mcp_playwright-browser-67892a0b_browser_click",
            "Bash",
        ]) {
            assert_eq!(m[orig], want);
        }
        let rev = build_reverse_map(&m);
        assert_eq!(rev["mcp__github_create-branch"], names[1]);
    }

    #[test]
    fn test_gateway_same_tool_on_two_servers_disambiguated() {
        let a = format!("{GATEWAY_PREFIX}create-branch");
        let b =
            "mcp__gateway__mcp_github-0b9e7c1d-1111-2222-3333-444455556666-89abcdef_create-branch"
                .to_string();
        let m = build_short_name_map(&[a.clone(), b.clone()]);
        assert_ne!(m[&a], m[&b]);
        assert!(m[&b].starts_with("mcp__github_create-branch"), "{}", m[&b]);
    }

    #[test]
    fn test_non_gateway_long_mcp_name_falls_back_to_truncation() {
        let long = format!("mcp__srv__{}", "x".repeat(80));
        let short = shorten_name_if_needed(&long);
        assert_eq!(short.len(), 64);
        assert!(short.starts_with("mcp__xxx"));
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

    #[test]
    fn test_over_limit_multibyte_name_truncates_on_char_boundary() {
        // 63 个 ASCII + "工具…":第 64 字节落在多字节字符中间,不得 panic
        let name = format!("{}工具名称", "a".repeat(63));
        assert_eq!(shorten_name_if_needed(&name), "a".repeat(63));
        let mcp = format!("mcp__srv__{}工具名称", "b".repeat(58));
        let short = shorten_name_if_needed(&mcp);
        assert!(short.len() <= NAME_LIMIT);
        assert_eq!(short, format!("mcp__{}", "b".repeat(58)));
    }
}
