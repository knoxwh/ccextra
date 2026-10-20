// 动态 provider 模型白名单过滤(antigravity)
use ccextra_core::route::ModelConfig;
use globset::Glob;

/// 按模式列表过滤模型;空列表 = 全量保留。
/// 模式支持 glob(如 "claude-opus-*")与精确名;"*" 匹配全部。
/// 无效模式告警并跳过(对齐 payload 规则的容错语义)。
/// 若 patterns 非空且全部 glob 编译失败 → Err(显式配置错误,勿静默掏空)。
pub fn filter_models(
    models: Vec<ModelConfig>,
    patterns: &[String],
) -> Result<Vec<ModelConfig>, String> {
    if patterns.is_empty() {
        return Ok(models);
    }
    let mut matchers = Vec::new();
    let mut invalid = 0usize;
    for pat in patterns {
        match Glob::new(pat) {
            Ok(glob) => matchers.push(glob.compile_matcher()),
            Err(error) => {
                tracing::warn!("无效模型过滤模式 {}: {}", pat, error);
                invalid += 1;
            }
        }
    }
    if matchers.is_empty() {
        // patterns 非空(上面已 early-return)且无一合法 → 配置脚枪
        return Err(format!("全部为非法 glob({invalid} 条)"));
    }
    Ok(models
        .into_iter()
        .filter(|model| matchers.iter().any(|m| m.is_match(&model.name)))
        .collect())
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
    fn empty_patterns_keep_all() {
        let models = vec![model("a"), model("b")];
        assert_eq!(filter_models(models.clone(), &[]).unwrap().len(), 2);
        assert_eq!(
            filter_models(models, &[]).unwrap(),
            vec![model("a"), model("b")]
        );
    }

    #[test]
    fn exact_and_glob_patterns_filter() {
        let models = vec![
            model("claude-opus-5-5-high"),
            model("gpt-5.2"),
            model("other"),
        ];
        let patterns: Vec<String> = vec!["claude-opus-*".into(), "gpt-5.2".into()];
        let filtered = filter_models(models, &patterns).unwrap();
        assert_eq!(filtered.len(), 2);
        assert_eq!(filtered[0].name, "claude-opus-5-5-high");
        assert_eq!(filtered[1].name, "gpt-5.2");
    }

    #[test]
    fn star_matches_everything() {
        let models = vec![model("a"), model("b")];
        let patterns: Vec<String> = vec!["*".into()];
        assert_eq!(filter_models(models, &patterns).unwrap().len(), 2);
    }

    #[test]
    fn no_match_yields_empty() {
        let models = vec![model("a")];
        let patterns: Vec<String> = vec!["zzz-*".into()];
        assert!(filter_models(models, &patterns).unwrap().is_empty());
    }

    #[test]
    fn all_invalid_globs_errors_distinctly() {
        let models = vec![model("a"), model("b")];
        // `[` 单独是非法 glob
        let patterns: Vec<String> = vec!["[".into(), "[unterminated".into()];
        let err = filter_models(models, &patterns).unwrap_err();
        assert!(err.contains("全部为非法 glob"), "got {err}");
    }

    #[test]
    fn partial_invalid_globs_still_filters_with_valid_ones() {
        let models = vec![model("keep-me"), model("drop-me")];
        let patterns: Vec<String> = vec!["[".into(), "keep-*".into()];
        let filtered = filter_models(models, &patterns).unwrap();
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0].name, "keep-me");
    }
}
