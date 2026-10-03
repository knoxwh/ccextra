// catalog.rs:Cursor 模型目录合成(原生 GetUsableModels)
//
// 原生目录拉取 → 白名单过滤 → ModelConfig 列表 → 合成 name "cursor" /
// protocol CursorSdk provider。刷新失败由调用方保留最近成功 provider
// (对齐 antigravity 语义)。白名单裸 base 命中变体家族(grok-4.7-high 等)
// 时合成 effort_levels 标记模型,转换层把钳制后的思考等级拼进 model id;
// 目录已有裸 id 的模型(auto/composer-2.5)不经家族展开。

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use ccextra_core::route::{ModelConfig, Protocol, ProviderConfig};

use super::models::{fetch_models, CursorModelEntry};
use super::session::CursorSessions;

/// Cursor 运行时:双向流会话表 + 可重载配置
///
/// 挂 AppState.cursor,不进 ConfigSnapshot(进程状态不入不可变快照)
pub struct CursorRuntime {
    pub sessions: CursorSessions,
    pub config: tokio::sync::RwLock<CursorConfig>,
}

/// Cursor 运行时配置(reload 可整体替换)
#[derive(Clone)]
pub struct CursorConfig {
    /// 凭证目录(绝对路径)
    pub auth_dir: PathBuf,
    /// 模型白名单;空 = 全量
    pub models: Vec<String>,
    /// 上游 base_url 覆盖(默认 api2.cursor.sh)
    pub base_url: Option<String>,
    /// 客户端版本头覆盖
    pub client_version: Option<String>,
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

/// 目录变体家族检测:base-{level} 形态的等级后缀
///
/// `-fast` 与 `thinking` 等非等级后缀不算;返回目录中实际存在的等级
fn family_levels(catalog: &[CursorModelEntry], base: &str) -> Vec<String> {
    let mut levels = Vec::new();
    for entry in catalog {
        let Some(suffix) = entry.id.strip_prefix(base) else {
            continue;
        };
        let Some(level) = suffix.strip_prefix('-') else {
            continue;
        };
        if ccextra_core::thinking::Level::parse(level).is_some() {
            levels.push(level.to_string());
        }
    }
    levels
}

/// 家族缺省等级:优先 medium,否则取等级中位(按档位排序)
fn default_effort_level(levels: &[String]) -> String {
    if levels.iter().any(|level| level == "medium") {
        return "medium".to_string();
    }
    let mut ranked: Vec<&str> = levels.iter().map(String::as_str).collect();
    ranked.sort_by_key(|level| {
        ccextra_core::thinking::level_rank(
            ccextra_core::thinking::Level::parse(level).expect("family_levels 已过滤非法等级"),
        )
    });
    ranked
        .get(ranked.len() / 2)
        .map(|level| (*level).to_string())
        .unwrap_or_else(|| "medium".to_string())
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
/// - `default` 条目生成 `{name: "default", alias: "default"}`:name 是发往
///   上游的 model id,必须保持目录原值(上游拒绝 "auto");白名单写
///   `default` 或 `auto` 均归一后命中该条目
/// - 与现有 provider 的 alias 冲突跳过并告警(不阻断;name 可跨 provider 重复)
pub fn build_cursor_models(
    catalog: &[CursorModelEntry],
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
        // name 是发往上游的 model id:default 条目必须保持目录原值 "default"
        // (上游拒绝 "auto",实测 ERROR_BAD_MODEL_NAME);归一仅用于白名单匹配
        let name = match matched.map(|raw| raw.split_once(':')) {
            Some(Some((_, tail))) if valid_pinned_tail(tail) => format!("{}:{tail}", entry.id),
            Some(Some(_)) => {
                tracing::warn!(model = %entry.id, "cursor 白名单固定参数段非法(须为 k=v 逗号分隔),跳过");
                continue;
            }
            _ => entry.id.clone(),
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
    // 家族展开:白名单条目未命中任何目录 id 时,检测 base-{level} 变体家族
    // (上游目录以变体形态发布思考等级,如 grok-4.7-high;裸 base 上游按
    // default/auto 处理)。合成单一模型,name 携带 effort_levels 标记,
    // 转换层把钳制后的 effort 拼进 model id
    if !allow_all {
        // 同 build 内已产出的 alias(精确路径 + 先前的家族条目):
        // 重复白名单条目(如裸 base 与钉参条目同 base)不重复物化
        let mut emitted_aliases: HashSet<String> = models.iter().map(|m| m.alias.clone()).collect();
        for (pattern, raw) in &entries {
            // glob 已命中目录 id 的条目走上面的精确路径,不重复展开
            if catalog.iter().any(|entry| {
                let id = if entry.id == "default" {
                    "auto"
                } else {
                    entry.id.as_str()
                };
                glob_matches(pattern, id)
            }) {
                continue;
            }
            let levels = family_levels(catalog, pattern);
            if levels.is_empty() {
                continue;
            }
            let alias = if pattern == "auto" {
                "default"
            } else {
                pattern.as_str()
            };
            if taken_aliases.contains(alias) || emitted_aliases.contains(alias) {
                tracing::warn!(model = %pattern, "cursor 模型 alias 与现有 provider 冲突,跳过");
                continue;
            }
            // name = base + 固定参数段(原样)+ effort_levels 标记;
            // 等级用 '+' 分隔,避开参数段的逗号切分。
            // 家族展开意味着目录无裸 base 条目,无 effort 的请求也必须拼
            // 等级后缀,否则上游 not_found;effort_default 指定缺省等级
            let levels_csv = levels.join("+");
            let default_level = default_effort_level(&levels);
            let name = match raw.split_once(':') {
                Some((_, tail)) if valid_pinned_tail(tail) => {
                    format!("{pattern}:{tail},effort_levels={levels_csv},effort_default={default_level}")
                }
                Some(_) => {
                    tracing::warn!(model = %pattern, "cursor 白名单固定参数段非法(须为 k=v 逗号分隔),跳过");
                    continue;
                }
                None => {
                    format!("{pattern}:effort_levels={levels_csv},effort_default={default_level}")
                }
            };
            models.push(ModelConfig {
                name,
                alias: alias.to_string(),
                max_input_tokens: None,
                max_tokens: None,
            });
            emitted_aliases.insert(alias.to_string());
        }
    }
    models
}

/// 合成 cursor provider(base_url 进 provider,handler 从路由快照读取)
pub fn synthesize_cursor_provider(
    models: Vec<ModelConfig>,
    base_url: &str,
    metadata: HashMap<String, String>,
) -> ProviderConfig {
    ProviderConfig::new(
        "cursor".to_string(),
        Protocol::CursorSdk,
        vec![base_url.to_string()],
        "managed".to_string(),
        None,
        false,
        models,
    )
    .with_metadata(metadata)
}

/// 拉取目录并合成 provider:凭证保鲜 → 原生 GetUsableModels → 过滤合成
///
/// 失败返回 None(调用方保留最近成功 provider);成功返回 provider,
/// 空目录按设计发布空 cursor model 集
pub async fn load_cursor_provider(
    config: &CursorConfig,
    existing: &[ProviderConfig],
    proxy_url: Option<&str>,
) -> Option<ProviderConfig> {
    // 凭证刷新与目录拉取走同一出站代理(Run 路径用全局 proxy,此处对齐)
    let credential = super::ensure_credential_fresh(&config.auth_dir, proxy_url, None)
        .await
        .map_err(|e| {
            tracing::warn!("Cursor 凭证不可用,目录刷新跳过: {e:#}");
            e
        })
        .ok()?;
    let base_url = config
        .base_url
        .clone()
        .unwrap_or_else(|| super::constants::DEFAULT_BASE_URL.to_string());
    let client_version = config
        .client_version
        .clone()
        .unwrap_or_else(|| super::constants::DEFAULT_CLIENT_VERSION.to_string());
    let catalog = fetch_models(
        &base_url,
        &client_version,
        &credential.access_token,
        proxy_url,
    )
    .await
    .map_err(|e| {
        tracing::warn!("Cursor 模型目录拉取失败,保留现有: {e}");
        e
    })
    .ok()?;
    let models = build_cursor_models(&catalog, &config.models, existing);
    let mut metadata = std::collections::HashMap::new();
    metadata.insert("base_url".to_string(), base_url.clone());
    metadata.insert(
        "auth_dir".to_string(),
        config.auth_dir.to_string_lossy().to_string(),
    );
    metadata.insert(
        "credential_id".to_string(),
        super::provider::credential_fingerprint(&credential),
    );
    metadata.insert("client_version".to_string(), client_version);
    Some(synthesize_cursor_provider(models, &base_url, metadata))
}

/// 组装原生运行时(会话表 + 配置);cli 与 reload 共用
pub fn new_cursor_runtime(config: CursorConfig) -> Arc<CursorRuntime> {
    Arc::new(CursorRuntime {
        sessions: CursorSessions::default(),
        config: tokio::sync::RwLock::new(config),
    })
}

#[cfg(test)]
mod tests {
    use super::super::models::CursorModelEntry;
    use super::*;

    fn entry(id: &str) -> CursorModelEntry {
        CursorModelEntry { id: id.to_string() }
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
        let provider = synthesize_cursor_provider(
            build_cursor_models(&[], &[], &[]),
            "https://api2.cursor.sh",
            HashMap::new(),
        );
        assert_eq!(provider.name, "cursor");
        assert_eq!(provider.protocol, Protocol::CursorSdk);
        assert!(provider.models.is_empty());
        assert_eq!(provider.base_urls().len(), 1);
    }

    #[test]
    fn catalog_default_id_keeps_default_model() {
        // 目录实际返回 id "default"(非 auto);归一仅用于白名单匹配,
        // name(上游 model id)必须保持 "default"(上游拒绝 "auto")
        let models = build_cursor_models(
            &[entry("default"), entry("composer-2.5")],
            &["default".into()],
            &[],
        );
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].name, "default");
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
    fn whitelist_pinned_default_base_keeps_default_model() {
        let models = build_cursor_models(
            &[entry("default")],
            &["default:optimize_for=intelligence".to_string()],
            &[],
        );
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].name, "default:optimize_for=intelligence");
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
    fn unmatched_whitelist_base_expands_effort_family() {
        // 目录无裸 grok-4.7,只有变体:家族展开合成单一模型,-fast 不算等级
        let catalog = vec![
            entry("grok-4.7-low"),
            entry("grok-4.7-medium"),
            entry("grok-4.7-high"),
            entry("grok-4.7-high-fast"),
            entry("composer-2.5"),
        ];
        let models = build_cursor_models(&catalog, &["grok-4.7".into()], &[]);
        assert_eq!(models.len(), 1);
        assert_eq!(
            models[0].name,
            "grok-4.7:effort_levels=low+medium+high,effort_default=medium"
        );
        assert_eq!(models[0].alias, "grok-4.7");
    }

    #[test]
    fn exact_id_whitelist_skips_family_expansion() {
        // 目录已有裸 id(auto/composer-2.5 形态):精确路径,不合成家族模型
        let catalog = vec![entry("composer-2.5"), entry("composer-2.5-fast")];
        let models = build_cursor_models(&catalog, &["composer-2.5".into()], &[]);
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].name, "composer-2.5");
    }

    #[test]
    fn glob_hit_whitelist_skips_family_expansion() {
        // glob 命中目录 id 时按 id 逐个物化,不再对条目做家族展开
        let catalog = vec![entry("grok-4.7-low"), entry("grok-4.7-high")];
        let models = build_cursor_models(&catalog, &["grok-*".into()], &[]);
        assert_eq!(models.len(), 2);
        assert_eq!(models[0].name, "grok-4.7-low");
        assert_eq!(models[1].name, "grok-4.7-high");
    }

    #[test]
    fn family_expansion_keeps_pinned_params_in_name() {
        let catalog = vec![entry("grok-4.7-low"), entry("grok-4.7-high")];
        let models = build_cursor_models(
            &catalog,
            &["grok-4.7:reasoning_effort=high".to_string()],
            &[],
        );
        assert_eq!(models.len(), 1);
        assert_eq!(
            models[0].name,
            "grok-4.7:reasoning_effort=high,effort_levels=low+high,effort_default=high"
        );
    }

    #[test]
    fn family_expansion_skips_alias_conflict() {
        let existing = vec![provider("other", vec![("x", "grok-4.7")])];
        let catalog = vec![entry("grok-4.7-low"), entry("grok-4.7-high")];
        let models = build_cursor_models(&catalog, &["grok-4.7".into()], &existing);
        assert!(models.is_empty());
    }

    #[test]
    fn duplicate_whitelist_entries_dedup_by_alias() {
        // 裸 base 与同 base 钉参条目:只物化一个家族模型
        let catalog = vec![entry("grok-4.7-low"), entry("grok-4.7-high")];
        let models = build_cursor_models(
            &catalog,
            &[
                "grok-4.7".to_string(),
                "grok-4.7:reasoning_effort=high".to_string(),
            ],
            &[],
        );
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].alias, "grok-4.7");
    }

    #[test]
    fn family_expansion_requires_level_suffix() {
        // thinking 等非等级后缀不算家族变体
        let catalog = vec![
            entry("claude-4.5-sonnet"),
            entry("claude-4.5-sonnet-thinking"),
        ];
        let models = build_cursor_models(&catalog, &["claude-4.5-sonnet".into()], &[]);
        // 裸 id 精确命中,thinking 变体不参与
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].name, "claude-4.5-sonnet");
    }
}
