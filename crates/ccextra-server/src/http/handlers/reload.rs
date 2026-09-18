use crate::http::auth::auth_cache;
use crate::http::error::AppError;
use crate::http::{AppState, RuntimeConfig};
use crate::upstream::UpstreamClient;
use axum::extract::State;
use ccextra_core::route::validate_providers;

/// 热重载:重读配置文件,校验后更新 providers / payload / 运行时配置。
///
/// 三把独立写锁分别获取,非全局原子 —— 期间并发请求可能见到部分更新
/// (如新 providers 配旧 normalize)。热重载低频,取舍见 docs/design.md §8。
pub async fn handle_reload(State(state): State<AppState>) -> Result<&'static str, AppError> {
    let data = (state.reload)()
        .await
        .map_err(|e| AppError::new(anyhow::anyhow!("重读配置失败: {e}")))?;
    validate_providers(&data.providers)
        .map_err(|e| AppError::new(anyhow::anyhow!("配置校验失败: {e}")))?;
    *state.providers.write().await = data.providers;
    *state.payload_rules.write().await = data.payload_rules;
    *state.runtime.write().await = RuntimeConfig {
        normalize: data.normalize,
        logging: data.logging,
        secret: data.secret,
        upstream: UpstreamClient::with_ant_pool(data.proxy_url, data.antigravity.as_ref()),
        user_agents: data.user_agents,
        thinking_registry: data.thinking_registry,
    };
    // secret 可能变更,旧 bcrypt 校验结果一律作废(不比较新旧值)
    if let Ok(mut cache) = auth_cache().lock() {
        cache.clear();
    }
    tracing::info!("配置热重载完成");
    Ok("reloaded")
}
