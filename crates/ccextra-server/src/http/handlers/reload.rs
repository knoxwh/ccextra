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
    let providers = data.providers;
    validate_providers(&providers)
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
            providers,
            payload_rules: data.payload_rules,
            runtime,
            refresh: data.refresh.clone(),
        });
    }
    // Cursor sidecar 生命周期对齐新配置:禁用变启用启动、启用变禁用停止、
    // 目录变化受控重启,其余字段原子更新;catalog 重新合成后按版本保护发布
    reconcile_cursor_runtime(&state, data.refresh.cursor).await;
    // secret 可能变更,旧 bcrypt 校验结果一律作废(不比较新旧值)
    if let Ok(mut cache) = auth_cache().lock() {
        cache.clear();
    }
    tracing::info!("配置热重载完成");
    Ok("reloaded")
}

/// Cursor 运行时与新配置对齐
///
/// - (None, None):无操作
/// - (None, Some):启动 sidecar + 拉取 catalog 合成 provider
/// - (Some, None):停止 sidecar(cursor provider 已不在新 providers)
/// - (Some, Some):auth_dir 变化受控重启;配置原子更新;catalog 刷新
pub(crate) async fn reconcile_cursor_runtime(
    state: &AppState,
    desired: Option<crate::cursor::CursorConfig>,
) {
    use crate::cursor::CursorSidecarConfig;

    let current = state.cursor.read().ok().and_then(|guard| guard.clone());
    match (current, desired) {
        (None, None) => {}
        (None, Some(config)) => match spawn_cursor_runtime(&config).await {
            Ok(runtime) => {
                if let Ok(mut guard) = state.cursor.write() {
                    *guard = Some(runtime);
                }
                refresh_cursor_provider(state, &config).await;
            }
            Err(error) => {
                tracing::warn!("Cursor sidecar 启动失败,保持未启用: {error:#}")
            }
        },
        (Some(runtime), None) => {
            runtime.sidecar.shutdown().await;
            if let Ok(mut guard) = state.cursor.write() {
                *guard = None;
            }
            tracing::info!("Cursor sidecar 已停止");
        }
        (Some(runtime), Some(config)) => {
            let dir_changed = {
                let current = runtime.config.read().await;
                current.auth_dir != config.auth_dir
            };
            if dir_changed {
                let sidecar_config = CursorSidecarConfig {
                    sidecar_dir: sidecar_dir_or_default(),
                    auth_dir: config.auth_dir.clone(),
                    ..Default::default()
                };
                if let Err(error) = runtime.sidecar.reconfigure(sidecar_config).await {
                    tracing::warn!("Cursor sidecar 重启失败,保留旧进程: {error:#}");
                }
            }
            *runtime.config.write().await = config.clone();
            refresh_cursor_provider(state, &config).await;
        }
    }
}

/// 启动 sidecar 并组装运行时
async fn spawn_cursor_runtime(
    config: &crate::cursor::CursorConfig,
) -> anyhow::Result<std::sync::Arc<crate::cursor::CursorRuntime>> {
    use crate::cursor::{CursorSidecar, CursorSidecarConfig};

    let sidecar = CursorSidecar::start(CursorSidecarConfig {
        sidecar_dir: sidecar_dir_or_default(),
        auth_dir: config.auth_dir.clone(),
        ..Default::default()
    })
    .await?;
    Ok(std::sync::Arc::new(crate::cursor::CursorRuntime {
        sidecar,
        config: tokio::sync::RwLock::new(config.clone()),
    }))
}

/// sidecar 目录:exe 同级优先,回退仓库根
fn sidecar_dir_or_default() -> std::path::PathBuf {
    let exe = std::env::current_exe().unwrap_or_else(|_| std::path::PathBuf::from("."));
    let repository_root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    crate::cursor::resolve_sidecar_dir(&exe, &repository_root).unwrap_or_else(|error| {
        tracing::warn!("Cursor sidecar 目录定位失败: {error:#}");
        std::path::PathBuf::from("sidecar/cursor")
    })
}

/// 拉取 cursor catalog 并按版本保护发布(失败保留现有 provider)
async fn refresh_cursor_provider(state: &AppState, config: &crate::cursor::CursorConfig) {
    let Some(runtime) = state.cursor.read().ok().and_then(|guard| guard.clone()) else {
        return;
    };
    let snapshot = state.config.read().await.clone();
    // 旧 cursor provider 不算冲突源:其模型集即将被新 catalog 整体替换
    let existing: Vec<_> = snapshot
        .providers
        .iter()
        .filter(|p| p.name != "cursor")
        .cloned()
        .collect();
    let Some(provider) =
        crate::cursor::load_cursor_provider(&runtime.sidecar, config, &existing).await
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
