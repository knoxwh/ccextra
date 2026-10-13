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

    // 5. Cursor 运行时对齐新配置(失败记录但不阻塞主流程)
    match reconcile_cursor_runtime(&state, data.refresh.cursor).await {
        Ok(_) => result.reloaded.push("cursor".into()),
        Err(e) => result.failed.push(ReloadError {
            component: "cursor".into(),
            reason: e.to_string(),
        }),
    }

    // 6. 清认证缓存(失败无碍)
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

/// Cursor 运行时与新配置对齐
///
/// - (None, None):无操作
/// - (None, Some):组装运行时 + 拉取 catalog 合成 provider
/// - (Some, None):停用(cursor provider 已不在新 providers)
/// - (Some, Some):配置原子更新;catalog 刷新
pub(crate) async fn reconcile_cursor_runtime(
    state: &AppState,
    desired: Option<crate::cursor::CursorConfig>,
) -> anyhow::Result<()> {
    let current = state.cursor.read().ok().and_then(|guard| guard.clone());
    match (current, desired) {
        (None, None) => Ok(()),
        (None, Some(config)) => {
            let runtime = crate::cursor::new_cursor_runtime(config.clone());
            if let Ok(mut guard) = state.cursor.write() {
                *guard = Some(runtime);
            }
            refresh_cursor_provider(state, &config).await;
            Ok(())
        }
        (Some(_runtime), None) => {
            if let Ok(mut guard) = state.cursor.write() {
                *guard = None;
            }
            tracing::info!("Cursor 原生路径已停用");
            Ok(())
        }
        (Some(runtime), Some(config)) => {
            *runtime.config.write().await = config.clone();
            refresh_cursor_provider(state, &config).await;
            Ok(())
        }
    }
}

/// 拉取 cursor catalog 并按版本保护发布(失败保留现有 provider)
async fn refresh_cursor_provider(state: &AppState, config: &crate::cursor::CursorConfig) {
    let snapshot = state.config.read().await.clone();
    // 旧 cursor provider 不算冲突源:其模型集即将被新 catalog 整体替换
    let existing: Vec<_> = snapshot
        .providers
        .iter()
        .filter(|p| p.name != "cursor")
        .cloned()
        .collect();
    let proxy_url = snapshot.runtime.upstream.global_proxy().map(str::to_string);
    let Some(provider) =
        crate::cursor::load_cursor_provider(config, &existing, proxy_url.as_deref()).await
    else {
        return;
    };
    let mut providers = snapshot
        .providers
        .iter()
        .filter(|p| p.name != "cursor")
        .cloned()
        .collect::<Vec<_>>();
    providers.push(provider);
    match crate::http::publish_refreshed_providers(&state.config, snapshot.version, providers).await
    {
        Ok(true) => tracing::info!("Cursor catalog 已随 reload 更新"),
        Ok(false) => {}
        Err(error) => tracing::warn!("Cursor catalog 发布校验失败,保持现有: {error:#}"),
    }
}
