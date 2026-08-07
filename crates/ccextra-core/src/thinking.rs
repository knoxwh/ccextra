// 思考级别映射
//
// 语义:
// - budget 阈值决定最小/低/中/高级别(auto/none 单列)
// - thinking 配置 → reasoning.effort 字符串,直映射不钳制
//   (ccextra 无模型能力表,转换层忠实透传)

/// 思考级别(与 Anthropic thinking 级别对应)
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

    /// 解析字符串(大小写不敏感)
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

/// 阈值
const THRESHOLD_MINIMAL: i64 = 512;
const THRESHOLD_LOW: i64 = 1024;
const THRESHOLD_MEDIUM: i64 = 8192;
const THRESHOLD_HIGH: i64 = 24576;

/// 默认支持集(无 registry 时用安全的通用集,排除 auto/none/minimal)
pub const DEFAULT_SUPPORTED: [Level; 5] = [
    Level::Low,
    Level::Medium,
    Level::High,
    Level::XHigh,
    Level::Max,
];

/// budget → level
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

/// 请求 body → effort 字符串
///
/// 优先读取顶层 `output_config.effort`(Claude Code 2.1+ 新格式:
/// thinking 只含 type,effort 单独放顶层),回退 legacy
/// `thinking.output_config.effort` / budget 映射。返回 None 表示不注入。
pub fn resolve_effort_from_body(body: &serde_json::Value) -> Option<&'static str> {
    // thinking 显式 disabled 时忽略残留 effort(对齐上游钳制行为)
    if body.get("thinking").and_then(|t| t.get("type")).and_then(|v| v.as_str()) == Some("disabled")
    {
        return Some(Level::None.as_str());
    }
    // Claude Code 2.1+:effort 在请求顶层 output_config
    if let Some(e) = body
        .get("output_config")
        .and_then(|o| o.get("effort"))
        .and_then(|v| v.as_str())
        .and_then(Level::parse)
    {
        return Some(e.as_str());
    }
    body.get("thinking").and_then(resolve_effort)
}

/// thinking 配置 → effort 字符串
///
/// 直映射不钳制:转换层直接取 budget→level 结果透传(无模型能力表
/// 校验)。返回 None 表示不注入,调用方回退默认 effort("medium")。
pub fn resolve_effort(thinking: &serde_json::Value) -> Option<&'static str> {
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
            // 显式 effort 优先(Claude 4.6),缺省 xhigh
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
    Some(level.as_str())
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
        for l in [
            Level::None,
            Level::Auto,
            Level::Minimal,
            Level::Low,
            Level::Medium,
            Level::High,
            Level::XHigh,
            Level::Max,
        ] {
            assert_eq!(Level::parse(l.as_str()).unwrap(), l);
        }
        assert_eq!(Level::parse("HIGH"), Some(Level::High));
        assert_eq!(Level::parse("bogus"), None);
    }

    #[test]
    fn test_resolve_effort_enabled_budget() {
        let t = json!({"type": "enabled", "budget_tokens": 2000});
        assert_eq!(resolve_effort(&t), Some("medium"));
    }

    #[test]
    fn test_resolve_effort_enabled_auto_budget() {
        // enabled + budget=-1 → budget_to_level(-1)=Auto
        let t = json!({"type": "enabled", "budget_tokens": -1});
        assert_eq!(resolve_effort(&t), Some("auto"));
    }

    #[test]
    fn test_resolve_effort_disabled_none() {
        let t = json!({"type": "disabled"});
        assert_eq!(resolve_effort(&t), Some("none"));
    }

    #[test]
    fn test_resolve_effort_adaptive_uses_output_config() {
        let t = json!({"type": "adaptive", "output_config": {"effort": "high"}});
        assert_eq!(resolve_effort(&t), Some("high"));
    }

    #[test]
    fn test_resolve_effort_adaptive_defaults_xhigh() {
        let t = json!({"type": "adaptive"});
        assert_eq!(resolve_effort(&t), Some("xhigh"));
    }

    #[test]
    fn test_resolve_effort_unknown_type() {
        let t = json!({"type": "bogus"});
        assert_eq!(resolve_effort(&t), None);
    }

    #[test]
    fn test_resolve_effort_from_body_prefers_top_level_output_config() {
        // Claude Code 2.1+:thinking 只含 type,effort 在顶层 output_config
        let b = json!({
            "thinking": {"type": "adaptive"},
            "output_config": {"effort": "max"}
        });
        assert_eq!(resolve_effort_from_body(&b), Some("max"));
    }

    #[test]
    fn test_resolve_effort_from_body_falls_back_to_thinking_effort() {
        // legacy:effort 内嵌 thinking.output_config
        let b = json!({
            "thinking": {"type": "adaptive", "output_config": {"effort": "high"}}
        });
        assert_eq!(resolve_effort_from_body(&b), Some("high"));
    }

    #[test]
    fn test_resolve_effort_from_body_falls_back_to_budget() {
        let b = json!({
            "thinking": {"type": "enabled", "budget_tokens": 8192}
        });
        assert_eq!(resolve_effort_from_body(&b), Some("medium"));
    }

    #[test]
    fn test_resolve_effort_from_body_disabled_wins() {
        // 顶层残留 effort 不覆盖 disabled
        let b = json!({
            "thinking": {"type": "disabled"},
            "output_config": {"effort": "max"}
        });
        assert_eq!(resolve_effort_from_body(&b), Some("none"));
    }

    #[test]
    fn test_resolve_effort_from_body_no_thinking_none() {
        let b = json!({"model": "x"});
        assert_eq!(resolve_effort_from_body(&b), None);
    }

    #[test]
    fn test_resolve_effort_from_body_invalid_effort_falls_back() {
        // 顶层 effort 非法 → 回退 thinking 分支
        let b = json!({
            "thinking": {"type": "adaptive", "output_config": {"effort": "high"}},
            "output_config": {"effort": "bogus"}
        });
        assert_eq!(resolve_effort_from_body(&b), Some("high"));
    }
}
