pub mod auth;
pub mod claude_relay;
pub mod error;
pub mod handlers;
pub mod retry;
pub mod session_tokens;
pub mod unfit;

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
    claude_inbound_user_agent, claude_passthrough_headers, ensure_claude_accept_encoding,
    ensure_oauth_beta, is_claude_passthrough_header_excluded, CLAUDE_ACCEPT_ENCODING,
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
/// 注意 `logging.level` 不生效:EnvFilter 仅启动装载一次(见 crates/ccextra-cli/src/main.rs)。
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
    pub codex_auth_dir: Option<std::path::PathBuf>,
    pub claude_auth_dir: Option<std::path::PathBuf>,
    /// Antigravity 模型白名单;空 = 全量
    pub antigravity_models: Option<Vec<String>>,
    /// Antigravity hub UA（与 user_agents.antigravity / REQUEST_UA 同源）
    pub antigravity_user_agent: Option<String>,
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
    /// Antigravity thoughtSignature 账本(与 Responses ReplayCache 分离)
    pub ag_replay_cache: crate::sse::antigravity_replay_cache::AgReplayCache,
    /// Kimi thinking 回放(protocol:claude Messages；与 AG/Responses 分离)
    pub kimi_thinking_replay_cache: crate::sse::kimi_thinking_replay_cache::KimiThinkingReplayCache,
    /// session_id → 最新 input_tokens(避免非 Claude 上游 count_tokens 估算不准导致 context 跳动)
    pub last_input_tokens: Arc<std::sync::Mutex<session_tokens::SessionTokenCache>>,
}

/// 预编译的模型匹配器(`*` 或 glob)
#[derive(Clone, Debug)]
enum ModelMatcher {
    All,
    Glob(globset::GlobMatcher),
}

impl ModelMatcher {
    fn is_match(&self, model: &str) -> bool {
        match self {
            Self::All => true,
            Self::Glob(g) => g.is_match(model),
        }
    }
}

#[derive(Clone, Debug, serde::Deserialize)]
pub struct PayloadRule {
    pub models: Vec<String>,
    /// 限定生效的目标协议;缺省 = 所有协议(参照 payload 的 protocol 字段)
    #[serde(default)]
    pub protocol: Option<Protocol>,
    pub params: serde_json::Map<String, Value>,
    /// 配置加载/reload 时编译;热路径只做 is_match
    #[serde(skip)]
    matchers: Vec<ModelMatcher>,
}

impl PayloadRule {
    /// 编译 models 中的 glob;`*` 为全匹配。非法 pattern 返回 Err(不静默忽略)。
    pub fn compile(&mut self) -> Result<(), String> {
        let mut matchers = Vec::with_capacity(self.models.len());
        for pat in &self.models {
            if pat == "*" {
                matchers.push(ModelMatcher::All);
                continue;
            }
            let glob = globset::Glob::new(pat)
                .map_err(|e| format!("无效 payload 模型模式 '{pat}': {e}"))?;
            matchers.push(ModelMatcher::Glob(glob.compile_matcher()));
        }
        self.matchers = matchers;
        Ok(())
    }

    /// 批量编译;任一失败则整批失败
    pub fn compile_all(rules: &mut [PayloadRule]) -> Result<(), String> {
        for rule in rules.iter_mut() {
            rule.compile()?;
        }
        Ok(())
    }

    pub(crate) fn matches_model(&self, model: &str) -> bool {
        debug_assert!(
            !self.matchers.is_empty() || self.models.is_empty(),
            "payload rule 未 compile"
        );
        self.matchers.iter().any(|m| m.is_match(model))
    }

    /// 测试/内联构造后立即编译
    #[cfg(test)]
    pub fn compiled(mut self) -> Self {
        self.compile().unwrap_or_else(|e| panic!("{e}"));
        self
    }
}

#[derive(Clone, Debug, serde::Deserialize)]
pub struct NormalizeConfig {
    pub enabled: bool,
    pub drift_detector: bool,
    /// 可选：drift 额外观测 tools description 软维（默认 false，热路径成本≈现状）。
    /// 仅会话级 DEBUG，永不进入父级 WARN。
    #[serde(default)]
    pub drift_tools_description: bool,
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
    serve_with_shutdown(addr, state, shutdown_signal()).await
}

pub(crate) async fn serve_with_shutdown<F>(
    addr: &str,
    state: AppState,
    shutdown: F,
) -> anyhow::Result<()>
where
    F: Future<Output = ()> + Send + 'static,
{
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("ccextra listening on {}", addr);

    let result = axum::serve(listener, app(state))
        .with_graceful_shutdown(shutdown)
        .await;
    result?;
    Ok(())
}

async fn shutdown_signal() {
    #[cfg(unix)]
    {
        let mut terminate =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
                .expect("install SIGTERM handler");
        let reason = tokio::select! {
            _ = tokio::signal::ctrl_c() => "SIGINT",
            _ = terminate.recv() => "SIGTERM",
        };
        tracing::info!("ccextra shutting down ({reason})");
    }
    #[cfg(not(unix))]
    {
        let _ = tokio::signal::ctrl_c().await;
        tracing::info!("ccextra shutting down (SIGINT)");
    }
}

#[cfg(test)]
mod tests;
