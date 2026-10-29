// Claude 侧工具 ID 方案(对齐 CLIProxyAPI
// translator/gemini/claude/gemini_claude_response.go 的 {name}-{counter}
// 生成与反解;sanitize 规则取自 util/claude_tool_id.go)
//
// 流式/非流式响应侧统一生成 `{name}-{counter}` 形式(经 sanitize),
// 请求侧可从该 id 反解工具名(去掉最后一个 "-" 段),保证
// functionResponse.name 与模型发出的 functionCall.name 一致。

use serde_json::Value;
use sha2::{Digest, Sha256};
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

/// AG Gemini 族稳定 tool_use id 前缀(对应 CPA `cpa_gemini_`)。
/// 用 ccextra 自有命名空间:不冒充 CPA 的保留 id,也不含 `-`,
/// 旧式 `{name}-{counter}` 反解对其返回空串而不会误拆出工具名。
pub const GEMINI_CLAUDE_TOOL_USE_ID_PREFIX: &str = "ccx_gemini_";

/// 规范 JSON:对象键排序、数字归一(整数值浮点写成整数),用于 args 比较、
/// 稳定 id 与 contextHash。对齐 CPA 的 json.Unmarshal→Marshal 往返
/// (Go 解成 float64 后 `1.0` 与 `1` 同形;本仓开了 arbitrary_precision,须手工归一)。
pub fn canonical_json(value: &Value) -> String {
    let mut out = String::new();
    write_canonical(value, &mut out);
    out
}

pub(crate) fn write_canonical(value: &Value, out: &mut String) {
    match value {
        Value::Null => out.push_str("null"),
        Value::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Value::Number(n) => out.push_str(&canonical_number(n)),
        Value::String(s) => out.push_str(&serde_json::to_string(s).unwrap_or_default()),
        Value::Array(arr) => {
            out.push('[');
            for (i, child) in arr.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_canonical(child, out);
            }
            out.push(']');
        }
        Value::Object(map) => {
            let mut keys: Vec<&String> = map.keys().collect();
            keys.sort();
            out.push('{');
            for (i, k) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&serde_json::to_string(k).unwrap_or_default());
                out.push(':');
                write_canonical(&map[k], out);
            }
            out.push('}');
        }
    }
}

fn canonical_number(n: &serde_json::Number) -> String {
    if let Some(i) = n.as_i64() {
        return i.to_string();
    }
    if let Some(u) = n.as_u64() {
        return u.to_string();
    }
    match n.as_f64() {
        Some(f) if f.is_finite() && f.fract() == 0.0 && f.abs() < 1e15 => {
            format!("{}", f as i64)
        }
        Some(f) if f.is_finite() => format!("{f}"),
        _ => n.to_string(),
    }
}

/// 由上游原生 functionCall (id, name, args) 派生 Claude 侧稳定 tool_use id
/// (对齐 CPA util.GeminiClaudeToolUseID:sha256(id\0name\0canonicalArgs) 前 16 字节 hex)。
/// id 或 name 为空返回空串(调用方回落 `{name}-{counter}`)。缺失 args 视为 `{}`。
pub fn gemini_claude_tool_use_id(call_id: &str, name: &str, args: Option<&Value>) -> String {
    let call_id = call_id.trim();
    let name = name.trim();
    if call_id.is_empty() || name.is_empty() {
        return String::new();
    }
    let args = match args {
        Some(v) => canonical_json(v),
        None => "{}".to_string(),
    };
    let mut hasher = Sha256::new();
    hasher.update(call_id.as_bytes());
    hasher.update([0u8]);
    hasher.update(name.as_bytes());
    hasher.update([0u8]);
    hasher.update(args.as_bytes());
    let digest = hasher.finalize();
    format!(
        "{GEMINI_CLAUDE_TOOL_USE_ID_PREFIX}{}",
        hex::encode(&digest[..16])
    )
}

/// 是否为本仓保留的稳定 tool_use id(对齐 CPA IsGeminiClaudeToolUseID)。
pub fn is_gemini_claude_tool_use_id(id: &str) -> bool {
    let Some(digest) = id.trim().strip_prefix(GEMINI_CLAUDE_TOOL_USE_ID_PREFIX) else {
        return false;
    };
    digest.len() == 32 && digest.bytes().all(|b| b.is_ascii_hexdigit())
}

/// call_id 超 64 字符确定性截短:保留前缀 + `_` + sha256 前 8 字节 hex,总长 ≤ 64
/// (对齐 CPA shortenCodexCallIDIfNeeded / shortenCodexReplayCallIDIfNeeded,两处同算法)
pub(crate) fn shorten_call_id(id: &str) -> String {
    const LIMIT: usize = 64;
    if id.len() <= LIMIT {
        return id.to_string();
    }
    let digest = Sha256::digest(id.as_bytes());
    let suffix = format!("_{}", hex::encode(&digest[..8]));
    let mut prefix_len = LIMIT.saturating_sub(suffix.len());
    while prefix_len > 0 && !id.is_char_boundary(prefix_len) {
        prefix_len -= 1;
    }
    format!("{}{}", &id[..prefix_len], suffix)
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

    #[test]
    fn test_gemini_claude_tool_use_id_stable_and_reserved() {
        let a = gemini_claude_tool_use_id(
            "call-1",
            "Read",
            Some(&serde_json::json!({"b": 1, "a": "x"})),
        );
        // 键序 / 1.0 与 1 不影响
        let b: Value = serde_json::from_str(r#"{"a":"x","b":1.0}"#).unwrap();
        assert_eq!(a, gemini_claude_tool_use_id("call-1", "Read", Some(&b)));
        assert!(a.starts_with(GEMINI_CLAUDE_TOOL_USE_ID_PREFIX));
        assert_eq!(a.len(), GEMINI_CLAUDE_TOOL_USE_ID_PREFIX.len() + 32);
        assert!(is_gemini_claude_tool_use_id(&a));
        // 符合 Claude tool_use.id 字符集
        assert_eq!(sanitize_claude_tool_id(&a), a);
        // 旧式反解不会误拆出工具名
        assert_eq!(tool_name_from_claude_tool_use_id(&a), "");
        assert_ne!(a, gemini_claude_tool_use_id("call-2", "Read", Some(&b)));
        assert_ne!(a, gemini_claude_tool_use_id("call-1", "Write", Some(&b)));
        assert_eq!(gemini_claude_tool_use_id("", "Read", None), "");
        assert_eq!(gemini_claude_tool_use_id("call-1", " ", None), "");
        assert!(!is_gemini_claude_tool_use_id("Read-3"));
        assert!(!is_gemini_claude_tool_use_id("ccx_gemini_xyz"));
    }
}
