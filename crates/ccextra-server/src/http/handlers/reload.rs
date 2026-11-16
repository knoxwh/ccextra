use crate::http::auth::auth_cache;
use crate::http::error::AppError;
use crate::http::{AppState, RuntimeConfig};
use crate::upstream::UpstreamClient;
use axum::Json;
use axum::extract::State;
use ccextra_core::route::{
    Protocol, ProviderConfig, validate_provider_proxies, validate_providers,
};
use serde::{Deserialize, Serialize};

/// 热重载结果:收集所有成功与失败项(对齐 magpie syncProjects 错误收集模式)
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ReloadResult {
    /// 成功重载的组件
    pub reloaded: Vec<String>,
    /// 失败的组件及原因
    pub failed: Vec<ReloadError>,
    /// 是否已写入统一配置快照（providers/runtime 回滚或加载 Err 均为 false）
    #[serde(default)]
    pub published: bool,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ReloadError {
    pub component: String,
    pub reason: String,
}

/// 热重载核心路径（HTTP 与文件监视共用）。
/// 按专用互斥锁串行加载并发布；失败收集到 result.failed；加载期间不持有配置读锁。
pub async fn reload_now(state: &AppState) -> Result<ReloadResult, AppError> {
    let _reload_guard = state.reload_lock.lock().await;
    let mut result = ReloadResult::default();

    // 1. 读取配置文件(失败则快速失败，无法继续)
    let mut data = match (state.reload)().await {
        Ok(d) => d,
        Err(e) => return Err(AppError::new(anyhow::anyhow!("配置文件读取失败: {e}"))),
    };
    if data.antigravity_load_failed {
        let current = state.config.read().await.clone();
        carry_over_antigravity(
            &mut data.providers,
            &current.providers,
            data.proxy_url.as_deref(),
        );
    }

    // 2. 验证 providers 与生效代理(失败记录但尝试继续构建 runtime;不发布,保留旧配置)
    let providers_valid = match validate_providers(&data.providers)
        .and_then(|_| validate_provider_proxies(&data.providers, data.proxy_url.as_deref()))
        .map_err(anyhow::Error::from)
        .and_then(|_| data.refresh.validate_oauth_proxy())
    {
        Ok(_) => {
            result.reloaded.push("providers".into());
            true
        }
        Err(e) => {
            result.failed.push(ReloadError {
                component: "providers".into(),
                reason: format!("校验失败: {e}"),
            });
            false
        }
    };

    // 3. 构建 runtime 配置(纯字段拷贝，不会失败)
    let runtime = build_runtime_config(&data);
    result.reloaded.push("runtime".into());

    // providers 校验失败但 runtime 成功时，仍不更新配置
    if !providers_valid {
        tracing::warn!("providers 校验失败，不更新配置");
        return Ok(result);
    }

    // 4. 全局原子更新统一不可变快照并递增版本号;
    //    同写锁内 clear unfit,与 handle_messages 同读锁捕获的 (snapshot, generation) 成对
    {
        let mut cfg_lock = state.config.write().await;
        let next_version = cfg_lock.version.wrapping_add(1);
        *cfg_lock = std::sync::Arc::new(crate::http::ConfigSnapshot {
            version: next_version,
            providers: data.providers,
            payload_rules: data.payload_rules,
            runtime,
            refresh: data.refresh.clone(),
        });
        // 清空 unfit 并增代(旧标记与持旧世代的飞行 mark 均失效)
        crate::http::unfit::global_registry().clear();
    }
    result.reloaded.push("unfit_registry".into());
    result.published = true;

    // 5. 清认证缓存(失败无碍)
    if auth_cache().lock().ok().map(|mut c| c.clear()).is_some() {
        result.reloaded.push("auth_cache".into());
    } else {
        result.failed.push(ReloadError {
            component: "auth_cache".into(),
            reason: "lock failed".into(),
        });
    }

    tracing::info!(
        "配置热重载完成: {} 成功, {} 失败",
        result.reloaded.len(),
        result.failed.len()
    );
    Ok(result)
}

/// AG 拉取失败时沿用当前快照中动态注入的 Antigravity providers(对齐 CPA 失败保旧)。
/// - 只取带 `credential_path` 的注入项;静态配置的 antigravity provider 已在新配置里,不重复沿用
/// - 追加在末尾、alias 已被占用的模型丢弃:与正常加载 AG 最后合并(merge_providers)一致
/// - proxy_url 换成新配置的全局代理(注入时同源写入,见 antigravity::provider)
///
/// 返回沿用的 provider 数。
fn carry_over_antigravity(
    providers: &mut Vec<ProviderConfig>,
    current: &[ProviderConfig],
    proxy_url: Option<&str>,
) -> usize {
    let mut seen: std::collections::HashSet<String> = providers
        .iter()
        .flat_map(|p| p.models.iter().map(|m| m.alias.clone()))
        .collect();
    let mut carried = 0usize;
    for p in current.iter().filter(|p| is_injected_antigravity(p)) {
        let mut p = p.clone();
        p.proxy_url = proxy_url.map(str::to_string);
        p.models.retain(|m| seen.insert(m.alias.clone()));
        providers.push(p);
        carried += 1;
    }
    if carried > 0 {
        tracing::warn!(carried, "Antigravity 拉取失败,沿用当前快照的 AG providers");
    } else {
        tracing::info!("Antigravity 拉取失败,当前快照无已注入的 AG providers 可沿用");
    }
    carried
}

fn is_injected_antigravity(p: &ProviderConfig) -> bool {
    p.protocol == Protocol::Antigravity
        && p.metadata
            .as_ref()
            .is_some_and(|m| m.contains_key("credential_path"))
}

/// HTTP `POST /reload`：委托 [`reload_now`]。
pub async fn handle_reload(State(state): State<AppState>) -> Result<Json<ReloadResult>, AppError> {
    Ok(Json(reload_now(&state).await?))
}

/// 构建 runtime 配置
fn build_runtime_config(data: &crate::http::ReloadData) -> RuntimeConfig {
    RuntimeConfig {
        normalize: data.normalize.clone(),
        logging: data.logging.clone(),
        secret: data.secret.clone(),
        upstream: UpstreamClient::with_ant_pool(data.proxy_url.clone(), data.antigravity.as_ref()),
        user_agents: data.user_agents.clone(),
        thinking_registry: data.thinking_registry.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ccextra_core::route::ModelConfig;

    fn model(alias: &str) -> ModelConfig {
        ModelConfig {
            name: alias.into(),
            alias: alias.into(),
            max_input_tokens: None,
            max_tokens: None,
        }
    }

    fn provider(name: &str, protocol: Protocol, aliases: &[&str]) -> ProviderConfig {
        ProviderConfig::new(
            name.into(),
            protocol,
            vec!["https://example.invalid".into()],
            "fixture-key".into(),
            Some("http://old-proxy.invalid:8080".into()),
            false,
            aliases.iter().map(|a| model(a)).collect(),
        )
    }

    fn injected_ag(name: &str, aliases: &[&str]) -> ProviderConfig {
        provider(name, Protocol::Antigravity, aliases).with_metadata(
            [("credential_path".to_string(), format!("/tmp/{name}.json"))]
                .into_iter()
                .collect(),
        )
    }

    /// 沿用的 AG 用新配置的全局代理,不带旧 proxy_url
    #[test]
    fn carried_antigravity_uses_new_proxy_url() {
        let mut providers = vec![];
        let current = vec![injected_ag("ag-1", &["g1"])];
        assert_eq!(
            carry_over_antigravity(&mut providers, &current, Some("direct")),
            1
        );
        assert_eq!(providers[0].proxy_url.as_deref(), Some("direct"));
    }

    /// alias 冲突时新加载的 providers 优先,与正常加载 AG 最后合并一致
    #[test]
    fn carried_antigravity_loses_alias_conflict_like_normal_merge() {
        let mut providers = vec![provider("claude-sub", Protocol::Claude, &["shared"])];
        let current = vec![injected_ag("ag-1", &["shared", "g1"])];
        carry_over_antigravity(&mut providers, &current, Some("direct"));
        assert_eq!(providers.len(), 2);
        assert_eq!(providers[0].models[0].alias, "shared");
        let aliases: Vec<_> = providers[1]
            .models
            .iter()
            .map(|m| m.alias.as_str())
            .collect();
        assert_eq!(aliases, vec!["g1"]);
    }

    /// 静态配置的 antigravity provider 不沿用(新配置自带;删掉的不能复活)
    #[test]
    fn static_antigravity_is_not_carried() {
        let mut providers = vec![];
        let current = vec![
            provider("static-ag", Protocol::Antigravity, &["s1"]),
            injected_ag("ag-1", &["g1"]),
        ];
        assert_eq!(
            carry_over_antigravity(&mut providers, &current, Some("direct")),
            1
        );
        assert_eq!(providers[0].name, "ag-1");
    }

    /// 当前快照没有已注入的 AG:不改 providers,返回 0(不打 WARN)
    #[test]
    fn nothing_carried_without_injected_antigravity() {
        let mut providers = vec![provider("claude-sub", Protocol::Claude, &["c1"])];
        let current = vec![provider("claude-old", Protocol::Claude, &["c0"])];
        assert_eq!(
            carry_over_antigravity(&mut providers, &current, Some("direct")),
            0
        );
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].name, "claude-sub");
    }
}
