use crate::cursor::{provider as cursor_provider, store as cursor_store};
use crate::http::auth::auth_cache;
use crate::http::error::AppError;
use crate::http::{AppState, RuntimeConfig};
use crate::upstream::UpstreamClient;
use axum::extract::State;
use ccextra_core::route::validate_providers;
use std::collections::HashSet;

/// 热重载按取得专用互斥锁的顺序加载并发布；失败保留当前快照及版本。
/// 加载期间不持有配置锁，请求和后台刷新仍可访问当前快照。
pub async fn handle_reload(State(state): State<AppState>) -> Result<&'static str, AppError> {
    let _reload_guard = state.reload_lock.lock().await;
    let data = (state.reload)()
        .await
        .map_err(|e| AppError::new(anyhow::anyhow!("重读配置失败: {e}")))?;
    let mut providers = data.providers;
    // Cursor 拉取失败时仅保留凭证目录和身份未变的旧 provider；新配置别名优先。
    // 新集合已有 Cursor(静态配置)或旧快照没有则不携带。
    if data.cursor_load_failed
        && !providers
            .iter()
            .any(|provider| provider.protocol == ccextra_core::route::Protocol::Cursor)
    {
        let carried = {
            let current = state.config.read().await;
            current
                .providers
                .iter()
                .find(|provider| provider.protocol == ccextra_core::route::Protocol::Cursor)
                .cloned()
        };
        if let Some(mut old) = carried {
            let same_credential = data.refresh.cursor_auth_dir.as_ref().is_some_and(|dir| {
                old.metadata.as_ref().is_some_and(|metadata| {
                    metadata
                        .get("auth_dir")
                        .is_some_and(|path| std::path::Path::new(path) == dir.as_path())
                        && cursor_store::load(dir).ok().is_some_and(|credential| {
                            metadata.get("credential_id").is_some_and(|identity| {
                                identity == &cursor_provider::credential_fingerprint(&credential)
                            })
                        })
                })
            });
            if same_credential {
                let aliases: HashSet<&str> = providers
                    .iter()
                    .flat_map(|provider| provider.models.iter().map(|model| model.alias.as_str()))
                    .collect();
                old.models
                    .retain(|model| !aliases.contains(model.alias.as_str()));
                if !old.models.is_empty() {
                    tracing::warn!("Cursor 目录拉取失败,保留已发布 Cursor provider");
                    providers.push(old);
                }
            }
        }
    }
    validate_providers(&providers)
        .map_err(|e| AppError::new(anyhow::anyhow!("配置校验失败: {e}")))?;
    let cursor_identity = providers
        .iter()
        .find(|provider| provider.protocol == ccextra_core::route::Protocol::Cursor)
        .and_then(|provider| provider.metadata.as_ref()?.get("credential_id").cloned());

    let runtime = RuntimeConfig {
        normalize: data.normalize,
        logging: data.logging,
        secret: data.secret,
        upstream: UpstreamClient::with_ant_pool(data.proxy_url, data.antigravity.as_ref()),
        user_agents: data.user_agents,
        thinking_registry: data.thinking_registry,
    };

    // 全局原子更新统一不可变快照并递增版本号
    {
        let mut cfg_lock = state.config.write().await;
        let next_version = cfg_lock.version.wrapping_add(1);
        *cfg_lock = std::sync::Arc::new(crate::http::ConfigSnapshot {
            version: next_version,
            providers,
            payload_rules: data.payload_rules,
            runtime,
            refresh: data.refresh,
        });
    }
    state
        .cursor_sessions
        .retain_identity(cursor_identity.as_deref());

    // secret 可能变更,旧 bcrypt 校验结果一律作废(不比较新旧值)
    if let Ok(mut cache) = auth_cache().lock() {
        cache.clear();
    }
    tracing::info!("配置热重载完成");
    Ok("reloaded")
}
