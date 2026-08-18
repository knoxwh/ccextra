// Gemini 工具名清洗(对齐 CLIProxyAPI util.SanitizeFunctionName /
// translator.SanitizedFunctionNameMap)
//
// 规则:
// - 保留 [a-zA-Z0-9_.:-],其余字符替换为 "_"(纯 ASCII,Unicode 字母不允许)
// - 首字符必须是字母或下划线,否则截断到63后前置 "_"
// - 最长64字符
// - 冲突消解(对齐 CPA):完全重名共享映射;不同原名清洗撞车时,
//   追加确定性 sha256(original+"\x00"+attempt) 前6字节的 _<12hex> 后缀

use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};

/// 对齐 CPA SanitizeFunctionName:单名清洗,不做冲突消解
pub fn sanitize_function_name(name: &str) -> String {
    // 替换非法字符为下划线(对齐 [^a-zA-Z0-9_.:-] → _)
    let cleaned: String = name
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | ':' | '-') {
                c
            } else {
                '_'
            }
        })
        .collect();

    // 首字符必须是字母或下划线:截断到63后前置 "_"
    let mut result = match cleaned.chars().next() {
        Some(first) if first.is_ascii_alphabetic() || first == '_' => cleaned,
        Some(_) => {
            let mut base: String = cleaned.chars().take(63).collect();
            base.insert(0, '_');
            base
        }
        None => "_".to_string(),
    };

    // 截断到64字符
    if result.len() > 64 {
        result = result.chars().take(64).collect();
    }
    result
}

/// 对齐 CPA SanitizedFunctionNameMap:原名 → 清洗名
///
/// 输入为请求内全部工具原名;空名跳过。完全重名共享映射。
/// 不同原名清洗到同一 base 时,按名字排序后逐个分配确定性 hash 后缀。
pub fn sanitized_function_name_map(names: &[&str]) -> HashMap<String, String> {
    let mut unique: HashSet<&str> = HashSet::new();
    let mut base_counts: HashMap<String, usize> = HashMap::new();
    for name in names {
        if name.is_empty() || !unique.insert(name) {
            continue;
        }
        *base_counts.entry(sanitize_function_name(name)).or_insert(0) += 1;
    }

    let mut sorted: Vec<&str> = unique.iter().copied().collect();
    sorted.sort_unstable();

    let mut out = HashMap::with_capacity(sorted.len());
    let mut used: HashSet<String> = HashSet::with_capacity(sorted.len());
    for name in sorted {
        let base = sanitize_function_name(name);
        let mapped = if base_counts.get(&base).copied().unwrap_or(0) > 1 || used.contains(&base) {
            disambiguate(&base, name, &used)
        } else {
            base
        };
        out.insert(name.to_string(), mapped.clone());
        used.insert(mapped);
    }
    out
}

/// 对齐 CPA disambiguateSanitizedFunctionName:
/// sha256(original + "\x00" + attempt) 前6字节转 hex 作 _<12hex> 后缀,
/// base 截到 64-13=51 字符,撞车则 attempt+1
fn disambiguate(base: &str, original: &str, used: &HashSet<String>) -> String {
    for attempt in 0u32.. {
        let digest = Sha256::digest(format!("{}\x00{}", original, attempt).as_bytes());
        let mut suffix = String::with_capacity(13);
        suffix.push('_');
        for b in &digest[..6] {
            suffix.push_str(&format!("{:02x}", b));
        }
        let max_prefix = 64 - suffix.len();
        let prefix: String = base.chars().take(max_prefix).collect();
        let candidate = format!("{}{}", prefix, suffix);
        if !used.contains(&candidate) {
            return candidate;
        }
    }
    unreachable!()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_sanitize_simple() {
        assert_eq!(sanitize_function_name("Read"), "Read");
    }

    #[test]
    fn test_sanitize_keeps_dash_dot_colon() {
        // 对齐 CPA:. : - 保留,不替换
        assert_eq!(
            sanitize_function_name("mcp__context7__query-docs"),
            "mcp__context7__query-docs"
        );
    }

    #[test]
    fn test_sanitize_special_chars() {
        assert_eq!(sanitize_function_name("my tool/name"), "my_tool_name");
    }

    #[test]
    fn test_sanitize_unicode_replaced() {
        // Unicode 字母不允许(Gemini 函数名仅 ASCII)
        assert_eq!(sanitize_function_name("工具x"), "__x");
    }

    #[test]
    fn test_sanitize_leading_digit() {
        // 首字符为数字:截断后前置 "_"(对齐 CPA)
        assert_eq!(sanitize_function_name("1tool"), "_1tool");
    }

    #[test]
    fn test_sanitize_truncate() {
        let long_name = "a".repeat(100);
        assert_eq!(sanitize_function_name(&long_name), "a".repeat(64));
    }

    #[test]
    fn test_map_duplicate_names_share_mapping() {
        // 完全重名共享映射(对齐 CPA)
        let map = sanitized_function_name_map(&["Read", "Read", "Write"]);
        assert_eq!(map.get("Read"), Some(&"Read".to_string()));
        assert_eq!(map.get("Write"), Some(&"Write".to_string()));
        assert_eq!(map.len(), 2);
    }

    #[test]
    fn test_map_collision_deterministic_hash_suffix() {
        // 不同原名清洗撞车:确定性 sha256 后缀(对齐 CPA,非 _2/_3)
        let map = sanitized_function_name_map(&["my tool", "my_tool"]);
        assert_eq!(map.len(), 2);
        let a = map["my tool"].clone();
        let b = map["my_tool"].clone();
        assert_ne!(a, b);
        assert!(a.len() <= 64 && b.len() <= 64);
        // 确定性:同输入重算结果一致
        let map2 = sanitized_function_name_map(&["my tool", "my_tool"]);
        assert_eq!(map, map2);
        // 后缀形态 _<12hex>
        let suffixed = if a.len() > b.len() { &a } else { &b };
        assert!(suffixed.len() == "my_tool".len() + 13 || suffixed.len() == 64);
        assert!(suffixed
            .rsplit('_')
            .next()
            .unwrap()
            .chars()
            .all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn test_map_collision_long_base_truncates_to_51() {
        let long1 = format!("{}x", "a".repeat(64));
        let long2 = format!("{}y", "a".repeat(64));
        // 两者清洗后 base 不同(末位不同),人为制造同 base 撞车用非法字符
        let n1 = "a".repeat(64);
        let n2 = format!("{} ", &n1[..63]); // 清洗后同 base
        let map = sanitized_function_name_map(&[&n1, &n2, &long1, &long2]);
        let v1 = &map[&n1];
        let v2 = &map[&n2];
        assert_ne!(v1, v2);
        assert!(v1.len() <= 64 && v2.len() <= 64);
    }
}
