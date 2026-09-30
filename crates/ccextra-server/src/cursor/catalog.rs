// catalog.rs:Cursor 模型目录合成
//
// sidecar /models 发现账户目录 → 白名单过滤 → ModelConfig 列表 →
// 合成 name "cursor" / protocol CursorSdk provider。刷新失败由调用方
// 保留最近成功 provider(对齐 antigravity 语义)。

use std::path::PathBuf;
use std::sync::Arc;

use ccextra_core::route::{ModelConfig, Protocol, ProviderConfig};

use super::sidecar::CursorSidecar;

/// Cursor 运行时:sidecar 进程 + 可重载配置
///
/// 挂 AppState.cursor,不进 ConfigSnapshot(进程状态不入不可变快照)
pub struct CursorRuntime {
    pub sidecar: Arc<CursorSidecar>,
    pub config: tokio::sync::RwLock<CursorConfig>,
}

/// Cursor 运行时配置(reload 可整体替换)
#[derive(Clone)]
pub struct CursorConfig {
    /// 凭证目录(绝对路径)
    pub auth_dir: PathBuf,
    /// 模型白名单;空 = 全量
    pub models: Vec<String>,
    /// sidecar 空闲回收秒数
    pub idle_secs: u64,
    /// sidecar 最大 Agent 数
    pub max_agents: usize,
    /// SDK 工作目录(local.cwd)
    pub workspace_dir: PathBuf,
}

/// 启用判定:cursor_auth_dir 存在且 trim 后非空
pub fn cursor_enabled(auth_dir_raw: Option<&str>) -> bool {
    auth_dir_raw.is_some_and(|dir| !dir.trim().is_empty())
}

/// 白名单 pattern 归一:`default` 视为 `auto` 的别名
fn normalize_pattern(pattern: &str) -> String {
    if pattern == "default" {
        "auto".to_string()
    } else {
        pattern.to_string()
    }
}

/// 白名单过滤 + 冲突剔除,产出 cursor provider 的模型集
///
/// - 白名单空 = 全放行;glob 展开对目录 id 精确匹配
/// - `auto` 生成 `{name: "auto", alias: "default"}`(name 兜底仍支持入站 auto)
/// - 与现有 provider 的 name/alias 冲突跳过并告警(不阻断)
pub fn build_cursor_models(
    catalog: &[String],
    whitelist: &[String],
    existing: &[ProviderConfig],
) -> Vec<ModelConfig> {
    use std::collections::HashSet;

    let patterns: Vec<String> = whitelist.iter().map(|p| normalize_pattern(p)).collect();
    let allow_all = patterns.is_empty();

    let matches_whitelist = |id: &str| {
        if allow_all {
            return true;
        }
        patterns.iter().any(|pattern| {
            globset::Glob::new(pattern)
                .ok()
                .map(|glob| glob.compile_matcher().is_match(id))
                .unwrap_or_else(|| pattern == id)
        })
    };

    // 现有 provider 已占用的 name 与 alias
    let taken_names: HashSet<&str> = existing
        .iter()
        .flat_map(|p| p.models.iter().map(|m| m.name.as_str()))
        .collect();
    let taken_aliases: HashSet<&str> = existing
        .iter()
        .flat_map(|p| p.models.iter().map(|m| m.alias.as_str()))
        .collect();

    let mut models = Vec::new();
    for raw_id in catalog {
        // catalog 端 id 为 default,与白名单归一后的 auto 对齐
        let id = if raw_id == "default" {
            "auto"
        } else {
            raw_id.as_str()
        };
        if !matches_whitelist(id) {
            continue;
        }
        let (name, alias) = if id == "auto" {
            ("auto", "default")
        } else {
            (id, id)
        };
        if taken_names.contains(name) || taken_aliases.contains(alias) {
            tracing::warn!(model = raw_id, "cursor 模型与现有 provider 冲突,跳过");
            continue;
        }
        models.push(ModelConfig {
            name: name.to_string(),
            alias: alias.to_string(),
            max_input_tokens: None,
            max_tokens: None,
        });
    }
    models
}

/// 合成 cursor provider(base_url 占位空串,不经通用 upstream)
pub fn synthesize_cursor_provider(models: Vec<ModelConfig>) -> ProviderConfig {
    ProviderConfig::new(
        "cursor".to_string(),
        Protocol::CursorSdk,
        vec![String::new()],
        "managed".to_string(),
        None,
        false,
        models,
    )
}

/// 拉取目录并合成 provider:凭证保鲜 → sidecar /models → 过滤合成
///
/// 失败返回 None(调用方保留最近成功 provider);成功空目录按设计
/// 发布空 cursor model 集(返回 Some)
pub async fn load_cursor_provider(
    sidecar: &CursorSidecar,
    config: &CursorConfig,
    existing: &[ProviderConfig],
) -> Option<ProviderConfig> {
    let credential = super::ensure_credential_fresh(&config.auth_dir, None, None)
        .await
        .map_err(|e| {
            tracing::warn!("Cursor 凭证不可用,目录刷新跳过: {e:#}");
            e
        })
        .ok()?;
    let catalog = sidecar
        .models(&credential.access_token)
        .await
        .map_err(|e| {
            tracing::warn!("Cursor sidecar 模型目录拉取失败,保留现有: {e}");
            e
        })
        .ok()?;
    let models = build_cursor_models(&catalog, &config.models, existing);
    Some(synthesize_cursor_provider(models))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider(name: &str, models: Vec<(&str, &str)>) -> ProviderConfig {
        ProviderConfig::new(
            name.to_string(),
            Protocol::OpenAiChat,
            vec!["https://example.com".into()],
            "key".into(),
            None,
            false,
            models
                .into_iter()
                .map(|(name, alias)| ModelConfig {
                    name: name.into(),
                    alias: alias.into(),
                    max_input_tokens: None,
                    max_tokens: None,
                })
                .collect(),
        )
    }

    #[test]
    fn cursor_enabled_requires_non_blank_auth_dir() {
        assert!(!cursor_enabled(None));
        assert!(!cursor_enabled(Some("")));
        assert!(!cursor_enabled(Some("   ")));
        assert!(cursor_enabled(Some("~/.ccextra/cursor")));
    }

    #[test]
    fn empty_whitelist_allows_all_catalog_ids() {
        let models = build_cursor_models(&["auto".into(), "composer-2.5".into()], &[], &[]);
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].name, "auto");
        assert_eq!(models[0].alias, "default");
        assert_eq!(models[1].name, "composer-2.5");
        assert_eq!(models[1].alias, "composer-2.5");
    }

    #[test]
    fn whitelist_default_maps_to_auto() {
        let models = build_cursor_models(
            &["auto".into(), "composer-2.5".into()],
            &["default".into()],
            &[],
        );
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].name, "auto");
        assert_eq!(models[0].alias, "default");
    }

    #[test]
    fn whitelist_glob_filters_catalog() {
        let models = build_cursor_models(
            &[
                "auto".into(),
                "composer-2.5".into(),
                "grok-4.7-xhigh".into(),
            ],
            &["composer-*".into()],
            &[],
        );
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].name, "composer-2.5");
    }

    #[test]
    fn conflicting_names_and_aliases_are_skipped() {
        let existing = vec![
            provider("other", vec![("auto", "auto")]),
            provider("another", vec![("gpt-x", "composer-2.5")]),
        ];
        let models = build_cursor_models(
            &[
                "auto".into(),
                "composer-2.5".into(),
                "grok-4.7-xhigh".into(),
            ],
            &[],
            &existing,
        );
        // auto 与现有 name 冲突;composer-2.5 与现有 alias 冲突;只剩 grok
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].name, "grok-4.7-xhigh");
    }

    #[test]
    fn empty_catalog_yields_empty_provider() {
        let provider = synthesize_cursor_provider(build_cursor_models(&[], &[], &[]));
        assert_eq!(provider.name, "cursor");
        assert_eq!(provider.protocol, Protocol::CursorSdk);
        assert!(provider.models.is_empty());
        assert_eq!(provider.base_urls().len(), 1);
    }
}

#[test]
fn catalog_default_id_maps_to_auto_model() {
    // SDK catalog 实际返回 id "default"(非 auto),须归一后与白名单 default 匹配
    let models = build_cursor_models(
        &["default".into(), "composer-2.5".into()],
        &["default".into()],
        &[],
    );
    assert_eq!(models.len(), 1);
    assert_eq!(models[0].name, "auto");
    assert_eq!(models[0].alias, "default");
}
