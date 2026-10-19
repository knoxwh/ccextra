use crate::http::auth::auth_cache;
use crate::http::error::AppError;
use crate::http::{AppState, RuntimeConfig};
use crate::upstream::UpstreamClient;
use axum::extract::State;
use axum::Json;
use ccextra_core::route::validate_providers;
use serde::{Deserialize, Serialize};

/// 热重载结果:收集所有成功与失败项(对齐 magpie syncProjects 错误收集模式)
#[derive(Debug, Default, Serialize, Deserialize)]
pub struct ReloadResult {
    /// 成功重载的组件
    pub reloaded: Vec<String>,
    /// 失败的组件及原因
    pub failed: Vec<ReloadError>,
}

#[derive(Debug, Serialize, Deserialize)]
pub struct ReloadError {
    pub component: String,
    pub reason: String,
}

/// 热重载按取得专用互斥锁的顺序加载并发布；失败收集到 result.failed 但继续处理其他组件。
/// 加载期间不持有配置锁，请求和后台刷新仍可访问当前快照。
pub async fn handle_reload(State(state): State<AppState>) -> Result<Json<ReloadResult>, AppError> {
    let _reload_guard = state.reload_lock.lock().await;
    let mut result = ReloadResult::default();

    // 1. 读取配置文件(失败则快速失败，无法继续)
    let data = match (state.reload)().await {
        Ok(d) => d,
        Err(e) => return Err(AppError::new(anyhow::anyhow!("配置文件读取失败: {e}"))),
    };

    // 2. 验证 providers(失败记录但尝试继续构建 runtime)
    let providers_valid = match validate_providers(&data.providers) {
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

    // 3. 构建 runtime 配置(失败则整体回滚，不更新任何配置)
    let runtime = match build_runtime_config(&data) {
        Ok(r) => {
            result.reloaded.push("runtime".into());
            r
        }
        Err(e) => {
            result.failed.push(ReloadError {
                component: "runtime".into(),
                reason: e.to_string(),
            });
            tracing::warn!("runtime 配置构建失败，回滚整个 reload: {e}");
            return Ok(Json(result));
        }
    };

    // providers 校验失败但 runtime 成功时，仍不更新配置
    if !providers_valid {
        tracing::warn!("providers 校验失败，不更新配置");
        return Ok(Json(result));
    }

    // 4. 全局原子更新统一不可变快照并递增版本号
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
    }

    // 5. 清认证缓存(失败无碍)
    if auth_cache().lock().ok().map(|mut c| c.clear()).is_some() {
        result.reloaded.push("auth_cache".into());
    } else {
        result.failed.push(ReloadError {
            component: "auth_cache".into(),
            reason: "lock failed".into(),
        });
    }

    // 6. 清空 unfit 注册表(旧 provider/model 标记不得残留)
    crate::http::unfit::global_registry().clear();
    result.reloaded.push("unfit_registry".into());

    tracing::info!(
        "配置热重载完成: {} 成功, {} 失败",
        result.reloaded.len(),
        result.failed.len()
    );
    Ok(Json(result))
}

/// 构建 runtime 配置(独立函数便于错误处理)
fn build_runtime_config(data: &crate::http::ReloadData) -> anyhow::Result<RuntimeConfig> {
    Ok(RuntimeConfig {
        normalize: data.normalize.clone(),
        logging: data.logging.clone(),
        secret: data.secret.clone(),
        upstream: UpstreamClient::with_ant_pool(data.proxy_url.clone(), data.antigravity.as_ref()),
        user_agents: data.user_agents.clone(),
        thinking_registry: data.thinking_registry.clone(),
    })
}
