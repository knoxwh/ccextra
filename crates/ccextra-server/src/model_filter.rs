// 动态 provider 模型白名单过滤(antigravity/cursor 共用)
use ccextra_core::route::ModelConfig;
use globset::Glob;

/// 按模式列表过滤模型;空列表 = 全量保留。
/// 模式支持 glob(如 "claude-opus-*")与精确名;"*" 匹配全部。
/// 无效模式告警并跳过(对齐 payload 规则的容错语义)。
pub fn filter_models(models: Vec<ModelConfig>, patterns: &[String]) -> Vec<ModelConfig> {
    if patterns.is_empty() {
        return models;
    }
    let matchers: Vec<_> = patterns
        .iter()
        .filter_map(|pat| match Glob::new(pat) {
            Ok(glob) => Some(glob.compile_matcher()),
            Err(error) => {
                tracing::warn!("无效模型过滤模式 {}: {}", pat, error);
                None
            }
        })
        .collect();
    models
        .into_iter()
        .filter(|model| matchers.iter().any(|m| m.is_match(&model.name)))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn model(name: &str) -> ModelConfig {
        ModelConfig {
            name: name.into(),
            alias: name.into(),
            max_input_tokens: None,
            max_tokens: None,
        }
    }

    #[test]
    fn empty_patterns_keep_all_models() {
        let models = vec![model("a"), model("b")];
        assert_eq!(filter_models(models.clone(), &[]).len(), 2);
        assert_eq!(filter_models(models, &[]), vec![model("a"), model("b")]);
    }

    #[test]
    fn exact_and_glob_patterns_filter() {
        let models = vec![
            model("claude-opus-5-5-high"),
            model("gpt-5.2"),
            model("other"),
        ];
        let patterns: Vec<String> = vec!["claude-opus-*".into(), "gpt-5.2".into()];
        let filtered = filter_models(models, &patterns);
        assert_eq!(filtered.len(), 2);
        assert_eq!(filtered[0].name, "claude-opus-5-5-high");
        assert_eq!(filtered[1].name, "gpt-5.2");
    }

    #[test]
    fn star_matches_everything() {
        let models = vec![model("a"), model("b")];
        let patterns: Vec<String> = vec!["*".into()];
        assert_eq!(filter_models(models, &patterns).len(), 2);
    }

    #[test]
    fn no_match_yields_empty() {
        let models = vec![model("a")];
        let patterns: Vec<String> = vec!["zzz-*".into()];
        assert!(filter_models(models, &patterns).is_empty());
    }
}
