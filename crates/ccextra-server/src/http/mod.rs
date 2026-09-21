pub mod auth;
pub mod claude_relay;
pub mod error;
pub mod handlers;
pub mod retry;
pub mod session_tokens;

use axum::{
    routing::{get, post},
    Router,
};
use ccextra_core::cache_stabilization::drift_detector::DriftState;
use ccextra_core::route::{Protocol, ProviderConfig};
use serde_json::Value;
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::upstream::UpstreamClient;
use handlers::count_tokens::handle_count_tokens;
use handlers::messages::handle_messages;
use handlers::models::handle_models;
use handlers::reload::handle_reload;

pub use auth::{check_secret, extract_key, verify_cached};
pub use claude_relay::{
    claude_inbound_user_agent, claude_relay_headers, is_claude_relay_header_excluded,
};
pub use error::{extract_upstream_error, to_anthropic_error, AppError};
pub use retry::{
    compute_retry_delay, jitter_backoff, parse_retry_after, CF_EDGE_MAX_RETRY_BACKOFF,
    RETRY_BASE_DELAY, RETRY_MAX_DELAY, RETRY_TOTAL_BUDGET,
};

/// 配置重载闭包类型
pub type ReloadFn =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = anyhow::Result<ReloadData>> + Send>> + Send + Sync>;

/// /reload 可替换的运行时配置。整块写锁替换,单个字段不单独加锁。
/// 注意 `logging.level` 不生效:EnvFilter 仅启动装载一次(见 cli/main.rs)。
#[derive(Clone)]
pub struct RuntimeConfig {
    pub normalize: NormalizeConfig,
    pub logging: LoggingConfig,
    /// 入口 secret key;Some 时需 x-api-key 匹配
    pub secret: Option<String>,
    /// 上游 HTTP 客户端(封装全局代理)。每次 /reload 无条件重建,
    /// 连接池随之丢弃 —— 低频操作,取舍见 docs/design.md §8。
    pub upstream: UpstreamClient,
    /// User-Agent 字符串(启动或 /reload 时加载)
    pub user_agents: UserAgentSet,
    /// reasoning 级别注册表(启动或 /reload 从用户 models.json 读入;空 = 不钳)
    pub thinking_registry: Arc<Vec<ccextra_core::thinking::ModelCapability>>,
}

/// User-Agent 配置集(启动时从 config 加载,Arc 包装避免请求时 clone)
#[derive(Clone)]
pub struct UserAgentSet {
    pub claude_cli: Arc<String>,
    pub codex_tui: Arc<String>,
    pub grok_version: Arc<String>,
    pub antigravity: Arc<String>,
}

/// 热重载结果:闭包重读配置文件,返回新配置
pub struct ReloadData {
    pub refresh: ProviderRefreshConfig,
    pub providers: Vec<ProviderConfig>,
    pub payload_rules: Vec<PayloadRule>,
    pub normalize: NormalizeConfig,
    pub logging: LoggingConfig,
    pub secret: Option<String>,
    /// 全局代理 URL;"direct"/"" 或 None = 直连
    pub proxy_url: Option<String>,
    /// Antigravity 连接池配置(默认短连接,对齐 CPA connection-pool)
    pub antigravity: Option<crate::upstream::AntigravityConfig>,
    pub user_agents: UserAgentSet,
    pub thinking_registry: Arc<Vec<ccextra_core::thinking::ModelCapability>>,
}

/// 当前生效配置的后台刷新输入，目录在 CLI 中解析为相对配置文件的路径。
#[derive(Clone, Default)]
pub struct ProviderRefreshConfig {
    pub auth_dir: Option<std::path::PathBuf>,
    pub xai_auth_dir: Option<std::path::PathBuf>,
    pub proxy_url: Option<String>,
    pub static_providers: Vec<ProviderConfig>,
}

/// 统一不可变配置快照(对齐阶段 D 规格)
#[derive(Clone)]
pub struct ConfigSnapshot {
    pub version: u64,
    pub providers: Vec<ProviderConfig>,
    pub payload_rules: Vec<PayloadRule>,
    pub runtime: RuntimeConfig,
    pub refresh: ProviderRefreshConfig,
}

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<RwLock<Arc<ConfigSnapshot>>>,
    /// 串行化 reload 加载和发布，不阻塞请求读取快照。
    pub reload_lock: Arc<tokio::sync::Mutex<()>>,
    /// 重读配置文件的闭包(由 cli 构造,捕获 config 路径)
    pub reload: ReloadFn,
    /// drift 观测状态(会话 → 上次结构哈希;按 openai/anthropic handler 分桶)
    pub drift: DriftState,
    /// reasoning replay 缓存(会话 → 上一轮 replay 项;responses+grok 用,
    /// 对齐 CPA xai reasoning replay;server 层持有,core 无 IO)
    pub replay_cache: crate::sse::replay_cache::ReplayCache,
    /// session_id → 最新 input_tokens(避免非 Claude 上游 count_tokens 估算不准导致 context 跳动)
    pub last_input_tokens: Arc<std::sync::Mutex<session_tokens::SessionTokenCache>>,
}

#[derive(Clone, Debug, serde::Deserialize)]
pub struct PayloadRule {
    pub models: Vec<String>,
    /// 限定生效的目标协议;缺省 = 所有协议(参照 payload 的 protocol 字段)
    #[serde(default)]
    pub protocol: Option<Protocol>,
    pub params: serde_json::Map<String, Value>,
}

#[derive(Clone, Debug, serde::Deserialize)]
pub struct NormalizeConfig {
    pub enabled: bool,
    pub drift_detector: bool,
}

#[derive(Clone, Debug, serde::Deserialize)]
pub struct LoggingConfig {
    pub level: String,
    pub request_body: bool,
}

async fn health_check() -> &'static str {
    "ok"
}

pub async fn publish_refreshed_providers(
    config: &Arc<RwLock<Arc<ConfigSnapshot>>>,
    expected_version: u64,
    providers: Vec<ProviderConfig>,
) -> anyhow::Result<bool> {
    ccextra_core::route::validate_providers(&providers)?;
    let mut guard = config.write().await;
    if guard.version != expected_version || guard.providers == providers {
        return Ok(false);
    }
    let next_version = guard.version.wrapping_add(1);
    *guard = Arc::new(ConfigSnapshot {
        version: next_version,
        providers,
        payload_rules: guard.payload_rules.clone(),
        runtime: guard.runtime.clone(),
        refresh: guard.refresh.clone(),
    });
    Ok(true)
}

pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/v1/messages", post(handle_messages))
        .route("/v1/messages/count_tokens", post(handle_count_tokens))
        .route("/v1/models", get(handle_models))
        .route("/health", axum::routing::get(health_check))
        .route("/reload", post(handle_reload))
        .with_state(state)
}

pub async fn serve(addr: &str, state: AppState) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("ccextra listening on {}", addr);

    axum::serve(listener, app(state)).await?;
    Ok(())
}

#[cfg(test)]
mod tests;
