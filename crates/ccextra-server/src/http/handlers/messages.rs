use axum::{
    body::{to_bytes, Body},
    extract::State,
    http::{header, HeaderMap},
    response::Response,
};
use bytes::Bytes;
use ccextra_core::cache_stabilization::{
    maybe_observe_request_drift_with, observe_cache_usage, DriftApiKind, DriftObserveOptions,
};
use ccextra_core::convert::{
    clamp_passthrough_effort, convert_passthrough, convert_to_antigravity_with,
    convert_to_gemini_with_registry, convert_to_openai_chat_with, convert_to_openai_responses_with,
    is_antigravity_thought_signature_invalid, is_kimi_thinking_replay_model,
    is_thinking_signature_invalid, kimi_thinking_replay_model_family, sanitize_gpt_reasoning_items,
    sanitize_openai_chat_after_payload, sanitize_passthrough_prompt,
    trim_encrypted_reasoning_items,
};
use ccextra_core::normalize::{
    normalize_anthropic_full, normalize_anthropic_pretransform, normalize_target_post, TargetShape,
};
use ccextra_core::prompt_cache::inject_prompt_cache_key;
use ccextra_core::route::{resolve_route, Protocol};
use ccextra_core::session::{
    extract_claude_code_agent_id, extract_claude_code_session, extract_claude_code_thread,
};
use futures::StreamExt;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::http::auth::check_secret;
use crate::http::claude_relay::{
    claude_inbound_user_agent, claude_passthrough_headers, ensure_oauth_beta,
};
use crate::http::error::{to_anthropic_error, AppError};
use crate::http::retry::{parse_google_retry_delay, parse_retry_after, retry_after_header_secs};
use crate::http::{AppState, PayloadRule};
use crate::sse::antigravity_replay_cache::AgStreamReplayExtractor;
use crate::sse::kimi_thinking_replay_cache::KimiStreamReplayExtractor;
use crate::sse::replay_cache::StreamReplayExtractor;
use crate::sse::SseStreamPin;
use crate::upstream::{is_gpt_model, is_grok_model, UpstreamResponse};

/// 诊断日志请求序号:与毫秒时间戳组合,避免并发请求覆盖同一文件。
static UPSTREAM_LOG_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// 解析 payload 后最终模型。仅 OpenAI 的无效覆盖回退路由模型并同步写回 body,
/// 确保 Grok 判定与 UpstreamClient 读取的 model 一致；其余协议保留 payload 原语义。
pub(crate) fn resolve_outbound_model(
    body: &mut Value,
    fallback: &str,
    protocol: Protocol,
) -> String {
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
pub(crate) fn should_inject_prompt_cache_key(
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
pub(crate) fn apply_payload_overrides(
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
        let matched = rule.matches_model(model);
        if matched {
            for (key, val) in &rule.params {
                body[key] = val.clone();
            }
        }
    }
}

// ── 上游请求诊断落盘(配合 logs/upstream_request_*)────────

pub(crate) fn upstream_log_stem(session: &str, ts: u64, sequence: u64, protocol: &str) -> String {
    format!("{session}_{ts}_{sequence}.{protocol}")
}

/// 入站请求头是否含密钥,落盘时脱敏。
pub(crate) fn is_sensitive_header(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    matches!(
        n.as_str(),
        "authorization" | "proxy-authorization" | "cookie" | "set-cookie" | "x-api-key"
    ) || n.contains("api-key")
        || n.ends_with("-token")
        || n.ends_with("-secret")
}

pub(crate) fn header_value_text(value: &axum::http::HeaderValue) -> String {
    String::from_utf8_lossy(value.as_bytes()).into_owned()
}

/// 入站 HeaderMap → JSON(密钥字段写成 `[redacted]`;同名多值保留为数组)。
pub(crate) fn inbound_headers_json(headers: &HeaderMap) -> Value {
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

/// AG replay 账本是否因上游错误清理（逐字对齐 CPA
/// clearAntigravityReasoningReplayOnInvalidSignature：仅 status == 400，body 小写后含
/// thoughtsignature / thought_signature / signature）。只用于 AG 账本，不影响其它协议的签名门禁。
fn should_clear_ag_replay_on_error(protocol: Protocol, status: u16, body: &[u8]) -> bool {
    matches!(protocol, Protocol::Antigravity)
        && status == 400
        && is_antigravity_thought_signature_invalid(body)
}

/// 流式响应旁路提取 replay 的作用域;按协议互斥(Responses / Antigravity / Kimi)
enum ReplayTap {
    None,
    /// (cache, key, request_fingerprint)
    Responses(crate::sse::replay_cache::ReplayCache, String, String),
    /// (cache, key, 读账本快照, 外发 request):提交走 ReplaceIfUnchanged(snapshot)
    Antigravity(
        crate::sse::antigravity_replay_cache::AgReplayCache,
        String,
        crate::sse::antigravity_replay_cache::AgReplaySnapshot,
        Value,
    ),
    /// Kimi thinking:(cache, key, replay_applied)
    Kimi(
        crate::sse::kimi_thinking_replay_cache::KimiThinkingReplayCache,
        String,
        bool,
    ),
}

/// 每个上游 SSE 字节块喂给 replay 提取器
type ReplayPush = Box<dyn FnMut(&[u8]) + Send>;

struct RelayParams {
    protocol: Protocol,
    estimated_input_tokens: Option<usize>,
    tool_names: Option<Arc<HashMap<String, String>>>,
    /// 转换前 Claude 请求快照(Gemini/AG 响应用来回剥占位 tool args)
    original_claude_request: Option<Arc<Value>>,
    replay_tap: ReplayTap,
    signature_model: Option<Arc<str>>,
    /// 客户端入站 model 别名(Responses message_start 预填)
    inbound_model: Option<Arc<str>>,
    token_scope: Option<crate::http::session_tokens::TokenCacheScope>,
    drift_usage_scope: Option<crate::http::session_tokens::DriftUsageScope>,
}

/// 按协议旁路提取 replay：Responses 用 StreamReplayExtractor；
/// Antigravity 用 AgStreamReplayExtractor（thoughtSignature 账本）。
fn relay_with_replay_tap<S>(stream: S, params: RelayParams) -> SseStreamPin
where
    S: futures::Stream<Item = Result<Bytes, std::io::Error>> + Send + 'static,
{
    let stream =
        crate::http::session_tokens::tap_input_tokens(stream, params.protocol, params.token_scope);
    let stream = crate::http::session_tokens::tap_drift_cache_usage(
        stream,
        params.protocol,
        params.drift_usage_scope,
    );
    let mut push: ReplayPush = match params.replay_tap {
        ReplayTap::None => Box::new(|_| {}),
        ReplayTap::Responses(cache, key, request_fingerprint) => {
            let mut extractor = StreamReplayExtractor::new(cache, key, request_fingerprint);
            Box::new(move |bytes| extractor.push(bytes))
        }
        ReplayTap::Antigravity(cache, key, snapshot, request) => {
            let mut extractor = AgStreamReplayExtractor::new(cache, key, snapshot, request);
            Box::new(move |bytes| extractor.push(bytes))
        }
        ReplayTap::Kimi(cache, key, applied) => {
            let mut extractor = KimiStreamReplayExtractor::new(cache, key, applied);
            Box::new(move |bytes| extractor.push(bytes))
        }
    };
    let stream = stream.inspect(move |result| {
        if let Ok(bytes) = result {
            push(bytes);
        }
    });
    crate::sse::relay(
        params.protocol,
        stream,
        params.estimated_input_tokens,
        params.tool_names,
        params.signature_model,
        params.original_claude_request,
        params.inbound_model.as_deref(),
    )
}

/// 首个转换帧已是 error 时，尚未向客户端发字节，可安全重试上游一次。
fn is_initial_sse_error(frame: &Bytes) -> bool {
    frame.starts_with(b"event: error\n")
}

/// SSE 注释行帧(如心跳 `: keepalive\n\n`):保活占位,无业务语义。
fn is_sse_comment_frame(frame: &Bytes) -> bool {
    frame.starts_with(b":")
}

/// 跳过心跳注释帧,等到首个业务帧(或 error/EOF)。
/// 心跳由 `with_idle_keepalive` 注入;首帧前触发时会骗过首帧错误检测,
/// 提前提交 200 丧失重试窗口(codex/grok 的 eventsource 同样丢弃注释帧)。
async fn next_business_frame(out: &mut SseStreamPin) -> Option<Result<Bytes, std::io::Error>> {
    loop {
        match out.next().await {
            Some(Ok(frame)) if is_sse_comment_frame(&frame) => continue,
            other => return other,
        }
    }
}

/// 从首帧 error 事件提取 error.message(提取失败回退原始文本)。
fn initial_sse_error_message(frame: &Bytes) -> String {
    let text = String::from_utf8_lossy(frame);
    text.lines()
        .find_map(|line| line.strip_prefix("data: "))
        .and_then(|data| serde_json::from_str::<Value>(data).ok())
        .and_then(|v| {
            v.pointer("/error/message")
                .and_then(|m| m.as_str())
                .map(String::from)
        })
        .unwrap_or_else(|| text.trim().to_string())
}

fn prepend_sse_frame(first: Result<Bytes, std::io::Error>, rest: SseStreamPin) -> SseStreamPin {
    Box::pin(futures::stream::once(async move { first }).chain(rest))
}

/// OAuth 订阅凭证刷新目标(预刷新失败硬失败;上游 401 强制刷新后重试一次)。
/// `path` 为扫描到的凭证文件实际路径。
#[derive(Debug, Clone)]
pub(crate) enum OAuthRefreshTarget {
    Antigravity { path: String },
    Claude { path: String },
    Codex { path: String },
    Xai { path: String },
}

pub(crate) struct PreparedMessageRequest {
    pub route: ccextra_core::route::RouteDecision,
    pub body_json: Value,
    /// 已序列化(+可选 zstd)的出站 body;URL/401/unfit 重试复用。
    /// body_json 变更或 chatgpt-account-id 使 compress 翻转后须 reencode
    pub encoded_body: bytes::Bytes,
    pub is_stream: bool,
    pub upstream_base_urls: Vec<String>,
    /// 本轮实际生效的 base_url(多 URL 轮转后记录;内部重试用它,不倒退回 `[0]`)
    pub active_base_url: Option<String>,
    pub upstream_key: String,
    /// Antigravity 连接池 scope 用 refresh_token（稳定跨 access 刷新）；其它协议为 None
    pub pool_refresh_token: Option<String>,
    pub upstream_proxy: Option<String>,
    /// 可刷新的 OAuth 订阅凭证;API key provider 为 None
    pub oauth_refresh: Option<OAuthRefreshTarget>,
    pub session_id: Option<String>,
    pub thread_id: Option<String>,
    pub extra_headers: HeaderMap,
    pub inbound_user_agent: Option<String>,
    /// 入站 URI query(不含 `?`);Claude 直通原样接到上游 path。
    pub inbound_query: Option<String>,
    pub estimated_input_tokens: Option<usize>,
    pub tool_names: Option<Arc<HashMap<String, String>>>,
    /// 转换前 Claude 请求快照(仅 Gemini/AG;响应用来回剥占位 tool args)
    pub original_claude_request: Option<Arc<Value>>,
    pub replay_scope: Option<(crate::sse::replay_cache::ReplayCache, String, String)>,
    /// Antigravity thoughtSignature 账本 scope：(cache, key)，key 见 `ag_replay_session_key`
    pub ag_replay_scope: Option<(crate::sse::antigravity_replay_cache::AgReplayCache, String)>,
    /// 本请求读 AG 账本时的快照（对齐 CPA scope.cacheSnapshot，供 IfUnchanged 清理）
    pub ag_replay_snapshot: crate::sse::antigravity_replay_cache::AgReplaySnapshot,
    /// Kimi thinking replay：(cache, "{family}:{session}", applied)
    pub kimi_replay_scope: Option<(
        crate::sse::kimi_thinking_replay_cache::KimiThinkingReplayCache,
        String,
        bool,
    )>,
    pub signature_model: Option<Arc<str>>,
    /// 客户端入站 model 别名(body.model 路由前;Responses 回包 message_start 预填)
    pub inbound_model: Arc<str>,
    pub token_scope: Option<crate::http::session_tokens::TokenCacheScope>,
    /// drift 开启且父级稳定时：响应路径关联 cache usage（与 token_scope 分离）
    pub drift_usage_scope: Option<crate::http::session_tokens::DriftUsageScope>,
    /// 与 config 快照同读锁捕获的 unfit 世代;/reload 写锁内 clear 增代后旧请求 mark 丢弃
    pub unfit_generation: u64,
}

pub(crate) struct ExecutedUpstream {
    pub status: axum::http::StatusCode,
    pub upstream: Option<UpstreamResponse>,
    pub preloaded_stream: Option<SseStreamPin>,
}

pub(crate) async fn prepare_message_request(
    state: &AppState,
    headers: &HeaderMap,
    body_bytes: &[u8],
    config_snapshot: &crate::http::ConfigSnapshot,
    inbound_query: Option<&str>,
    unfit_generation: u64,
) -> Result<PreparedMessageRequest, AppError> {
    let normalize_enabled = config_snapshot.runtime.normalize.enabled;
    let drift_enabled = config_snapshot.runtime.normalize.drift_detector;
    let mut drift_parent_for_usage: Option<String> = None;
    let log_request_body = config_snapshot.runtime.logging.request_body;
    let thinking_registry = &config_snapshot.runtime.thinking_registry;
    let request_id = headers
        .get("x-request-id")
        .and_then(|v| v.to_str().ok())
        .unwrap_or("unknown");

    if log_request_body {
        tracing::debug!("请求体: {}", String::from_utf8_lossy(body_bytes));
    }
    let mut body_json: Value = serde_json::from_slice(body_bytes)
        .map_err(|e| AppError::bad_request(format!("请求体 JSON 解析失败: {e}")))?;

    // 1. 入站 model(复制为 String,避免借用 body_json 阻碍后续可变借用)
    let model = body_json
        .get("model")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AppError::bad_request("缺少 model 字段"))?
        .to_string();

    // 2. 路由决策(基于不可变配置快照,无须持锁)
    let providers = &config_snapshot.providers;
    let route = resolve_route(&model, providers)?;
    let payload_rules = &config_snapshot.payload_rules;

    // 3. 归一化第一遍(按协议:claude 直通全量 / 其他转换前精简)
    if normalize_enabled {
        match route.protocol {
            Protocol::Claude => {
                let counts = normalize_anthropic_full(&mut body_json, request_id);
                tracing::debug!(?counts, "normalize_anthropic_full");
            }
            _ => {
                let counts =
                    normalize_anthropic_pretransform(&mut body_json, route.protocol, request_id);
                tracing::debug!(?counts, protocol = ?route.protocol, "normalize_anthropic_pretransform");
            }
        }
    }

    // 4. 协议转换(含目标侧归一化)
    let is_stream = body_json
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let cc_session = extract_claude_code_session(headers, &body_json);
    let inbound_prompt_cache_key = body_json
        .get("prompt_cache_key")
        .and_then(|v| v.as_str())
        .filter(|s| !s.trim().is_empty())
        .map(str::to_string);

    let estimated_input_tokens = if !is_stream || matches!(route.protocol, Protocol::Claude) {
        None
    } else {
        cc_session
            .as_deref()
            .and_then(|s| state.last_input_tokens.lock().ok()?.get(s))
            .or(Some(0))
    };

    let mut tool_names: Option<Arc<HashMap<String, String>>> = None;
    let mut original_claude_request: Option<Arc<Value>> = None;
    let mut request_fingerprint = String::new();
    let mut kimi_replay_applied = false;
    // AG thoughtSignature 账本 key：读（注入）与写（响应提交）共用同一值
    let mut ag_replay_key: Option<String> = None;
    let mut ag_replay_snapshot = crate::sse::antigravity_replay_cache::AgReplaySnapshot::default();

    match route.protocol {
        Protocol::Claude => {
            convert_passthrough(&mut body_json, &route.upstream_model)?;
            if sanitize_passthrough_prompt(&mut body_json, &route.upstream_model) {
                tracing::debug!(
                    model = %route.upstream_model,
                    "claude 直通 system 已清洗"
                );
            }
            if clamp_passthrough_effort(&mut body_json, &route.upstream_model, thinking_registry) {
                tracing::debug!(
                    model = %route.upstream_model,
                    "claude 直通 effort 已钳制到模型支持档"
                );
            }
            // Kimi thinking 回放：Claude Code 会剥掉非 Anthropic 签名的 thinking，
            // 在第三方 Claude-compatible（Moonshot 等）多轮工具续接前补回。
            if is_kimi_thinking_replay_model(&route.upstream_model) {
                if let Some(sess) = cc_session.as_deref() {
                    let family = kimi_thinking_replay_model_family(&route.upstream_model);
                    let key = format!("{family}:{sess}");
                    use crate::sse::kimi_thinking_replay_cache::KimiReplayApplyOutcome;
                    match state
                        .kimi_thinking_replay_cache
                        .apply_to_claude_body(&key, &mut body_json)
                    {
                        KimiReplayApplyOutcome::Applied => {
                            kimi_replay_applied = true;
                            tracing::debug!(
                                session = %sess,
                                family = %family,
                                "kimi thinking replay 已注入"
                            );
                        }
                        KimiReplayApplyOutcome::Unchanged => {
                            tracing::debug!(
                                session = %sess,
                                family = %family,
                                "kimi thinking replay 账本未改写"
                            );
                        }
                        KimiReplayApplyOutcome::Miss => {}
                    }
                }
            }
            // drift 仅观测：sanitize/effort 之后，避免计费指纹造成假 system 漂移
            drift_parent_for_usage = maybe_observe_request_drift_with(
                drift_enabled,
                &state.drift,
                headers,
                &body_json,
                DriftApiKind::Anthropic,
                DriftObserveOptions {
                    tools_description: config_snapshot.runtime.normalize.drift_tools_description,
                },
            );
        }
        Protocol::OpenAiChat => {
            convert_to_openai_chat_with(&mut body_json, &route.upstream_model, thinking_registry)?;
            if normalize_enabled {
                normalize_target_post(&mut body_json, TargetShape::OpenAiChat, request_id);
            }
            // drift 仅观测：转换 + post-normalize 之后（与 normalize.enabled 独立）
            drift_parent_for_usage = maybe_observe_request_drift_with(
                drift_enabled,
                &state.drift,
                headers,
                &body_json,
                DriftApiKind::OpenAiChat,
                DriftObserveOptions {
                    tools_description: config_snapshot.runtime.normalize.drift_tools_description,
                },
            );
        }
        Protocol::OpenAiResponses => {
            let rev = convert_to_openai_responses_with(
                &mut body_json,
                &route.upstream_model,
                thinking_registry,
            )?;
            if !rev.is_empty() {
                tool_names = Some(Arc::new(rev));
            }
            if normalize_enabled {
                normalize_target_post(&mut body_json, TargetShape::OpenAiResponses, request_id);
            }
            // drift 仅观测：post-normalize 之后、replay 注入之前
            drift_parent_for_usage = maybe_observe_request_drift_with(
                drift_enabled,
                &state.drift,
                headers,
                &body_json,
                DriftApiKind::OpenAiResponses,
                DriftObserveOptions {
                    tools_description: config_snapshot.runtime.normalize.drift_tools_description,
                },
            );
            if let Some(sess) = cc_session.as_deref() {
                let key = format!("{}:{}", route.upstream_model, sess);
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
                request_fingerprint =
                    ccextra_core::convert::compute_input_prefix_fingerprint(&body_json);
            }
        }
        Protocol::Gemini => {
            original_claude_request = Some(Arc::new(body_json.clone()));
            let (gemini_body, short_to_original) = convert_to_gemini_with_registry(
                &body_json,
                &route.upstream_model,
                ccextra_core::convert::gemini::SchemaFlavor::Gemini,
                thinking_registry,
            );
            body_json = gemini_body;
            if !short_to_original.is_empty() {
                tool_names = Some(Arc::new(short_to_original));
            }
        }
        Protocol::Antigravity => {
            original_claude_request = Some(Arc::new(body_json.clone()));
            if ccextra_core::antigravity_uses_reasoning_replay(&route.upstream_model) {
                ag_replay_key = cc_session.as_deref().map(|sess| {
                    crate::sse::antigravity_replay_cache::ag_replay_session_key(
                        &route.upstream_model,
                        sess,
                        &extract_claude_code_agent_id(headers),
                        &ccextra_core::convert::claude_replay_system_lane(&body_json),
                    )
                });
            }
            let project_id = {
                let provider = route.provider_config(providers);
                provider
                    .and_then(|p| p.metadata.as_ref())
                    .and_then(|m| m.get("project_id"))
                    .map(|s| s.as_str())
            };

            let (antigravity_body, short_to_original) = convert_to_antigravity_with(
                &body_json,
                &route.upstream_model,
                project_id,
                thinking_registry,
                cc_session.as_deref(),
            );
            body_json = antigravity_body;
            if !short_to_original.is_empty() {
                tool_names = Some(Arc::new(short_to_original));
            }
            if ccextra_core::antigravity_uses_reasoning_replay(&route.upstream_model) {
                // 工具表（对齐 CPA toolSchemas 来源：原始 Claude 请求）+ 上游短名映射
                let replay_tools = ccextra_core::convert::AgReplayToolContext {
                    original_request: original_claude_request.as_deref(),
                    short_to_original: tool_names.as_deref(),
                };
                if let (Some(sess), Some(key)) = (cc_session.as_deref(), ag_replay_key.as_deref()) {
                    use crate::sse::antigravity_replay_cache::AgReplayApplyOutcome;
                    let (outcome, snapshot) = state
                        .ag_replay_cache
                        .apply_to_antigravity_body_with_tools(key, &mut body_json, replay_tools);
                    ag_replay_snapshot = snapshot;
                    match outcome {
                        AgReplayApplyOutcome::Applied { items } => {
                            tracing::debug!(
                                session = %sess,
                                model = %route.upstream_model,
                                items,
                                "antigravity reasoning replay 已注入"
                            );
                        }
                        AgReplayApplyOutcome::Unchanged { items } => {
                            tracing::debug!(
                                session = %sess,
                                model = %route.upstream_model,
                                items,
                                "antigravity reasoning replay 账本未改写"
                            );
                        }
                        AgReplayApplyOutcome::Invalid { items } => {
                            tracing::warn!(
                                session = %sess,
                                model = %route.upstream_model,
                                items,
                                "antigravity reasoning replay 账本规范化失败，跳过注入"
                            );
                        }
                        AgReplayApplyOutcome::MissingRequest => {
                            tracing::warn!(
                                session = %sess,
                                model = %route.upstream_model,
                                "antigravity reasoning replay 缺少 request 字段，跳过注入"
                            );
                        }
                        AgReplayApplyOutcome::RolledBack { items } => {
                            tracing::debug!(
                                session = %sess,
                                model = %route.upstream_model,
                                items,
                                "antigravity reasoning replay 破坏配对，已退回原始请求并按快照清账本"
                            );
                        }
                        AgReplayApplyOutcome::Miss => {
                            // 对齐 CPA：普通 miss 保持安静，避免每轮刷屏
                        }
                        AgReplayApplyOutcome::PairingInvalid { message } => {
                            // 对齐 CPA statusErr 400 → Claude invalid_request_error。每个 AG Gemini
                            // 族请求都校验；历史坏了时每轮同样 400，直到用户 rewind 或 /clear。
                            tracing::debug!(
                                session = %sess,
                                model = %route.upstream_model,
                                "antigravity replay 历史配对无效，本地 400"
                            );
                            return Err(AppError::bad_request(message));
                        }
                    }
                } else {
                    // 无会话：无账本，但历史稳定 id 仍须降级（对齐 CPA 兜底不依赖 scope）；
                    // 配对无效同样本地 400（见上，坏历史每轮重现）
                    if let Some(message) =
                        crate::sse::antigravity_replay_cache::prepare_antigravity_body_without_ledger(
                            &mut body_json,
                            replay_tools,
                        )
                    {
                        return Err(AppError::bad_request(message));
                    }
                }
            }
        }
    }

    if matches!(
        route.protocol,
        Protocol::OpenAiChat | Protocol::OpenAiResponses
    ) {
        if let Some(key) = inbound_prompt_cache_key {
            body_json["prompt_cache_key"] = Value::String(key);
        }
    }

    // 5. payload 参数覆盖
    apply_payload_overrides(&mut body_json, &model, route.protocol, payload_rules);

    let outbound_model =
        resolve_outbound_model(&mut body_json, &route.upstream_model, route.protocol);

    if matches!(route.protocol, Protocol::OpenAiResponses) && is_gpt_model(&outbound_model) {
        if let Some(obj) = body_json.as_object_mut() {
            // ChatGPT Codex OAuth 拒收采样/兼容字段(对齐 CPA delete + sub2api
            // openAICodexOAuthUnsupportedFields);须在 payload_overrides 之后剥,
            // 否则 *gpt* 规则注入的 temperature 等会打到订阅上游 400。
            for key in [
                "previous_response_id",
                "generate",
                "safety_identifier",
                "stream_options",
                "max_output_tokens",
                "max_completion_tokens",
                "temperature",
                "top_p",
                "frequency_penalty",
                "presence_penalty",
                "chat_template_kwargs",
                "user",
                "metadata",
                "truncation",
                "stop_sequences",
                "background",
            ] {
                obj.remove(key);
            }
            // 空 instructions 对齐 CPA DeleteBytes:省略而非发 ""
            if obj.get("instructions").and_then(|v| v.as_str()) == Some("") {
                obj.remove("instructions");
            }
        }
        if sanitize_gpt_reasoning_items(&mut body_json) {
            tracing::debug!("已清理无效 GPT reasoning encrypted_content");
        }
    }

    // Grok Responses:对齐 CPA xai_executor 明确 DeleteBytes 的字段;
    // 保留 temperature/top_p/max_output_tokens(CPA preserveXAIResponsesOutputControls /
    // 原生 ModelEntry 可带采样)。须在 payload_overrides 之后。
    if matches!(route.protocol, Protocol::OpenAiResponses) && is_grok_model(&outbound_model) {
        if let Some(obj) = body_json.as_object_mut() {
            for key in [
                "previous_response_id",
                "prompt_cache_retention",
                "safety_identifier",
                "stream_options",
                "stop",
            ] {
                obj.remove(key);
            }
        }
    }

    // OpenAI Chat:payload 后再守卫(kimi 温度 / 非 qwen|glm 剥 tool_stream / MCT 键对齐)
    if matches!(route.protocol, Protocol::OpenAiChat) {
        sanitize_openai_chat_after_payload(&mut body_json, &outbound_model);
    }

    // 6. 对齐 StripPromptCacheRetention
    if !matches!(route.protocol, Protocol::Claude) {
        body_json
            .as_object_mut()
            .map(|m| m.remove("prompt_cache_retention"));
    }

    // 7. 上游连接参数提取并释放读锁
    let (
        upstream_base_urls,
        mut upstream_key,
        mut pool_refresh_token,
        upstream_proxy,
        provider_prompt_cache_key,
        antigravity_refresh,
        xai_refresh,
        codex_refresh,
        claude_refresh,
    ) = {
        let provider = route
            .provider_config(providers)
            .ok_or_else(|| AppError::new(anyhow::anyhow!("provider 未找到: {}", route.provider)))?;
        let meta = provider.metadata.as_ref();
        let pool_refresh_token = if matches!(route.protocol, Protocol::Antigravity) {
            meta.and_then(|m| m.get("refresh_token"))
                .map(|s| s.as_str().trim().to_string())
                .filter(|s| !s.is_empty())
        } else {
            None
        };
        let credential_path = |provider_type: &str| {
            meta.filter(|m| m.get("provider_type").map(|s| s.as_str()) == Some(provider_type))
                .and_then(|m| m.get("credential_path").cloned())
        };
        let antigravity_refresh = if matches!(route.protocol, Protocol::Antigravity) {
            meta.and_then(|m| m.get("credential_path")).cloned()
        } else {
            None
        };
        let xai_refresh = credential_path("xai");
        let codex_refresh = credential_path("codex");
        let claude_refresh = credential_path("claude");
        (
            provider.base_urls().to_vec(),
            provider.key.clone(),
            pool_refresh_token,
            provider.proxy_url.clone(),
            provider.prompt_cache_key,
            antigravity_refresh,
            xai_refresh,
            codex_refresh,
            claude_refresh,
        )
    };
    // 空 base_url 列表会让轮转循环与内部重试越界,提前拒绝
    if upstream_base_urls.is_empty() {
        return Err(AppError::new(anyhow::anyhow!(
            "provider {} 未配置 base_url",
            route.provider
        )));
    }

    // OAuth 订阅:运行时 token 校验与自动刷新;失败硬失败,不得继续打旧 token
    let mut oauth_refresh: Option<OAuthRefreshTarget> = None;
    let mut codex_account_id: Option<String> = None;

    if let Some(path) = antigravity_refresh {
        match crate::antigravity::ensure_credential_fresh(
            std::path::Path::new(&path),
            upstream_proxy.as_deref(),
            None,
        )
        .await
        {
            Ok(fresh_cred) => {
                upstream_key = fresh_cred.access_token;
                if !fresh_cred.refresh_token.trim().is_empty() {
                    pool_refresh_token = Some(fresh_cred.refresh_token);
                }
                // 对齐 CPA projectIDForRequest:缺 project 硬失败;发现结果写回出站 body
                let pid = fresh_cred.project_id.trim();
                if pid.is_empty() {
                    return Err(AppError::bad_request("antigravity auth missing project_id"));
                }
                body_json["project"] = serde_json::Value::String(pid.to_string());
                oauth_refresh = Some(OAuthRefreshTarget::Antigravity { path });
            }
            Err(e) => {
                return Err(AppError::unauthorized(format!(
                    "Antigravity 凭证刷新失败,拒绝使用旧 token: {e}"
                )));
            }
        }
    }

    if let Some(path) = xai_refresh {
        match crate::xai::ensure_credential_fresh(
            std::path::Path::new(&path),
            upstream_proxy.as_deref(),
            None,
        )
        .await
        {
            Ok(fresh_cred) => {
                upstream_key = fresh_cred.access_token;
                oauth_refresh = Some(OAuthRefreshTarget::Xai { path });
            }
            Err(e) => {
                return Err(AppError::unauthorized(format!(
                    "xAI 凭证刷新失败,拒绝使用旧 token: {e}"
                )));
            }
        }
    }

    if let Some(path) = codex_refresh {
        match crate::codex::ensure_credential_fresh(
            std::path::Path::new(&path),
            upstream_proxy.as_deref(),
            None,
        )
        .await
        {
            Ok(fresh_cred) => {
                upstream_key = fresh_cred.access_token;
                if !fresh_cred.account_id.is_empty() {
                    codex_account_id = Some(fresh_cred.account_id);
                }
                oauth_refresh = Some(OAuthRefreshTarget::Codex { path });
            }
            Err(e) => {
                return Err(AppError::unauthorized(format!(
                    "Codex 凭证刷新失败,拒绝使用旧 token: {e}"
                )));
            }
        }
    }

    if let Some(path) = claude_refresh {
        match crate::claude::ensure_credential_fresh(
            std::path::Path::new(&path),
            upstream_proxy.as_deref(),
            None,
        )
        .await
        {
            Ok(fresh_cred) => {
                upstream_key = fresh_cred.access_token;
                oauth_refresh = Some(OAuthRefreshTarget::Claude { path });
            }
            Err(e) => {
                return Err(AppError::unauthorized(format!(
                    "Claude 凭证刷新失败,拒绝使用旧 token: {e}"
                )));
            }
        }
    }

    // prompt_cache_key 注入
    if should_inject_prompt_cache_key(provider_prompt_cache_key, route.protocol, &outbound_model)
        && inject_prompt_cache_key(&mut body_json, cc_session.as_deref())
    {
        tracing::debug!("prompt_cache_key 已注入");
    }

    // 首发前预降级:该 (provider, model) 已有 unfit 标记时直接改 body,
    // 避免每个请求都先吃一次 400 再重试。adapt 幂等,重试路径再次调用安全。
    let registry = crate::http::unfit::global_registry();
    if registry.has_any(&route.provider, &route.upstream_model)
        && crate::http::unfit::adapt_request_for_unfit(
            &mut body_json,
            &route.provider,
            &route.upstream_model,
            registry.as_ref(),
            route.protocol,
        )
    {
        tracing::info!(
            provider = route.provider,
            model = route.upstream_model,
            "unfit: pre-adapted before first upstream send"
        );
    }

    // 诊断落盘
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
        let log_dir = config_snapshot
            .runtime
            .logging
            .log_dir
            .as_deref()
            .unwrap_or_else(|| std::path::Path::new("logs"));
        let _ = std::fs::create_dir_all(log_dir);
        let dump_obj = json!({
            "inbound_headers": inbound_headers_json(headers),
            "upstream_body": body_json,
        });
        let path = log_dir.join(format!("upstream_request_{log_stem}.json"));
        let dumped = serde_json::to_vec_pretty(&dump_obj)
            .ok()
            .and_then(|bytes| std::fs::write(&path, bytes).ok());
        if dumped.is_none() {
            tracing::warn!(path = %path.display(), "上游请求诊断信息落盘失败");
        }
    }

    let inbound_user_agent = if matches!(route.protocol, Protocol::Claude) {
        claude_inbound_user_agent(headers)
    } else {
        None
    };
    let extra_headers = if matches!(route.protocol, Protocol::Claude) {
        // 原生 Claude 直通:最小透传(保留 UA/Stainless);转换路径不走这里
        let mut map = claude_passthrough_headers(headers);
        if crate::claude::is_claude_oauth_token(&upstream_key) {
            ensure_oauth_beta(&mut map);
        }
        map
    } else if let Some(account_id) = codex_account_id.as_deref() {
        // Codex 订阅身份头 (对齐 CPA: 仅 OAuth 账号发,API key provider 无 metadata 不发)
        let mut map = HeaderMap::new();
        if let Ok(value) = axum::http::HeaderValue::from_str(account_id) {
            map.insert(
                axum::http::HeaderName::from_static("chatgpt-account-id"),
                value,
            );
        }
        map
    } else {
        HeaderMap::new()
    };

    let is_grok = is_grok_model(&outbound_model);
    let (session_id, thread_id) = if matches!(route.protocol, Protocol::OpenAiResponses) {
        (cc_session.as_deref(), extract_claude_code_thread(headers))
    } else if (is_grok && matches!(route.protocol, Protocol::OpenAiChat))
        || matches!(route.protocol, Protocol::Antigravity)
    {
        (cc_session.as_deref(), None)
    } else {
        (None, None)
    };

    // Responses 专用；Antigravity 不占用本 scope（形状不兼容）。
    let replay_scope = if matches!(route.protocol, Protocol::OpenAiResponses) {
        session_id.map(|s| {
            (
                state.replay_cache.clone(),
                format!("{}:{}", route.upstream_model, s),
                request_fingerprint.clone(),
            )
        })
    } else {
        None
    };

    let ag_replay_scope = if matches!(route.protocol, Protocol::Antigravity)
        && ccextra_core::antigravity_uses_reasoning_replay(&route.upstream_model)
    {
        ag_replay_key
            .clone()
            .map(|key| (state.ag_replay_cache.clone(), key))
    } else {
        None
    };

    let kimi_replay_scope = if matches!(route.protocol, Protocol::Claude)
        && is_kimi_thinking_replay_model(&route.upstream_model)
    {
        cc_session.as_deref().map(|s| {
            let family = kimi_thinking_replay_model_family(&route.upstream_model);
            (
                state.kimi_thinking_replay_cache.clone(),
                format!("{family}:{s}"),
                kimi_replay_applied,
            )
        })
    } else {
        None
    };

    let signature_model: Option<Arc<str>> = matches!(route.protocol, Protocol::Antigravity)
        .then(|| Arc::from(route.upstream_model.as_str()));

    let compress = extra_headers.contains_key("chatgpt-account-id");
    let encoded_body = crate::upstream::encode_request_body(route.protocol, &body_json, compress)
        .await
        .map_err(AppError::new)?;

    Ok(PreparedMessageRequest {
        route,
        body_json,
        encoded_body,
        is_stream,
        upstream_base_urls,
        active_base_url: None,
        upstream_key,
        pool_refresh_token,
        upstream_proxy,
        oauth_refresh,
        session_id: session_id.map(str::to_string),
        thread_id,
        extra_headers,
        inbound_user_agent: inbound_user_agent.map(str::to_string),
        inbound_query: inbound_query
            .map(str::trim)
            .filter(|q| !q.is_empty())
            .map(str::to_string),
        estimated_input_tokens,
        tool_names,
        original_claude_request,
        replay_scope,
        ag_replay_scope,
        ag_replay_snapshot,
        kimi_replay_scope,
        signature_model,
        inbound_model: Arc::from(model.as_str()),
        token_scope: session_id
            .map(|session| (Arc::clone(&state.last_input_tokens), session.to_string())),
        drift_usage_scope: drift_parent_for_usage.map(|parent| (state.drift.clone(), parent)),
        unfit_generation,
    })
}

pub(crate) async fn execute_upstream_request(
    prepared: &mut PreparedMessageRequest,
    upstream_client: &crate::upstream::UpstreamClient,
    user_agents: &crate::http::UserAgentSet,
) -> Result<ExecutedUpstream, AppError> {
    // 单轮请求:429/5xx/网络错误在同轮内轮转多 base_url,耗尽后快速失败,
    // 末次上游错误原样返给客户端(Claude Code 自带退避重试,代理不代等)
    enum Out {
        Ok(UpstreamResponse),
        Fail(UpstreamResponse),
        Net(anyhow::Error),
    }
    let mut active_base_url: Option<String> = None;
    let outcome = 'round: {
        for (idx, base_url) in prepared.upstream_base_urls.iter().enumerate() {
            let has_next = idx + 1 < prepared.upstream_base_urls.len();
            match upstream_client
                .request_encoded(
                    base_url,
                    &prepared.upstream_key,
                    prepared.route.protocol,
                    prepared.upstream_proxy.as_deref(),
                    prepared.encoded_body.clone(),
                    &prepared.body_json,
                    prepared.is_stream,
                    prepared.session_id.as_deref(),
                    prepared.thread_id.as_deref(),
                    &prepared.extra_headers,
                    user_agents,
                    prepared.inbound_user_agent.as_deref(),
                    prepared.pool_refresh_token.as_deref(),
                    prepared.inbound_query.as_deref(),
                )
                .await
            {
                Ok(resp) => {
                    let status = resp.status;
                    // 429/5xx 轮转下一 base_url(429 对齐 codex retry_429: false 不本地重试,
                    // 5xx 此前直接放弃回退,现与网络错误同等对待)
                    if (status.as_u16() == 429 || status.is_server_error()) && has_next {
                        tracing::debug!(
                            status = status.as_u16(),
                            "上游可重试失败,回退到下一个 base_url: {}",
                            base_url
                        );
                        continue;
                    }
                    active_base_url = Some(base_url.clone());
                    if status.is_success() {
                        break 'round Out::Ok(resp);
                    }
                    break 'round Out::Fail(resp);
                }
                Err(e) => {
                    if has_next {
                        tracing::debug!("上游请求错误,回退到下一个 base_url: {}", base_url);
                        continue;
                    }
                    break 'round Out::Net(e);
                }
            }
        }
        unreachable!("base_urls 非空,循环内必 return/break")
    };
    prepared.active_base_url = active_base_url;
    let mut upstream = match outcome {
        Out::Ok(resp) | Out::Fail(resp) => Some(resp),
        // 网络错误已轮转完全部 base_url:直接返给客户端,不本地退避重试
        Out::Net(e) => return Err(e.into()),
    };
    let mut status = upstream.as_ref().expect("上游响应应存在").status;
    let mut preloaded_stream = None;

    if prepared.is_stream
        && matches!(
            prepared.route.protocol,
            Protocol::OpenAiChat | Protocol::OpenAiResponses
        )
        && status.is_success()
    {
        for attempt in 0..=1 {
            let current = upstream.take().expect("上游响应应存在");
            status = current.status;
            let mut out = relay_with_replay_tap(
                current.body.bytes_stream(),
                RelayParams {
                    protocol: prepared.route.protocol,
                    estimated_input_tokens: prepared.estimated_input_tokens,
                    tool_names: prepared.tool_names.clone(),
                    original_claude_request: prepared.original_claude_request.clone(),
                    replay_tap: match prepared.replay_scope.clone() {
                        Some((cache, key, fingerprint)) => {
                            ReplayTap::Responses(cache, key, fingerprint)
                        }
                        None => ReplayTap::None,
                    },
                    signature_model: prepared.signature_model.clone(),
                    inbound_model: Some(Arc::clone(&prepared.inbound_model)),
                    token_scope: prepared.token_scope.clone(),
                    drift_usage_scope: prepared.drift_usage_scope.clone(),
                },
            );
            // 首帧只认业务帧:跳过心跳注释帧,防 keepalive 骗过错误检测提前提交 200
            let first = next_business_frame(&mut out).await;
            let bad = match &first {
                Some(Ok(frame)) => is_initial_sse_error(frame),
                Some(Err(_)) | None => true,
            };
            if bad {
                if attempt == 0 {
                    tracing::warn!(
                        protocol = ?prepared.route.protocol,
                        retry_attempt = attempt + 1,
                        "首帧转换失败，重试上游请求"
                    );
                    upstream = Some(prepared.send_retry(upstream_client, user_agents).await?);
                    status = upstream.as_ref().expect("上游响应应存在").status;
                    if !status.is_success() {
                        break;
                    }
                    continue;
                }
                // 二次仍失败:不提交 200,转 502 HTTP 错误交客户端整回合重试
                let msg = match &first {
                    Some(Ok(frame)) => initial_sse_error_message(frame),
                    Some(Err(e)) => e.to_string(),
                    None => "上游流在首帧前结束".to_string(),
                };
                return Err(AppError::with_status(
                    axum::http::StatusCode::BAD_GATEWAY,
                    format!("上游首帧失败: {msg}"),
                ));
            }
            let first = first.expect("bad 为 false 时必有首帧");
            preloaded_stream = Some(prepend_sse_frame(first, out));
            break;
        }
    }

    Ok(ExecutedUpstream {
        status,
        upstream,
        preloaded_stream,
    })
}

pub(crate) async fn deliver_response(
    state: &AppState,
    prepared: &mut PreparedMessageRequest,
    mut executed: ExecutedUpstream,
    upstream_client: &crate::upstream::UpstreamClient,
    user_agents: &crate::http::UserAgentSet,
) -> Result<Response, AppError> {
    let mut status = executed.status;

    // 上游错误:转 anthropic error 形状
    if !status.is_success() {
        let failed = executed.upstream.take().expect("上游响应应存在");
        // Retry-After 透传给客户端(对齐 codex 把 server retry advice 透出给上层):
        // 429 快速失败后由客户端按上游声明退避,代理不代等
        let mut retry_after = parse_retry_after(failed.body.headers());
        let (err_bytes, err_truncated) =
            crate::limits::read_error_body_or_anthropic(failed.body, status).await?;
        let mut final_status = status;
        let mut final_bytes = err_bytes;
        let mut final_truncated = err_truncated;
        let mut retried_ok = false;

        // 上游 401:OAuth 凭证强制刷新后重试一次;刷新失败硬失败,不得继续打旧 token
        if status.as_u16() == 401 {
            match prepared.refresh_rejected_token().await {
                Ok(true) => {
                    tracing::info!(
                        protocol = ?prepared.route.protocol,
                        provider = %prepared.route.provider,
                        "上游 401,凭证已刷新,重试一次"
                    );
                    if let Some((cache, key, _)) = prepared.replay_scope.as_ref() {
                        cache.invalidate(key);
                    }
                    // AG 账本不清：对齐 CPA，401 只走凭证刷新重试，
                    // clearAntigravityReasoningReplayOnInvalidSignature 仅处理 400
                    if let Some((cache, key, _)) = prepared.kimi_replay_scope.as_ref() {
                        cache.invalidate(key);
                    }
                    let retry = prepared.send_retry(upstream_client, user_agents).await?;
                    final_status = retry.status;
                    // 成败均采用重试码:后续 unfit / AG·Kimi 清账本 / encrypted trim
                    // 必须看到最终上游结果,而非首发 401(401 不触发任何门禁)
                    status = retry.status;
                    if retry.status.is_success() {
                        executed.upstream = Some(retry);
                        retried_ok = true;
                    } else {
                        retry_after = parse_retry_after(retry.body.headers());
                        let (bytes, truncated) =
                            crate::limits::read_error_body_or_anthropic(retry.body, final_status)
                                .await?;
                        final_bytes = bytes;
                        final_truncated = truncated;
                    }
                }
                Ok(false) => {}
                Err(e) => return Err(e),
            }
        }

        // Unfit 探测与自适应降级(对齐 magpie gateway.go:3440-3490)
        if !final_truncated && (status.as_u16() == 400 || status.as_u16() == 422) {
            let err_text = String::from_utf8_lossy(&final_bytes);
            if let Some(kind) =
                crate::http::unfit::detect_unfit_from_error(status.as_u16(), &err_text)
            {
                let registry = crate::http::unfit::global_registry();
                registry.mark(
                    &prepared.route.provider,
                    &prepared.route.upstream_model,
                    kind,
                    prepared.unfit_generation,
                );

                // 原地修复请求并重试
                if crate::http::unfit::adapt_request_for_unfit(
                    &mut prepared.body_json,
                    &prepared.route.provider,
                    &prepared.route.upstream_model,
                    registry.as_ref(),
                    prepared.route.protocol,
                ) {
                    tracing::info!(
                        provider = prepared.route.provider,
                        model = prepared.route.upstream_model,
                        kind = ?kind,
                        "unfit detected, retrying with adapted request"
                    );
                    prepared.reencode_body().await?;
                    if let Some((cache, key, _)) = prepared.replay_scope.as_ref() {
                        cache.invalidate(key);
                    }
                    // AG 账本不清：CPA 无 unfit 自适应重试路径，也无对应清理；重试结果仍由
                    // 签名无效清理（400 + IfUnchanged）与成功提交（ReplaceIfUnchanged）处理
                    if let Some((cache, key, _)) = prepared.kimi_replay_scope.as_ref() {
                        cache.invalidate(key);
                    }
                    let retry = prepared.send_retry(upstream_client, user_agents).await?;
                    final_status = retry.status;
                    // 后续门禁(AG/Kimi 账本清理、trim)看重试后的状态,与 401 分支一致
                    status = retry.status;
                    if retry.status.is_success() {
                        executed.upstream = Some(retry);
                        retried_ok = true;
                    } else {
                        retry_after = parse_retry_after(retry.body.headers());
                        let (bytes, truncated) =
                            crate::limits::read_error_body_or_anthropic(retry.body, final_status)
                                .await?;
                        final_bytes = bytes;
                        final_truncated = truncated;
                    }
                }
            }
        }

        if !retried_ok
            && !final_truncated
            && should_clear_ag_replay_on_error(
                prepared.route.protocol,
                status.as_u16(),
                &final_bytes,
            )
        {
            tracing::warn!(
                protocol = ?prepared.route.protocol,
                "antigravity thoughtSignature 无效,清空 replay 账本"
            );
            if let Some((cache, key)) = prepared.ag_replay_scope.as_ref() {
                // 对齐 CPA DeleteAntigravityReasoningReplayItemsIfUnchanged(scope.cacheSnapshot)
                cache.invalidate_if_unchanged(key, &prepared.ag_replay_snapshot);
            }
        }

        // Kimi：注入后仍 400/422 → 清账本（对齐 CPA shouldClearKimiThinkingReplayAfterError）
        if !retried_ok && !final_truncated && (status.as_u16() == 400 || status.as_u16() == 422) {
            if let Some((cache, key, applied)) = prepared.kimi_replay_scope.as_ref() {
                if *applied {
                    tracing::warn!(
                        protocol = ?prepared.route.protocol,
                        "kimi thinking replay 注入后上游 400/422,清空账本"
                    );
                    cache.invalidate(key);
                }
            }
        }

        if !retried_ok
            && !final_truncated
            && (status.as_u16() == 400 || status.as_u16() == 422)
            && matches!(prepared.route.protocol, Protocol::OpenAiResponses)
            && is_thinking_signature_invalid(&final_bytes)
            && trim_encrypted_reasoning_items(&mut prepared.body_json)
        {
            tracing::warn!(
                protocol = ?prepared.route.protocol,
                "invalid_encrypted_content,剥离 reasoning 后重试一次"
            );
            prepared.reencode_body().await?;
            if let Some((cache, key, _)) = prepared.replay_scope.as_ref() {
                cache.invalidate(key);
            }
            let retry = prepared.send_retry(upstream_client, user_agents).await?;
            final_status = retry.status;
            // 后续门禁(AG/Kimi 账本清理、trim)看重试后的状态,与 401 分支一致
            status = retry.status;
            if retry.status.is_success() {
                executed.upstream = Some(retry);
                retried_ok = true;
            } else {
                retry_after = parse_retry_after(retry.body.headers());
                let (bytes, truncated) =
                    crate::limits::read_error_body_or_anthropic(retry.body, final_status).await?;
                final_bytes = bytes;
                final_truncated = truncated;
            }
        }

        if !retried_ok {
            let body = if final_truncated {
                serde_json::to_vec(&json!({
                    "type": "error",
                    "error": {
                        "type": "api_error",
                        "message": format!(
                            "上游错误 body 超过 256 KiB 上限,已截断(status {})",
                            final_status.as_u16()
                        )
                    }
                }))
                .unwrap_or_default()
            } else {
                to_anthropic_error(&final_bytes, final_status)
            };
            // Antigravity 429 常不带 Retry-After 头,等待时长在 body 的 RetryInfo 等字段里;
            // 上游头优先(对齐 CPA antigravity executor ParseRetryDelay)
            if retry_after.is_none()
                && final_status == axum::http::StatusCode::TOO_MANY_REQUESTS
                && !final_truncated
                && matches!(prepared.route.protocol, Protocol::Antigravity)
            {
                retry_after = parse_google_retry_delay(&final_bytes).map(retry_after_header_secs);
            }
            let mut builder = Response::builder()
                .status(final_status)
                .header(header::CONTENT_TYPE, "application/json");
            if let Some(ra) = retry_after {
                builder = builder.header("retry-after", ra.as_secs());
            }
            return builder
                .body(Body::from(body))
                .map_err(|e| AppError::new(anyhow::anyhow!("构造错误响应失败: {e}")));
        }
    }

    // 响应转换
    if prepared.is_stream {
        let out = if let Some(out) = executed.preloaded_stream {
            out
        } else {
            let upstream = executed.upstream.take().expect("上游响应应存在");
            {
                let replay_tap =
                    if let Some((cache, key, fingerprint)) = prepared.replay_scope.clone() {
                        ReplayTap::Responses(cache, key, fingerprint)
                    } else if let Some((cache, key)) = prepared.ag_replay_scope.as_ref() {
                        ReplayTap::Antigravity(
                            cache.clone(),
                            key.clone(),
                            prepared.ag_replay_snapshot.clone(),
                            prepared
                                .body_json
                                .get("request")
                                .cloned()
                                .unwrap_or_else(|| Value::Object(Default::default())),
                        )
                    } else if let Some((cache, key, applied)) = prepared.kimi_replay_scope.clone() {
                        ReplayTap::Kimi(cache, key, applied)
                    } else {
                        ReplayTap::None
                    };
                relay_with_replay_tap(
                    upstream.body.bytes_stream(),
                    RelayParams {
                        protocol: prepared.route.protocol,
                        estimated_input_tokens: prepared.estimated_input_tokens,
                        tool_names: prepared.tool_names.clone(),
                        original_claude_request: prepared.original_claude_request.clone(),
                        replay_tap,
                        signature_model: prepared.signature_model.clone(),
                        inbound_model: Some(Arc::clone(&prepared.inbound_model)),
                        token_scope: prepared.token_scope.clone(),
                        drift_usage_scope: prepared.drift_usage_scope.clone(),
                    },
                )
            }
        };
        Ok(Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .header(header::CACHE_CONTROL, "no-cache")
            .body(Body::from_stream(out))
            .map_err(|e| AppError::new(anyhow::anyhow!("构造流式响应失败: {e}")))?)
    } else {
        let upstream = executed.upstream.take().expect("上游响应应存在");
        let body_bytes = crate::limits::read_success_body_or_anthropic(upstream.body).await?;
        // 成功 body 只解析一次,供 replay / AG / Kimi / drift / 协议转换复用
        let parsed: Option<Value> = serde_json::from_slice(&body_bytes).ok();
        if let Some(ref v) = parsed {
            if let Some((cache, key, fingerprint)) = prepared.replay_scope.as_ref() {
                if matches!(prepared.route.protocol, Protocol::OpenAiResponses)
                    && v.get("object").and_then(|o| o.as_str()) == Some("response")
                {
                    let wrapped = json!({"response": v});
                    cache.store_from_completed(key, &wrapped, fingerprint);
                }
            }
            if let Some((cache, key)) = prepared.ag_replay_scope.as_ref() {
                if matches!(prepared.route.protocol, Protocol::Antigravity) {
                    if let Some(request) = prepared.body_json.get("request") {
                        cache.store_from_response(key, &prepared.ag_replay_snapshot, request, v);
                    }
                }
            }
            if let Some((cache, key, _)) = prepared.kimi_replay_scope.as_ref() {
                if matches!(prepared.route.protocol, Protocol::Claude) {
                    cache.store_from_message_response(key, v);
                }
            }
            if let Some((drift, parent)) = prepared.drift_usage_scope.as_ref() {
                let (cache_read, cache_creation) = match prepared.route.protocol {
                    Protocol::Claude => (
                        v.pointer("/usage/cache_read_input_tokens")
                            .and_then(|t| t.as_i64())
                            .unwrap_or(0),
                        v.pointer("/usage/cache_creation_input_tokens")
                            .and_then(|t| t.as_i64())
                            .unwrap_or(0),
                    ),
                    Protocol::OpenAiChat => (
                        v.pointer("/usage/prompt_tokens_details/cached_tokens")
                            .and_then(|t| t.as_i64())
                            .unwrap_or(0),
                        v.pointer("/usage/prompt_tokens_details/cache_write_tokens")
                            .or_else(|| {
                                v.pointer("/usage/prompt_tokens_details/cache_creation_tokens")
                            })
                            .and_then(|t| t.as_i64())
                            .unwrap_or(0),
                    ),
                    Protocol::OpenAiResponses => (
                        v.pointer("/usage/input_tokens_details/cached_tokens")
                            .and_then(|t| t.as_i64())
                            .unwrap_or(0),
                        v.pointer("/usage/input_tokens_details/cache_write_tokens")
                            .or_else(|| {
                                v.pointer("/usage/input_tokens_details/cache_creation_tokens")
                            })
                            .and_then(|t| t.as_i64())
                            .unwrap_or(0),
                    ),
                    _ => (0, 0),
                };
                if matches!(
                    prepared.route.protocol,
                    Protocol::Claude | Protocol::OpenAiChat | Protocol::OpenAiResponses
                ) {
                    observe_cache_usage(drift, parent, cache_read, cache_creation);
                }
            }
        }
        let converted = match parsed {
            Some(v) => {
                if let Some(sid) = prepared.session_id.as_deref() {
                    let input_tokens = match prepared.route.protocol {
                        Protocol::OpenAiChat => {
                            v.pointer("/usage/prompt_tokens").and_then(|t| t.as_i64())
                        }
                        Protocol::OpenAiResponses => {
                            v.pointer("/usage/input_tokens").and_then(|t| t.as_i64())
                        }
                        Protocol::Gemini | Protocol::Antigravity => {
                            let inner = if prepared.route.protocol == Protocol::Antigravity {
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
                match prepared.route.protocol {
                    Protocol::Claude => None,
                    Protocol::OpenAiChat => crate::sse::non_stream::openai_chat_to_anthropic(&v),
                    Protocol::OpenAiResponses => crate::sse::non_stream::responses_to_anthropic(
                        &v,
                        prepared.tool_names.as_deref(),
                    ),
                    Protocol::Gemini => {
                        use ccextra_core::convert::convert_gemini_response;
                        Some(convert_gemini_response(
                            &v,
                            prepared.tool_names.as_deref().unwrap_or(&HashMap::new()),
                            None,
                            prepared.original_claude_request.as_deref(),
                        ))
                    }
                    Protocol::Antigravity => {
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
                            prepared.tool_names.as_deref().unwrap_or(&HashMap::new()),
                            Some(prepared.route.upstream_model.as_str()),
                            prepared.original_claude_request.as_deref(),
                        ))
                    }
                }
            }
            None => None,
        };
        // 对齐 CPA 7f2f17d7:转换层产出的 Claude api_error(如 MALFORMED)抬 502 供客户端重试
        if let Some(out) = converted.as_ref() {
            if out.get("type").and_then(|t| t.as_str()) == Some("error") {
                let msg = out
                    .pointer("/error/message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("upstream translated api_error")
                    .to_string();
                let payload = serde_json::to_vec(out).unwrap_or_else(|_| {
                    serde_json::to_vec(&serde_json::json!({
                        "type": "error",
                        "error": {"type": "api_error", "message": msg}
                    }))
                    .unwrap_or_default()
                });
                return Response::builder()
                    .status(axum::http::StatusCode::BAD_GATEWAY)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(payload))
                    .map_err(|e| AppError::new(anyhow::anyhow!("构造响应失败: {e}")));
            }
        }
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

impl PreparedMessageRequest {
    async fn reencode_body(&mut self) -> Result<(), AppError> {
        let compress = self.extra_headers.contains_key("chatgpt-account-id");
        self.encoded_body =
            crate::upstream::encode_request_body(self.route.protocol, &self.body_json, compress)
                .await
                .map_err(AppError::new)?;
        Ok(())
    }

    /// Codex 401 刷新后写入 chatgpt-account-id。
    /// 初始无该头时 body 未压;补头后 compress 变 true,须 reencode,
    /// 否则重试仍发明文却声明 Content-Encoding: zstd。已有头则只更新值。
    async fn apply_codex_account_id_after_refresh(
        &mut self,
        account_id: &str,
    ) -> Result<(), AppError> {
        if account_id.is_empty() {
            return Ok(());
        }
        let compress_became_true = !self.extra_headers.contains_key("chatgpt-account-id");
        let Ok(value) = axum::http::HeaderValue::from_str(account_id) else {
            return Ok(());
        };
        self.extra_headers.insert(
            axum::http::HeaderName::from_static("chatgpt-account-id"),
            value,
        );
        if compress_became_true {
            self.reencode_body().await?;
        }
        Ok(())
    }

    async fn send_retry(
        &self,
        client: &crate::upstream::UpstreamClient,
        user_agents: &crate::http::UserAgentSet,
    ) -> anyhow::Result<UpstreamResponse> {
        client
            .request_encoded(
                self.active_base_url
                    .as_deref()
                    .unwrap_or(&self.upstream_base_urls[0]),
                &self.upstream_key,
                self.route.protocol,
                self.upstream_proxy.as_deref(),
                self.encoded_body.clone(),
                &self.body_json,
                self.is_stream,
                self.session_id.as_deref(),
                self.thread_id.as_deref(),
                &self.extra_headers,
                user_agents,
                self.inbound_user_agent.as_deref(),
                self.pool_refresh_token.as_deref(),
                self.inbound_query.as_deref(),
            )
            .await
    }

    /// 上游 401 后强制刷新被拒 token。
    /// Ok(true)=已换新 access_token;Ok(false)=非 OAuth/无可刷新目标;Err=刷新失败(硬失败)。
    async fn refresh_rejected_token(&mut self) -> Result<bool, AppError> {
        let Some(target) = self.oauth_refresh.clone() else {
            return Ok(false);
        };
        let rejected = self.upstream_key.clone();
        let proxy = self.upstream_proxy.as_deref();
        match target {
            OAuthRefreshTarget::Antigravity { path } => {
                let fresh = crate::antigravity::ensure_credential_fresh(
                    std::path::Path::new(&path),
                    proxy,
                    Some(&rejected),
                )
                .await
                .map_err(|e| {
                    AppError::unauthorized(format!(
                        "Antigravity 401 后刷新失败,拒绝使用旧 token: {e}"
                    ))
                })?;
                self.upstream_key = fresh.access_token;
                if !fresh.refresh_token.trim().is_empty() {
                    self.pool_refresh_token = Some(fresh.refresh_token);
                }
                let pid = fresh.project_id.trim();
                if pid.is_empty() {
                    return Err(AppError::bad_request("antigravity auth missing project_id"));
                }
                // 与首跳 ensure_credential_fresh 一致:把新 project_id 写回出站 body
                self.body_json["project"] = serde_json::Value::String(pid.to_string());
                self.reencode_body().await?;
                Ok(true)
            }
            OAuthRefreshTarget::Claude { path } => {
                let fresh = crate::claude::ensure_credential_fresh(
                    std::path::Path::new(&path),
                    proxy,
                    Some(&rejected),
                )
                .await
                .map_err(|e| {
                    AppError::unauthorized(format!("Claude 401 后刷新失败,拒绝使用旧 token: {e}"))
                })?;
                self.upstream_key = fresh.access_token;
                Ok(true)
            }
            OAuthRefreshTarget::Codex { path } => {
                let fresh = crate::codex::ensure_credential_fresh(
                    std::path::Path::new(&path),
                    proxy,
                    Some(&rejected),
                )
                .await
                .map_err(|e| {
                    AppError::unauthorized(format!("Codex 401 后刷新失败,拒绝使用旧 token: {e}"))
                })?;
                self.upstream_key = fresh.access_token;
                self.apply_codex_account_id_after_refresh(&fresh.account_id)
                    .await?;
                Ok(true)
            }
            OAuthRefreshTarget::Xai { path } => {
                let fresh = crate::xai::ensure_credential_fresh(
                    std::path::Path::new(&path),
                    proxy,
                    Some(&rejected),
                )
                .await
                .map_err(|e| {
                    AppError::unauthorized(format!("xAI 401 后刷新失败,拒绝使用旧 token: {e}"))
                })?;
                self.upstream_key = fresh.access_token;
                Ok(true)
            }
        }
    }
}

pub async fn handle_messages(
    State(state): State<AppState>,
    uri: axum::http::Uri,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, AppError> {
    // 同读锁捕获快照与 unfit 世代,与 /reload 写锁内 clear 成对:
    // prepare 中途 await 期间 reload 增代后,旧快照请求的 mark 必被丢弃。
    let (effective_snapshot, unfit_generation) = state.snapshot_and_unfit_generation().await;
    check_secret(&headers, &effective_snapshot.runtime.secret).await?;
    let bytes = to_bytes(body, crate::limits::INBOUND_BODY_LIMIT)
        .await
        .map_err(|e| AppError::new(anyhow::anyhow!("读请求体失败: {e}")))?;

    let mut prepared = prepare_message_request(
        &state,
        &headers,
        &bytes,
        &effective_snapshot,
        uri.query(),
        unfit_generation,
    )
    .await?;

    let executed = execute_upstream_request(
        &mut prepared,
        &effective_snapshot.runtime.upstream,
        &effective_snapshot.runtime.user_agents,
    )
    .await?;

    deliver_response(
        &state,
        &mut prepared,
        executed,
        &effective_snapshot.runtime.upstream,
        &effective_snapshot.runtime.user_agents,
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;

    fn frame(s: &str) -> Result<Bytes, std::io::Error> {
        Ok(Bytes::from(s.to_string()))
    }

    fn pinned(frames: Vec<Result<Bytes, std::io::Error>>) -> SseStreamPin {
        Box::pin(stream::iter(frames))
    }

    /// 多个心跳后到达 error 帧:心跳全部跳过,仍被首帧错误检测识别,message 可提取。
    #[tokio::test]
    async fn next_business_frame_returns_error_frame_after_keepalive() {
        let mut out = pinned(vec![
            frame(": keepalive\n\n"),
            frame(": keepalive\n\n"),
            frame(
                "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"api_error\",\"message\":\"boom\"}}\n\n",
            ),
        ]);
        let first = next_business_frame(&mut out).await.unwrap().unwrap();
        assert!(is_initial_sse_error(&first));
        assert_eq!(initial_sse_error_message(&first), "boom");
    }

    /// 只有心跳后 EOF:返回 None(触发重试/502 路径),不把心跳当首帧。
    #[tokio::test]
    async fn next_business_frame_keepalive_then_eof_is_none() {
        let mut out = pinned(vec![frame(": keepalive\n\n")]);
        assert!(next_business_frame(&mut out).await.is_none());
    }

    /// error 帧 data 非法 JSON 时回退原始文本,不 panic。
    #[test]
    fn initial_sse_error_message_falls_back_to_raw_text() {
        let f = Bytes::from("event: error\ndata: not-json\n\n");
        assert!(is_initial_sse_error(&f));
        assert_eq!(
            initial_sse_error_message(&f),
            "event: error\ndata: not-json"
        );
    }

    fn stub_prepared_without_account_id() -> PreparedMessageRequest {
        let body_json = json!({"model": "gpt-test", "input": []});
        PreparedMessageRequest {
            route: ccextra_core::route::RouteDecision {
                provider: "codex".into(),
                provider_index: 0,
                protocol: Protocol::OpenAiResponses,
                upstream_model: "gpt-test".into(),
            },
            body_json,
            encoded_body: Bytes::new(),
            is_stream: false,
            upstream_base_urls: vec!["https://example.invalid".into()],
            active_base_url: None,
            upstream_key: "tok".into(),
            pool_refresh_token: None,
            upstream_proxy: None,
            oauth_refresh: None,
            session_id: None,
            thread_id: None,
            extra_headers: HeaderMap::new(),
            inbound_user_agent: None,
            inbound_query: None,
            estimated_input_tokens: None,
            tool_names: None,
            original_claude_request: None,
            replay_scope: None,
            ag_replay_scope: None,
            ag_replay_snapshot: Default::default(),
            kimi_replay_scope: None,
            signature_model: None,
            inbound_model: Arc::from("gpt-test"),
            token_scope: None,
            drift_usage_scope: None,
            unfit_generation: 0,
        }
    }

    /// AG 账本清理门禁对齐 CPA：仅 400；422 / 401 / 其它协议不清；含 "signature" 即命中。
    #[test]
    fn ag_replay_clear_gate_matches_cpa() {
        let body = br#"{"error":{"message":"Corrupted thought signature."}}"#;
        assert!(should_clear_ag_replay_on_error(
            Protocol::Antigravity,
            400,
            body
        ));
        assert!(should_clear_ag_replay_on_error(
            Protocol::Antigravity,
            400,
            b"signature mismatch"
        ));
        assert!(!should_clear_ag_replay_on_error(
            Protocol::Antigravity,
            422,
            body
        ));
        assert!(!should_clear_ag_replay_on_error(
            Protocol::Antigravity,
            401,
            body
        ));
        assert!(!should_clear_ag_replay_on_error(
            Protocol::Claude,
            400,
            body
        ));
        assert!(!should_clear_ag_replay_on_error(
            Protocol::Antigravity,
            400,
            br#"{"error":{"message":"quota exceeded"}}"#
        ));
    }

    #[test]
    fn auth_retry_gates_see_retry_status_not_initial_401() {
        // 真实覆盖在 deliver_response:401 刷新重试后 `status = retry.status`,
        // 门禁(400/422)看到最终码;首发 401 本身不触发任何门禁。
        // 本测仅钉住门禁谓词的上界:400 命中、401 不命中。
        // 钉住 deliver 那行赋值需 OAuth 刷新 + 两跳 mock 上游,留给集成测。
        let retry = axum::http::StatusCode::BAD_REQUEST;
        assert!(crate::http::unfit::detect_unfit_from_error(
            retry.as_u16(),
            r#"{"error":{"message":"unsupported response_format"}}"#
        )
        .is_some());
        assert!(crate::http::unfit::detect_unfit_from_error(
            axum::http::StatusCode::UNAUTHORIZED.as_u16(),
            r#"{"error":{"message":"unsupported response_format"}}"#
        )
        .is_none());
    }

    /// Codex 401 helper:无 chatgpt-account-id → 插入后须 reencode 为 zstd。
    /// 删掉 helper 内 reencode 调用会使本测变红。
    #[tokio::test]
    async fn apply_codex_account_id_enables_zstd_when_header_was_absent() {
        let mut prepared = stub_prepared_without_account_id();
        prepared.reencode_body().await.unwrap();
        let plain = prepared.encoded_body.clone();
        assert!(!plain.is_empty());
        assert!(serde_json::from_slice::<Value>(&plain).is_ok());

        prepared
            .apply_codex_account_id_after_refresh("acct-1")
            .await
            .unwrap();
        assert!(prepared.extra_headers.contains_key("chatgpt-account-id"));
        let compressed = prepared.encoded_body.clone();
        assert_ne!(plain, compressed);
        let decoded = zstd::stream::decode_all(&compressed[..]).expect("should be zstd");
        assert_eq!(decoded.as_slice(), plain.as_ref());
    }

    /// 已有 chatgpt-account-id 时只更新头,不触发 reencode(compress 未翻转)。
    #[tokio::test]
    async fn apply_codex_account_id_skips_reencode_when_header_present() {
        let mut prepared = stub_prepared_without_account_id();
        prepared.extra_headers.insert(
            axum::http::HeaderName::from_static("chatgpt-account-id"),
            "acct-old".parse().unwrap(),
        );
        prepared.reencode_body().await.unwrap();
        let before = prepared.encoded_body.clone();
        assert!(zstd::stream::decode_all(&before[..]).is_ok());

        prepared
            .apply_codex_account_id_after_refresh("acct-new")
            .await
            .unwrap();
        assert_eq!(
            prepared
                .extra_headers
                .get("chatgpt-account-id")
                .and_then(|v| v.to_str().ok()),
            Some("acct-new")
        );
        assert_eq!(prepared.encoded_body, before);
    }

    /// 空 account_id 为 no-op(不插头、不 reencode)。
    #[tokio::test]
    async fn apply_codex_account_id_empty_is_noop() {
        let mut prepared = stub_prepared_without_account_id();
        prepared.reencode_body().await.unwrap();
        let before = prepared.encoded_body.clone();
        prepared
            .apply_codex_account_id_after_refresh("")
            .await
            .unwrap();
        assert!(!prepared.extra_headers.contains_key("chatgpt-account-id"));
        assert_eq!(prepared.encoded_body, before);
    }
}
