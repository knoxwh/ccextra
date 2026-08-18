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
    convert_passthrough, convert_to_openai_chat, convert_to_openai_responses, ConvertError,
};
use ccextra_core::count_tokens::count_claude_input_tokens;
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
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex as StdMutex, OnceLock};
use tokio::sync::RwLock;

use crate::sse::parser::{SseEvent, SseParser};
use crate::sse::SseStreamPin;
use crate::upstream::UpstreamClient;

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
/// Claude Code 的 /context 记账会调此端点。自定义 base URL 无原生
/// count_tokens 契约(非 Anthropic 官方一律本地估算),直接
/// 用 O200kBase 对请求体估算 input_tokens,不走上游。
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
    let payload = String::from_utf8_lossy(&bytes);
    match count_claude_input_tokens(&payload) {
        Ok(result) => Response::builder()
            .status(StatusCode::OK)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(serde_json::to_vec(&result).unwrap()))
            .map_err(|e| AppError::new(anyhow::anyhow!("构造 count_tokens 响应失败: {e}"))),
        Err(e) => Response::builder()
            .status(StatusCode::BAD_REQUEST)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(
                json!({"type": "error", "error": {"type": "invalid_request_error", "message": e}})
                    .to_string(),
            ))
            .map_err(|e| AppError::new(anyhow::anyhow!("构造 count_tokens 错误响应失败: {e}"))),
    }
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

/// 构建 claude 直通的透传/重建头(对齐 applyClaudeHeaders 中转场景)。
///
/// anthropic-beta 按 body 内容条件重建,再追加 caller 自带 beta(去重);
/// anthropic-version / x-app / stainless 系列等身份头仅透传(有就转发,
/// 没有不补——中转站不校验,官方上游才需要完整强制集)。
fn claude_relay_headers(headers: &HeaderMap, body: &Value) -> Vec<(String, String)> {
    // 1. anthropic-beta 重建(基础集 + body 条件 + caller 追加)
    let mut betas: Vec<String> = vec!["claude-code-20250219".to_string()];
    let has_thinking = body.get("thinking").map(|t| !t.is_null()).unwrap_or(false);
    let has_thinking_display = body
        .get("thinking")
        .and_then(|t| t.get("display"))
        .map(|d| !d.is_null())
        .unwrap_or(false);
    if has_thinking && !has_thinking_display {
        betas.push("redact-thinking".to_string());
    }
    if body.get("tools").map(|t| !t.is_null()).unwrap_or(false) {
        betas.push("advanced-tool-use".to_string());
    }
    betas.push("effort-2025-11-24".to_string());
    if body.get("speed").and_then(|s| s.as_str()) == Some("fast") {
        betas.push("fast-mode".to_string());
    }
    // caller 自带 beta 追加(去重)
    if let Some(v) = headers.get("anthropic-beta").and_then(|v| v.to_str().ok()) {
        for b in v.split(',').map(|s| s.trim()).filter(|s| !s.is_empty()) {
            if !betas.iter().any(|x| x == b) {
                betas.push(b.to_string());
            }
        }
    }

    let mut out: Vec<(String, String)> = vec![("anthropic-beta".into(), betas.join(","))];

    // 2. 身份头透传(仅入站存在时)
    const PASSTHROUGH: &[&str] = &[
        "anthropic-version",
        "x-app",
        "x-claude-code-session-id",
        "anthropic-dangerous-direct-browser-access",
        "x-stainless-lang",
        "x-stainless-package-version",
        "x-stainless-runtime",
        "x-stainless-runtime-version",
        "x-stainless-os",
        "x-stainless-arch",
        "x-stainless-timeout",
        "x-stainless-retry-count",
    ];
    for name in PASSTHROUGH {
        if let Some(v) = headers.get(*name).and_then(|v| v.to_str().ok()) {
            out.push(((*name).to_string(), v.to_string()));
        }
    }

    out
}

async fn handle_messages(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, AppError> {
    // 一次性 clone 运行时快照值后立即释放读锁,避免跨 await 持锁阻塞 /reload
    let (secret, log_request_body, normalize_enabled, normalize_drift_detector, upstream_client) = {
        let rt = state.runtime.read().await;
        (
            rt.secret.clone(),
            rt.logging.request_body,
            rt.normalize.enabled,
            rt.normalize.drift_detector,
            rt.upstream.clone(),
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
    let is_stream = body_json
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    // Claude Code 会话 ID 须在转换前提取(转换后 metadata 被丢弃),供 prompt_cache_key 用
    let cc_session = extract_claude_code_session(&headers, &body_json);

    // 入站 Claude body 本地估算输入 token(对齐 ClaudeInputTokenState)。
    // 多数上游流中不带 usage(chat 只在最后 chunk 带,responses 只在流尾),
    // message_start 又必须第一帧发,故用此值占位,让 cc context 过程中接近
    // 真实而非跳 1;流尾 message_delta 以真实 usage 覆盖。claude 直通不经
    // 状态机、非流式不进 SSE,均传 None。
    let estimated_input_tokens = if !is_stream || matches!(route.protocol, Protocol::Claude) {
        None
    } else {
        count_claude_input_tokens(&serde_json::to_string(&body_json).unwrap_or_default())
            .ok()
            .map(|c| c.input_tokens)
    };

    // 工具名还原表(short→original),responses 转换侧产出,供流式/非流式响应还原
    let mut tool_names: Option<Arc<HashMap<String, String>>> = None;

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
            if normalize_enabled {
                normalize_target_post(&mut body_json, TargetShape::OpenAiResponses);
                observe_drift_for(
                    &state.drift,
                    &headers,
                    &body_json,
                    DriftApiKind::OpenAiResponses,
                    normalize_drift_detector,
                );
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
        }
    }

    // 5. payload 参数覆盖(转换后注入;claude 直通需显式 protocol 才生效)
    apply_payload_overrides(&mut body_json, &model, route.protocol, &payload_rules);

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
    let (upstream_base_urls, upstream_key, upstream_proxy, provider_prompt_cache_key) = {
        let provider = find_provider(&providers, &route.provider)
            .ok_or_else(|| AppError::new(anyhow::anyhow!("provider 未找到: {}", route.provider)))?;
        (
            provider.base_urls().to_vec(),
            provider.key.clone(),
            provider.proxy_url.clone(),
            provider.prompt_cache_key,
        )
    };
    drop(payload_rules);
    drop(providers);

    // prompt_cache_key 注入(provider 级开关;仅 openai 协议;key=session_id,对齐 codex 0.147)
    if provider_prompt_cache_key
        && matches!(
            route.protocol,
            Protocol::OpenAiChat | Protocol::OpenAiResponses
        )
        && inject_prompt_cache_key(&mut body_json, cc_session.as_deref())
    {
        tracing::debug!("prompt_cache_key 已注入");
    }

    // 诊断:request_body 开启时落盘最终上游 body,供逐轮 diff 定位缓存漂移。
    // 文件名按会话+时间+序号,body 与响应元信息共用同一标识。
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
        let path = format!("logs/upstream_body_{log_stem}.json");
        let dumped = serde_json::to_vec_pretty(&body_json)
            .ok()
            .and_then(|bytes| {
                std::fs::create_dir_all("logs").ok()?;
                std::fs::write(&path, bytes).ok()
            });
        if dumped.is_none() {
            tracing::warn!(path = %path, "上游 body 落盘失败");
        }
    }

    // claude 直通:透传/重建上游头(对齐 applyClaudeHeaders 中转场景)
    let extra_headers = if matches!(route.protocol, Protocol::Claude) {
        claude_relay_headers(&headers, &body_json)
    } else {
        Vec::new()
    };

    // responses 协议:session-id/thread-id 头(对齐 Codex 官方客户端)
    // - session-id:会话级 UUID(整会话稳定,与 prompt_cache_key 解耦)
    // - thread-id:线程级 UUID(对齐上游请求关联/日志追踪)
    let (session_id, thread_id) = if matches!(route.protocol, Protocol::OpenAiResponses) {
        (cc_session.as_deref(), extract_claude_code_thread(&headers))
    } else {
        (None, None)
    };

    // 多 base_url 回退(对齐 CPA antigravity executor:网络错误、429 切下一个 URL)
    let mut upstream = None;
    let mut last_err = None;
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
            )
            .await
        {
            Ok(resp) => {
                // 429 且还有下一个 URL:直接切换(CPA 另有按 body 决策分类,此处从简)
                if resp.status.as_u16() == 429 && idx + 1 < upstream_base_urls.len() {
                    tracing::debug!("上游 429,回退到下一个 base_url: {}", base_url);
                    drop(resp);
                    continue;
                }
                upstream = Some(resp);
                break;
            }
            Err(e) => {
                last_err = Some(e);
                if idx + 1 < upstream_base_urls.len() {
                    tracing::debug!("上游请求错误,回退到下一个 base_url: {}", base_url);
                    continue;
                }
            }
        }
    }
    let mut upstream = match upstream {
        Some(u) => Some(u),
        None => return Err(last_err.expect("上游请求应已尝试").into()),
    };
    let mut status = upstream.as_ref().expect("上游响应应存在").status;
    let mut preloaded_stream = None;

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
            let meta_base = upstream_meta_base(
                log_request_body,
                current.body.headers(),
                ts,
                request_seq,
                &sess,
                &proto,
                &model,
                &route.upstream_model,
                status,
                &body_json,
            );
            // 基础 meta 先落盘:错误响应、流式提前断开也保留响应头指纹。
            let initial_meta_written = meta_base.as_ref().map(write_upstream_meta).unwrap_or(true);
            let mut out = relay_stream_with_meta(
                route.protocol,
                current.body.bytes_stream(),
                estimated_input_tokens,
                tool_names.clone(),
                meta_base,
                initial_meta_written,
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
    if !status.is_success() {
        let upstream = upstream.take().expect("上游响应应存在");
        if let Some(meta) = upstream_meta_base(
            log_request_body,
            upstream.body.headers(),
            ts,
            request_seq,
            &sess,
            &proto,
            &model,
            &route.upstream_model,
            status,
            &body_json,
        ) {
            write_upstream_meta(&meta);
        }
        let body_bytes = upstream.body.bytes().await?;
        return Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(to_anthropic_error(&body_bytes)))
            .map_err(|e| AppError::new(anyhow::anyhow!("构造错误响应失败: {e}")));
    }

    // 7. 响应转换
    if is_stream {
        // 流式:claude 直通字节转发;转换路径走 SSE 状态机。
        let out = if let Some(out) = preloaded_stream {
            out
        } else {
            let upstream = upstream.take().expect("上游响应应存在");
            let meta_base = upstream_meta_base(
                log_request_body,
                upstream.body.headers(),
                ts,
                request_seq,
                &sess,
                &proto,
                &model,
                &route.upstream_model,
                status,
                &body_json,
            );
            let initial_meta_written = meta_base.as_ref().map(write_upstream_meta).unwrap_or(true);
            relay_stream_with_meta(
                route.protocol,
                upstream.body.bytes_stream(),
                estimated_input_tokens,
                tool_names,
                meta_base,
                initial_meta_written,
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
        if let Some(meta) = upstream_meta_base(
            log_request_body,
            upstream.body.headers(),
            ts,
            request_seq,
            &sess,
            &proto,
            &model,
            &route.upstream_model,
            status,
            &body_json,
        ) {
            write_upstream_meta(&meta);
        }
        let body_bytes = upstream.body.bytes().await?;
        let converted = match serde_json::from_slice::<Value>(&body_bytes) {
            Ok(v) => match route.protocol {
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
                    ))
                }
                Protocol::Antigravity => {
                    // Antigravity 响应为 {"response": {...gemini...}} 信封,先解包;
                    // 对齐 CPA:usageMetadata 可能在信封根,补回内层
                    use ccextra_core::convert::convert_gemini_response;
                    let mut inner = v.get("response").cloned().unwrap_or_else(|| v.clone());
                    if inner.get("usageMetadata").is_none() {
                        if let Some(u) = v.get("usageMetadata") {
                            inner["usageMetadata"] = u.clone();
                        }
                    }
                    Some(convert_gemini_response(
                        &inner,
                        tool_names.as_deref().unwrap_or(&HashMap::new()),
                    ))
                }
            },
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

// ── 上游响应元信息诊断(配合 logs/upstream_body_* 定位缓存漂移)────────────

fn upstream_log_stem(session: &str, ts: u64, sequence: u64, protocol: &str) -> String {
    format!("{session}_{ts}_{sequence}.{protocol}")
}

/// 从上游 Responses SSE 字节流中提取首个 response_id。
/// 复用 SSE 解析器,避免跨 chunk 或 JSON 空白导致误判。
struct ResponsesIdScanner {
    parser: SseParser,
    done: bool,
}

impl ResponsesIdScanner {
    fn new() -> Self {
        Self {
            parser: SseParser::new(),
            done: false,
        }
    }

    /// 喂入一个 chunk,命中返回 id,否则 None。命中后不再解析。
    fn push(&mut self, bytes: &[u8]) -> Option<String> {
        if self.done {
            return None;
        }
        let events = self.parser.push(bytes);
        self.scan(events)
    }

    /// 流结束时 flush 没有末尾空行的事件。
    fn finish(&mut self) -> Option<String> {
        if self.done {
            return None;
        }
        let events = self.parser.finish();
        self.scan(events)
    }

    fn scan(&mut self, events: Vec<SseEvent>) -> Option<String> {
        for event in events {
            let Ok(value) = serde_json::from_str::<Value>(&event.data) else {
                continue;
            };
            let created = event.event.as_deref() == Some("response.created")
                || value.get("type").and_then(Value::as_str) == Some("response.created");
            if !created {
                continue;
            }
            let Some(id) = value
                .pointer("/response/id")
                .and_then(Value::as_str)
                .filter(|id| !id.is_empty())
            else {
                continue;
            };
            self.done = true;
            return Some(id.to_string());
        }
        None
    }
}

/// 包裹 SSE 输出流:结束后执行一次 on_end(补写漏配的响应元信息)。
fn sse_meta_finalizer<F>(inner: SseStreamPin, on_end: F) -> SseStreamPin
where
    F: FnOnce() + Send + 'static,
{
    Box::pin(async_stream::stream! {
        let mut inner = inner;
        while let Some(item) = inner.next().await {
            yield item;
        }
        on_end();
    })
}

fn write_meta_file(path: &std::path::Path, meta: &Value) -> bool {
    let Ok(bytes) = serde_json::to_vec_pretty(meta) else {
        return false;
    };
    std::fs::write(path, bytes).is_ok()
}

/// 落盘上游响应元信息(诊断)。文件名与 upstream_body_* 同会话同 ts,一对一定位。
fn write_upstream_meta(meta: &Value) -> bool {
    let ts = meta["ts"].as_u64().unwrap_or(0);
    let request_seq = meta["request_seq"].as_u64().unwrap_or(0);
    let sess = meta["session"].as_str().unwrap_or("nosess");
    let proto = meta["protocol"].as_str().unwrap_or("unknown");
    let stem = upstream_log_stem(sess, ts, request_seq, proto);
    let path = format!("logs/upstream_meta_{stem}.json");
    let ok = std::fs::create_dir_all("logs").is_ok()
        && write_meta_file(std::path::Path::new(&path), meta);
    if !ok {
        tracing::warn!(path = %path, "上游响应元信息落盘失败");
    }
    ok
}

/// 构造上游响应诊断元信息。流式首帧重试后以最终响应头覆盖同一文件。
#[allow(clippy::too_many_arguments)]
fn upstream_meta_base(
    enabled: bool,
    headers: &reqwest::header::HeaderMap,
    ts: u64,
    request_seq: u64,
    session: &str,
    protocol: &str,
    model: &str,
    upstream_model: &str,
    status: reqwest::StatusCode,
    body: &Value,
) -> Option<Value> {
    if !enabled {
        return None;
    }
    let mut response_headers = serde_json::Map::new();
    for name in [
        "cf-ray",
        "x-request-id",
        "x-response-id",
        "server",
        "alt-svc",
        "via",
        "x-served-by",
        "x-cache-status",
        "cf-cache-status",
        "age",
    ] {
        if let Some(value) = headers.get(name) {
            response_headers.insert(name.into(), json!(value.to_str().unwrap_or("")));
        }
    }
    Some(json!({
        "ts": ts,
        "request_seq": request_seq,
        "session": session,
        "protocol": protocol,
        "model": model,
        "upstream_model": upstream_model,
        "status": status.as_u16(),
        "prompt_cache_key": body.get("prompt_cache_key"),
        "response_id": Value::Null,
        "headers": response_headers,
    }))
}

/// 包裹转换流并维护可选响应诊断。首帧门控会先轮询该流，故诊断也必须在此处接入。
fn relay_stream_with_meta<S>(
    protocol: Protocol,
    stream: S,
    estimated_input_tokens: Option<usize>,
    tool_names: Option<Arc<HashMap<String, String>>>,
    meta_base: Option<Value>,
    initial_meta_written: bool,
) -> SseStreamPin
where
    S: futures::Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
{
    let Some(meta) = meta_base else {
        return crate::sse::relay(protocol, stream, estimated_input_tokens, tool_names);
    };
    let shared = Arc::new(StdMutex::new(meta));
    let written = Arc::new(AtomicBool::new(initial_meta_written));
    if matches!(protocol, Protocol::OpenAiResponses) {
        let scanner = Arc::new(StdMutex::new(ResponsesIdScanner::new()));
        let scan = Arc::clone(&scanner);
        let shared_for_scan = Arc::clone(&shared);
        let written_for_scan = Arc::clone(&written);
        let tapped = stream.inspect(move |result| {
            if let Ok(bytes) = result {
                if let Some(id) = scan.lock().unwrap().push(bytes) {
                    let mut meta = shared_for_scan.lock().unwrap();
                    meta["response_id"] = json!(id);
                    let ok = write_upstream_meta(&meta);
                    written_for_scan.store(ok, Ordering::Release);
                }
            }
        });
        let relayed = crate::sse::relay(protocol, tapped, estimated_input_tokens, tool_names);
        sse_meta_finalizer(relayed, move || {
            if let Some(id) = scanner.lock().unwrap().finish() {
                let mut meta = shared.lock().unwrap();
                meta["response_id"] = json!(id);
                let ok = write_upstream_meta(&meta);
                written.store(ok, Ordering::Release);
            }
            if !written.load(Ordering::Acquire) {
                let ok = write_upstream_meta(&shared.lock().unwrap());
                written.store(ok, Ordering::Release);
            }
        })
    } else {
        let relayed = crate::sse::relay(protocol, stream, estimated_input_tokens, tool_names);
        sse_meta_finalizer(relayed, move || {
            if !written.load(Ordering::Acquire) {
                let ok = write_upstream_meta(&shared.lock().unwrap());
                written.store(ok, Ordering::Release);
            }
        })
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

    #[test]
    fn upstream_log_stem_includes_request_sequence() {
        let first = upstream_log_stem("sessabcd", 123, 7, "openairesponses");
        let second = upstream_log_stem("sessabcd", 123, 8, "openairesponses");
        assert_ne!(first, second);
        assert_eq!(first, "sessabcd_123_7.openairesponses");
    }

    #[test]
    fn write_meta_file_reports_write_failure() {
        let path = std::env::temp_dir().join(format!(
            "ccextra-meta-test-{}-{}",
            std::process::id(),
            UPSTREAM_LOG_SEQUENCE.fetch_add(1, Ordering::Relaxed),
        ));
        std::fs::create_dir_all(&path).unwrap();
        let meta = json!({"response_id": "resp_1"});
        assert!(write_meta_file(&path.join("meta.json"), &meta));
        assert!(!write_meta_file(&path, &meta));
        std::fs::remove_dir_all(path).unwrap();
    }

    #[test]
    fn responses_id_scanner_single_chunk() {
        // 单 chunk 内命中 response.created 的 id。
        let mut scanner = ResponsesIdScanner::new();
        let id = scanner.push(
            br#"event: response.created
data: {"type":"response.created","response":{"id":"resp_abc123","model":"gpt-5"}}

"#,
        );
        assert_eq!(id.as_deref(), Some("resp_abc123"));
        // 命中后不再解析后续事件。
        assert_eq!(scanner.push(b"anything"), None);
    }

    #[test]
    fn responses_id_scanner_across_chunks() {
        // 事件在任意 JSON 字符位置切开仍可命中。
        let mut scanner = ResponsesIdScanner::new();
        assert_eq!(
            scanner.push(
                br#"event: response.created
data: {"type":"response.crea"#
            ),
            None
        );
        assert_eq!(scanner.push(br#"ted","response":{"id":"resp_xyz"#), None);
        assert_eq!(
            scanner.push(
                br#"","model":"gpt-5"}}

"#
            ),
            Some("resp_xyz".to_string())
        );
    }

    #[test]
    fn responses_id_scanner_accepts_json_whitespace() {
        let mut scanner = ResponsesIdScanner::new();
        let id = scanner.push(
            br#"event: response.created
data: { "type": "response.created", "response": { "id": "resp_space" } }

"#,
        );
        assert_eq!(id.as_deref(), Some("resp_space"));
    }

    #[test]
    fn responses_id_scanner_ignores_non_created_ids() {
        let mut scanner = ResponsesIdScanner::new();
        let id = scanner.push(
            br#"event: response.output_item.added
data: {"type":"response.output_item.added","item":{"id":"item_1"}}

event: response.created
data: {"type":"response.created","response":{"id":"resp_1"}}

"#,
        );
        assert_eq!(id.as_deref(), Some("resp_1"));
    }

    #[test]
    fn responses_id_scanner_accepts_data_type_without_event_name() {
        let mut scanner = ResponsesIdScanner::new();
        let id = scanner.push(
            br#"data: {"type":"response.created","response":{"id":"resp_no_event"}}

"#,
        );
        assert_eq!(id.as_deref(), Some("resp_no_event"));
    }

    #[test]
    fn responses_id_scanner_flushes_eof_without_empty_line() {
        let mut scanner = ResponsesIdScanner::new();
        assert_eq!(
            scanner.push(
                br#"event: response.created
data: {"type":"response.created","response":{"id":"resp_eof"}}"#
            ),
            None
        );
        assert_eq!(scanner.finish().as_deref(), Some("resp_eof"));
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

    #[test]
    fn test_claude_relay_beta_rebuild_with_thinking_and_tools() {
        // thinking(无 display)→ redact-thinking;tools → advanced-tool-use
        let headers = headers_with(&[("anthropic-beta", "interleaved-thinking-2025-05-14")]);
        let body = json!({
            "thinking": {"type": "enabled", "budget_tokens": 4096},
            "tools": [{"name": "t"}]
        });
        let out = claude_relay_headers(&headers, &body);
        let beta = out.iter().find(|(k, _)| k == "anthropic-beta").unwrap();
        let parts: Vec<&str> = beta.1.split(',').collect();
        assert_eq!(parts[0], "claude-code-20250219");
        assert!(parts.contains(&"redact-thinking"));
        assert!(parts.contains(&"advanced-tool-use"));
        assert!(parts.contains(&"effort-2025-11-24"));
        // caller beta 追加
        assert!(parts.contains(&"interleaved-thinking-2025-05-14"));
    }

    #[test]
    fn test_claude_relay_beta_no_redact_when_display_present() {
        // thinking.display 存在 → 不加 redact-thinking(与直通头重建一致)
        let headers = HeaderMap::new();
        let body = json!({"thinking": {"type": "enabled", "display": "summarized"}});
        let out = claude_relay_headers(&headers, &body);
        let beta = &out.iter().find(|(k, _)| k == "anthropic-beta").unwrap().1;
        assert!(!beta.contains("redact-thinking"));
    }

    #[test]
    fn test_claude_relay_beta_fast_mode() {
        let headers = HeaderMap::new();
        let body = json!({"speed": "fast"});
        let out = claude_relay_headers(&headers, &body);
        let beta = &out.iter().find(|(k, _)| k == "anthropic-beta").unwrap().1;
        assert!(beta.contains("fast-mode"));
    }

    #[test]
    fn test_claude_relay_identity_headers_passthrough_only() {
        // 入站有 → 透传;没有 → 不补
        let headers = headers_with(&[
            ("anthropic-version", "2023-06-01"),
            ("x-app", "cli"),
            ("x-stainless-os", "macOS"),
        ]);
        let body = json!({});
        let out = claude_relay_headers(&headers, &body);
        assert!(out
            .iter()
            .any(|(k, v)| k == "anthropic-version" && v == "2023-06-01"));
        assert!(out.iter().any(|(k, v)| k == "x-app" && v == "cli"));
        assert!(out
            .iter()
            .any(|(k, v)| k == "x-stainless-os" && v == "macOS"));
        // 未提供的头不出现
        assert!(!out.iter().any(|(k, _)| k == "x-stainless-arch"));

        let out2 = claude_relay_headers(&HeaderMap::new(), &body);
        assert!(!out2.iter().any(|(k, _)| k == "anthropic-version"));
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
            })),
            reload: reload_returning_secret(None),
            drift: DriftState::new(1000),
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
}
