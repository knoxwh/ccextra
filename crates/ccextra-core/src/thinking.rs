// 思考级别映射与模型能力注册表
//
// 语义:
// - budget 阈值决定最小/低/中/高级别(auto/none 单列)
// - thinking 配置 → reasoning.effort 字符串,按调用方传入的注册表钳制
// - 注册表由 cli 从用户 models.json 读入;core 不读文件、不编进二进制
// - 查不到模型或不传表 → 不钳制

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

/// 模型能力定义(来自用户 models.json)
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ModelCapability {
    pub id: String,
    pub reasoning_levels: Vec<String>,
    /// 固定 effort:设置后凡 clamp 介入的 effort 一律改用该值(不钳制);
    /// 非法级别值视为未设置
    #[serde(default)]
    pub force_effort: Option<String>,
}

#[derive(Deserialize)]
struct RegistryFile {
    models: Vec<ModelCapability>,
}

/// 解析用户 models.json 文本。core 不读文件。
pub fn parse_registry(json: &str) -> Result<Vec<ModelCapability>, serde_json::Error> {
    serde_json::from_str::<RegistryFile>(json).map(|r| r.models)
}

/// 按上游模型名查注册表条目(大小写不敏感精确匹配)
fn find_capability<'a>(
    model: &str,
    registry: &'a [ModelCapability],
) -> Option<&'a ModelCapability> {
    let model_lower = model.trim().to_ascii_lowercase();
    registry
        .iter()
        .find(|cap| cap.id.to_ascii_lowercase() == model_lower)
}

/// 查找模型支持的 reasoning_levels(精确匹配或别名匹配,测试可注入 registry)
///
/// 匹配逻辑(大小写不敏感):
/// 1. 精确匹配:`glm-5.1` 匹配 `glm-5.1`
/// 2. 别名匹配:`glm-5.1-27717e1a8a72-glm51` 也能匹配到独立条目
fn lookup_model_levels_impl(model: &str, registry: &[ModelCapability]) -> Option<Vec<Level>> {
    find_capability(model, registry).map(|cap| {
        cap.reasoning_levels
            .iter()
            .filter_map(|s| Level::parse(s))
            .collect()
    })
}

/// 查询模型固定 effort(force_effort;非法级别值视为未设置)
pub fn forced_effort(model: &str, registry: &[ModelCapability]) -> Option<&'static str> {
    find_capability(model, registry)
        .and_then(|cap| cap.force_effort.as_deref())
        .and_then(Level::parse)
        .map(Level::as_str)
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

/// 钳制 effort 到模型支持的最近级别
///
/// 查不到模型或注册表为空 → 不钳制
pub fn clamp_effort<'a>(effort: &'a str, model: &str, registry: &[ModelCapability]) -> &'a str {
    let Some(effort_level) = Level::parse(effort) else {
        return effort; // 非法值直透
    };

    // 查注册表(精确匹配)
    let Some(supported) = lookup_model_levels_impl(model, registry) else {
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
    fn test_resolve_effort_modes() {
        for (thinking, expected) in [
            (
                json!({"type": "enabled", "budget_tokens": 2000}),
                Some("medium"),
            ),
            (
                json!({"type": "enabled", "budget_tokens": -1}),
                Some("auto"),
            ),
            (json!({"type": "disabled"}), Some("none")),
            (
                json!({"type": "adaptive", "output_config": {"effort": "high"}}),
                Some("high"),
            ),
            (json!({"type": "adaptive"}), Some("xhigh")),
            (json!({"type": "bogus"}), None),
        ] {
            assert_eq!(resolve_effort(&thinking), expected, "{thinking}");
        }
    }

    #[test]
    fn test_resolve_effort_from_body_precedence() {
        for (body, expected) in [
            (
                json!({"thinking": {"type": "adaptive"}, "output_config": {"effort": "max"}}),
                Some("max"),
            ),
            (
                json!({"thinking": {"type": "adaptive", "output_config": {"effort": "high"}}}),
                Some("high"),
            ),
            (
                json!({"thinking": {"type": "enabled", "budget_tokens": 8192}}),
                Some("medium"),
            ),
            (
                json!({"thinking": {"type": "disabled"}, "output_config": {"effort": "max"}}),
                Some("none"),
            ),
            (json!({"model": "x"}), None),
            (
                json!({"thinking": {"type": "adaptive", "output_config": {"effort": "high"}}, "output_config": {"effort": "bogus"}}),
                Some("high"),
            ),
        ] {
            assert_eq!(resolve_effort_from_body(&body), expected, "{body}");
        }
    }

    #[test]
    fn test_clamp_effort_without_supported_model() {
        for (effort, model) in [
            ("max", "unknown-model"),
            ("xhigh", "unknown-model"),
            ("bogus", "any-model"),
        ] {
            assert_eq!(clamp_effort(effort, model, &[]), effort);
        }
    }

    #[test]
    fn test_parse_registry_roundtrip() {
        let json = r#"{"models":[
            {"id":"gpt-6-astra","reasoning_levels":["low","medium"],"force_effort":"low"},
            {"id":"kimi-k3","reasoning_levels":["low","high"]}
        ]}"#;
        let reg = parse_registry(json).unwrap();
        assert_eq!(reg.len(), 2);
        assert_eq!(reg[0].id, "gpt-6-astra");
        assert_eq!(reg[0].reasoning_levels, ["low", "medium"]);
        assert_eq!(reg[0].force_effort.as_deref(), Some("low"));
        assert_eq!(reg[1].force_effort, None);
        assert_eq!(clamp_effort("high", "gpt-6-astra", &reg), "medium");
    }

    #[test]
    fn test_parse_registry_invalid_json() {
        assert!(parse_registry("{").is_err());
    }

    #[test]
    fn test_forced_effort_lookup() {
        let reg = vec![
            ModelCapability {
                id: "glm-5.3".into(),
                reasoning_levels: vec!["low".into(), "high".into()],
                force_effort: Some("high".into()),
            },
            ModelCapability {
                id: "bad".into(),
                reasoning_levels: vec!["low".into()],
                force_effort: Some("bogus".into()),
            },
        ];
        assert_eq!(forced_effort("glm-5.3", &reg), Some("high"));
        // 大小写不敏感
        assert_eq!(forced_effort("GLM-5.3", &reg), Some("high"));
        // 非法级别值视为未设置
        assert_eq!(forced_effort("bad", &reg), None);
        // 未收录模型
        assert_eq!(forced_effort("missing", &reg), None);
    }

    // mock registry 供测试用(不依赖 models.json 实际内容)
    fn mock_registry() -> Vec<ModelCapability> {
        vec![
            ModelCapability {
                id: "glm-5.1".into(),
                reasoning_levels: vec![
                    "low".into(),
                    "medium".into(),
                    "high".into(),
                    "xhigh".into(),
                ],
                force_effort: None,
            },
            ModelCapability {
                id: "glm-5.2".into(),
                reasoning_levels: vec!["low".into(), "high".into(), "max".into()],
                force_effort: None,
            },
            ModelCapability {
                id: "gpt-5.6-terra".into(),
                reasoning_levels: vec![
                    "low".into(),
                    "medium".into(),
                    "high".into(),
                    "xhigh".into(),
                ],
                force_effort: None,
            },
            ModelCapability {
                id: "gpt-5.6-sol".into(),
                reasoning_levels: vec![
                    "low".into(),
                    "medium".into(),
                    "high".into(),
                    "xhigh".into(),
                ],
                force_effort: None,
            },
            ModelCapability {
                id: "grok-4.6".into(),
                reasoning_levels: vec![
                    "low".into(),
                    "medium".into(),
                    "high".into(),
                    "xhigh".into(),
                ],
                force_effort: None,
            },
            ModelCapability {
                id: "kimi-k3".into(),
                reasoning_levels: vec!["low".into(), "high".into(), "max".into()],
                force_effort: None,
            },
            ModelCapability {
                id: "gemini-3.8-flash-high".into(),
                reasoning_levels: vec!["high".into()],
                force_effort: None,
            },
        ]
    }

    #[test]
    fn test_clamp_effort_registry_models() {
        let reg = mock_registry();
        for (effort, model, expected) in [
            ("max", "glm-5.1", "xhigh"),
            ("xhigh", "glm-5.1", "xhigh"),
            ("high", "glm-5.1", "high"),
            ("max", "glm-5.2", "max"),
            ("xhigh", "glm-5.2", "high"),
            ("max", "gpt-5.6-terra", "xhigh"),
            ("max", "gpt-5.6-sol", "xhigh"),
            ("max", "grok-4.6", "xhigh"),
            ("xhigh", "grok-4.6", "xhigh"),
            ("high", "grok-4.6", "high"),
            ("medium", "grok-4.6", "medium"),
            ("max", "kimi-k3", "max"),
            ("xhigh", "kimi-k3", "high"),
            ("medium", "kimi-k3", "low"),
            ("high", "kimi-k3", "high"),
            ("max", "GLM-5.1", "xhigh"),
            ("xhigh", "Kimi-K3", "high"),
            ("max", "gemini-3.8-flash-high", "high"),
            ("medium", "gemini-3.8-flash-high", "high"),
        ] {
            assert_eq!(
                clamp_effort(effort, model, &reg),
                expected,
                "{model}: {effort}"
            );
        }
    }

    #[test]
    fn test_clamp_to_nearest_cases() {
        for (effort, supported, expected) in [
            (
                Level::Medium,
                &[Level::Low, Level::Medium, Level::High][..],
                Level::Medium,
            ),
            (
                Level::Max,
                &[Level::Low, Level::Medium, Level::High, Level::XHigh][..],
                Level::XHigh,
            ),
            (Level::Medium, &[Level::Low, Level::High][..], Level::Low),
        ] {
            assert_eq!(clamp_to_nearest(effort, supported), expected);
        }
    }
}
