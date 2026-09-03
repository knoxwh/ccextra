// HTTP 服务入口:axum /v1/messages
//
// 完整管线:
// 1. 解析入站 anthropic body + 入口认证
// 2. 路由决策 model → provider → protocol
// 3. 归一化第一遍(claude 全量 / 其余协议精简子集)
// 4. 协议转换(claude 直通 + 四条 body-to-body)
// 5. normalize_target_post(仅 openai 转换路径)+ drift 观测
// 6. payload 参数覆盖
// 7. prompt_cache_key 注入(仅 openai)+ 诊断落盘
// 8. 上游请求(多 base_url 按序回退)
// 9. 响应转发(直通字节 / 流式 SSE 状态机)

use axum::{
    body::{to_bytes, Body},
    extract::State,
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use bytes::Bytes;
use ccextra_core::cache_stabilization::drift_detector::derive_session_key as drift_derive_session_key;
use ccextra_core::cache_stabilization::drift_detector::{
    compute_structural_hash, is_ancillary_request, observe_drift, ApiKind as DriftApiKind,
    DriftState,
};
use ccextra_core::convert::{
    convert_passthrough, convert_to_openai_chat, convert_to_openai_responses,
    is_thinking_signature_invalid, sanitize_gpt_reasoning_items, trim_encrypted_reasoning_items,
    ConvertError,
};
use ccextra_core::normalize::{
    normalize_anthropic_full, normalize_anthropic_pretransform, normalize_target_post, TargetShape,
};
use ccextra_core::prompt_cache::inject_prompt_cache_key;
use ccextra_core::route::{
    resolve_route, validate_providers, Protocol, ProviderConfig, RouteError,
};
use ccextra_core::secret::looks_like_bcrypt;
use ccextra_core::session::{extract_claude_code_session, extract_claude_code_thread};
use futures::StreamExt;
use globset::Glob;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use tokio::sync::RwLock;

use crate::sse::replay_cache::StreamReplayExtractor;
use crate::sse::SseStreamPin;
use crate::upstream::{is_gpt_model, is_grok_model, UpstreamClient, UpstreamResponse};

/// 配置重载闭包类型
pub type ReloadFn =
    Arc<dyn Fn() -> Pin<Box<dyn Future<Output = anyhow::Result<ReloadData>> + Send>> + Send + Sync>;

/// /reload 可替换的运行时配置。整块写锁替换,单个字段不单独加锁。
/// 注意 `logging.level` 不生效:EnvFilter 仅启动装载一次(见 cli/main.rs)。
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
    pub providers: Vec<ProviderConfig>,
    pub payload_rules: Vec<PayloadRule>,
    pub normalize: NormalizeConfig,
    pub logging: LoggingConfig,
    pub secret: Option<String>,
    /// 全局代理 URL;"direct"/"" 或 None = 直连
    pub proxy_url: Option<String>,
    pub user_agents: UserAgentSet,
}

#[derive(Clone)]
pub struct AppState {
    pub providers: Arc<RwLock<Vec<ProviderConfig>>>,
    pub payload_rules: Arc<RwLock<Vec<PayloadRule>>>,
    pub runtime: Arc<RwLock<RuntimeConfig>>,
    /// 重读配置文件的闭包(由 cli 构造,捕获 config 路径)
    pub reload: ReloadFn,
    /// drift 观测状态(会话 → 上次结构哈希;按 openai/anthropic handler 分桶)
    pub drift: DriftState,
    /// reasoning replay 缓存(会话 → 上一轮 replay 项;responses+grok 用,
    /// 对齐 CPA xai reasoning replay;server 层持有,core 无 IO)
    pub replay_cache: crate::sse::replay_cache::ReplayCache,
    /// session_id → 最新 input_tokens(避免非 Claude 上游 count_tokens 估算不准导致 context 跳动)
    pub last_input_tokens: Arc<std::sync::Mutex<std::collections::HashMap<String, usize>>>,
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

pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/v1/messages", post(handle_messages))
        .route("/v1/messages/count_tokens", post(handle_count_tokens))
        .route("/v1/models", get(handle_models))
        .route("/health", axum::routing::get(health_check))
        .route("/reload", post(handle_reload))
        .with_state(state)
}

/// bcrypt 验证结果缓存:key→已验证,避免每请求一次 ~100ms 的 bcrypt verify
/// 上限 1024 条,超限清空(防内存无限增长);secret 可热重载,/reload 一律清空缓存
static AUTH_CACHE: OnceLock<StdMutex<HashMap<String, bool>>> = OnceLock::new();

/// 诊断日志请求序号:与毫秒时间戳组合,避免并发请求覆盖同一文件。
static UPSTREAM_LOG_SEQUENCE: AtomicU64 = AtomicU64::new(0);

// 上游可重试错误的统一退避参数(对齐通用网关重试与 grok retry)
const RETRY_BASE_DELAY: std::time::Duration = std::time::Duration::from_millis(300);
const RETRY_MAX_DELAY: std::time::Duration = std::time::Duration::from_millis(1500);
const RETRY_TOTAL_BUDGET: std::time::Duration = std::time::Duration::from_secs(3);

/// 52x 错误最大退避时间(对齐 grok MAX_RETRY_BACKOFF,防 Cloudflare 120s 挂死)
const CF_EDGE_MAX_RETRY_BACKOFF: std::time::Duration = std::time::Duration::from_secs(30);

/// +/-20% jitter 抖动(对齐 grok jitter_backoff),打散并发客户端重试风暴
fn jitter_backoff(base: std::time::Duration) -> std::time::Duration {
    use std::hash::{Hash, Hasher};
    static JITTER_SEQ: AtomicU64 = AtomicU64::new(0);

    let base_ms = base.as_millis() as u64;
    if base_ms == 0 {
        return base;
    }
    let jitter_range = base_ms / 5;
    let mut hasher = std::hash::DefaultHasher::new();
    JITTER_SEQ.fetch_add(1, Ordering::Relaxed).hash(&mut hasher);
    std::thread::current().id().hash(&mut hasher);
    let jitter = if jitter_range > 0 {
        hasher.finish() % (jitter_range * 2 + 1)
    } else {
        0
    };
    std::time::Duration::from_millis(base_ms.saturating_sub(jitter_range) + jitter)
}

/// Retry-After 头解析:仅支持秒数形式(HTTP-date 忽略,按退避公式兜底)
fn parse_retry_after(headers: &reqwest::header::HeaderMap) -> Option<std::time::Duration> {
    let raw = headers.get(reqwest::header::RETRY_AFTER)?.to_str().ok()?;
    let secs = raw.trim().parse::<u64>().ok()?;
    Some(std::time::Duration::from_secs(secs))
}

/// 第 attempt 次失败(0 起)后的等待时长:
/// - 429: 完整尊重 Retry-After(对齐 grok/OpenAI rate limit)
/// - 52x/5xx 等边缘错误: Retry-After 钳位到 30s 并加 +/-20% jitter 抖动
/// - 其余/网络错误: base * 2^attempt 指数退避 + jitter
/// - 总预算耗尽返回 None 不再重试。
fn compute_retry_delay(
    attempt: u32,
    started_at: std::time::Instant,
    headers: &reqwest::header::HeaderMap,
    status: Option<reqwest::StatusCode>,
) -> Option<std::time::Duration> {
    let elapsed = started_at.elapsed();
    if elapsed >= RETRY_TOTAL_BUDGET {
        return None;
    }

    let is_429 = status.map(|s| s.as_u16() == 429).unwrap_or(false);
    let is_cf_52x = status
        .map(|s| {
            let c = s.as_u16();
            (520..=529).contains(&c)
        })
        .unwrap_or(false);

    let mut delay = if let Some(retry_after) = parse_retry_after(headers) {
        if is_429 {
            // 429 真实限流直接按上游声明等待
            retry_after.min(RETRY_MAX_DELAY)
        } else if is_cf_52x {
            // Cloudflare 52x 往往下发 60-120s，钳位到 30s + 抖动
            jitter_backoff(retry_after.min(CF_EDGE_MAX_RETRY_BACKOFF))
        } else {
            jitter_backoff(retry_after.min(RETRY_MAX_DELAY))
        }
    } else {
        let backoff = RETRY_BASE_DELAY.saturating_mul(1u32 << attempt.min(4));
        jitter_backoff(backoff.min(RETRY_MAX_DELAY))
    };

    // 头给出的等待也不得突破总预算(截断而非放弃,末次机会照试)
    if elapsed + delay > RETRY_TOTAL_BUDGET {
        delay = RETRY_TOTAL_BUDGET - elapsed;
    }
    Some(delay)
}

fn auth_cache() -> &'static StdMutex<HashMap<String, bool>> {
    AUTH_CACHE.get_or_init(|| StdMutex::new(HashMap::new()))
}

/// bcrypt 校验(带缓存;锁毒化时降级直验)
fn verify_cached(key: &str, expected: &str) -> bool {
    match auth_cache().lock() {
        Ok(mut cache) => {
            if let Some(&ok) = cache.get(key) {
                return ok;
            }
            let ok = bcrypt::verify(key, expected).unwrap_or(false);
            if cache.len() >= 1024 {
                cache.clear();
            }
            cache.insert(key.to_string(), ok);
            ok
        }
        Err(_) => bcrypt::verify(key, expected).unwrap_or(false),
    }
}

/// 校验入口 key:secret 未配置时放行;配置时需匹配,否则 401
/// 支持 x-api-key 与 Authorization: Bearer 两种头(兼容 cc-switch 等工具)
/// secret 为 bcrypt 哈希时用 verify,否则明文比对(便于测试/旧配置)
fn check_secret(headers: &HeaderMap, secret: &Option<String>) -> Result<(), AppError> {
    let Some(expected) = secret else {
        return Ok(());
    };
    let got = extract_key(headers);
    let ok = if looks_like_bcrypt(expected) {
        verify_cached(got, expected)
    } else {
        got == expected
    };
    if ok {
        Ok(())
    } else {
        Err(AppError::unauthorized(
            "x-api-key 或 Authorization 缺失/不匹配",
        ))
    }
}

/// 从请求头提取 key:x-api-key 优先,其次 Authorization: Bearer
/// scheme 大小写不敏感(RFC 6750),容忍任意空白
fn extract_key(headers: &HeaderMap) -> &str {
    if let Some(v) = headers.get("x-api-key").and_then(|v| v.to_str().ok()) {
        if !v.is_empty() {
            return v;
        }
    }
    if let Some(v) = headers.get("authorization").and_then(|v| v.to_str().ok()) {
        if let Some((scheme, token)) = v.split_once(char::is_whitespace) {
            if scheme.eq_ignore_ascii_case("bearer") {
                let token = token.trim();
                if !token.is_empty() {
                    return token;
                }
            }
        }
    }
    ""
}

/// 构建 Anthropic 格式模型列表(参考 GetAvailableModels claude 分支)
fn build_models_list(providers: &[ProviderConfig]) -> Value {
    let mut data = Vec::new();
    for provider in providers {
        for model in &provider.models {
            data.push(serde_json::json!({
                "id": model.alias,
                "object": "model",
                "owned_by": provider.name,
                "type": "model",
                "display_name": model.alias,
                "max_input_tokens": model.max_input_tokens.unwrap_or(200000),
                "max_tokens": model.max_tokens.unwrap_or(64000),
            }));
        }
    }
    serde_json::json!({ "data": data })
}

/// POST /v1/messages/count_tokens:本地 token 估算(带 secret 认证)
///
/// Claude Code 的 /context 记账会调此端点。Claude 协议上游转发到真实 API
/// 获取精确计数；非 Claude 协议一律本地估算(O200kBase)。
async fn handle_count_tokens(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, AppError> {
    let secret = state.runtime.read().await.secret.clone();
    check_secret(&headers, &secret)?;
    let bytes = to_bytes(body, 10 * 1024 * 1024)
        .await
        .map_err(|e| AppError::new(anyhow::anyhow!("读请求体失败: {e}")))?;

    // 解析 model 字段
    let body_json: Value = serde_json::from_slice(&bytes)
        .map_err(|e| AppError::new(anyhow::anyhow!("解析请求体失败: {e}")))?;
    let model = body_json
        .get("model")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AppError::new(anyhow::anyhow!("缺少 model 字段")))?;

    // 路由判定
    let providers = state.providers.read().await;
    let route = resolve_route(model, &providers)
        .map_err(|e| AppError::new(anyhow::anyhow!("路由失败: {e}")))?;

    // Claude 协议:转发上游
    if route.protocol == Protocol::Claude {
        let provider = find_provider(&providers, &route.provider)
            .ok_or_else(|| AppError::new(anyhow::anyhow!("provider 未找到")))?;
        let base_url = provider.base_urls()[0].clone(); // 取首个 URL（count_tokens 无需回退）
        let key = provider.key.clone();
        let proxy_url = provider.proxy_url.clone();
        let (upstream_client, user_agents) = {
            let runtime = state.runtime.read().await;
            (runtime.upstream.clone(), runtime.user_agents.clone())
        };
        drop(providers); // 释放读锁

        let url = format!(
            "{}/v1/messages/count_tokens",
            base_url.trim_end_matches('/')
        );
        let proxy_key = upstream_client.resolve_proxy(proxy_url.as_deref());
        let client = upstream_client.client_for(&proxy_key);
        let inbound_user_agent = claude_inbound_user_agent(&headers);
        let extra_headers = claude_relay_headers(&headers);
        let mut request = client
            .post(&url)
            .header(
                header::USER_AGENT,
                inbound_user_agent.unwrap_or(user_agents.claude_cli.as_str()),
            )
            .bearer_auth(&key);
        for (name, value) in &extra_headers {
            request = request.header(name, value);
        }
        let resp = request
            .json(&body_json)
            .send()
            .await
            .map_err(|e| AppError::new(anyhow::anyhow!("上游请求失败: {e}")))?;

        let status = resp.status();
        let body_bytes = resp
            .bytes()
            .await
            .map_err(|e| AppError::new(anyhow::anyhow!("读上游响应失败: {e}")))?;

        return Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body_bytes))
            .map_err(|e| AppError::new(anyhow::anyhow!("构造响应失败: {e}")));
    }

    // 非 Claude 协议:读缓存(有上轮真实值返回,无缓存返回 0)
    let session = extract_claude_code_session(&headers, &body_json);
    let tokens = session
        .as_deref()
        .and_then(|s| state.last_input_tokens.lock().ok()?.get(s).copied())
        .unwrap_or(0);

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(format!(r#"{{"input_tokens":{}}}"#, tokens)))
        .map_err(|e| AppError::new(anyhow::anyhow!("构造响应失败: {e}")))
}

/// GET /v1/models:返回配置定义的模型列表(带 secret 认证)
async fn handle_models(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let secret = state.runtime.read().await.secret.clone();
    check_secret(&headers, &secret)?;
    let providers = state.providers.read().await;
    let body = build_models_list(&providers);
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(serde_json::to_vec(&body).unwrap()))
        .map_err(|e| AppError::new(anyhow::anyhow!("构造模型列表响应失败: {e}")))
}

async fn health_check() -> &'static str {
    "ok"
}

/// 热重载:重读配置文件,校验后更新 providers / payload / 运行时配置。
///
/// 三把独立写锁分别获取,非全局原子 —— 期间并发请求可能见到部分更新
/// (如新 providers 配旧 normalize)。热重载低频,取舍见 docs/design.md §8。
async fn handle_reload(State(state): State<AppState>) -> Result<&'static str, AppError> {
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
        upstream: UpstreamClient::new(data.proxy_url),
        user_agents: data.user_agents,
    };
    // secret 可能变更,旧 bcrypt 校验结果一律作废(不比较新旧值)
    if let Ok(mut cache) = auth_cache().lock() {
        cache.clear();
    }
    tracing::info!("配置热重载完成");
    Ok("reloaded")
}

/// 按 provider 名查找 provider 配置
fn find_provider<'a>(providers: &'a [ProviderConfig], name: &str) -> Option<&'a ProviderConfig> {
    providers.iter().find(|p| p.name == name)
}

/// 解析 payload 后最终模型。仅 OpenAI 的无效覆盖回退路由模型并同步写回 body,
/// 确保 Grok 判定与 UpstreamClient 读取的 model 一致；其余协议保留 payload 原语义。
fn resolve_outbound_model(body: &mut Value, fallback: &str, protocol: Protocol) -> String {
    match body
        .get("model")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
    {
        Some(model) => model.to_string(),
        None => {
            if matches!(protocol, Protocol::OpenAiChat | Protocol::OpenAiResponses) {
                body["model"] = Value::String(fallback.to_string());
            }
            fallback.to_string()
        }
    }
}

/// prompt_cache_key 注入闸门:provider 开关 + openai 协议,chat+grok 跳过。
/// 官方 CLI 不把该字段映射上 chat 线,粘性走 x-grok-conv-id。
fn should_inject_prompt_cache_key(
    provider_prompt_cache_key: bool,
    protocol: Protocol,
    upstream_model: &str,
) -> bool {
    provider_prompt_cache_key
        && matches!(protocol, Protocol::OpenAiChat | Protocol::OpenAiResponses)
        && !(matches!(protocol, Protocol::OpenAiChat) && is_grok_model(upstream_model))
}

/// 应用 payload 参数覆盖(支持 "*glm*" 通配;协议限定,缺省 = 所有协议)
/// claude 直通默认不注入:必须显式声明 `protocol: claude` 才生效,
/// 避免无协议规则误覆盖直通 body。
fn apply_payload_overrides(
    body: &mut Value,
    model: &str,
    protocol: Protocol,
    rules: &[PayloadRule],
) {
    for rule in rules {
        if matches!(protocol, Protocol::Claude) && rule.protocol.is_none() {
            continue;
        }
        if let Some(p) = rule.protocol {
            if p != protocol {
                continue;
            }
        }
        let matched = rule.models.iter().any(|pat| {
            if pat == "*" {
                return true;
            }
            Glob::new(pat)
                .map(|g| g.compile_matcher().is_match(model))
                .unwrap_or(false)
        });
        if matched {
            for (key, val) in &rule.params {
                body[key] = val.clone();
            }
        }
    }
}

/// 观测 body 结构漂移(对齐 openai/anthropic handler 的 drift 检测)。
/// 辅助请求(标题生成等)跳过——它们共享会话键但 body 形状不同,会比较出假漂移。
/// `enabled` 接 normalize.drift_detector 开关,关闭时跳过观测。
fn observe_drift_for(
    drift: &DriftState,
    headers: &HeaderMap,
    body: &Value,
    kind: DriftApiKind,
    enabled: bool,
) {
    if !enabled {
        return;
    }
    if is_ancillary_request(body, kind) {
        tracing::debug!(?kind, "skipped drift detection for ancillary request");
        return;
    }
    let identity = drift_derive_session_key(headers, body, kind);
    let structural_hash = compute_structural_hash(body, kind);
    observe_drift(drift, &identity, structural_hash);
}

/// 构建 Claude 中转请求头:保留入站头,排除认证、代理重建及连接管理头。
fn claude_relay_headers(headers: &HeaderMap) -> HeaderMap {
    let connection_header_names: Vec<String> = headers
        .get_all(header::CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|name| !name.is_empty())
        .map(str::to_ascii_lowercase)
        .collect();
    let mut relay_headers = HeaderMap::new();
    for (name, value) in headers.iter() {
        if !is_claude_relay_header_excluded(name.as_str(), &connection_header_names) {
            relay_headers.append(name.clone(), value.clone());
        }
    }
    relay_headers
}

fn is_claude_relay_header_excluded(name: &str, connection_header_names: &[String]) -> bool {
    let name = name.to_ascii_lowercase();
    connection_header_names.iter().any(|item| item == &name)
        || matches!(
            name.as_str(),
            "authorization"
                | "x-api-key"
                | "user-agent"
                | "host"
                | "content-length"
                | "connection"
                | "keep-alive"
                | "proxy-connection"
                | "proxy-authenticate"
                | "proxy-authorization"
                | "te"
                | "trailer"
                | "transfer-encoding"
                | "upgrade"
                | "http2-settings"
        )
}

fn claude_inbound_user_agent(headers: &HeaderMap) -> Option<&str> {
    headers
        .get(header::USER_AGENT)
        .and_then(|value| value.to_str().ok())
        .filter(|value| !value.is_empty())
}

async fn handle_messages(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, AppError> {
    // 一次性 clone 运行时快照值后立即释放读锁,避免跨 await 持锁阻塞 /reload
    let (
        secret,
        log_request_body,
        normalize_enabled,
        normalize_drift_detector,
        upstream_client,
        user_agents,
    ) = {
        let rt = state.runtime.read().await;
        (
            rt.secret.clone(),
            rt.logging.request_body,
            rt.normalize.enabled,
            rt.normalize.drift_detector,
            rt.upstream.clone(),
            rt.user_agents.clone(),
        )
    };
    check_secret(&headers, &secret)?;
    let bytes = to_bytes(body, 10 * 1024 * 1024)
        .await
        .map_err(|e| AppError::new(anyhow::anyhow!("读请求体失败: {e}")))?;
    if log_request_body {
        tracing::debug!("请求体: {}", String::from_utf8_lossy(&bytes));
    }
    let mut body_json: Value = serde_json::from_slice(&bytes)
        .map_err(|e| AppError::bad_request(format!("请求体 JSON 解析失败: {e}")))?;

    // 1. 入站 model(复制为 String,避免借用 body_json 阻碍后续可变借用)
    let model = body_json
        .get("model")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AppError::bad_request("缺少 model 字段"))?
        .to_string();

    // 2. 路由决策(先定协议,再选归一化模式;对齐 按目标协议分流)
    let providers = state.providers.read().await;
    let route = resolve_route(&model, &providers)?;
    let payload_rules = state.payload_rules.read().await;

    // 检测跨轮工具调用死循环并对齐 grok-build 双阶梯处理(仅针对 grok 模型)
    if is_grok_model(&route.upstream_model) {
        match ccextra_core::doom_loop::check_action_stationarity(&body_json) {
            ccextra_core::doom_loop::StationarityVerdict::HardStop { run_len, tool_name } => {
                tracing::warn!(
                    model = %model,
                    upstream = %route.upstream_model,
                    tool_name = %tool_name,
                    run_len,
                    "action stationarity: 达到硬停机阈值，直接熔断结束回合"
                );
                let is_stream = body_json
                    .get("stream")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                if is_stream {
                    let text = format!(
                        "Loop detected: tool `{tool_name}` repeated with identical arguments {run_len} times. Halting turn."
                    );
                    let frames = vec![
                        crate::sse::emit::message_start(
                            "msg_stationarity_stop",
                            &model,
                            0,
                            0,
                            true,
                        ),
                        crate::sse::emit::content_block_start_text(0),
                        crate::sse::emit::content_block_delta_text(0, &text),
                        crate::sse::emit::content_block_stop(0),
                        crate::sse::emit::message_delta("end_turn", None, 0, 0, 0, 0),
                        crate::sse::emit::message_stop(),
                    ];
                    let stream =
                        futures::stream::iter(frames.into_iter().map(Ok::<_, std::io::Error>));
                    return Response::builder()
                        .status(StatusCode::OK)
                        .header(header::CONTENT_TYPE, "text/event-stream")
                        .header(header::CACHE_CONTROL, "no-cache")
                        .body(Body::from_stream(stream))
                        .map_err(|e| AppError::new(anyhow::anyhow!("构造熔断流响应失败: {e}")));
                } else {
                    let text = format!(
                        "Loop detected: tool `{tool_name}` repeated with identical arguments {run_len} times. Halting turn."
                    );
                    let resp = json!({
                        "id": "msg_stationarity_stop",
                        "type": "message",
                        "role": "assistant",
                        "model": model,
                        "content": [{"type": "text", "text": text}],
                        "stop_reason": "end_turn",
                        "stop_sequence": null,
                        "usage": {"input_tokens": 0, "output_tokens": 0}
                    });
                    return Response::builder()
                        .status(StatusCode::OK)
                        .header(header::CONTENT_TYPE, "application/json")
                        .body(Body::from(serde_json::to_vec(&resp).unwrap()))
                        .map_err(|e| AppError::new(anyhow::anyhow!("构造熔断响应失败: {e}")));
                }
            }
            ccextra_core::doom_loop::StationarityVerdict::Nudge { .. } => {
                if ccextra_core::doom_loop::inject_loop_recovery_reminder_if_needed(&mut body_json)
                {
                    tracing::warn!(model = %model, upstream = %route.upstream_model, "检测到 Grok 工具调用循环，已注入 RECOVERY_REMINDER");
                }
            }
            ccextra_core::doom_loop::StationarityVerdict::None => {}
        }
    }

    // 3. 归一化第一遍(按协议:claude 直通全量 / openai 转换前精简)
    // 对齐:claude 直通走 /v1/messages(全量),openai 走转换前
    // 精简子集(跳过 tool-def sort / volatile / cache_control / drift——
    // 这些在转换后 openai handler 处理)
    if normalize_enabled {
        match route.protocol {
            Protocol::Claude => {
                let counts = normalize_anthropic_full(&mut body_json);
                tracing::debug!(?counts, "normalize_anthropic_full");
                observe_drift_for(
                    &state.drift,
                    &headers,
                    &body_json,
                    DriftApiKind::Anthropic,
                    normalize_drift_detector,
                );
            }
            _ => {
                let counts = normalize_anthropic_pretransform(&mut body_json);
                tracing::debug!(?counts, "normalize_anthropic_pretransform");
            }
        }
    }

    // 4. 协议转换(含目标侧归一化)
    // stream 缺省对齐 Anthropic API 语义(false 非流):Claude Code 非流重试
    // 不带 stream 字段,按 true 会把上游 SSE 流回给期望 JSON 的客户端。
    let is_stream = body_json
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    // Claude Code 会话 ID 须在转换前提取(转换后 metadata 被丢弃),供 prompt_cache_key 用
    let cc_session = extract_claude_code_session(&headers, &body_json);
    // openai 转换器重建 body,丢未知顶层字段;入站非空 prompt_cache_key 转换后原样写回
    let inbound_prompt_cache_key = body_json
        .get("prompt_cache_key")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string);

    // 流式 SSE message_start 占位 input_tokens(对齐 ClaudeInputTokenState)。
    // 多数上游流中不带 usage(chat 只在最后 chunk 带,responses 只在流尾),
    // message_start 又必须第一帧发,故用估算占位,让 cc context 过程中接近
    // 真实而非跳 1;流尾 message_delta 以真实 usage 覆盖。claude 直通不经
    // 状态机、非流式不进 SSE,均传 None。
    // 注意:非 Claude 协议 count_tokens 已改用缓存,此处从缓存读上轮真实值。
    let estimated_input_tokens = if !is_stream || matches!(route.protocol, Protocol::Claude) {
        None
    } else {
        cc_session
            .as_deref()
            .and_then(|s| state.last_input_tokens.lock().ok()?.get(s).copied())
            .or(Some(0))
    };

    // 工具名还原表(short→original),responses 转换侧产出,供流式/非流式响应还原
    let mut tool_names: Option<Arc<HashMap<String, String>>> = None;
    let mut request_fingerprint = String::new();

    match route.protocol {
        Protocol::Claude => {
            convert_passthrough(&mut body_json, &route.upstream_model)?;
        }
        Protocol::OpenAiChat => {
            convert_to_openai_chat(&mut body_json, &route.upstream_model)?;
            if normalize_enabled {
                normalize_target_post(&mut body_json, TargetShape::OpenAiChat);
                observe_drift_for(
                    &state.drift,
                    &headers,
                    &body_json,
                    DriftApiKind::OpenAiChat,
                    normalize_drift_detector,
                );
            }
        }
        Protocol::OpenAiResponses => {
            // reverse map:short→original(超长工具名缩短后,响应侧还原原名)
            let rev = convert_to_openai_responses(&mut body_json, &route.upstream_model)?;
            if !rev.is_empty() {
                tool_names = Some(Arc::new(rev));
            }
            // reasoning replay 注入(对齐 CPA applyCodexReasoningReplayCacheRequired:
            // responses 协议的 reasoning 是服务器端状态,store=false 时上游不保留,
            // 须回放上一轮 encrypted_content,否则模型丢失决策记忆重复发相同工具
            // 调用。CPA codexReasoningReplayEnabledForSource 只判断来源协议
            // FormatClaude,不限模型;ccextra 入站协议恒为 anthropic,故 responses
            // 协议全部启用)。缓存 key = "{model}:{session}"(对齐
            // xaiReasoningReplayCacheKey / codexReasoningReplayScope 的
            // model+session 连续性边界)。
            if let Some(sess) = cc_session.as_deref() {
                let key = format!("{}:{}", route.upstream_model, sess);
                // grok 上游无加密信封时保留明文 reasoning 回放
                // (对齐 grok-build 官方行为,其余上游维持仅加密回放)
                if state.replay_cache.apply_to_body(
                    &key,
                    &mut body_json,
                    is_grok_model(&route.upstream_model),
                ) {
                    tracing::debug!(
                        session = %sess,
                        model = %route.upstream_model,
                        "reasoning replay 已注入"
                    );
                }
                // 注入后计算 input 前缀指纹(marker 锚定用)
                request_fingerprint =
                    ccextra_core::convert::compute_input_prefix_fingerprint(&body_json);
            }
            if normalize_enabled {
                normalize_target_post(&mut body_json, TargetShape::OpenAiResponses);
            }
        }
        Protocol::Gemini => {
            use ccextra_core::convert::convert_to_gemini;
            let (gemini_body, short_to_original) =
                convert_to_gemini(&body_json, &route.upstream_model);
            body_json = gemini_body;
            if !short_to_original.is_empty() {
                tool_names = Some(Arc::new(short_to_original));
            }
        }
        Protocol::Antigravity => {
            // Antigravity 使用包裹后的 Gemini 格式
            use ccextra_core::convert::convert_to_antigravity;

            // 从 provider metadata 中提取 project_id
            let project_id = {
                let provider = find_provider(&providers, &route.provider);
                provider
                    .and_then(|p| p.metadata.as_ref())
                    .and_then(|m| m.get("project_id"))
                    .map(|s| s.as_str())
            };

            let (antigravity_body, short_to_original) =
                convert_to_antigravity(&body_json, &route.upstream_model, project_id);
            body_json = antigravity_body;
            if !short_to_original.is_empty() {
                tool_names = Some(Arc::new(short_to_original));
            }

            // reasoning replay 注入(对齐 CPA prepareAntigravityGeminiReasoningReplayPayload:
            // antigravity gemini/flash/agent 模型启用 replay,claude 模型不启用;入站协议
            // 恒为 anthropic,符合 CPA 对 sourceFormat 的判断。信封里的 request 字段才是
            // Gemini 格式,注入目标是信封内层的 request.contents)。
            if ccextra_core::antigravity_uses_reasoning_replay(&route.upstream_model) {
                if let Some(sess) = cc_session.as_deref() {
                    let key = format!("{}:{}", route.upstream_model, sess);
                    if let Some(request) = body_json.get_mut("request") {
                        if state.replay_cache.apply_to_body(&key, request, false) {
                            tracing::debug!(
                                session = %sess,
                                model = %route.upstream_model,
                                "antigravity reasoning replay 已注入"
                            );
                        }
                    }
                }
            }
        }
    }

    // openai 转换丢顶层未知字段;payload 前写回,payload 仍可覆盖
    if matches!(
        route.protocol,
        Protocol::OpenAiChat | Protocol::OpenAiResponses
    ) {
        if let Some(key) = inbound_prompt_cache_key {
            body_json["prompt_cache_key"] = Value::String(key);
        }
    }

    // 5. payload 参数覆盖(转换后注入;claude 直通需显式 protocol 才生效)
    apply_payload_overrides(&mut body_json, &model, route.protocol, &payload_rules);

    // grok 判定与缓存闸门跟出站 model:payload 可把 gpt-* 改成 grok-*
    // OpenAI payload 若把 model 置空/改成非字符串,回写路由模型,保证 body 与头部判定一致。
    let outbound_model =
        resolve_outbound_model(&mut body_json, &route.upstream_model, route.protocol);

    // tool_result 截断(仅 Grok Responses 协议保留 grok-build 策略;
    // GPT/Codex 上游对齐 CPA 不做截断,避免破坏代码读取导致死循环)
    if normalize_enabled
        && matches!(route.protocol, Protocol::OpenAiResponses)
        && is_grok_model(&outbound_model)
    {
        if let Err(e) = ccextra_core::cache_stabilization::truncate_tool_results::truncate(
            &mut body_json,
            ccextra_core::normalize::UpstreamTruncation::GrokBuild,
        ) {
            tracing::warn!("tool_result truncation failed: {}", e);
        }
    }

    if normalize_enabled && matches!(route.protocol, Protocol::OpenAiResponses) {
        // drift 必须看到最终 Responses body,避免大工具输出参与前缀哈希。
        observe_drift_for(
            &state.drift,
            &headers,
            &body_json,
            DriftApiKind::OpenAiResponses,
            normalize_drift_detector,
        );
    }

    // GPT/Codex 在最终 model 与 payload 落定后校验 reasoning 回放信封。
    if matches!(route.protocol, Protocol::OpenAiResponses) && is_gpt_model(&outbound_model) {
        if let Some(obj) = body_json.as_object_mut() {
            for key in [
                "previous_response_id",
                "generate",
                "safety_identifier",
                "stream_options",
            ] {
                obj.remove(key);
            }
        }
        if sanitize_gpt_reasoning_items(&mut body_json) {
            tracing::debug!("已清理无效 GPT reasoning encrypted_content");
        }
    }

    // 6. 对齐 StripPromptCacheRetention:openai 上游拒绝 prompt_cache_retention
    // (HTTP 400 "Unsupported parameter: prompt_cache_retention"),claude 直通保留
    if !matches!(route.protocol, Protocol::Claude) {
        body_json
            .as_object_mut()
            .map(|m| m.remove("prompt_cache_retention"));
    }

    // 7. 上游请求
    // 从配置中 clone 出上游所需字段后立即释放两把读锁,避免整个上游请求
    // (慢上游/长连接建立)期间持锁,防止 /reload 写锁被无限期阻塞。
    let (upstream_base_urls, mut upstream_key, upstream_proxy, provider_prompt_cache_key) = {
        let provider = find_provider(&providers, &route.provider)
            .ok_or_else(|| AppError::new(anyhow::anyhow!("provider 未找到: {}", route.provider)))?;
        (
            provider.base_urls().to_vec(),
            provider.key.clone(),
            provider.proxy_url.clone(),
            provider.prompt_cache_key,
        )
    };

    // Antigravity 协议运行时 token 校验与自动刷新（对齐 CLIProxyAPI ensureAccessToken）
    if matches!(route.protocol, Protocol::Antigravity) {
        if let Some(provider) = find_provider(&providers, &route.provider) {
            if let Some(meta) = &provider.metadata {
                if let (Some(auth_dir_str), Some(email)) = (meta.get("auth_dir"), meta.get("email"))
                {
                    let auth_dir = std::path::Path::new(auth_dir_str);
                    match crate::antigravity::ensure_credential_fresh(
                        auth_dir,
                        email,
                        upstream_proxy.as_deref(),
                    )
                    .await
                    {
                        Ok(fresh_cred) => {
                            upstream_key = fresh_cred.access_token;
                        }
                        Err(e) => {
                            tracing::warn!(email = %email, "Antigravity 凭证运行时刷新失败: {e}");
                        }
                    }
                }
            }
        }
    }

    // xAI 运行时 token 校验与自动刷新
    if let Some(provider) = find_provider(&providers, &route.provider) {
        if let Some(meta) = &provider.metadata {
            if meta.get("provider_type").map(|s| s.as_str()) == Some("xai") {
                if let Some(auth_dir_str) = meta.get("auth_dir") {
                    let auth_dir = std::path::Path::new(auth_dir_str);
                    let email = meta.get("email").map(|s| s.as_str()).unwrap_or("");
                    let sub = meta.get("sub").map(|s| s.as_str()).unwrap_or("");
                    let id = if !email.is_empty() { email } else { sub };
                    match crate::xai::ensure_credential_fresh(
                        auth_dir,
                        id,
                        upstream_proxy.as_deref(),
                    )
                    .await
                    {
                        Ok(fresh_cred) => {
                            upstream_key = fresh_cred.access_token;
                        }
                        Err(e) => {
                            tracing::warn!(
                                email = %email,
                                sub = %sub,
                                "xAI 凭证运行时刷新失败: {e}"
                            );
                        }
                    }
                }
            }
        }
    }
    drop(payload_rules);
    drop(providers);

    // prompt_cache_key 注入(provider 级开关;仅 openai;chat+grok 跳过,对齐 grok-build)
    if should_inject_prompt_cache_key(provider_prompt_cache_key, route.protocol, &outbound_model)
        && inject_prompt_cache_key(&mut body_json, cc_session.as_deref())
    {
        tracing::debug!("prompt_cache_key 已注入");
    }

    // 诊断:request_body 开启时落盘请求信息(headers + body),供逐轮 diff。
    // 文件名按会话+时间+序号。
    let sess = cc_session
        .as_deref()
        .map(|s| s.chars().take(8).collect::<String>())
        .unwrap_or_else(|| "nosess".into());
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0);
    let proto = format!("{:?}", route.protocol).to_lowercase();
    let request_seq = if log_request_body {
        UPSTREAM_LOG_SEQUENCE.fetch_add(1, Ordering::Relaxed)
    } else {
        0
    };
    let log_stem = upstream_log_stem(&sess, ts, request_seq, &proto);
    if log_request_body {
        let _ = std::fs::create_dir_all("logs");
        let dump_obj = json!({
            "inbound_headers": inbound_headers_json(&headers),
            "upstream_body": body_json,
        });
        let path = format!("logs/upstream_request_{log_stem}.json");
        let dumped = serde_json::to_vec_pretty(&dump_obj)
            .ok()
            .and_then(|bytes| std::fs::write(&path, bytes).ok());
        if dumped.is_none() {
            tracing::warn!(path = %path, "上游请求诊断信息落盘失败");
        }
    }

    let inbound_user_agent = if matches!(route.protocol, Protocol::Claude) {
        claude_inbound_user_agent(&headers)
    } else {
        None
    };
    let extra_headers = if matches!(route.protocol, Protocol::Claude) {
        claude_relay_headers(&headers)
    } else {
        HeaderMap::new()
    };

    // responses 协议:session-id/thread-id 头(对齐 Codex 官方客户端)
    // - session-id:会话级 UUID(整会话稳定,与 prompt_cache_key 解耦)
    // - thread-id:线程级 UUID(对齐上游请求关联/日志追踪)
    // grok 模型(chat/responses):session_id 用于 x-grok-conv-id 会话路由
    let is_grok = is_grok_model(&outbound_model);
    let (session_id, thread_id) = if matches!(route.protocol, Protocol::OpenAiResponses) {
        (cc_session.as_deref(), extract_claude_code_thread(&headers))
    } else if is_grok && matches!(route.protocol, Protocol::OpenAiChat) {
        (cc_session.as_deref(), None)
    } else {
        (None, None)
    };
    // reasoning replay 提取条件:
    // - responses 协议 + 有会话身份;key 为 "{model}:{session}",对齐 CPA
    //   codexReasoningReplayEnabledForSource 只判断来源协议不限模型
    // - antigravity 协议 + gemini/flash/agent 模型 + 有会话身份;对齐 CPA
    //   antigravityUsesReasoningReplayCache 的模型过滤(claude 不启用)
    let replay_scope = if matches!(route.protocol, Protocol::OpenAiResponses) {
        session_id.map(|s| {
            (
                state.replay_cache.clone(),
                format!("{}:{}", route.upstream_model, s),
                request_fingerprint.clone(),
            )
        })
    } else if matches!(route.protocol, Protocol::Antigravity)
        && ccextra_core::antigravity_uses_reasoning_replay(&route.upstream_model)
    {
        cc_session.as_deref().map(|s| {
            (
                state.replay_cache.clone(),
                format!("{}:{}", route.upstream_model, s),
                request_fingerprint.clone(),
            )
        })
    } else {
        None
    };
    tracing::debug!(
        session_id = ?session_id,
        thread_id = ?thread_id,
        cc_session = ?cc_session,
        is_grok = is_grok,
        protocol = ?route.protocol,
        "会话 ID 派发"
    );

    // 多 base_url 回退(对齐 CPA antigravity executor:网络错误、429 切下一个 URL)
    // 外层统一退避重试:网络错误 / 429 / 5xx 按指数退避(尊重 Retry-After)
    // 重试整个轮换过程;4xx 客户端错误不重试(对齐通用网关)。总预算
    // RETRY_TOTAL_BUDGET 封顶,避免客户端长时间悬挂。预算耗尽时把最后一次
    // 失败响应原样交给下方错误转换路径。
    let mut upstream: Option<UpstreamResponse> = None;
    let mut last_fail: Option<UpstreamResponse> = None;
    let mut last_err = None;
    let retry_started_at = std::time::Instant::now();
    let mut attempt: u32 = 0;
    loop {
        // 单轮:按序尝试各 base_url,首个「可接受响应」即用
        // (429 且还有下一个 URL 时换下一个立即试,不耗退避预算)
        enum Out {
            Ok(UpstreamResponse),
            Fail(UpstreamResponse),
            Net(anyhow::Error),
        }
        let outcome = 'round: {
            for (idx, base_url) in upstream_base_urls.iter().enumerate() {
                match upstream_client
                    .request(
                        base_url,
                        &upstream_key,
                        route.protocol,
                        upstream_proxy.as_deref(),
                        &body_json,
                        is_stream,
                        session_id,
                        thread_id.as_deref(),
                        &extra_headers,
                        &user_agents,
                        inbound_user_agent,
                    )
                    .await
                {
                    Ok(resp) => {
                        if resp.status.as_u16() == 429 && idx + 1 < upstream_base_urls.len() {
                            tracing::debug!("上游 429,回退到下一个 base_url: {}", base_url);
                            continue;
                        }
                        if resp.status.is_success() {
                            break 'round Out::Ok(resp);
                        }
                        break 'round Out::Fail(resp);
                    }
                    Err(e) => {
                        if idx + 1 < upstream_base_urls.len() {
                            tracing::debug!("上游请求错误,回退到下一个 base_url: {}", base_url);
                            continue;
                        }
                        break 'round Out::Net(e);
                    }
                }
            }
            unreachable!("base_urls 非空,循环内必 return/break")
        };
        match outcome {
            Out::Ok(resp) => {
                upstream = Some(resp);
                break;
            }
            Out::Fail(resp) if resp.status.as_u16() == 429 || resp.status.is_server_error() => {
                let status = resp.status;
                let wait = compute_retry_delay(
                    attempt,
                    retry_started_at,
                    resp.body.headers(),
                    Some(status),
                );
                attempt += 1;
                match wait {
                    Some(d) => {
                        tracing::warn!(
                            status = status.as_u16(),
                            attempt,
                            delay_ms = d.as_millis() as u64,
                            "上游可重试失败,退避后重试"
                        );
                        tokio::time::sleep(d).await;
                    }
                    None => {
                        last_fail = Some(resp);
                        break;
                    }
                }
            }
            Out::Fail(resp) => {
                // 4xx(400/401/403 等):客户端参数类错误,重试无意义
                last_fail = Some(resp);
                break;
            }
            Out::Net(e) => {
                last_err = Some(e);
                let wait = compute_retry_delay(attempt, retry_started_at, &HeaderMap::new(), None);
                attempt += 1;
                match wait {
                    Some(d) => tokio::time::sleep(d).await,
                    None => break,
                }
            }
        }
    }
    let mut upstream = match upstream {
        Some(u) => Some(u),
        None => match (last_fail, last_err) {
            (Some(f), _) => Some(f),
            (None, Some(e)) => return Err(e.into()),
            (None, None) => None,
        },
    };
    let mut status = upstream.as_ref().expect("上游响应应存在").status;
    let mut preloaded_stream = None;

    // antigravity 响应侧 thoughtSignature 归一化需上游模型名(对齐 CPA 取请求 model);
    // plain gemini 原样透传,其余协议不参与。
    let signature_model: Option<Arc<str>> = matches!(route.protocol, Protocol::Antigravity)
        .then(|| Arc::from(route.upstream_model.as_str()));

    // OpenAI 流在首个 Anthropic SSE 帧前失败时，尚未向客户端输出，可重试一次。
    // 已取得首帧后立即放行，后续流保持实时转发，不缓冲完整响应。
    if is_stream
        && matches!(
            route.protocol,
            Protocol::OpenAiChat | Protocol::OpenAiResponses
        )
        && status.is_success()
    {
        for attempt in 0..=1 {
            let current = upstream.take().expect("上游响应应存在");
            status = current.status;
            let mut out = relay_with_replay_tap(
                route.protocol,
                current.body.bytes_stream(),
                estimated_input_tokens,
                tool_names.clone(),
                replay_scope.clone(),
                signature_model.clone(),
            );
            let first = out.next().await;
            let retry = match &first {
                Some(Ok(frame)) => is_initial_sse_error(frame),
                Some(Err(_)) | None => true,
            };
            if retry && attempt == 0 {
                tracing::warn!(
                    protocol = ?route.protocol,
                    retry_attempt = attempt + 1,
                    "首帧转换失败，重试上游请求"
                );
                upstream = Some(
                    upstream_client
                        .request(
                            &upstream_base_urls[0],
                            &upstream_key,
                            route.protocol,
                            upstream_proxy.as_deref(),
                            &body_json,
                            is_stream,
                            session_id,
                            thread_id.as_deref(),
                            &extra_headers,
                            &user_agents,
                            inbound_user_agent,
                        )
                        .await?,
                );
                status = upstream.as_ref().expect("上游响应应存在").status;
                if !status.is_success() {
                    break;
                }
                continue;
            }
            let Some(first) = first else {
                return Err(AppError::new(anyhow::anyhow!("上游流在首帧前结束")));
            };
            preloaded_stream = Some(prepend_sse_frame(first, out));
            break;
        }
    }

    // 上游错误:转 anthropic error 形状
    // (OpenAI 的 {"error":{...}} 直接透传客户端不认,对齐 WriteErrorResponse)
    // Responses 400 + invalid_encrypted_content / thinking signature invalid /
    // grok "Could not decrypt":剥离 reasoning.encrypted_content 再请求一次。
    if !status.is_success() {
        let failed = upstream.take().expect("上游响应应存在");
        let err_bytes = failed.body.bytes().await?;
        let mut final_status = status;
        let mut final_bytes = err_bytes;
        let mut retried_ok = false;

        if (status.as_u16() == 400 || status.as_u16() == 422)
            && matches!(route.protocol, Protocol::OpenAiResponses)
            && is_thinking_signature_invalid(&final_bytes)
            && trim_encrypted_reasoning_items(&mut body_json)
        {
            tracing::warn!(
                protocol = ?route.protocol,
                "invalid_encrypted_content,剥离 reasoning 后重试一次"
            );
            // 缓存的 replay 项含同一无效 encrypted_content,一并清掉
            // (对齐 clearCodexReasoningReplayOnInvalidSignature:
            // 签名被上游拒绝后不得下轮再注入)
            if let Some((cache, key, _)) = replay_scope.as_ref() {
                cache.invalidate(key);
            }
            let retry = upstream_client
                .request(
                    &upstream_base_urls[0],
                    &upstream_key,
                    route.protocol,
                    upstream_proxy.as_deref(),
                    &body_json,
                    is_stream,
                    session_id,
                    thread_id.as_deref(),
                    &extra_headers,
                    &user_agents,
                    inbound_user_agent,
                )
                .await?;
            final_status = retry.status;
            if retry.status.is_success() {
                status = retry.status;
                upstream = Some(retry);
                retried_ok = true;
            } else {
                final_bytes = retry.body.bytes().await?;
            }
        }

        if !retried_ok {
            return Response::builder()
                .status(final_status)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(to_anthropic_error(&final_bytes)))
                .map_err(|e| AppError::new(anyhow::anyhow!("构造错误响应失败: {e}")));
        }
    }

    // 7. 响应转换
    if is_stream {
        // 流式:claude 直通字节转发;转换路径走 SSE 状态机。
        let out = if let Some(out) = preloaded_stream {
            out
        } else {
            let upstream = upstream.take().expect("上游响应应存在");
            relay_with_replay_tap(
                route.protocol,
                upstream.body.bytes_stream(),
                estimated_input_tokens,
                tool_names,
                replay_scope,
                signature_model.clone(),
            )
        };
        Ok(Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .header(header::CACHE_CONTROL, "no-cache")
            .body(Body::from_stream(out))
            .map_err(|e| AppError::new(anyhow::anyhow!("构造流式响应失败: {e}")))?)
    } else {
        // 非流:上游 JSON 转回 Anthropic messages 形状(Claude Code 的
        // 标题生成 / /compact 回退等非流式请求;claude 直通已是 Anthropic 形状)。
        let upstream = upstream.take().expect("上游响应应存在");
        let body_bytes = upstream.body.bytes().await?;
        // 非流 reasoning replay 提取:
        // - responses:REST 顶层 Response(object=response)同样提取 replay 项
        // - antigravity:{"response": {...}} 信封内层提取(对齐 CPA
        //   cacheAntigravityReasoningReplayFromResponse 从响应 body 提取)
        if let Some((cache, key, fingerprint)) = replay_scope.as_ref() {
            if let Ok(v) = serde_json::from_slice::<Value>(&body_bytes) {
                match route.protocol {
                    Protocol::OpenAiResponses => {
                        if v.get("object").and_then(|o| o.as_str()) == Some("response") {
                            // 包一层 completed 形状复用提取逻辑
                            let wrapped = json!({"response": v});
                            cache.store_from_completed(key, &wrapped, fingerprint);
                        }
                    }
                    Protocol::Antigravity => {
                        // 信封内层 response 字段可能包含 candidates 和 reasoning 签名
                        if let Some(inner) = v.get("response") {
                            let wrapped = json!({"response": inner});
                            cache.store_from_completed(key, &wrapped, fingerprint);
                        }
                    }
                    _ => {}
                }
            }
        }
        let converted = match serde_json::from_slice::<Value>(&body_bytes) {
            Ok(v) => {
                // 提取真实 usage.input_tokens 写入缓存(非流式路径)
                if let Some(sid) = session_id {
                    let input_tokens = match route.protocol {
                        Protocol::OpenAiChat => {
                            v.pointer("/usage/prompt_tokens").and_then(|t| t.as_i64())
                        }
                        Protocol::OpenAiResponses => {
                            v.pointer("/usage/input_tokens").and_then(|t| t.as_i64())
                        }
                        Protocol::Gemini | Protocol::Antigravity => {
                            // Antigravity 信封内层
                            let inner = if route.protocol == Protocol::Antigravity {
                                v.get("response").unwrap_or(&v)
                            } else {
                                &v
                            };
                            inner
                                .pointer("/usageMetadata/promptTokenCount")
                                .and_then(|t| t.as_i64())
                        }
                        Protocol::Claude => None,
                    };
                    if let Some(tokens) = input_tokens {
                        if tokens > 0 {
                            let _ =
                                state.last_input_tokens.lock().ok().map(|mut cache| {
                                    cache.insert(sid.to_string(), tokens as usize)
                                });
                        }
                    }
                }
                match route.protocol {
                    Protocol::Claude => None,
                    Protocol::OpenAiChat => crate::sse::non_stream::openai_chat_to_anthropic(&v),
                    Protocol::OpenAiResponses => {
                        crate::sse::non_stream::responses_to_anthropic(&v, tool_names.as_deref())
                    }
                    Protocol::Gemini => {
                        use ccextra_core::convert::convert_gemini_response;
                        Some(convert_gemini_response(
                            &v,
                            tool_names.as_deref().unwrap_or(&HashMap::new()),
                            None,
                        ))
                    }
                    Protocol::Antigravity => {
                        // Antigravity 响应为 {"response": {...gemini...}} 信封,先解包;
                        // usageMetadata/cpaUsageMetadata 可能位于信封根或内层。
                        use ccextra_core::convert::convert_gemini_response;
                        let mut inner = v.get("response").cloned().unwrap_or_else(|| v.clone());
                        if inner.get("usageMetadata").is_none() {
                            let usage = inner
                                .get("cpaUsageMetadata")
                                .or_else(|| v.get("usageMetadata"))
                                .or_else(|| v.get("cpaUsageMetadata"))
                                .cloned();
                            if let Some(usage) = usage {
                                inner["usageMetadata"] = usage;
                            }
                        }
                        Some(convert_gemini_response(
                            &inner,
                            tool_names.as_deref().unwrap_or(&HashMap::new()),
                            Some(route.upstream_model.as_str()),
                        ))
                    }
                }
            }
            Err(_) => None,
        };
        let payload = converted
            .and_then(|out| serde_json::to_vec(&out).ok())
            .map(Bytes::from)
            .unwrap_or(body_bytes);
        Ok(Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(payload))
            .map_err(|e| AppError::new(anyhow::anyhow!("构造响应失败: {e}")))?)
    }
}

/// 上游错误 body → anthropic 错误形状
/// `{"type":"error","error":{"type":...,"message":...}}`
///
/// 兼容两类上游错误结构:
/// 1. OpenAI 标准 `{"error":{"type":...,"message":...}}`
/// 2. 阿里云百炼 `{"code":"Throttling.RateQuota","message":"{\"error\":{...}}"}`——
///    code/message 平铺在顶层,且 message 是嵌套 JSON 字符串,真实错误藏在里面。
///    提取不到 error 时不丢真实信息,用顶层 code/message 兜底。
fn to_anthropic_error(body: &[u8]) -> Vec<u8> {
    let (raw_type, raw_message) = extract_upstream_error(body);
    let err_type = match raw_type.to_lowercase().as_str() {
        t @ ("invalid_request_error"
        | "authentication_error"
        | "permission_error"
        | "not_found_error"
        | "rate_limit_error"
        | "overloaded_error") => t.to_string(),
        "rate_limit" | "requests" | "tokens" => "rate_limit_error".to_string(),
        // 阿里云百炼/OpenAI 的裸类型名(EngineOverloadedError 等)归入语义相近的错误
        t if t.contains("overload") => "overloaded_error".to_string(),
        t if t.contains("rate") || t.contains("quota") => "rate_limit_error".to_string(),
        t if t.contains("auth") || t.contains("apikey") || t.contains("forbidden") => {
            "authentication_error".to_string()
        }
        _ => "api_error".to_string(),
    };
    let message = if raw_message.is_empty() {
        "upstream error".to_string()
    } else {
        raw_message
    };
    serde_json::to_vec(&json!({
        "type": "error",
        "error": {"type": err_type, "message": message}
    }))
    .unwrap_or_default()
}

/// 从上游错误 body 提取 (type, message)。优先标准 `error` 对象,其次
/// 顶层 `code`/`message`(百炼风格),message 为字符串时尝试二次解析嵌套 JSON。
fn extract_upstream_error(body: &[u8]) -> (String, String) {
    let Ok(v) = serde_json::from_slice::<Value>(body) else {
        return (
            String::new(),
            String::from_utf8_lossy(body).trim().to_string(),
        );
    };

    // OpenAI 标准结构
    if let Some(err) = v.get("error") {
        let t = err
            .get("type")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        let m = err
            .get("message")
            .and_then(|x| x.as_str())
            .unwrap_or("")
            .to_string();
        if !t.is_empty() || !m.is_empty() {
            return (t, m);
        }
    }

    // 阿里云百炼风格:顶层 code/message
    let code = v
        .get("code")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let msg = v.get("message").cloned();
    // message 可能是嵌套 JSON 字符串,真实错误对象藏在里面
    if let Some(Value::String(s)) = &msg {
        if let Ok(inner) = serde_json::from_str::<Value>(s) {
            if let Some(err) = inner.get("error") {
                let t = err
                    .get("type")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                let m = err
                    .get("message")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .to_string();
                if !t.is_empty() || !m.is_empty() {
                    return (if t.is_empty() { code } else { t }, m);
                }
            }
            // 二次解析的对象里没有 error,直接取其 message(若有)
            if let Some(m) = inner.get("message").and_then(|x| x.as_str()) {
                return (code, m.to_string());
            }
        }
    }
    match msg {
        // message 是普通字符串(二次解析失败或非嵌套)
        Some(Value::String(s)) => (code, s),
        _ => (code, String::new()),
    }
}

// ── 上游请求诊断落盘(配合 logs/upstream_request_*)────────

fn upstream_log_stem(session: &str, ts: u64, sequence: u64, protocol: &str) -> String {
    format!("{session}_{ts}_{sequence}.{protocol}")
}

/// 入站请求头是否含密钥,落盘时脱敏。
fn is_sensitive_header(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    matches!(
        n.as_str(),
        "authorization" | "proxy-authorization" | "cookie" | "set-cookie" | "x-api-key"
    ) || n.contains("api-key")
        || n.ends_with("-token")
        || n.ends_with("-secret")
}

fn header_value_text(value: &axum::http::HeaderValue) -> String {
    String::from_utf8_lossy(value.as_bytes()).into_owned()
}

/// 入站 HeaderMap → JSON(密钥字段写成 `[redacted]`;同名多值保留为数组)。
fn inbound_headers_json(headers: &HeaderMap) -> Value {
    let mut map = serde_json::Map::new();
    for (name, value) in headers.iter() {
        let key = name.as_str();
        let val = if is_sensitive_header(key) {
            "[redacted]".to_string()
        } else {
            header_value_text(value)
        };
        match map.get_mut(key) {
            Some(Value::Array(items)) => items.push(json!(val)),
            Some(existing) => {
                let first = existing.take();
                map.insert(key.to_string(), json!([first, val]));
            }
            None => {
                map.insert(key.to_string(), json!(val));
            }
        }
    }
    Value::Object(map)
}

/// 仅当带 replay scope 时加提取 tap。
/// responses 协议与 antigravity 协议都需从流中提取 replay 项。
fn relay_with_replay_tap<S>(
    protocol: Protocol,
    stream: S,
    estimated_input_tokens: Option<usize>,
    tool_names: Option<Arc<HashMap<String, String>>>,
    replay_scope: Option<(crate::sse::replay_cache::ReplayCache, String, String)>,
    signature_model: Option<Arc<str>>,
) -> SseStreamPin
where
    S: futures::Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
{
    match replay_scope {
        Some((cache, key, request_fingerprint)) => {
            let mut extractor = StreamReplayExtractor::new(cache, key, request_fingerprint);
            let tapped = stream.inspect(move |result| {
                if let Ok(bytes) = result {
                    extractor.push(bytes);
                }
            });
            crate::sse::relay(
                protocol,
                tapped,
                estimated_input_tokens,
                tool_names,
                signature_model,
            )
        }
        None => crate::sse::relay(
            protocol,
            stream,
            estimated_input_tokens,
            tool_names,
            signature_model,
        ),
    }
}

/// 首个转换帧已是 error 时，尚未向客户端发字节，可安全重试上游一次。
fn is_initial_sse_error(frame: &Bytes) -> bool {
    frame.starts_with(b"event: error\n")
}

fn prepend_sse_frame(first: Result<Bytes, std::io::Error>, rest: SseStreamPin) -> SseStreamPin {
    Box::pin(futures::stream::once(async move { first }).chain(rest))
}

pub async fn serve(addr: &str, state: AppState) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("ccextra listening on {}", addr);

    axum::serve(listener, app(state)).await?;
    Ok(())
}

// 错误处理
#[derive(Debug)]
pub struct AppError {
    pub status: StatusCode,
    pub err: anyhow::Error,
}

impl AppError {
    pub fn new(err: impl Into<anyhow::Error>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            err: err.into(),
        }
    }

    pub fn unauthorized(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::UNAUTHORIZED,
            err: anyhow::anyhow!(msg.into()),
        }
    }

    pub fn bad_request(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            err: anyhow::anyhow!(msg.into()),
        }
    }

    pub fn not_found(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            err: anyhow::anyhow!(msg.into()),
        }
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let error_type = match self.status {
            StatusCode::BAD_REQUEST => "invalid_request_error",
            StatusCode::UNAUTHORIZED => "authentication_error",
            StatusCode::NOT_FOUND => "not_found_error",
            StatusCode::UNPROCESSABLE_ENTITY => "invalid_request_error",
            _ => "api_error",
        };
        let body = json!({
            "type": "error",
            "error": {
                "type": error_type,
                "message": self.err.to_string()
            }
        });
        (self.status, Json(body)).into_response()
    }
}

impl From<RouteError> for AppError {
    fn from(err: RouteError) -> Self {
        match err {
            RouteError::ModelNotFound(_) => Self::not_found(err.to_string()),
            RouteError::AliasConflict(_) => Self::new(err),
        }
    }
}

impl From<anyhow::Error> for AppError {
    fn from(err: anyhow::Error) -> Self {
        Self::new(err)
    }
}

impl From<ConvertError> for AppError {
    fn from(err: ConvertError) -> Self {
        Self::new(err)
    }
}

impl From<reqwest::Error> for AppError {
    fn from(err: reqwest::Error) -> Self {
        Self::new(err)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::Request;
    use serde_json::json;
    use tower::util::ServiceExt;

    // 测试用默认 User-Agent 值
    const TEST_CLAUDE_CLI: &str = "claude-cli/2.1.246";
    const TEST_CODEX_TUI: &str = "codex_cli_rs/0.149.1 (Mac OS 26.6.2; arm64)";
    const TEST_GROK_VERSION: &str = "1.0.5";
    const TEST_ANTIGRAVITY: &str = "antigravity/hub/2.10.0 darwin/arm64";

    fn test_user_agents() -> UserAgentSet {
        UserAgentSet {
            claude_cli: Arc::new(TEST_CLAUDE_CLI.to_string()),
            codex_tui: Arc::new(TEST_CODEX_TUI.to_string()),
            grok_version: Arc::new(TEST_GROK_VERSION.to_string()),
            antigravity: Arc::new(TEST_ANTIGRAVITY.to_string()),
        }
    }

    #[test]
    fn upstream_log_stem_includes_request_sequence() {
        let first = upstream_log_stem("sessabcd", 123, 7, "openairesponses");
        let second = upstream_log_stem("sessabcd", 123, 8, "openairesponses");
        assert_ne!(first, second);
        assert_eq!(first, "sessabcd_123_7.openairesponses");
    }

    #[test]
    fn inbound_headers_json_redacts_secrets_keeps_rest() {
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", "sk-live-secret".parse().unwrap());
        headers.insert("authorization", "Bearer abc".parse().unwrap());
        headers.insert("user-agent", "claude-cli/2.1.250".parse().unwrap());
        headers.insert("x-claude-code-session-id", "sess-1".parse().unwrap());
        headers.insert("anthropic-beta", "oauth-2025-04-20".parse().unwrap());
        let dumped = inbound_headers_json(&headers);
        assert_eq!(dumped["x-api-key"], "[redacted]");
        assert_eq!(dumped["authorization"], "[redacted]");
        assert_eq!(dumped["user-agent"], "claude-cli/2.1.250");
        assert_eq!(dumped["x-claude-code-session-id"], "sess-1");
        assert_eq!(dumped["anthropic-beta"], "oauth-2025-04-20");
    }

    #[test]
    fn inbound_headers_json_keeps_duplicate_names() {
        let mut headers = HeaderMap::new();
        headers.append("x-extra", "one".parse().unwrap());
        headers.append("x-extra", "two".parse().unwrap());
        let dumped = inbound_headers_json(&headers);
        assert_eq!(dumped["x-extra"], json!(["one", "two"]));
    }

    fn headers_with(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(
                k.parse::<axum::http::header::HeaderName>().unwrap(),
                v.parse().unwrap(),
            );
        }
        h
    }

    fn relay_has_header_value(headers: &HeaderMap, name: &str, expected: &str) -> bool {
        headers
            .get_all(name)
            .iter()
            .any(|value| value.to_str().ok() == Some(expected))
    }

    #[test]
    fn test_should_inject_prompt_cache_key_matrix() {
        // chat+grok 假
        assert!(!should_inject_prompt_cache_key(
            true,
            Protocol::OpenAiChat,
            "grok-4.6"
        ));
        assert!(!should_inject_prompt_cache_key(
            true,
            Protocol::OpenAiChat,
            "Grok-4.6"
        ));
        // responses+grok 真
        assert!(should_inject_prompt_cache_key(
            true,
            Protocol::OpenAiResponses,
            "grok-4.6"
        ));
        // chat+非 grok 真
        assert!(should_inject_prompt_cache_key(
            true,
            Protocol::OpenAiChat,
            "gpt-4"
        ));
        // 开关关一律假
        assert!(!should_inject_prompt_cache_key(
            false,
            Protocol::OpenAiChat,
            "gpt-4"
        ));
        assert!(!should_inject_prompt_cache_key(
            false,
            Protocol::OpenAiResponses,
            "grok-4.6"
        ));
        // 非 openai 假
        assert!(!should_inject_prompt_cache_key(
            true,
            Protocol::Claude,
            "grok-4.6"
        ));
    }

    #[test]
    fn test_chat_grok_gate_preserves_existing_prompt_cache_key() {
        // 闸门跳过,不调用 inject;已有非空 key 不剥
        let mut body = json!({"model": "grok-4.6", "prompt_cache_key": "user-key"});
        if should_inject_prompt_cache_key(true, Protocol::OpenAiChat, "grok-4.6") {
            inject_prompt_cache_key(&mut body, Some("sess-abc"));
        }
        assert_eq!(body["prompt_cache_key"], "user-key");
    }

    #[test]
    fn test_openai_outbound_model_invalid_payload_falls_back_in_body() {
        for protocol in [Protocol::OpenAiChat, Protocol::OpenAiResponses] {
            for invalid in [
                Value::String(String::new()),
                Value::String("  ".to_string()),
                Value::Null,
                Value::Bool(true),
            ] {
                let mut body = json!({"model": invalid});
                assert_eq!(
                    resolve_outbound_model(&mut body, "grok-4.6", protocol),
                    "grok-4.6"
                );
                assert_eq!(body["model"], "grok-4.6");
            }
        }
    }

    #[test]
    fn test_non_openai_invalid_payload_model_keeps_body() {
        for protocol in [Protocol::Claude, Protocol::Gemini, Protocol::Antigravity] {
            let mut body = json!({"model": null});
            assert_eq!(
                resolve_outbound_model(&mut body, "claude-opus-5", protocol),
                "claude-opus-5"
            );
            assert!(body["model"].is_null());
        }
    }

    #[test]
    fn test_outbound_model_keeps_nonempty_payload_override() {
        let mut body = json!({"model": "grok-4.6"});
        assert_eq!(
            resolve_outbound_model(&mut body, "gpt-4", Protocol::OpenAiChat),
            "grok-4.6"
        );
        assert_eq!(body["model"], "grok-4.6");
    }

    #[test]
    fn test_claude_relay_preserves_inbound_beta_verbatim() {
        let headers = headers_with(&[("anthropic-beta", "custom-beta,custom-beta")]);
        let out = claude_relay_headers(&headers);
        assert!(relay_has_header_value(
            &out,
            "anthropic-beta",
            "custom-beta,custom-beta"
        ));
    }

    #[test]
    fn test_claude_relay_forwards_custom_headers_and_filters_transport_headers() {
        let headers = headers_with(&[
            ("x-custom-header", "custom-value"),
            ("authorization", "Bearer inbound"),
            ("x-api-key", "inbound-key"),
            ("user-agent", "claude-code/inbound"),
            ("connection", "keep-alive, x-remove-me"),
            ("x-remove-me", "remove-me"),
        ]);
        let out = claude_relay_headers(&headers);
        assert!(relay_has_header_value(
            &out,
            "x-custom-header",
            "custom-value"
        ));
        assert!(!out.contains_key("authorization"));
        assert!(!out.contains_key("x-api-key"));
        assert!(!out.contains_key("user-agent"));
        assert!(!out.contains_key("connection"));
        assert!(!out.contains_key("x-remove-me"));
    }

    #[test]
    fn test_claude_relay_forwards_all_non_transport_headers() {
        let headers = headers_with(&[
            ("x-custom-header", "custom-value"),
            ("anthropic-beta", "beta-a,beta-a"),
            ("x-api-key", "inbound-key"),
            ("authorization", "Bearer inbound"),
            ("host", "inbound.example"),
            ("content-length", "99"),
            ("connection", "keep-alive"),
            ("transfer-encoding", "chunked"),
            ("user-agent", "claude-code/inbound"),
        ]);
        let out = claude_relay_headers(&headers);
        assert!(relay_has_header_value(
            &out,
            "x-custom-header",
            "custom-value"
        ));
        assert!(relay_has_header_value(
            &out,
            "anthropic-beta",
            "beta-a,beta-a"
        ));
        for excluded in [
            "x-api-key",
            "authorization",
            "host",
            "content-length",
            "connection",
            "transfer-encoding",
            "user-agent",
        ] {
            assert!(!out.contains_key(excluded), "{excluded}");
        }
    }
    #[test]
    fn test_claude_relay_beta_no_redact_when_display_present() {
        let headers = headers_with(&[("anthropic-beta", "caller-beta")]);
        let out = claude_relay_headers(&headers);
        assert!(relay_has_header_value(
            &out,
            "anthropic-beta",
            "caller-beta"
        ));
    }

    #[test]
    fn test_claude_relay_beta_fast_mode() {
        let headers = HeaderMap::new();
        let out = claude_relay_headers(&headers);
        assert!(!out.contains_key("anthropic-beta"));
    }

    #[test]
    fn test_claude_relay_identity_headers_passthrough_only() {
        let headers = headers_with(&[
            ("anthropic-version", "2023-06-01"),
            ("x-app", "cli"),
            ("x-stainless-os", "macOS"),
        ]);
        let out = claude_relay_headers(&headers);
        assert!(relay_has_header_value(
            &out,
            "anthropic-version",
            "2023-06-01"
        ));
        assert!(relay_has_header_value(&out, "x-app", "cli"));
        assert!(relay_has_header_value(&out, "x-stainless-os", "macOS"));
        assert!(!out.contains_key("x-stainless-arch"));

        let out2 = claude_relay_headers(&HeaderMap::new());
        assert!(out2.is_empty());
    }

    fn mock_state() -> AppState {
        let providers_yaml = r#"
- name: test-claude
  protocol: claude
  base_url: "https://mock.example.com"
  key: sk-test
  models:
    - name: claude-opus-5
      alias: test-opus
- name: test-openai
  protocol: openai_chat
  base_url: "https://mock-openai.example.com"
  key: sk-openai
  proxy_url: "direct"
  models:
    - name: gpt-4
      alias: test-gpt
"#;
        let providers: Vec<ProviderConfig> = serde_yaml::from_str(providers_yaml).unwrap();
        AppState {
            providers: Arc::new(RwLock::new(providers)),
            payload_rules: Arc::new(RwLock::new(vec![])),
            runtime: Arc::new(RwLock::new(RuntimeConfig {
                normalize: NormalizeConfig {
                    enabled: false,
                    drift_detector: false,
                },
                logging: LoggingConfig {
                    level: "info".into(),
                    request_body: false,
                },
                secret: None,
                upstream: UpstreamClient::new(None),
                user_agents: test_user_agents(),
            })),
            reload: reload_returning_secret(None),
            drift: DriftState::new(1000),
            replay_cache: crate::sse::replay_cache::ReplayCache::new(
                std::time::Duration::from_secs(3600),
                1024,
            ),
            last_input_tokens: Arc::new(std::sync::Mutex::new(std::collections::HashMap::new())),
        }
    }

    /// 首次上游 200 空流尚未向客户端输出时，应重试一次并转发第二次结果。
    #[tokio::test]
    async fn test_openai_responses_retries_empty_stream_before_output() {
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let handler_attempts = Arc::clone(&attempts);
        let upstream = Router::new().route(
            "/responses",
            post(move || {
                let attempts = Arc::clone(&handler_attempts);
                async move {
                    let body = if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                        String::new()
                    } else {
                        concat!(
                            "event: response.created\n",
                            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_retry\",\"model\":\"gpt-5\"}}\n\n",
                            "event: response.output_text.delta\n",
                            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"retry ok\"}\n\n",
                            "event: response.completed\n",
                            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_retry\",\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n"
                        )
                        .to_string()
                    };
                    ([(header::CONTENT_TYPE, "text/event-stream")], body)
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, upstream).await.unwrap();
        });

        let state = mock_state();
        let provider_yaml = format!(
            r#"
name: test-responses
protocol: openai_responses
base_url: "http://{}"
key: sk-test
proxy_url: "direct"
models:
  - name: gpt-5
    alias: test-responses
"#,
            upstream_addr
        );
        let provider: ProviderConfig = serde_yaml::from_str(&provider_yaml).unwrap();
        state.providers.write().await.push(provider);
        let app = app(state);
        let request = Request::builder()
            .uri("/v1/messages")
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::to_vec(&json!({
                    "model": "test-responses",
                    "max_tokens": 64,
                    "stream": true,
                    "messages": [{"role": "user", "content": "hi"}]
                }))
                .unwrap(),
            ))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        let response_body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        server.abort();

        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert!(String::from_utf8_lossy(&response_body).contains("retry ok"));
    }

    #[tokio::test]
    async fn test_responses_uses_override_model_for_tool_result_truncation() {
        let captured: Arc<StdMutex<Option<Value>>> = Arc::new(StdMutex::new(None));
        let handler_captured = Arc::clone(&captured);
        let upstream = Router::new().route(
            "/responses",
            post(move |body: axum::body::Bytes| {
                let captured = Arc::clone(&handler_captured);
                async move {
                    *captured.lock().unwrap() = serde_json::from_slice(&body).ok();
                    (
                        StatusCode::OK,
                        [(header::CONTENT_TYPE, "application/json")],
                        json!({
                            "object": "response",
                            "id": "resp-override",
                            "model": "grok-4.6",
                            "output": [],
                            "usage": {"input_tokens": 1, "output_tokens": 1}
                        })
                        .to_string(),
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, upstream).await.unwrap();
        });

        let state = mock_state();
        state.runtime.write().await.normalize.enabled = true;
        *state.payload_rules.write().await = vec![PayloadRule {
            models: vec!["test-responses-override".into()],
            protocol: Some(Protocol::OpenAiResponses),
            params: [("model".into(), json!("grok-4.6"))].into_iter().collect(),
        }];
        let provider: ProviderConfig = serde_yaml::from_str(&format!(
            r#"
name: test-responses-override
protocol: openai_responses
base_url: "http://{upstream_addr}"
key: sk-test
proxy_url: "direct"
models:
  - name: gpt-5
    alias: test-responses-override
"#
        ))
        .unwrap();
        state.providers.write().await.push(provider);
        let app = app(state);
        let request = Request::builder()
            .uri("/v1/messages")
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::to_vec(&json!({
                    "model": "test-responses-override",
                    "max_tokens": 64,
                    "messages": [
                        {"role": "assistant", "content": [{"type": "tool_use", "id": "tool_1", "name": "read", "input": {}}]},
                        {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "tool_1", "content": "x".repeat(20_000)}]}
                    ]
                }))
                .unwrap(),
            ))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        server.abort();

        assert_eq!(response.status(), StatusCode::OK);
        let body = captured.lock().unwrap().clone().unwrap();
        assert_eq!(body["model"], "grok-4.6");
        let output = body["input"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["type"] == "function_call_output")
            .unwrap()["output"]
            .as_str()
            .unwrap();
        assert_eq!(
            output.len(),
            20_000,
            "Grok strategy must retain 20KB output"
        );
    }

    /// GPT Responses 在请求前剥离格式无效的 encrypted_content，避免触发 400 重试。
    #[tokio::test]
    async fn test_openai_responses_sanitizes_invalid_encrypted_content_before_request() {
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let handler_attempts = Arc::clone(&attempts);
        let saw_encrypted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let saw_trimmed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let handler_encrypted = Arc::clone(&saw_encrypted);
        let handler_trimmed = Arc::clone(&saw_trimmed);
        let upstream = Router::new().route(
            "/responses",
            post(move |body: axum::body::Bytes| {
                let attempts = Arc::clone(&handler_attempts);
                let saw_encrypted = Arc::clone(&handler_encrypted);
                let saw_trimmed = Arc::clone(&handler_trimmed);
                async move {
                    attempts.fetch_add(1, Ordering::SeqCst);
                    let parsed: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
                    let has_enc = parsed
                        .get("input")
                        .and_then(|v| v.as_array())
                        .map(|items| {
                            items.iter().any(|item| {
                                item.get("type").and_then(|t| t.as_str()) == Some("reasoning")
                                    && item.get("encrypted_content").is_some()
                            })
                        })
                        .unwrap_or(false);
                    if has_enc {
                        saw_encrypted.store(true, Ordering::SeqCst);
                        (
                            StatusCode::BAD_REQUEST,
                            [(header::CONTENT_TYPE, "application/json")],
                            json!({
                                "error": {
                                    "type": "invalid_request_error",
                                    "code": "invalid_encrypted_content",
                                    "message": "invalid_encrypted_content"
                                }
                            })
                            .to_string(),
                        )
                    } else {
                        saw_trimmed.store(true, Ordering::SeqCst);
                        (
                            StatusCode::OK,
                            [(header::CONTENT_TYPE, "application/json")],
                            json!({
                                "type": "response.completed",
                                "response": {
                                    "id": "resp_trim",
                                    "model": "gpt-5",
                                    "output": [{
                                        "type": "message",
                                        "content": [{"type": "output_text", "text": "trimmed ok"}]
                                    }],
                                    "usage": {"input_tokens": 1, "output_tokens": 1}
                                }
                            })
                            .to_string(),
                        )
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, upstream).await.unwrap();
        });

        let state = mock_state();
        let provider_yaml = format!(
            r#"
name: test-responses-trim
protocol: openai_responses
base_url: "http://{}"
key: sk-test
proxy_url: "direct"
models:
  - name: gpt-5
    alias: test-responses-trim
"#,
            upstream_addr
        );
        let provider: ProviderConfig = serde_yaml::from_str(&provider_yaml).unwrap();
        state.providers.write().await.push(provider);
        let app = app(state);
        let request = Request::builder()
            .uri("/v1/messages")
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::to_vec(&json!({
                    "model": "test-responses-trim",
                    "max_tokens": 64,
                    "stream": false,
                    "messages": [
                        {"role": "assistant", "content": [
                            {"type": "thinking", "thinking": "t", "signature": "gAAAA-replay"}
                        ]},
                        {"role": "user", "content": "hi"}
                    ]
                }))
                .unwrap(),
            ))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        let status = response.status();
        let response_body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        server.abort();

        assert_eq!(attempts.load(Ordering::SeqCst), 1);
        assert!(
            !saw_encrypted.load(Ordering::SeqCst),
            "首次请求不应带 encrypted_content"
        );
        assert!(
            saw_trimmed.load(Ordering::SeqCst),
            "请求应在发送前剥离无效 encrypted_content"
        );
        assert_eq!(status, StatusCode::OK);
        assert!(String::from_utf8_lossy(&response_body).contains("trimmed ok"));
    }

    #[tokio::test]
    async fn test_openai_responses_retries_valid_encrypted_content_after_upstream_rejection() {
        const VALID: &str = "gAAAAAAAAAAAAQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyAhIiMkJSYnKCkqKywtLi8wMTIzNDU2Nzg5Ojs8PT4_QA";
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let saw_encrypted = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let saw_trimmed = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let handler_attempts = Arc::clone(&attempts);
        let handler_encrypted = Arc::clone(&saw_encrypted);
        let handler_trimmed = Arc::clone(&saw_trimmed);
        let upstream = Router::new().route(
            "/responses",
            post(move |body: axum::body::Bytes| {
                let attempts = Arc::clone(&handler_attempts);
                let saw_encrypted = Arc::clone(&handler_encrypted);
                let saw_trimmed = Arc::clone(&handler_trimmed);
                async move {
                    let parsed: Value = serde_json::from_slice(&body).unwrap();
                    let has_encrypted = parsed["input"].as_array().unwrap().iter().any(|item| {
                        item["type"] == "reasoning" && item.get("encrypted_content").is_some()
                    });
                    if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                        saw_encrypted.store(has_encrypted, Ordering::SeqCst);
                        (
                            StatusCode::BAD_REQUEST,
                            [(header::CONTENT_TYPE, "application/json")],
                            r#"{"error":{"code":"invalid_encrypted_content"}}"#,
                        )
                    } else {
                        saw_trimmed.store(!has_encrypted, Ordering::SeqCst);
                        (
                            StatusCode::OK,
                            [(header::CONTENT_TYPE, "application/json")],
                            r#"{"type":"response.completed","response":{"id":"resp_retry","model":"gpt-5","output":[],"usage":{"input_tokens":1,"output_tokens":1}}}"#,
                        )
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });

        let state = mock_state();
        let provider: ProviderConfig = serde_yaml::from_str(&format!(
            "name: test-responses-retry\nprotocol: openai_responses\nbase_url: http://{}\nkey: sk-test\nproxy_url: direct\nmodels:\n  - name: gpt-5\n    alias: test-responses-retry\n",
            upstream_addr
        ))
        .unwrap();
        state.providers.write().await.push(provider);
        let app = app(state);
        let request = Request::builder()
            .uri("/v1/messages")
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::to_vec(&json!({
                    "model": "test-responses-retry", "max_tokens": 64, "stream": false,
                    "messages": [
                        {"role": "assistant", "content": [{"type": "thinking", "thinking": "t", "signature": VALID}]},
                        {"role": "user", "content": "hi"}
                    ]
                }))
                .unwrap(),
            ))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        server.abort();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
        assert!(saw_encrypted.load(Ordering::SeqCst));
        assert!(saw_trimmed.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn test_openai_responses_retries_on_422_invalid_encrypted_content() {
        const VALID: &str = "gAAAAAAAAAAAAQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyAhIiMkJSYnKCkqKywtLi8wMTIzNDU2Nzg5Ojs8PT4_QA";
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let handler_attempts = Arc::clone(&attempts);
        let upstream = Router::new().route(
            "/responses",
            post(move || {
                let attempts = Arc::clone(&handler_attempts);
                async move {
                    if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                        (
                            StatusCode::UNPROCESSABLE_ENTITY,
                            [(header::CONTENT_TYPE, "application/json")],
                            r#"{"error":{"code":"invalid_encrypted_content","message":"could not decrypt"}}"#,
                        )
                    } else {
                        (
                            StatusCode::OK,
                            [(header::CONTENT_TYPE, "application/json")],
                            r#"{"type":"response.completed","response":{"id":"resp_422_ok","model":"gpt-5","output":[],"usage":{"input_tokens":1,"output_tokens":1}}}"#,
                        )
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });

        let state = mock_state();
        let provider: ProviderConfig = serde_yaml::from_str(&format!(
            "name: test-422-retry\nprotocol: openai_responses\nbase_url: http://{}\nkey: sk-test\nproxy_url: direct\nmodels:\n  - name: gpt-5\n    alias: test-422-retry\n",
            upstream_addr
        ))
        .unwrap();
        state.providers.write().await.push(provider);
        let app = app(state);
        let request = Request::builder()
            .uri("/v1/messages")
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::to_vec(&json!({
                    "model": "test-422-retry", "max_tokens": 64, "stream": false,
                    "messages": [
                        {"role": "assistant", "content": [{"type": "thinking", "thinking": "t", "signature": VALID}]},
                        {"role": "user", "content": "hi"}
                    ]
                }))
                .unwrap(),
            ))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        server.abort();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn test_grok_model_triggers_loop_recovery_reminder_non_grok_skips() {
        let captured = Arc::new(StdMutex::new(None));
        let handler_captured = Arc::clone(&captured);
        let upstream = Router::new().route(
            "/responses",
            post(move |body: axum::body::Bytes| {
                let captured = Arc::clone(&handler_captured);
                async move {
                    *captured.lock().unwrap() = Some(serde_json::from_slice::<Value>(&body).unwrap());
                    (
                        StatusCode::OK,
                        [(header::CONTENT_TYPE, "application/json")],
                        r#"{"type":"response.completed","response":{"id":"resp_loop_test","model":"grok-4","output":[],"usage":{"input_tokens":1,"output_tokens":1}}}"#,
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });

        let state = mock_state();
        let provider: ProviderConfig = serde_yaml::from_str(&format!(
            "name: test-grok-provider\nprotocol: openai_responses\nbase_url: http://{}\nkey: sk-test\nproxy_url: direct\nmodels:\n  - name: grok-4\n    alias: my-grok\n  - name: gpt-5\n    alias: my-gpt\n",
            upstream_addr
        ))
        .unwrap();
        state.providers.write().await.push(provider);
        let app = app(state);

        let repeating_messages = json!([
            {
                "role": "assistant",
                "content": [{"type": "tool_use", "id": "call_1", "name": "Read", "input": {"path": "a"}}]
            },
            {
                "role": "user",
                "content": [{"type": "tool_result", "tool_use_id": "call_1", "content": ""}]
            },
            {
                "role": "assistant",
                "content": [{"type": "tool_use", "id": "call_2", "name": "Read", "input": {"path": "a"}}]
            },
            {
                "role": "user",
                "content": [{"type": "tool_result", "tool_use_id": "call_2", "content": ""}]
            },
            {
                "role": "assistant",
                "content": [{"type": "tool_use", "id": "call_3", "name": "Read", "input": {"path": "a"}}]
            },
            {
                "role": "user",
                "content": [{"type": "tool_result", "tool_use_id": "call_3", "content": ""}]
            },
            {
                "role": "assistant",
                "content": [{"type": "tool_use", "id": "call_4", "name": "Read", "input": {"path": "a"}}]
            },
            {
                "role": "user",
                "content": [{"type": "tool_result", "tool_use_id": "call_4", "content": ""}]
            }
        ]);

        // 1. 请求 grok 模型 -> 触发注入 RECOVERY_REMINDER
        let request = Request::builder()
            .uri("/v1/messages")
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::to_vec(&json!({
                    "model": "my-grok",
                    "max_tokens": 64,
                    "stream": false,
                    "messages": repeating_messages
                }))
                .unwrap(),
            ))
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = captured.lock().unwrap().take().unwrap();
        let body_str = body.to_string();
        assert!(body_str.contains("Your messages have been flagged as looping"));

        // 2. 请求 gpt 模型 -> 不应触发注入
        let request = Request::builder()
            .uri("/v1/messages")
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::to_vec(&json!({
                    "model": "my-gpt",
                    "max_tokens": 64,
                    "stream": false,
                    "messages": repeating_messages
                }))
                .unwrap(),
            ))
            .unwrap();
        let response = app.clone().oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let body = captured.lock().unwrap().take().unwrap();
        let body_str = body.to_string();
        assert!(!body_str.contains("Your messages have been flagged as looping"));

        // 3. 请求 grok 模型且达到 HardStop 阈值 (8次相同 Read) -> 直接熔断结束回合
        let mut hard_stop_messages = Vec::new();
        for i in 0..8 {
            hard_stop_messages.push(json!({
                "role": "assistant",
                "content": [{"type": "tool_use", "id": format!("c_{i}"), "name": "Read", "input": {"file": "a"}}]
            }));
            hard_stop_messages.push(json!({
                "role": "user",
                "content": [{"type": "tool_result", "tool_use_id": format!("c_{i}"), "content": ""}]
            }));
        }
        let request = Request::builder()
            .uri("/v1/messages")
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::to_vec(&json!({
                    "model": "my-grok",
                    "max_tokens": 64,
                    "stream": false,
                    "messages": hard_stop_messages
                }))
                .unwrap(),
            ))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let resp_bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
        let resp_json: Value = serde_json::from_slice(&resp_bytes).unwrap();
        assert_eq!(resp_json["stop_reason"], "end_turn");
        assert!(resp_json["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Halting turn"));

        server.abort();
    }

    #[tokio::test]
    async fn test_gpt_responses_strips_execute_only_payload_fields() {
        let captured: Arc<StdMutex<Option<Value>>> = Arc::new(StdMutex::new(None));
        let handler_captured = Arc::clone(&captured);
        let upstream = Router::new().route(
            "/responses",
            post(move |body: axum::body::Bytes| {
                let captured = Arc::clone(&handler_captured);
                async move {
                    *captured.lock().unwrap() = Some(serde_json::from_slice(&body).unwrap());
                    (
                        [(header::CONTENT_TYPE, "application/json")],
                        json!({
                            "type": "response.completed",
                            "response": {
                                "id": "resp_final",
                                "model": "gpt-5",
                                "output": [],
                                "usage": {"input_tokens": 1, "output_tokens": 1}
                            }
                        })
                        .to_string(),
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, upstream).await.unwrap();
        });

        let state = mock_state();
        let provider: ProviderConfig = serde_yaml::from_str(&format!(
            r#"
name: test-responses-final
protocol: openai_responses
base_url: "http://{}"
key: sk-test
proxy_url: "direct"
models:
  - name: gpt-5
    alias: test-responses-final
"#,
            upstream_addr
        ))
        .unwrap();
        state.providers.write().await.push(provider);
        state.payload_rules.write().await.push(PayloadRule {
            models: vec!["test-responses-final".into()],
            protocol: Some(Protocol::OpenAiResponses),
            params: json!({
                "previous_response_id": "resp_previous",
                "generate": true,
                "safety_identifier": "safe-id",
                "stream_options": {"include_usage": true},
                "prompt_cache_retention": "24h",
                "temperature": 0.1
            })
            .as_object()
            .unwrap()
            .clone(),
        });
        let app = app(state);
        let request = Request::builder()
            .uri("/v1/messages")
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::to_vec(&json!({
                    "model": "test-responses-final",
                    "max_tokens": 64,
                    "messages": [{"role": "user", "content": "hi"}]
                }))
                .unwrap(),
            ))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        server.abort();

        assert_eq!(response.status(), StatusCode::OK);
        let body = captured.lock().unwrap().clone().unwrap();
        for key in [
            "previous_response_id",
            "generate",
            "safety_identifier",
            "stream_options",
            "prompt_cache_retention",
        ] {
            assert!(body.get(key).is_none(), "{key} 不应发往 GPT Responses");
        }
        assert_eq!(body["temperature"], 0.1);
    }

    #[tokio::test]
    async fn test_health_check() {
        let app = app(mock_state());
        let req = Request::builder()
            .uri("/health")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), 1024).await.unwrap();
        assert_eq!(body, "ok");
    }

    #[tokio::test]
    async fn test_missing_model_field() {
        let app = app(mock_state());
        let body_json = json!({"messages": [{"role": "user", "content": "hi"}]});
        let req = Request::builder()
            .uri("/v1/messages")
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body_json).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        let body = to_bytes(resp.into_body(), 1024).await.unwrap();
        let body_json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body_json["type"], "error");
        assert_eq!(body_json["error"]["type"], "invalid_request_error");
        assert!(body_json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("缺少 model 字段"));
    }

    #[tokio::test]
    async fn test_invalid_json() {
        let app = app(mock_state());
        let req = Request::builder()
            .uri("/v1/messages")
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from("{invalid json"))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

        let body = to_bytes(resp.into_body(), 1024).await.unwrap();
        let body_json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body_json["type"], "error");
        assert_eq!(body_json["error"]["type"], "invalid_request_error");
        assert!(body_json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("JSON 解析失败"));
    }

    #[tokio::test]
    async fn test_model_not_found() {
        let app = app(mock_state());
        let body_json = json!({
            "model": "unknown-model",
            "messages": [{"role": "user", "content": "hi"}]
        });
        let req = Request::builder()
            .uri("/v1/messages")
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body_json).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::NOT_FOUND);

        let body = to_bytes(resp.into_body(), 1024).await.unwrap();
        let body_json: Value = serde_json::from_slice(&body).unwrap();
        assert_eq!(body_json["type"], "error");
        assert_eq!(body_json["error"]["type"], "not_found_error");
        assert!(body_json["error"]["message"]
            .as_str()
            .unwrap()
            .contains("未找到"));
    }

    #[test]
    fn test_payload_override_wildcard() {
        let mut body = json!({"model": "glm-5.1", "max_tokens": 1024});
        let rules = vec![PayloadRule {
            models: vec!["*glm*".into()],
            protocol: Some(Protocol::OpenAiChat),
            params: {
                let mut map = serde_json::Map::new();
                map.insert("max_tokens".into(), json!(32000));
                map.insert("temperature".into(), json!(0.1));
                map
            },
        }];
        apply_payload_overrides(&mut body, "glm-5.1", Protocol::OpenAiChat, &rules);
        assert_eq!(body["max_tokens"], 32000);
        assert_eq!(body["temperature"], 0.1);
    }

    #[test]
    fn test_payload_override_protocol_gate() {
        // 规则限定 openai_chat,claude 直通不应注入
        let mut body = json!({"model": "glm-5.1", "max_tokens": 1024});
        let rules = vec![PayloadRule {
            models: vec!["*glm*".into()],
            protocol: Some(Protocol::OpenAiChat),
            params: {
                let mut map = serde_json::Map::new();
                map.insert("tool_stream".into(), json!(true));
                map
            },
        }];
        apply_payload_overrides(&mut body, "glm-5.1", Protocol::Claude, &rules);
        assert!(body.get("tool_stream").is_none());
        assert_eq!(body["max_tokens"], 1024);
    }

    #[test]
    fn test_payload_override_claude_requires_explicit_protocol() {
        // 无 protocol 字段的规则作用于 claude 直通:默认不注入
        let mut body = json!({"model": "evol-opus-5", "max_tokens": 1024});
        let rules = vec![PayloadRule {
            models: vec!["*evol*".into()],
            protocol: None,
            params: {
                let mut map = serde_json::Map::new();
                map.insert("max_tokens".into(), json!(32000));
                map
            },
        }];
        apply_payload_overrides(&mut body, "evol-opus-5", Protocol::Claude, &rules);
        assert_eq!(
            body["max_tokens"], 1024,
            "claude 直通无 protocol 规则不注入"
        );

        // 显式声明 protocol: claude → 注入
        let rules2 = vec![PayloadRule {
            models: vec!["*evol*".into()],
            protocol: Some(Protocol::Claude),
            params: {
                let mut map = serde_json::Map::new();
                map.insert("max_tokens".into(), json!(32000));
                map
            },
        }];
        apply_payload_overrides(&mut body, "evol-opus-5", Protocol::Claude, &rules2);
        assert_eq!(body["max_tokens"], 32000, "显式 claude 协议应注入");
    }

    #[test]
    fn test_payload_override_no_match() {
        let mut body = json!({"model": "claude-opus", "max_tokens": 1024});
        let rules = vec![PayloadRule {
            models: vec!["*glm*".into()],
            protocol: None,
            params: {
                let mut map = serde_json::Map::new();
                map.insert("max_tokens".into(), json!(32000));
                map
            },
        }];
        apply_payload_overrides(&mut body, "claude-opus", Protocol::OpenAiChat, &rules);
        assert_eq!(body["max_tokens"], 1024);
    }

    #[test]
    fn test_payload_override_star_matches_all() {
        let mut body = json!({"model": "any-model"});
        let rules = vec![PayloadRule {
            models: vec!["*".into()],
            protocol: None,
            params: {
                let mut map = serde_json::Map::new();
                map.insert("temperature".into(), json!(0.5));
                map
            },
        }];
        apply_payload_overrides(&mut body, "any-model", Protocol::OpenAiChat, &rules);
        assert_eq!(body["temperature"], 0.5);
    }

    #[tokio::test]
    async fn test_models_endpoint() {
        let app = app(mock_state());
        let req = Request::builder()
            .uri("/v1/models")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), 1024).await.unwrap();
        let json: Value = serde_json::from_slice(&body).unwrap();
        let data = json["data"].as_array().unwrap();
        assert_eq!(data.len(), 2);
        assert_eq!(data[0]["id"], "test-opus");
        assert_eq!(data[0]["object"], "model");
        assert_eq!(data[0]["owned_by"], "test-claude");
        assert_eq!(data[0]["display_name"], "test-opus");
        assert_eq!(data[1]["id"], "test-gpt");
    }

    #[test]
    fn test_build_models_list() {
        let providers_yaml = r#"
- name: p1
  protocol: claude
  base_url: "https://x.com"
  key: k
  models:
    - name: upstream-a
      alias: alias-a
- name: p2
  protocol: openai_chat
  base_url: "https://y.com"
  key: k
  models:
    - name: upstream-b
      alias: alias-b
    - name: upstream-c
      alias: alias-c
"#;
        let providers: Vec<ProviderConfig> = serde_yaml::from_str(providers_yaml).unwrap();
        let json = build_models_list(&providers);
        let data = json["data"].as_array().unwrap();
        assert_eq!(data.len(), 3);
        assert_eq!(data[0]["id"], "alias-a");
        assert_eq!(data[1]["id"], "alias-b");
        assert_eq!(data[2]["id"], "alias-c");
        assert_eq!(data[2]["owned_by"], "p2");
    }

    #[test]
    fn test_build_models_list_custom_context() {
        let providers_yaml = r#"
- name: p1
  protocol: claude
  base_url: "https://x.com"
  key: k
  models:
    - name: upstream-a
      alias: alias-a
      max_input_tokens: 128000
      max_tokens: 32000
"#;
        let providers: Vec<ProviderConfig> = serde_yaml::from_str(providers_yaml).unwrap();
        let json = build_models_list(&providers);
        let data = json["data"].as_array().unwrap();
        assert_eq!(data[0]["max_input_tokens"], 128000);
        assert_eq!(data[0]["max_tokens"], 32000);
    }

    #[test]
    fn test_build_models_list_default_context() {
        let providers_yaml = r#"
- name: p1
  protocol: claude
  base_url: "https://x.com"
  key: k
  models:
    - name: upstream-a
      alias: alias-a
"#;
        let providers: Vec<ProviderConfig> = serde_yaml::from_str(providers_yaml).unwrap();
        let json = build_models_list(&providers);
        let data = json["data"].as_array().unwrap();
        assert_eq!(data[0]["max_input_tokens"], 200000);
        assert_eq!(data[0]["max_tokens"], 64000);
    }

    #[test]
    fn test_check_secret_ok() {
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", "s3cret".parse().unwrap());
        assert!(check_secret(&headers, &Some("s3cret".into())).is_ok());
    }

    #[test]
    fn test_check_secret_missing() {
        let headers = HeaderMap::new();
        assert!(check_secret(&headers, &Some("s3cret".into())).is_err());
    }

    #[test]
    fn test_check_secret_wrong() {
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", "wrong".parse().unwrap());
        assert!(check_secret(&headers, &Some("s3cret".into())).is_err());
    }

    #[test]
    fn test_check_secret_disabled() {
        let headers = HeaderMap::new();
        assert!(check_secret(&headers, &None).is_ok());
    }

    #[test]
    fn test_check_secret_bearer() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer s3cret".parse().unwrap());
        assert!(check_secret(&headers, &Some("s3cret".into())).is_ok());
    }

    #[test]
    fn test_check_secret_bearer_wrong() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer wrong".parse().unwrap());
        assert!(check_secret(&headers, &Some("s3cret".into())).is_err());
    }

    #[test]
    fn test_extract_key_bearer() {
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "Bearer tok".parse().unwrap());
        assert_eq!(extract_key(&headers), "tok");
    }

    #[test]
    fn test_extract_key_bearer_case_and_whitespace() {
        // RFC 6750:scheme 大小写不敏感,容忍多余空白
        let mut headers = HeaderMap::new();
        headers.insert("authorization", "bearer     tok".parse().unwrap());
        assert_eq!(extract_key(&headers), "tok");
    }

    #[test]
    fn test_extract_key_prefers_x_api_key() {
        let mut headers = HeaderMap::new();
        headers.insert("x-api-key", "xkey".parse().unwrap());
        headers.insert("authorization", "Bearer bkey".parse().unwrap());
        assert_eq!(extract_key(&headers), "xkey");
    }

    #[tokio::test]
    async fn test_models_requires_secret() {
        let state = mock_state();
        state.runtime.write().await.secret = Some("s3cret".into());
        let app = app(state);
        // 无 key → 401
        let req = Request::builder()
            .uri("/v1/models")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        // 带 key → OK
        let req = Request::builder()
            .uri("/v1/models")
            .header("x-api-key", "s3cret")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
    }

    /// 构造带指定 secret 的 reload 闭包(providers 保持空,validate 必过)
    fn reload_returning_secret(secret: Option<String>) -> ReloadFn {
        Arc::new(move || {
            let secret = secret.clone();
            Box::pin(async move {
                Ok(ReloadData {
                    providers: vec![],
                    payload_rules: vec![],
                    normalize: NormalizeConfig {
                        enabled: false,
                        drift_detector: false,
                    },
                    logging: LoggingConfig {
                        level: "info".into(),
                        request_body: false,
                    },
                    secret,
                    proxy_url: None,
                    user_agents: test_user_agents(),
                })
            })
        })
    }

    /// /reload 真正把新 secret 装进 RuntimeConfig:重载前无 key 放行,
    /// 重载后同样请求应 401。删掉 handle_reload 里的 runtime 写入则失败。
    #[tokio::test]
    async fn test_reload_applies_new_secret() {
        let mut state = mock_state();
        assert!(state.runtime.read().await.secret.is_none());
        state.reload = reload_returning_secret(Some("sk-after-reload".into()));
        let app = app(state);

        let models_req = || {
            Request::builder()
                .uri("/v1/models")
                .body(Body::empty())
                .unwrap()
        };
        let before = app.clone().oneshot(models_req()).await.unwrap();
        assert_eq!(before.status(), StatusCode::OK, "重载前无 secret 应放行");

        let reload = Request::builder()
            .uri("/reload")
            .method("POST")
            .body(Body::empty())
            .unwrap();
        let resp = app.clone().oneshot(reload).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK, "/reload 应成功");

        let after = app.clone().oneshot(models_req()).await.unwrap();
        assert_eq!(
            after.status(),
            StatusCode::UNAUTHORIZED,
            "重载后新 secret 应生效"
        );
        let ok = Request::builder()
            .uri("/v1/models")
            .header("x-api-key", "sk-after-reload")
            .body(Body::empty())
            .unwrap();
        assert_eq!(app.oneshot(ok).await.unwrap().status(), StatusCode::OK);
    }

    /// /reload 清空 bcrypt 校验缓存:旧 secret 的 hash 命中过缓存后,
    /// 重载换新 secret,旧明文 key 不得再通过。
    #[tokio::test]
    async fn test_reload_clears_auth_cache() {
        let old_hash = bcrypt::hash("sk-old", 4).unwrap();
        let new_hash = bcrypt::hash("sk-new", 4).unwrap();
        let mut state = mock_state();
        state.runtime.write().await.secret = Some(old_hash);
        state.reload = reload_returning_secret(Some(new_hash));
        let app = app(state);

        let with_key = |k: &str| {
            Request::builder()
                .uri("/v1/models")
                .header("x-api-key", k)
                .body(Body::empty())
                .unwrap()
        };
        // 先命中一次,把 sk-old→true 写进 AUTH_CACHE
        let r = app.clone().oneshot(with_key("sk-old")).await.unwrap();
        assert_eq!(r.status(), StatusCode::OK);

        let reload = Request::builder()
            .uri("/reload")
            .method("POST")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app.clone().oneshot(reload).await.unwrap().status(),
            StatusCode::OK
        );

        let stale = app.clone().oneshot(with_key("sk-old")).await.unwrap();
        assert_eq!(
            stale.status(),
            StatusCode::UNAUTHORIZED,
            "旧 key 的缓存结果应随 /reload 作废"
        );
        let fresh = app.oneshot(with_key("sk-new")).await.unwrap();
        assert_eq!(fresh.status(), StatusCode::OK, "新 secret 应校验通过");
    }

    /// /reload 替换 normalize 与全局代理:新 UpstreamClient 带上新 proxy_url。
    #[tokio::test]
    async fn test_reload_applies_normalize_and_proxy() {
        let mut state = mock_state();
        state.reload = Arc::new(|| {
            Box::pin(async {
                Ok(ReloadData {
                    providers: vec![],
                    payload_rules: vec![],
                    normalize: NormalizeConfig {
                        enabled: true,
                        drift_detector: true,
                    },
                    logging: LoggingConfig {
                        level: "debug".into(),
                        request_body: true,
                    },
                    secret: None,
                    proxy_url: Some("socks5://127.0.0.1:1080".into()),
                    user_agents: test_user_agents(),
                })
            })
        });
        let runtime = state.runtime.clone();
        let req = Request::builder()
            .uri("/reload")
            .method("POST")
            .body(Body::empty())
            .unwrap();
        assert_eq!(
            app(state).oneshot(req).await.unwrap().status(),
            StatusCode::OK
        );

        let rt = runtime.read().await;
        assert!(rt.normalize.enabled, "normalize 应随 /reload 更新");
        assert!(rt.normalize.drift_detector);
        assert!(rt.logging.request_body, "logging 应随 /reload 更新");
        assert_eq!(
            rt.upstream.resolve_proxy_for_test(None),
            "socks5://127.0.0.1:1080",
            "全局代理应随 /reload 生效"
        );
    }

    #[tokio::test]
    async fn test_reload_endpoint() {
        let app = app(mock_state());
        let req = Request::builder()
            .uri("/reload")
            .method("POST")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        // reload 会失败(mock reload 返回空 providers),但端点应响应
        assert!(resp.status() == StatusCode::OK || resp.status().is_server_error());
    }

    /// 验证 handle_messages 在上游请求期间不持有配置读锁:
    /// 慢上游(2s 延迟)进行中时,/reload 应能在 500ms 内完成。
    #[tokio::test]
    async fn test_reload_completes_while_request_inflight() {
        use tokio::io::AsyncWriteExt;
        use tokio::net::TcpListener;

        // 慢上游:接受连接后延迟 2s 才写响应头
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            if let Ok((mut sock, _)) = listener.accept().await {
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                let _ = sock
                    .write_all(
                        b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 2\r\n\r\n{}",
                    )
                    .await;
            }
        });

        let mut state = mock_state();
        // 让 test-claude 指向慢 mock
        state.providers.write().await[0].set_base_url_for_test(format!("http://{addr}"));

        state.reload = reload_returning_secret(None);

        let app = app(state);

        // 发起进行中请求,不 await 完成
        let inflight_app = app.clone();
        tokio::spawn(async move {
            let req = Request::builder()
                .uri("/v1/messages")
                .method("POST")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({
                        "model": "test-opus",
                        "stream": false,
                        "messages": [{"role": "user", "content": "hi"}]
                    })
                    .to_string(),
                ))
                .unwrap();
            let _ = inflight_app.oneshot(req).await;
        });

        // 等 handler 进入上游请求阶段(已过路由决策)
        tokio::time::sleep(std::time::Duration::from_millis(300)).await;

        // /reload 应在 500ms 内完成,不被上游请求的锁阻塞
        let reload_req = Request::builder()
            .uri("/reload")
            .method("POST")
            .body(Body::empty())
            .unwrap();
        let result = tokio::time::timeout(
            std::time::Duration::from_millis(500),
            app.oneshot(reload_req),
        )
        .await;
        assert!(
            result.is_ok(),
            "/reload 超时:handle_messages 在上游请求期间仍持有配置读锁"
        );
    }

    // ── to_anthropic_error 上游错误透传 ─────────────────────────────────

    #[test]
    fn upstream_error_openai_standard_shape() {
        // OpenAI 标准 {"error":{type,message}},透传 type 并映射
        let body = br#"{"error":{"type":"rate_limit_error","message":"You are sending requests too quickly"}}"#;
        let out: Value = serde_json::from_slice(&to_anthropic_error(body)).unwrap();
        assert_eq!(out["error"]["type"], "rate_limit_error");
        assert_eq!(
            out["error"]["message"],
            "You are sending requests too quickly"
        );
    }

    #[test]
    fn upstream_error_bailian_nested_message() {
        // 阿里云百炼:code/message 平铺,messages 是嵌套 JSON 字符串
        let body = br#"{"code":"Throttling.RateQuota","message":"{\"error\":{\"message\":\"The engine is currently overloaded, please try again later\",\"type\":\"EngineOverloadedError\",\"param\":null,\"code\":\"EngineOverloadedError\"}}"}"#;
        let out: Value = serde_json::from_slice(&to_anthropic_error(body)).unwrap();
        // EngineOverloadedError 含 overload → overloaded_error
        assert_eq!(out["error"]["type"], "overloaded_error");
        assert_eq!(
            out["error"]["message"],
            "The engine is currently overloaded, please try again later"
        );
    }

    #[test]
    fn upstream_error_bailian_plain_code_message() {
        // code 含 quota → rate_limit_error;message 为普通字符串直接透传
        let body = br#"{"code":"Throttling.RateQuota","message":"limit exceeded"}"#;
        let out: Value = serde_json::from_slice(&to_anthropic_error(body)).unwrap();
        assert_eq!(out["error"]["type"], "rate_limit_error");
        assert_eq!(out["error"]["message"], "limit exceeded");
    }

    #[test]
    fn upstream_error_bailian_invalid_auth_code() {
        // code 含 auth → authentication_error
        let body = br#"{"code":"InvalidApiKey","message":"invalid api key"}"#;
        let out: Value = serde_json::from_slice(&to_anthropic_error(body)).unwrap();
        assert_eq!(out["error"]["type"], "authentication_error");
        assert_eq!(out["error"]["message"], "invalid api key");
    }

    #[test]
    fn upstream_error_unparseable_falls_back_to_raw() {
        // 非 JSON body:兜底 "upstream error"
        let out: Value =
            serde_json::from_slice(&to_anthropic_error(b"<html>502 bad gateway</html>")).unwrap();
        assert_eq!(out["error"]["type"], "api_error");
        assert_eq!(out["error"]["message"], "<html>502 bad gateway</html>");
    }

    #[derive(Clone, Default)]
    struct CapturedUpstream {
        headers: Arc<StdMutex<Option<HeaderMap>>>,
        body: Arc<StdMutex<Option<Value>>>,
    }

    impl CapturedUpstream {
        fn record(&self, headers: HeaderMap, body: Bytes) {
            *self.headers.lock().unwrap() = Some(headers);
            *self.body.lock().unwrap() = serde_json::from_slice(&body).ok();
        }
        fn header(&self, name: &str) -> Option<String> {
            self.headers.lock().unwrap().as_ref().and_then(|h| {
                h.get(name)
                    .and_then(|v| v.to_str().ok())
                    .map(str::to_string)
            })
        }
        fn header_values(&self, name: &str) -> Vec<String> {
            self.headers
                .lock()
                .unwrap()
                .as_ref()
                .map(|headers| {
                    headers
                        .get_all(name)
                        .iter()
                        .filter_map(|value| value.to_str().ok().map(str::to_string))
                        .collect()
                })
                .unwrap_or_default()
        }
        fn has_header(&self, name: &str) -> bool {
            self.headers
                .lock()
                .unwrap()
                .as_ref()
                .map(|h| h.contains_key(name))
                .unwrap_or(false)
        }
        fn body(&self) -> Value {
            self.body.lock().unwrap().clone().unwrap_or(json!({}))
        }
    }

    #[tokio::test]
    async fn test_claude_relay_forwards_headers_and_overrides_auth() {
        let captured = CapturedUpstream::default();
        let handler_cap = captured.clone();
        let upstream = Router::new().route(
            "/v1/messages",
            post(move |headers: HeaderMap, body: Bytes| {
                let captured = handler_cap.clone();
                async move {
                    captured.record(headers, body);
                    (
                        StatusCode::OK,
                        [(header::CONTENT_TYPE, "application/json")],
                        "{}",
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, upstream).await.unwrap();
        });

        let state = mock_state();
        state.providers.write().await[0].set_base_url_for_test(format!("http://{upstream_addr}"));
        let app = app(state);
        let request = Request::builder()
            .uri("/v1/messages")
            .method("POST")
            .header("content-type", "application/json")
            .header("x-custom-header", "custom-value")
            .header("anthropic-beta", "beta-a")
            .header("anthropic-beta", "beta-b")
            .header("x-api-key", "inbound-key")
            .header("authorization", "Bearer inbound")
            .header("user-agent", "claude-code/inbound")
            .header("connection", "x-remove-me")
            .header("x-remove-me", "remove-me")
            .body(Body::from(
                json!({
                    "model": "test-opus",
                    "max_tokens": 64,
                    "stream": false,
                    "messages": [{"role": "user", "content": "hi"}]
                })
                .to_string(),
            ))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        server.abort();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            captured.header("authorization").as_deref(),
            Some("Bearer sk-test")
        );
        assert_eq!(
            captured.header("x-custom-header").as_deref(),
            Some("custom-value")
        );
        assert_eq!(
            captured.header_values("anthropic-beta"),
            vec!["beta-a", "beta-b"]
        );
        assert_eq!(
            captured.header_values("user-agent"),
            vec!["claude-code/inbound"]
        );
        assert!(!captured.has_header("x-api-key"));
        assert!(!captured.has_header("x-remove-me"));
        assert_eq!(captured.body()["model"], "claude-opus-5");
    }

    #[tokio::test]
    async fn test_count_tokens_relay_uses_fallback_user_agent() {
        let captured = CapturedUpstream::default();
        let handler_cap = captured.clone();
        let upstream = Router::new().route(
            "/v1/messages/count_tokens",
            post(move |headers: HeaderMap, body: Bytes| {
                let captured = handler_cap.clone();
                async move {
                    captured.record(headers, body);
                    (
                        StatusCode::OK,
                        [(header::CONTENT_TYPE, "application/json")],
                        r#"{"input_tokens":123}"#,
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, upstream).await.unwrap();
        });

        let state = mock_state();
        state.providers.write().await[0].set_base_url_for_test(format!("http://{upstream_addr}"));
        let app = app(state);
        let request = Request::builder()
            .uri("/v1/messages/count_tokens")
            .method("POST")
            .header("content-type", "application/json")
            .header("x-custom-header", "custom-value")
            .header("anthropic-beta", "beta-a")
            .header("x-api-key", "inbound-key")
            .body(Body::from(
                json!({"model": "test-opus", "messages": []}).to_string(),
            ))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        let status = response.status();
        let body = to_bytes(response.into_body(), 1024).await.unwrap();
        server.abort();

        assert_eq!(status, StatusCode::OK);
        assert_eq!(body, r#"{"input_tokens":123}"#);
        assert_eq!(
            captured.header("authorization").as_deref(),
            Some("Bearer sk-test")
        );
        assert_eq!(
            captured.header("x-custom-header").as_deref(),
            Some("custom-value")
        );
        assert_eq!(captured.header("anthropic-beta").as_deref(), Some("beta-a"));
        assert_eq!(captured.header_values("user-agent"), vec![TEST_CLAUDE_CLI]);
        assert!(!captured.has_header("x-api-key"));
    }

    #[tokio::test]
    async fn test_chat_grok_headers_and_skips_prompt_cache_key() {
        let captured = CapturedUpstream::default();
        let handler_cap = captured.clone();
        let upstream = Router::new().route(
            "/chat/completions",
            post(move |headers: HeaderMap, body: Bytes| {
                let captured = handler_cap.clone();
                async move {
                    captured.record(headers, body);
                    (
                        StatusCode::OK,
                        [(header::CONTENT_TYPE, "application/json")],
                        json!({
                            "id": "chatcmpl_grok",
                            "model": "grok-4.6",
                            "choices": [{
                                "index": 0,
                                "message": {"role": "assistant", "content": "ok"},
                                "finish_reason": "stop"
                            }],
                            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
                        })
                        .to_string(),
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, upstream).await.unwrap();
        });

        let state = mock_state();
        let provider_yaml = format!(
            r#"
name: test-grok-chat
protocol: openai_chat
base_url: "http://{}"
key: sk-test
proxy_url: "direct"
prompt_cache_key: true
models:
  - name: grok-4.6
    alias: test-grok-chat
"#,
            upstream_addr
        );
        let provider: ProviderConfig = serde_yaml::from_str(&provider_yaml).unwrap();
        state.providers.write().await.push(provider);
        let app = app(state);
        let request = Request::builder()
            .uri("/v1/messages")
            .method("POST")
            .header("content-type", "application/json")
            .header("x-claude-code-session-id", "sess-abc-123")
            .body(Body::from(
                serde_json::to_vec(&json!({
                    "model": "test-grok-chat",
                    "max_tokens": 64,
                    "stream": false,
                    "messages": [{"role": "user", "content": "hi"}]
                }))
                .unwrap(),
            ))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        let status = response.status();
        server.abort();

        assert_eq!(status, StatusCode::OK);
        let ua = captured.header("user-agent").unwrap_or_default();
        assert!(
            ua.starts_with("grok-shell/1.0.5 ("),
            "UA 应为 grok-shell,实际 {ua}"
        );
        assert_eq!(
            captured.header("x-xai-token-auth").as_deref(),
            Some("xai-grok-cli")
        );
        assert_eq!(
            captured.header("x-grok-client-version").as_deref(),
            Some("1.0.5")
        );
        assert_eq!(
            captured.header("x-grok-client-identifier").as_deref(),
            Some("grok-shell")
        );
        assert_eq!(
            captured.header("x-grok-model-override").as_deref(),
            Some("grok-4.6")
        );
        assert_eq!(
            captured.header("x-grok-conv-id").as_deref(),
            Some("sess-abc-123")
        );
        assert!(!captured.has_header("x-grok-doom-loop-check"));
        assert!(!captured.has_header("x-grok-req-id"));
        assert!(!captured.has_header("x-grok-session-id"));
        assert!(!captured.has_header("x-grok-agent-id"));
        assert!(!captured.has_header("x-grok-turn-idx"));
        assert!(!captured.has_header("session-id"));
        assert!(!captured.has_header("thread-id"));
        assert!(
            captured.body().get("prompt_cache_key").is_none(),
            "chat+grok 开关开也不注入"
        );
    }

    #[tokio::test]
    async fn test_responses_grok_still_injects_prompt_cache_key() {
        let captured = CapturedUpstream::default();
        let handler_cap = captured.clone();
        let upstream = Router::new().route(
            "/responses",
            post(move |headers: HeaderMap, body: Bytes| {
                let captured = handler_cap.clone();
                async move {
                    captured.record(headers, body);
                    (
                        StatusCode::OK,
                        [(header::CONTENT_TYPE, "application/json")],
                        json!({
                            "type": "response.completed",
                            "response": {
                                "id": "resp_grok",
                                "model": "grok-4.6",
                                "output": [{
                                    "type": "message",
                                    "content": [{"type": "output_text", "text": "ok"}]
                                }],
                                "usage": {"input_tokens": 1, "output_tokens": 1}
                            }
                        })
                        .to_string(),
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, upstream).await.unwrap();
        });

        let state = mock_state();
        let provider_yaml = format!(
            r#"
name: test-grok-responses
protocol: openai_responses
base_url: "http://{}"
key: sk-test
proxy_url: "direct"
prompt_cache_key: true
models:
  - name: grok-4.6
    alias: test-grok-responses
"#,
            upstream_addr
        );
        let provider: ProviderConfig = serde_yaml::from_str(&provider_yaml).unwrap();
        state.providers.write().await.push(provider);
        let app = app(state);
        let request = Request::builder()
            .uri("/v1/messages")
            .method("POST")
            .header("content-type", "application/json")
            .header("x-claude-code-session-id", "sess-abc-123")
            .body(Body::from(
                serde_json::to_vec(&json!({
                    "model": "test-grok-responses",
                    "max_tokens": 64,
                    "stream": false,
                    "messages": [{"role": "user", "content": "hi"}]
                }))
                .unwrap(),
            ))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        let status = response.status();
        server.abort();

        assert_eq!(status, StatusCode::OK);
        let ua = captured.header("user-agent").unwrap_or_default();
        assert!(ua.starts_with("grok-shell/1.0.5 ("));
        assert_eq!(
            captured.header("x-xai-token-auth").as_deref(),
            Some("xai-grok-cli")
        );
        assert_eq!(
            captured.header("x-grok-doom-loop-check").as_deref(),
            Some("1024")
        );
        assert_eq!(
            captured.header("x-grok-conv-id").as_deref(),
            Some("sess-abc-123")
        );
        assert!(!captured.has_header("x-grok-req-id"));
        assert!(!captured.has_header("x-grok-session-id"));
        assert!(!captured.has_header("x-grok-agent-id"));
        assert!(!captured.has_header("x-grok-turn-idx"));
        assert_eq!(captured.body()["prompt_cache_key"], "sess-abc-123");
    }

    #[tokio::test]
    async fn test_chat_payload_override_gpt_to_grok_uses_outbound_model() {
        let captured = CapturedUpstream::default();
        let handler_cap = captured.clone();
        let upstream = Router::new().route(
            "/chat/completions",
            post(move |headers: HeaderMap, body: Bytes| {
                let captured = handler_cap.clone();
                async move {
                    captured.record(headers, body);
                    (
                        StatusCode::OK,
                        [(header::CONTENT_TYPE, "application/json")],
                        json!({
                            "id": "chatcmpl_override",
                            "model": "grok-4.6",
                            "choices": [{
                                "index": 0,
                                "message": {"role": "assistant", "content": "ok"},
                                "finish_reason": "stop"
                            }],
                            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
                        })
                        .to_string(),
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, upstream).await.unwrap();
        });

        let state = mock_state();
        let provider_yaml = format!(
            r#"
name: test-gpt-to-grok
protocol: openai_chat
base_url: "http://{}"
key: sk-test
proxy_url: "direct"
prompt_cache_key: true
models:
  - name: gpt-4
    alias: test-gpt-to-grok
"#,
            upstream_addr
        );
        let provider: ProviderConfig = serde_yaml::from_str(&provider_yaml).unwrap();
        state.providers.write().await.push(provider);
        let mut params = serde_json::Map::new();
        params.insert("model".into(), json!("grok-4.6"));
        state.payload_rules.write().await.push(PayloadRule {
            models: vec!["test-gpt-to-grok".into()],
            protocol: Some(Protocol::OpenAiChat),
            params,
        });
        let app = app(state);
        let request = Request::builder()
            .uri("/v1/messages")
            .method("POST")
            .header("content-type", "application/json")
            .header("x-claude-code-session-id", "sess-abc-123")
            .body(Body::from(
                serde_json::to_vec(&json!({
                    "model": "test-gpt-to-grok",
                    "max_tokens": 64,
                    "stream": false,
                    "messages": [{"role": "user", "content": "hi"}]
                }))
                .unwrap(),
            ))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        let status = response.status();
        server.abort();

        assert_eq!(status, StatusCode::OK);
        assert_eq!(captured.body()["model"], "grok-4.6");
        let ua = captured.header("user-agent").unwrap_or_default();
        assert!(
            ua.starts_with("grok-shell/1.0.5 ("),
            "UA 应为 grok-shell,实际 {ua}"
        );
        assert_eq!(
            captured.header("x-grok-model-override").as_deref(),
            Some("grok-4.6")
        );
        assert_eq!(
            captured.header("x-grok-conv-id").as_deref(),
            Some("sess-abc-123")
        );
        assert!(
            captured.body().get("prompt_cache_key").is_none(),
            "payload 改 grok 后 chat 不注入"
        );
    }

    #[tokio::test]
    async fn test_chat_grok_preserves_inbound_prompt_cache_key() {
        let captured = CapturedUpstream::default();
        let handler_cap = captured.clone();
        let upstream = Router::new().route(
            "/chat/completions",
            post(move |headers: HeaderMap, body: Bytes| {
                let captured = handler_cap.clone();
                async move {
                    captured.record(headers, body);
                    (
                        StatusCode::OK,
                        [(header::CONTENT_TYPE, "application/json")],
                        json!({
                            "id": "chatcmpl_key",
                            "model": "grok-4.6",
                            "choices": [{
                                "index": 0,
                                "message": {"role": "assistant", "content": "ok"},
                                "finish_reason": "stop"
                            }],
                            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
                        })
                        .to_string(),
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, upstream).await.unwrap();
        });

        let state = mock_state();
        let provider_yaml = format!(
            r#"
name: test-grok-chat-key
protocol: openai_chat
base_url: "http://{}"
key: sk-test
proxy_url: "direct"
prompt_cache_key: true
models:
  - name: grok-4.6
    alias: test-grok-chat-key
"#,
            upstream_addr
        );
        let provider: ProviderConfig = serde_yaml::from_str(&provider_yaml).unwrap();
        state.providers.write().await.push(provider);
        let app = app(state);
        let request = Request::builder()
            .uri("/v1/messages")
            .method("POST")
            .header("content-type", "application/json")
            .header("x-claude-code-session-id", "sess-abc-123")
            .body(Body::from(
                serde_json::to_vec(&json!({
                    "model": "test-grok-chat-key",
                    "max_tokens": 64,
                    "stream": false,
                    "prompt_cache_key": "user-key",
                    "messages": [{"role": "user", "content": "hi"}]
                }))
                .unwrap(),
            ))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        let status = response.status();
        server.abort();

        assert_eq!(status, StatusCode::OK);
        assert_eq!(captured.body()["prompt_cache_key"], "user-key");
    }

    #[tokio::test]
    async fn test_responses_preserves_inbound_prompt_cache_key() {
        let captured = CapturedUpstream::default();
        let handler_cap = captured.clone();
        let upstream = Router::new().route(
            "/responses",
            post(move |headers: HeaderMap, body: Bytes| {
                let captured = handler_cap.clone();
                async move {
                    captured.record(headers, body);
                    (
                        StatusCode::OK,
                        [(header::CONTENT_TYPE, "application/json")],
                        json!({
                            "type": "response.completed",
                            "response": {
                                "id": "resp_key",
                                "model": "grok-4.6",
                                "output": [{
                                    "type": "message",
                                    "content": [{"type": "output_text", "text": "ok"}]
                                }],
                                "usage": {"input_tokens": 1, "output_tokens": 1}
                            }
                        })
                        .to_string(),
                    )
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            axum::serve(listener, upstream).await.unwrap();
        });

        let state = mock_state();
        let provider_yaml = format!(
            r#"
name: test-grok-responses-key
protocol: openai_responses
base_url: "http://{}"
key: sk-test
proxy_url: "direct"
prompt_cache_key: true
models:
  - name: grok-4.6
    alias: test-grok-responses-key
"#,
            upstream_addr
        );
        let provider: ProviderConfig = serde_yaml::from_str(&provider_yaml).unwrap();
        state.providers.write().await.push(provider);
        let app = app(state);
        let request = Request::builder()
            .uri("/v1/messages")
            .method("POST")
            .header("content-type", "application/json")
            .header("x-claude-code-session-id", "sess-abc-123")
            .body(Body::from(
                serde_json::to_vec(&json!({
                    "model": "test-grok-responses-key",
                    "max_tokens": 64,
                    "stream": false,
                    "prompt_cache_key": "user-key",
                    "messages": [{"role": "user", "content": "hi"}]
                }))
                .unwrap(),
            ))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        let status = response.status();
        server.abort();

        assert_eq!(status, StatusCode::OK);
        assert_eq!(
            captured.body()["prompt_cache_key"],
            "user-key",
            "入站 key 不改写成 session"
        );
    }

    #[test]
    fn test_cf_retry_after_keeps_jitter_above_general_cap() {
        let started = std::time::Instant::now();
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, "2".parse().unwrap());
        let status = reqwest::StatusCode::from_u16(522).unwrap();

        let delay = compute_retry_delay(0, started, &headers, Some(status)).unwrap();

        assert!(
            delay > RETRY_MAX_DELAY,
            "52x Retry-After 不得被通用上限截断"
        );
        assert!(delay >= std::time::Duration::from_millis(1600));
        assert!(delay <= std::time::Duration::from_millis(2400));
    }

    #[test]
    fn test_retry_delay_respects_retry_after_and_budget() {
        let started = std::time::Instant::now();
        let mut headers = reqwest::header::HeaderMap::new();
        headers.insert(reqwest::header::RETRY_AFTER, "5".parse().unwrap());
        // 429: Retry-After 优先且封顶 1.5s
        let d = compute_retry_delay(
            0,
            started,
            &headers,
            Some(reqwest::StatusCode::TOO_MANY_REQUESTS),
        )
        .unwrap();
        assert_eq!(d, std::time::Duration::from_millis(1500));

        // 无头: 基础 300ms 经 jitter 在 [240ms, 360ms] 范围
        let d2 = compute_retry_delay(0, started, &reqwest::header::HeaderMap::new(), None).unwrap();
        assert!(d2 >= std::time::Duration::from_millis(240));
        assert!(d2 <= std::time::Duration::from_millis(360));

        // 总预算耗尽 → 不再重试
        std::thread::sleep(std::time::Duration::from_millis(20));
        let exhausted_start = started - (RETRY_TOTAL_BUDGET + std::time::Duration::from_secs(1));
        assert!(compute_retry_delay(0, exhausted_start, &headers, None).is_none());
    }

    /// 上游 500:按退避重试后成功;429 带 Retry-After 的路径由同一分支覆盖。
    #[tokio::test]
    async fn test_upstream_500_retries_with_backoff_then_succeeds() {
        let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let handler_attempts = Arc::clone(&attempts);
        let upstream = Router::new().route(
            "/chat/completions",
            post(move || {
                let attempts = Arc::clone(&handler_attempts);
                async move {
                    if attempts.fetch_add(1, Ordering::SeqCst) < 2 {
                        (
                            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                            [(header::CONTENT_TYPE, "application/json")],
                            r#"{"error":{"message":"overloaded"}}"#,
                        )
                    } else {
                        (
                            axum::http::StatusCode::OK,
                            [(header::CONTENT_TYPE, "application/json")],
                            r#"{"id":"c1","object":"chat.completion","model":"gpt-4","choices":[{"index":0,"message":{"role":"assistant","content":"retry ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1}}"#,
                        )
                    }
                }
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });

        let state = mock_state();
        let provider: ProviderConfig = serde_yaml::from_str(&format!(
            "name: test-500-retry\nprotocol: openai_chat\nbase_url: http://{}\nkey: sk-test\nproxy_url: direct\nmodels:\n  - name: gpt-4\n    alias: test-500-retry\n",
            upstream_addr
        ))
        .unwrap();
        state.providers.write().await.push(provider);
        let app = app(state);
        let request = Request::builder()
            .uri("/v1/messages")
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::to_vec(&json!({
                    "model": "test-500-retry", "max_tokens": 64, "stream": false,
                    "messages": [{"role": "user", "content": "hi"}]
                }))
                .unwrap(),
            ))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        server.abort();

        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(attempts.load(Ordering::SeqCst), 3);
    }

    /// 上游持续 503 耗尽退避预算:末次错误响应转 anthropic error 返回,
    /// 不返回 500 内部错误,客户端能拿到上游原始错误信息。
    #[tokio::test]
    async fn test_upstream_persistent_503_exhausts_budget_returns_error_shape() {
        let upstream = Router::new().route(
            "/chat/completions",
            post(|| async {
                (
                    axum::http::StatusCode::SERVICE_UNAVAILABLE,
                    [(header::CONTENT_TYPE, "application/json")],
                    r#"{"error":{"message":"down"}}"#,
                )
            }),
        );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let upstream_addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });

        let state = mock_state();
        let provider: ProviderConfig = serde_yaml::from_str(&format!(
            "name: test-503-exhaust\nprotocol: openai_chat\nbase_url: http://{}\nkey: sk-test\nproxy_url: direct\nmodels:\n  - name: gpt-4\n    alias: test-503-exhaust\n",
            upstream_addr
        ))
        .unwrap();
        state.providers.write().await.push(provider);
        let app = app(state);
        let request = Request::builder()
            .uri("/v1/messages")
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::to_vec(&json!({
                    "model": "test-503-exhaust", "max_tokens": 64, "stream": false,
                    "messages": [{"role": "user", "content": "hi"}]
                }))
                .unwrap(),
            ))
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        server.abort();

        assert_eq!(
            response.status(),
            axum::http::StatusCode::SERVICE_UNAVAILABLE
        );
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        let v: Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(v["type"], "error", "错误必须转 anthropic error 形状");
    }
}
