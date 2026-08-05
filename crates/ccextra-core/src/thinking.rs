// 思考级别映射(忠实移植 CPA,可跟随上游更新)
//
// 参考源:
// - 阈值与级别:     https://github.com/router-for-me/CLIProxyAPI/blob/main/internal/thinking/convert.go
// - 级别枚举:       https://github.com/router-for-me/CLIProxyAPI/blob/main/internal/thinking/types.go
// - thinking→effort: https://github.com/router-for-me/CLIProxyAPI/blob/main/internal/translator/codex/claude/codex_claude_request.go
// - 钳制逻辑:       https://github.com/router-for-me/CLIProxyAPI/blob/main/internal/thinking/apply.go
//
// 与 CPA 差异:CPA 靠每模型能力表(registry)在 ApplyThinking 钳制非法值;
// ccextra 无 registry,用调用方传入的 supported 集合钳制,非法值返回 None(不注入)。

/// 思考级别(对应 CPA ThinkingLevel)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    None,
    Auto,
    Minimal,
    Low,
    Medium,
    High,
    XHigh,
    Max,
}

impl Level {
    pub fn as_str(self) -> &'static str {
        match self {
            Level::None => "none",
            Level::Auto => "auto",
            Level::Minimal => "minimal",
            Level::Low => "low",
            Level::Medium => "medium",
            Level::High => "high",
            Level::XHigh => "xhigh",
            Level::Max => "max",
        }
    }

    /// 解析字符串(大小写不敏感,参考 CPA suffix.go)
    pub fn parse(s: &str) -> Option<Level> {
        match s.trim().to_ascii_lowercase().as_str() {
            "none" => Some(Level::None),
            "auto" => Some(Level::Auto),
            "minimal" => Some(Level::Minimal),
            "low" => Some(Level::Low),
            "medium" => Some(Level::Medium),
            "high" => Some(Level::High),
            "xhigh" => Some(Level::XHigh),
            "max" => Some(Level::Max),
            _ => None,
        }
    }
}

/// 阈值(参考 CPA convert.go)
const THRESHOLD_MINIMAL: i64 = 512;
const THRESHOLD_LOW: i64 = 1024;
const THRESHOLD_MEDIUM: i64 = 8192;
const THRESHOLD_HIGH: i64 = 24576;

/// 默认支持集(无 registry 时用安全的通用集,排除 auto/none/minimal)
pub const DEFAULT_SUPPORTED: [Level; 5] = [Level::Low, Level::Medium, Level::High, Level::XHigh, Level::Max];

/// budget → level(参考 CPA ConvertBudgetToLevel)
pub fn budget_to_level(budget: i64) -> Option<Level> {
    match budget {
        b if b < -1 => None, // 非法负值
        -1 => Some(Level::Auto),
        0 => Some(Level::None),
        b if b <= THRESHOLD_MINIMAL => Some(Level::Minimal),
        b if b <= THRESHOLD_LOW => Some(Level::Low),
        b if b <= THRESHOLD_MEDIUM => Some(Level::Medium),
        b if b <= THRESHOLD_HIGH => Some(Level::High),
        _ => Some(Level::XHigh),
    }
}

/// 钳制到模型支持的最高档(参考 CPA mapConfiguredHighIntent)
fn clamp_to(level: Level, supported: &[Level]) -> Level {
    let candidates: Vec<Level> = match level {
        Level::XHigh => vec![Level::XHigh, Level::Max, Level::High],
        Level::Max => vec![Level::Max, Level::XHigh, Level::High],
        other => vec![other],
    };
    for c in candidates {
        if supported.contains(&c) {
            return c;
        }
    }
    level
}

/// thinking 配置 → effort 字符串(参考 CPA codex_claude_request.go)
///
/// 返回 None 表示不注入(级别非法或不受支持)。supported 为模型允许的级别集。
pub fn resolve_effort(thinking: &serde_json::Value, supported: &[Level]) -> Option<&'static str> {
    let ty = thinking.get("type")?.as_str()?;
    let level = match ty {
        "enabled" => {
            let budget = thinking
                .get("budget_tokens")
                .and_then(|v| v.as_i64())
                .unwrap_or(-1);
            budget_to_level(budget)?
        }
        "adaptive" | "auto" => {
            // 显式 effort 优先(Claude 4.6),缺省 xhigh(与 CPA 一致)
            if let Some(e) = thinking
                .get("output_config")
                .and_then(|o| o.get("effort"))
                .and_then(|v| v.as_str())
            {
                Level::parse(e)?
            } else {
                Level::XHigh
            }
        }
        "disabled" => Level::None,
        _ => return None,
    };
    let clamped = clamp_to(level, supported);
    if supported.contains(&clamped) {
        Some(clamped.as_str())
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_budget_to_level_thresholds() {
        assert_eq!(budget_to_level(-1), Some(Level::Auto));
        assert_eq!(budget_to_level(0), Some(Level::None));
        assert_eq!(budget_to_level(512), Some(Level::Minimal));
        assert_eq!(budget_to_level(1024), Some(Level::Low));
        assert_eq!(budget_to_level(8192), Some(Level::Medium));
        assert_eq!(budget_to_level(24576), Some(Level::High));
        assert_eq!(budget_to_level(50000), Some(Level::XHigh));
        assert_eq!(budget_to_level(-5), None);
    }

    #[test]
    fn test_level_parse_roundtrip() {
        for l in [Level::None, Level::Auto, Level::Minimal, Level::Low, Level::Medium, Level::High, Level::XHigh, Level::Max] {
            assert_eq!(Level::parse(l.as_str()).unwrap(), l);
        }
        assert_eq!(Level::parse("HIGH"), Some(Level::High));
        assert_eq!(Level::parse("bogus"), None);
    }

    #[test]
    fn test_resolve_effort_enabled_budget() {
        let t = json!({"type": "enabled", "budget_tokens": 2000});
        assert_eq!(resolve_effort(&t, &DEFAULT_SUPPORTED), Some("medium"));
    }

    #[test]
    fn test_resolve_effort_auto_clamped_to_none() {
        // auto 不在默认支持集 → 钳制后仍不支持 → None
        let t = json!({"type": "enabled", "budget_tokens": -1});
        assert_eq!(resolve_effort(&t, &DEFAULT_SUPPORTED), None);
    }

    #[test]
    fn test_resolve_effort_disabled_not_in_supported() {
        let t = json!({"type": "disabled"});
        assert_eq!(resolve_effort(&t, &DEFAULT_SUPPORTED), None);
    }

    #[test]
    fn test_resolve_effort_adaptive_uses_output_config() {
        let t = json!({"type": "adaptive", "output_config": {"effort": "high"}});
        assert_eq!(resolve_effort(&t, &DEFAULT_SUPPORTED), Some("high"));
    }

    #[test]
    fn test_resolve_effort_adaptive_defaults_xhigh() {
        let t = json!({"type": "adaptive"});
        assert_eq!(resolve_effort(&t, &DEFAULT_SUPPORTED), Some("xhigh"));
    }

    #[test]
    fn test_resolve_effort_xhigh_clamped_to_custom_set() {
        // 支持集不含 xhigh/max,含 high → 钳到 high
        let t = json!({"type": "adaptive"});
        let supported = [Level::Low, Level::Medium, Level::High];
        assert_eq!(resolve_effort(&t, &supported), Some("high"));
    }

    #[test]
    fn test_resolve_effort_unknown_type() {
        let t = json!({"type": "bogus"});
        assert_eq!(resolve_effort(&t, &DEFAULT_SUPPORTED), None);
    }
}