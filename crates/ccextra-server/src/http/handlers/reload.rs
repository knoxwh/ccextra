use crate::http::auth::auth_cache;
use crate::http::error::AppError;
use crate::http::{AppState, RuntimeConfig};
use crate::upstream::UpstreamClient;
use axum::extract::State;
use ccextra_core::route::validate_providers;

/// 热重载按取得专用互斥锁的顺序加载并发布；失败保留当前快照及版本。
/// 加载期间不持有配置锁，请求和后台刷新仍可访问当前快照。
pub async fn handle_reload(State(state): State<AppState>) -> Result<&'static str, AppError> {
    let _reload_guard = state.reload_lock.lock().await;
    let data = (state.reload)()
        .await
        .map_err(|e| AppError::new(anyhow::anyhow!("重读配置失败: {e}")))?;
    validate_providers(&data.providers)
        .map_err(|e| AppError::new(anyhow::anyhow!("配置校验失败: {e}")))?;

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
            providers: data.providers,
            payload_rules: data.payload_rules,
            runtime,
            refresh: data.refresh,
        });
    }

    // secret 可能变更,旧 bcrypt 校验结果一律作废(不比较新旧值)
    if let Ok(mut cache) = auth_cache().lock() {
        cache.clear();
    }
    tracing::info!("配置热重载完成");
    Ok("reloaded")
}
