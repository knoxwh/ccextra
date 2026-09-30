// catalog.rs:Cursor 模型目录合成
//
// sidecar /models 发现账户目录 → 白名单过滤 → ModelConfig 列表 →
// 合成 name "cursor" / protocol CursorSdk provider。刷新失败由调用方
// 保留最近成功 provider(对齐 antigravity 语义)。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use ccextra_core::convert::CursorParamVocab;
use ccextra_core::route::{ModelConfig, Protocol, ProviderConfig};

use super::sidecar::CursorSidecar;

/// Cursor 运行时:sidecar 进程 + 可重载配置 + 模型参数词表
///
/// 挂 AppState.cursor,不进 ConfigSnapshot(进程状态不入不可变快照)
pub struct CursorRuntime {
    pub sidecar: Arc<CursorSidecar>,
    pub config: tokio::sync::RwLock<CursorConfig>,
    /// 模型参数词表(归一 id → params;目录刷新时整体替换)
    pub vocab: tokio::sync::RwLock<HashMap<String, Vec<CursorParamVocab>>>,
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

/// 白名单条目拆 base:`id:param=value` 取 `id` 段
fn split_whitelist_base(entry: &str) -> &str {
    entry.split(':').next().unwrap_or(entry)
}

/// 固定参数段合法性:逗号分隔的 `k=v`,k/v 均非空(与转换层 split_pinned_params 对齐)
fn valid_pinned_tail(tail: &str) -> bool {
    tail.split(',')
        .all(|pair| matches!(pair.split_once('='), Some((k, v)) if !k.is_empty() && !v.is_empty()))
}

fn glob_matches(pattern: &str, id: &str) -> bool {
    globset::Glob::new(pattern)
        .ok()
        .map(|glob| glob.compile_matcher().is_match(id))
        .unwrap_or_else(|| pattern == id)
}

/// 白名单过滤 + 冲突剔除,产出 cursor provider 的模型集
///
/// - 白名单空 = 全放行;glob 展开对目录 id 精确匹配
/// - 条目可带固定参数 `"id:param=value,param=value"`:base 参与匹配,
///   固定参数拼进 name 随路由传给转换层(入站仍用裸 base/alias)
/// - `auto` 生成 `{name: "auto", alias: "default"}`(name 兜底仍支持入站 auto)
/// - 与现有 provider 的 alias 冲突跳过并告警(不阻断;name 可跨 provider 重复)
pub fn build_cursor_models(
    catalog: &[super::client::CursorModelEntry],
    whitelist: &[String],
    existing: &[ProviderConfig],
) -> Vec<ModelConfig> {
    use std::collections::HashSet;

    // (归一 base pattern, 原始条目)——命中后用原始条目重建带参数的 name
    let entries: Vec<(String, &str)> = whitelist
        .iter()
        .map(|entry| {
            (
                normalize_pattern(split_whitelist_base(entry)),
                entry.as_str(),
            )
        })
        .collect();
    let allow_all = entries.is_empty();

    // 现有 provider 已占用的 alias(路由键;name 跨 provider 可重复,如 gpt-6.1-sol)
    let taken_aliases: HashSet<&str> = existing
        .iter()
        .flat_map(|p| p.models.iter().map(|m| m.alias.as_str()))
        .collect();

    let mut models = Vec::new();
    for entry in catalog {
        // catalog 端 id 为 default,与白名单归一后的 auto 对齐
        let id = if entry.id == "default" {
            "auto"
        } else {
            entry.id.as_str()
        };
        // 命中的白名单条目(取第一个);allow_all 时无条目,用裸 id
        let matched = if allow_all {
            None
        } else {
            entries
                .iter()
                .find(|(pattern, _)| glob_matches(pattern, id))
                .map(|(_, raw)| *raw)
        };
        if !allow_all && matched.is_none() {
            continue;
        }
        // name = 归一 base + 白名单固定参数段(原样保留参数顺序);
        // 非法参数段(缺 = 或空 k/v)跳过并告警,避免合成转换层无法解析的 name
        let name = match matched.map(|raw| raw.split_once(':')) {
            Some(Some((_, tail))) if valid_pinned_tail(tail) => format!("{id}:{tail}"),
            Some(Some(_)) => {
                tracing::warn!(model = %entry.id, "cursor 白名单固定参数段非法(须为 k=v 逗号分隔),跳过");
                continue;
            }
            _ => id.to_string(),
        };
        let alias = if id == "auto" { "default" } else { id };
        if taken_aliases.contains(alias) {
            tracing::warn!(model = %entry.id, "cursor 模型 alias 与现有 provider 冲突,跳过");
            continue;
        }
        models.push(ModelConfig {
            name,
            alias: alias.to_string(),
            max_input_tokens: None,
            max_tokens: None,
        });
    }
    models
}

/// 目录词表:归一 id → 参数列表(转换层 effort/thinking 映射依据)
pub fn build_cursor_vocab(
    catalog: &[super::client::CursorModelEntry],
) -> HashMap<String, Vec<CursorParamVocab>> {
    catalog
        .iter()
        .map(|entry| {
            let id = if entry.id == "default" {
                "auto".to_string()
            } else {
                entry.id.clone()
            };
            (id, entry.parameters.clone())
        })
        .collect()
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
/// 失败返回 None(调用方保留最近成功 provider 与词表);成功返回
/// (provider, 参数词表),空目录按设计发布空 cursor model 集
pub async fn load_cursor_provider(
    sidecar: &CursorSidecar,
    config: &CursorConfig,
    existing: &[ProviderConfig],
) -> Option<(ProviderConfig, HashMap<String, Vec<CursorParamVocab>>)> {
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
    let vocab = build_cursor_vocab(&catalog);
    Some((synthesize_cursor_provider(models), vocab))
}

#[cfg(test)]
mod tests {
    use super::super::client::CursorModelEntry;
    use super::*;

    fn entry(id: &str) -> CursorModelEntry {
        CursorModelEntry {
            id: id.to_string(),
            parameters: vec![],
        }
    }

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
        let models = build_cursor_models(&[entry("auto"), entry("composer-2.5")], &[], &[]);
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].name, "auto");
        assert_eq!(models[0].alias, "default");
        assert_eq!(models[1].name, "composer-2.5");
        assert_eq!(models[1].alias, "composer-2.5");
    }

    #[test]
    fn whitelist_default_maps_to_auto() {
        let models = build_cursor_models(
            &[entry("auto"), entry("composer-2.5")],
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
                entry("auto"),
                entry("composer-2.5"),
                entry("grok-4.7-xhigh"),
            ],
            &["composer-*".into()],
            &[],
        );
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].name, "composer-2.5");
    }

    #[test]
    fn name_conflict_alone_keeps_model_alias_conflict_skips() {
        // name 跨 provider 可重复(路由只看 alias);仅 alias 冲突才跳过
        let existing = vec![
            provider("other", vec![("grok-4.7", "x-grok-4.7")]),
            provider("another", vec![("gpt-x", "composer-2.5")]),
        ];
        let models = build_cursor_models(
            &[entry("grok-4.7"), entry("composer-2.5"), entry("auto")],
            &[],
            &existing,
        );
        // grok-4.7 仅 name 撞,保留;composer-2.5 alias 撞,跳过
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].name, "grok-4.7");
        assert_eq!(models[0].alias, "grok-4.7");
        assert_eq!(models[1].name, "auto");
        assert_eq!(models[1].alias, "default");
    }

    #[test]
    fn empty_catalog_yields_empty_provider() {
        let provider = synthesize_cursor_provider(build_cursor_models(&[], &[], &[]));
        assert_eq!(provider.name, "cursor");
        assert_eq!(provider.protocol, Protocol::CursorSdk);
        assert!(provider.models.is_empty());
        assert_eq!(provider.base_urls().len(), 1);
    }

    #[test]
    fn catalog_default_id_maps_to_auto_model() {
        // SDK catalog 实际返回 id "default"(非 auto),须归一后与白名单 default 匹配
        let models = build_cursor_models(
            &[entry("default"), entry("composer-2.5")],
            &["default".into()],
            &[],
        );
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].name, "auto");
        assert_eq!(models[0].alias, "default");
    }

    #[test]
    fn whitelist_pinned_params_flow_into_name() {
        // 固定参数条目:base 匹配目录 id,name 携带参数段,alias 保持裸 base
        let models = build_cursor_models(
            &[entry("auto-smart"), entry("composer-2.5")],
            &["auto-smart:optimize_for=intelligence".to_string()],
            &[],
        );
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].name, "auto-smart:optimize_for=intelligence");
        assert_eq!(models[0].alias, "auto-smart");
    }

    #[test]
    fn whitelist_pinned_default_base_normalizes_to_auto() {
        let models = build_cursor_models(
            &[entry("default")],
            &["default:optimize_for=intelligence".to_string()],
            &[],
        );
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].name, "auto:optimize_for=intelligence");
        assert_eq!(models[0].alias, "default");
    }

    #[test]
    fn whitelist_invalid_pinned_tail_skips_model() {
        // 缺 = 或空 k/v 的参数段无法被转换层解析,须跳过而非合成坏 name
        let models = build_cursor_models(
            &[entry("auto-smart"), entry("composer-2.5")],
            &[
                "auto-smart:foo".to_string(),
                "composer-2.5:optimize_for=".to_string(),
            ],
            &[],
        );
        assert!(models.is_empty());
    }

    #[test]
    fn build_vocab_normalizes_default_and_keeps_params() {
        let catalog = vec![
            CursorModelEntry {
                id: "default".to_string(),
                parameters: vec![],
            },
            CursorModelEntry {
                id: "grok-4.7".to_string(),
                parameters: vec![CursorParamVocab {
                    id: "reasoning_effort".to_string(),
                    values: vec!["low".to_string(), "xhigh".to_string()],
                }],
            },
        ];
        let vocab = build_cursor_vocab(&catalog);
        assert!(vocab.contains_key("auto"));
        assert_eq!(vocab["grok-4.7"][0].id, "reasoning_effort");
        assert_eq!(vocab["grok-4.7"][0].values, vec!["low", "xhigh"]);
    }
}
