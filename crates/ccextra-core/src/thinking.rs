// 思考级别映射与模型能力注册表
//
// 语义:
// - budget 阈值决定最小/低/中/高级别(auto/none 单列)
// - thinking 配置 → reasoning.effort 字符串,按模型能力钳制
// - 静态注册表(models.json)记录常见模型支持的 reasoning_levels
// - YAML 配置 max_reasoning_effort 可覆盖注册表

use once_cell::sync::Lazy;
use serde::Deserialize;

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

/// 模型能力定义(来自 models.json)
#[derive(Debug, Clone, Deserialize)]
struct ModelCapability {
    id: String,
    reasoning_levels: Vec<String>,
}

/// 静态注册表(编译时嵌入 models.json)
static MODEL_REGISTRY: Lazy<Vec<ModelCapability>> = Lazy::new(|| {
    const JSON: &str = include_str!("../models.json");
    #[derive(Deserialize)]
    struct Registry {
        models: Vec<ModelCapability>,
    }
    serde_json::from_str::<Registry>(JSON)
        .map(|r| r.models)
        .unwrap_or_default()
});

/// 查找模型支持的 reasoning_levels(精确匹配或别名匹配)
///
/// 匹配逻辑(大小写不敏感):
/// 1. 精确匹配:`glm-5.1` 匹配 `glm-5.1`
/// 2. 别名匹配:`glm-5.1-27717e1a8a72-glm51` 也能匹配到独立条目
fn lookup_model_levels(model: &str) -> Option<Vec<Level>> {
    let model_lower = model.trim().to_ascii_lowercase();
    MODEL_REGISTRY.iter().find_map(|cap| {
        if cap.id.to_ascii_lowercase() == model_lower {
            Some(
                cap.reasoning_levels
                    .iter()
                    .filter_map(|s| Level::parse(s))
                    .collect(),
            )
        } else {
            None
        }
    })
}

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
    if body
        .get("thinking")
        .and_then(|t| t.get("type"))
        .and_then(|v| v.as_str())
        == Some("disabled")
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

/// 钳制 effort 到模型支持的最大级别
///
/// 逻辑:查注册表,降级到最近支持级别(CLIProxyAPI 风格)
/// 查不到 → 不钳制
///
/// 示例:
/// - `clamp_effort("max", "glm-5.1")` → "xhigh"(注册表 glm-5.1 支持到 xhigh)
/// - `clamp_effort("max", "glm-5.2")` → "max"(注册表 glm-5.2 支持 max)
/// - `clamp_effort("max", "gpt-5.6-terra")` → "max"(注册表支持 max)
pub fn clamp_effort<'a>(effort: &'a str, model: &str) -> &'a str {
    let Some(effort_level) = Level::parse(effort) else {
        return effort; // 非法值直透
    };

    // 查注册表(精确匹配)
    let Some(supported) = lookup_model_levels(model) else {
        return effort; // 查不到,不钳制
    };
    clamp_to_nearest(effort_level, &supported).as_str()
}

/// 降级到最近支持级别(CLIProxyAPI clampLevel 逻辑)
///
/// 按标准序找最近级别,tie-break 时取低。
fn clamp_to_nearest(level: Level, supported: &[Level]) -> Level {
    if supported.is_empty() || supported.contains(&level) {
        return level;
    }
    let effort_rank = level_rank(level);
    let mut best = level;
    let mut best_dist = 255;
    for &sup in supported {
        let sup_rank = level_rank(sup);
        let dist = effort_rank.abs_diff(sup_rank);
        // tie-break:取低(sup_rank < best_rank)
        if dist < best_dist || (dist == best_dist && sup_rank < level_rank(best)) {
            best = sup;
            best_dist = dist;
        }
    }
    best
}

/// 级别排序(数值越大级别越高)
fn level_rank(level: Level) -> u8 {
    match level {
        Level::None => 0,
        Level::Auto => 1,
        Level::Minimal => 2,
        Level::Low => 3,
        Level::Medium => 4,
        Level::High => 5,
        Level::XHigh => 6,
        Level::Max => 7,
    }
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

    #[test]
    fn test_clamp_effort_no_limit() {
        assert_eq!(clamp_effort("max", "unknown-model"), "max");
        assert_eq!(clamp_effort("xhigh", "unknown-model"), "xhigh");
    }

    #[test]
    fn test_clamp_effort_invalid_effort_passthrough() {
        assert_eq!(clamp_effort("bogus", "any-model"), "bogus");
    }

    #[test]
    fn test_clamp_effort_registry_glm51() {
        // glm-5.1 注册表支持 low/medium/high/xhigh
        assert_eq!(clamp_effort("max", "glm-5.1"), "xhigh");
        assert_eq!(clamp_effort("xhigh", "glm-5.1"), "xhigh");
        assert_eq!(clamp_effort("high", "glm-5.1"), "high");
    }

    #[test]
    fn test_clamp_effort_registry_glm52() {
        // glm-5.2 支持 low/medium/high/xhigh/max
        assert_eq!(clamp_effort("max", "glm-5.2"), "max");
        assert_eq!(clamp_effort("xhigh", "glm-5.2"), "xhigh");
    }

    #[test]
    fn test_clamp_effort_registry_gpt56() {
        // gpt-5.6-terra 支持 low/medium/high/xhigh/max/ultra
        assert_eq!(clamp_effort("max", "gpt-5.6-terra"), "max");
        assert_eq!(clamp_effort("ultra", "gpt-5.6-sol"), "ultra");
    }

    #[test]
    fn test_clamp_effort_registry_grok46() {
        // grok-4.6 只支持 low/medium/high(对齐 CPA 注册表 xai 系)
        assert_eq!(clamp_effort("max", "grok-4.6"), "high");
        assert_eq!(clamp_effort("xhigh", "grok-4.6"), "high");
        assert_eq!(clamp_effort("high", "grok-4.6"), "high");
        assert_eq!(clamp_effort("medium", "grok-4.6"), "medium");
    }

    #[test]
    fn test_clamp_effort_registry_kimi() {
        // kimi-k3 支持 low/high/max
        assert_eq!(clamp_effort("max", "kimi-k3"), "max");
        assert_eq!(clamp_effort("xhigh", "kimi-k3"), "high"); // xhigh(4)距high(3)/max(5)都是1,tie-break取低
        assert_eq!(clamp_effort("medium", "kimi-k3"), "low"); // medium(2)距low(1)/high(3)都是1,tie-break取低
        assert_eq!(clamp_effort("high", "kimi-k3"), "high");
    }

    #[test]
    fn test_clamp_effort_registry_case_insensitive() {
        assert_eq!(clamp_effort("max", "GLM-5.1"), "xhigh");
        assert_eq!(clamp_effort("xhigh", "Kimi-K3"), "high");
    }

    #[test]
    fn test_clamp_to_nearest_exact_match() {
        let supported = vec![Level::Low, Level::Medium, Level::High];
        assert_eq!(clamp_to_nearest(Level::Medium, &supported), Level::Medium);
    }

    #[test]
    fn test_clamp_to_nearest_downgrade() {
        let supported = vec![Level::Low, Level::Medium, Level::High, Level::XHigh];
        assert_eq!(clamp_to_nearest(Level::Max, &supported), Level::XHigh);
    }

    #[test]
    fn test_clamp_to_nearest_tie_prefers_lower() {
        let supported = vec![Level::Low, Level::High];
        // medium 距离 low/high 都是 1,取低
        assert_eq!(clamp_to_nearest(Level::Medium, &supported), Level::Low);
    }
}
