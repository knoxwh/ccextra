use axum::{
    body::Body,
    extract::State,
    http::{HeaderMap, header},
    response::Response,
};
use bytes::Bytes;
use ccextra_core::cache_stabilization::{
    DriftApiKind, DriftObserveOptions, maybe_observe_request_drift_with, observe_cache_usage,
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
    TargetShape, normalize_anthropic_full, normalize_anthropic_pretransform, normalize_target_post,
};
use ccextra_core::prompt_cache::inject_prompt_cache_key;
use ccextra_core::route::{Protocol, resolve_route};
use ccextra_core::session::{
    extract_claude_code_agent_id, extract_claude_code_session, extract_claude_code_thread,
};
use futures::StreamExt;
use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use crate::http::auth::check_secret;
use crate::http::claude_relay::{claude_inbound_user_agent, claude_passthrough_headers};
use crate::http::error::{AppError, to_anthropic_error};
use crate::http::retry::{parse_google_retry_delay, parse_retry_after, retry_after_header_secs};
use crate::http::{AppState, PayloadRule};
use crate::sse::SseStreamPin;
use crate::sse::antigravity_replay_cache::AgStreamReplayExtractor;
use crate::sse::kimi_thinking_replay_cache::KimiStreamReplayExtractor;
use crate::sse::replay_cache::StreamReplayExtractor;
use crate::upstream::{UpstreamResponse, is_gpt_model, is_grok_model};

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
    if matches!(protocol, Protocol::AnthropicOfficial) {
        return;
    }
    for rule in rules {
        if matches!(protocol, Protocol::Claude) && rule.protocol.is_none() {
            continue;
        }
        if let Some(p) = rule.protocol
            && p != protocol
        {
            continue;
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

/// AG 403 是否为封号(对齐 CPA parseAntigravityTOSViolation:`error.code == 403`、
/// `error.status == PERMISSION_DENIED`、`error.details[]` 含 ErrorInfo `reason == TOS_VIOLATION`,
/// 后两者忽略大小写与首尾空白)。命中返回申诉链接,按 CPA redactTOSAppealURL 截掉 `?` / `#`
/// 之后的部分(防签名 / 跟踪参数进日志);无链接为空串。
fn antigravity_tos_violation_appeal_url(body: &[u8]) -> Option<String> {
    let v: Value = serde_json::from_slice(body).ok()?;
    let err = v.get("error")?;
    if err.get("code").and_then(Value::as_i64) != Some(403)
        || !err
            .get("status")
            .and_then(Value::as_str)
            .is_some_and(|s| s.trim().eq_ignore_ascii_case("PERMISSION_DENIED"))
    {
        return None;
    }
    err.get("details")?.as_array()?.iter().find_map(|d| {
        let is_tos = d.get("@type").and_then(Value::as_str)
            == Some("type.googleapis.com/google.rpc.ErrorInfo")
            && d.get("reason")
                .and_then(Value::as_str)
                .is_some_and(|r| r.trim().eq_ignore_ascii_case("TOS_VIOLATION"));
        is_tos.then(|| {
            let url = d
                .pointer("/metadata/appeal_url")
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim();
            url.split(['?', '#']).next().unwrap_or("").to_string()
        })
    })
}

/// 流式响应旁路提取 replay 的作用域;按协议互斥(Responses / Antigravity / Kimi)
enum ReplayTap {
    None,
    /// (cache, key, request_fingerprint)
    Responses(crate::sse::replay_cache::ReplayCache, String, String),
    /// 已按外发 request 构造的提取器(借用 request,不深拷贝):提交走 ReplaceIfUnchanged(snapshot)
    Antigravity(Box<AgStreamReplayExtractor>),
    /// Kimi thinking:(cache, key, replay_applied)
    Kimi(
        crate::sse::kimi_thinking_replay_cache::KimiThinkingReplayCache,
        String,
        bool,
    ),
}

/// 每个上游 SSE 字节块喂给 replay 提取器
type ReplayPush = Box<dyn FnMut(&[u8]) + Send>;

/// Gemini/AG 响应侧回剥占位 tool args 只读原始请求的 tools;
/// 快照仅保留 tools,避免长会话全量 body 深克隆。
fn tools_only_snapshot(body: &Value) -> Value {
    let mut snapshot = serde_json::Map::new();
    if let Some(tools) = body.get("tools") {
        snapshot.insert("tools".to_string(), tools.clone());
    }
    Value::Object(snapshot)
}

struct RelayParams {
    protocol: Protocol,
    estimated_input_tokens: Option<usize>,
    tool_names: Option<Arc<HashMap<String, String>>>,
    /// 转换前 Claude 请求 tools 快照(Gemini/AG 响应用来回剥占位 tool args)
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
        ReplayTap::Antigravity(mut extractor) => Box::new(move |bytes| extractor.push(bytes)),
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
            Some(Ok(frame)) if is_sse_comment_frame(&frame) => {}
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
    /// 转换前 Claude 请求 tools 快照(仅 Gemini/AG;响应用来回剥占位 tool args)
    pub original_claude_request: Option<Arc<Value>>,
    /// 按协议互斥的响应侧 replay 作用域(Responses / Antigravity / Kimi)
    pub replay: ReplayScope,
    pub signature_model: Option<Arc<str>>,
    /// 客户端入站 model 别名(body.model 路由前;Responses 回包 message_start 预填)
    pub inbound_model: Arc<str>,
    pub token_scope: Option<crate::http::session_tokens::TokenCacheScope>,
    /// drift 开启且父级稳定时：响应路径关联 cache usage（与 token_scope 分离）
    pub drift_usage_scope: Option<crate::http::session_tokens::DriftUsageScope>,
    /// 与 config 快照同读锁捕获的 unfit 世代;/reload 写锁内 clear 增代后旧请求 mark 丢弃
    pub unfit_generation: u64,
}

/// 按协议互斥的响应侧 replay 作用域(Responses / Antigravity / Kimi)
pub(crate) enum ReplayScope {
    None,
    Responses {
        cache: crate::sse::replay_cache::ReplayCache,
        key: String,
        fingerprint: String,
    },
    Antigravity {
        cache: crate::sse::antigravity_replay_cache::AgReplayCache,
        key: String,
        snapshot: crate::sse::antigravity_replay_cache::AgReplaySnapshot,
    },
    Kimi {
        cache: crate::sse::kimi_thinking_replay_cache::KimiThinkingReplayCache,
        key: String,
        applied: bool,
    },
}

impl ReplayScope {
    /// 401 / unfit 重试前失效缓存(AG 账本不清:对齐 CPA,仅 400 无效 signature 清理)
    fn invalidate_for_retry(&self) {
        match self {
            Self::Responses { cache, key, .. } => cache.invalidate(key),
            Self::Kimi { cache, key, .. } => cache.invalidate(key),
            Self::None | Self::Antigravity { .. } => {}
        }
    }

    fn stream_tap(&self, body_json: &Value) -> ReplayTap {
        match self {
            Self::None => ReplayTap::None,
            Self::Responses {
                cache,
                key,
                fingerprint,
            } => ReplayTap::Responses(cache.clone(), key.clone(), fingerprint.clone()),
            Self::Antigravity {
                cache,
                key,
                snapshot,
            } => {
                let empty = Value::Object(Default::default());
                ReplayTap::Antigravity(Box::new(AgStreamReplayExtractor::new(
                    cache.clone(),
                    key.clone(),
                    snapshot.clone(),
                    body_json.get("request").unwrap_or(&empty),
                )))
            }
            Self::Kimi {
                cache,
                key,
                applied,
            } => ReplayTap::Kimi(cache.clone(), key.clone(), *applied),
        }
    }
}

pub(crate) enum ExecutedUpstream {
    Raw(Box<UpstreamResponse>),
    Preloaded {
        status: axum::http::StatusCode,
        stream: SseStreamPin,
    },
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

    // 3. 归一化第一遍(按协议:claude 直通全量 / 官方透传跳过 / 其他转换前精简)
    if normalize_enabled {
        match route.protocol {
            Protocol::AnthropicOfficial => {}
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

    let estimated_input_tokens = if !is_stream
        || matches!(
            route.protocol,
            Protocol::Claude | Protocol::AnthropicOfficial
        ) {
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
        Protocol::AnthropicOfficial => {
            tracing::error!("AnthropicOfficial 不应进入 prepare_message_request");
            return Err(AppError::new(anyhow::anyhow!(
                "内部错误: 官方透传请求误入通用转换路径"
            )));
        }
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
            if is_kimi_thinking_replay_model(&route.upstream_model)
                && let Some(sess) = cc_session.as_deref()
            {
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
            original_claude_request = Some(Arc::new(tools_only_snapshot(&body_json)));
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
            original_claude_request = Some(Arc::new(tools_only_snapshot(&body_json)));
            if ccextra_core::antigravity_uses_reasoning_replay(&route.upstream_model) {
                // 对齐 CPA 85225381：账本按凭证隔离（优先 refresh_token 摘要，无则回退 key，原值不入 key / 日志）
                let credential_scope = route
                    .provider_config(providers)
                    .map(ag_replay_credential_scope_for_provider)
                    .unwrap_or_default();
                ag_replay_key = cc_session.as_deref().map(|sess| {
                    crate::sse::antigravity_replay_cache::ag_replay_session_key(
                        &route.upstream_model,
                        &credential_scope,
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
                ag_replay_snapshot = apply_antigravity_reasoning_replay(
                    state,
                    &route,
                    cc_session.as_deref(),
                    ag_replay_key.as_deref(),
                    original_claude_request.as_deref(),
                    tool_names.as_deref(),
                    &mut body_json,
                )?;
            }
        }
    }

    if matches!(
        route.protocol,
        Protocol::OpenAiChat | Protocol::OpenAiResponses
    ) && let Some(key) = inbound_prompt_cache_key
    {
        body_json["prompt_cache_key"] = Value::String(key);
    }

    // 5. payload 参数覆盖
    apply_payload_overrides(&mut body_json, &model, route.protocol, payload_rules);

    let outbound_model =
        resolve_outbound_model(&mut body_json, &route.upstream_model, route.protocol);

    strip_upstream_unsupported_fields(&mut body_json, route.protocol, &outbound_model);

    // 7. 上游连接参数提取并释放读锁
    let (
        upstream_base_urls,
        mut upstream_key,
        mut pool_refresh_token,
        upstream_proxy,
        provider_prompt_cache_key,
        oauth_target,
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
        let oauth_target = if matches!(route.protocol, Protocol::Antigravity) {
            meta.and_then(|m| m.get("credential_path"))
                .cloned()
                .map(|path| OAuthRefreshTarget::Antigravity { path })
        } else {
            match meta
                .and_then(|m| m.get("provider_type"))
                .map(|s| s.as_str())
            {
                Some("xai") => meta
                    .and_then(|m| m.get("credential_path"))
                    .cloned()
                    .map(|path| OAuthRefreshTarget::Xai { path }),
                Some("codex") => meta
                    .and_then(|m| m.get("credential_path"))
                    .cloned()
                    .map(|path| OAuthRefreshTarget::Codex { path }),
                _ => None,
            }
        };
        (
            provider.base_urls().to_vec(),
            provider.key.clone(),
            pool_refresh_token,
            provider.proxy_url.clone(),
            provider.prompt_cache_key,
            oauth_target,
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
    let (oauth_refresh, codex_account_id) = ensure_oauth_credentials_fresh(
        oauth_target,
        upstream_proxy.as_deref(),
        &mut upstream_key,
        &mut pool_refresh_token,
        &mut body_json,
    )
    .await?;

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
    dump_upstream_request(
        config_snapshot,
        headers,
        cc_session.as_deref(),
        route.protocol,
        &body_json,
    )
    .await;

    let inbound_user_agent = if matches!(route.protocol, Protocol::Claude) {
        claude_inbound_user_agent(headers)
    } else {
        None
    };
    let extra_headers = if matches!(route.protocol, Protocol::Claude) {
        // 原生 Claude 直通:最小透传(保留 UA/Stainless);转换路径不走这里
        claude_passthrough_headers(headers)
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

    // 按协议互斥构造响应侧 replay 作用域(Responses / Antigravity / Kimi)
    let replay = match route.protocol {
        Protocol::OpenAiResponses => {
            session_id.map_or(ReplayScope::None, |s| ReplayScope::Responses {
                cache: state.replay_cache.clone(),
                key: format!("{}:{}", route.upstream_model, s),
                fingerprint: request_fingerprint.clone(),
            })
        }
        Protocol::Antigravity
            if ccextra_core::antigravity_uses_reasoning_replay(&route.upstream_model) =>
        {
            ag_replay_key.map_or(ReplayScope::None, |key| ReplayScope::Antigravity {
                cache: state.ag_replay_cache.clone(),
                key,
                snapshot: ag_replay_snapshot,
            })
        }
        Protocol::Claude if is_kimi_thinking_replay_model(&route.upstream_model) => {
            cc_session.as_deref().map_or(ReplayScope::None, |s| {
                let family = kimi_thinking_replay_model_family(&route.upstream_model);
                ReplayScope::Kimi {
                    cache: state.kimi_thinking_replay_cache.clone(),
                    key: format!("{family}:{s}"),
                    applied: kimi_replay_applied,
                }
            })
        }
        _ => ReplayScope::None,
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
        replay,
        signature_model,
        inbound_model: Arc::from(model.as_str()),
        token_scope: session_id
            .map(|session| (Arc::clone(&state.last_input_tokens), session.to_string())),
        drift_usage_scope: drift_parent_for_usage.map(|parent| (state.drift.clone(), parent)),
        unfit_generation,
    })
}

/// OAuth 订阅:运行时 token 校验与自动刷新;失败硬失败,不得继续打旧 token。
/// 返回 (401 重试用的刷新目标, Codex 订阅账号 id)。
async fn ensure_oauth_credentials_fresh(
    target: Option<OAuthRefreshTarget>,
    upstream_proxy: Option<&str>,
    upstream_key: &mut String,
    pool_refresh_token: &mut Option<String>,
    body_json: &mut Value,
) -> Result<(Option<OAuthRefreshTarget>, Option<String>), AppError> {
    let Some(target) = target else {
        return Ok((None, None));
    };
    let mut codex_account_id: Option<String> = None;

    match &target {
        OAuthRefreshTarget::Antigravity { path } => {
            let fresh_cred = crate::antigravity::ensure_credential_fresh(
                std::path::Path::new(path),
                upstream_proxy,
                None,
            )
            .await
            .map_err(|e| {
                AppError::unauthorized(format!("Antigravity 凭证刷新失败,拒绝使用旧 token: {e}"))
            })?;
            *upstream_key = fresh_cred.access_token;
            if !fresh_cred.refresh_token.trim().is_empty() {
                *pool_refresh_token = Some(fresh_cred.refresh_token);
            }
            // 对齐 CPA projectIDForRequest:缺 project 硬失败;发现结果写回出站 body
            let pid = fresh_cred.project_id.trim();
            if pid.is_empty() {
                return Err(AppError::bad_request("antigravity auth missing project_id"));
            }
            body_json["project"] = serde_json::Value::String(pid.to_string());
        }
        OAuthRefreshTarget::Xai { path } => {
            let fresh_cred = crate::xai::ensure_credential_fresh(
                std::path::Path::new(path),
                upstream_proxy,
                None,
            )
            .await
            .map_err(|e| {
                AppError::unauthorized(format!("xAI 凭证刷新失败,拒绝使用旧 token: {e}"))
            })?;
            *upstream_key = fresh_cred.access_token;
        }
        OAuthRefreshTarget::Codex { path } => {
            let fresh_cred = crate::codex::ensure_credential_fresh(
                std::path::Path::new(path),
                upstream_proxy,
                None,
            )
            .await
            .map_err(|e| {
                AppError::unauthorized(format!("Codex 凭证刷新失败,拒绝使用旧 token: {e}"))
            })?;
            *upstream_key = fresh_cred.access_token;
            if !fresh_cred.account_id.is_empty() {
                codex_account_id = Some(fresh_cred.account_id);
            }
        }
    }

    Ok((Some(target), codex_account_id))
}

/// payload 覆盖之后按目标上游剥离不支持的字段(GPT Codex OAuth / Grok Responses /
/// OpenAI Chat 守卫 / StripPromptCacheRetention)。
fn strip_upstream_unsupported_fields(
    body_json: &mut Value,
    protocol: Protocol,
    outbound_model: &str,
) {
    if matches!(protocol, Protocol::OpenAiResponses) && is_gpt_model(outbound_model) {
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
        if sanitize_gpt_reasoning_items(body_json) {
            tracing::debug!("已清理无效 GPT reasoning encrypted_content");
        }
    }

    // Grok Responses:对齐 CPA xai_executor 明确 DeleteBytes 的字段;
    // 保留 temperature/top_p/max_output_tokens(CPA preserveXAIResponsesOutputControls /
    // 原生 ModelEntry 可带采样)。须在 payload_overrides 之后。
    if matches!(protocol, Protocol::OpenAiResponses)
        && is_grok_model(outbound_model)
        && let Some(obj) = body_json.as_object_mut()
    {
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

    // OpenAI Chat:payload 后再守卫(kimi 温度 / 非 qwen|glm 剥 tool_stream / MCT 键对齐)
    if matches!(protocol, Protocol::OpenAiChat) {
        sanitize_openai_chat_after_payload(body_json, outbound_model);
    }

    // 6. 对齐 StripPromptCacheRetention
    if !matches!(protocol, Protocol::Claude) {
        body_json
            .as_object_mut()
            .map(|m| m.remove("prompt_cache_retention"));
    }
}

/// 诊断落盘:`logging.request_body` 开启时把脱敏入站头与最终出站 body 写入日志目录。
async fn dump_upstream_request(
    config_snapshot: &crate::http::ConfigSnapshot,
    headers: &HeaderMap,
    cc_session: Option<&str>,
    protocol: Protocol,
    body_json: &Value,
) {
    if !config_snapshot.runtime.logging.request_body {
        return;
    }
    let sess = cc_session
        .map(|s| s.chars().take(8).collect::<String>())
        .unwrap_or_else(|| "nosess".into());
    let ts = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64);
    let proto = format!("{protocol:?}").to_lowercase();
    let request_seq = UPSTREAM_LOG_SEQUENCE.fetch_add(1, Ordering::Relaxed);
    let log_stem = upstream_log_stem(&sess, ts, request_seq, &proto);
    let log_dir = config_snapshot
        .runtime
        .logging
        .log_dir
        .clone()
        .unwrap_or_else(|| std::path::PathBuf::from("logs"));
    let dump_obj = json!({
        "inbound_headers": inbound_headers_json(headers),
        "upstream_body": body_json,
    });
    let path = log_dir.join(format!("upstream_request_{log_stem}.json"));
    let log_path = path.clone();
    let dumped = tokio::task::spawn_blocking(move || {
        let _ = std::fs::create_dir_all(&log_dir);
        serde_json::to_vec_pretty(&dump_obj)
            .ok()
            .and_then(|bytes| std::fs::write(&path, bytes).ok())
    })
    .await
    .ok()
    .flatten();
    if dumped.is_none() {
        tracing::warn!(path = %log_path.display(), "上游请求诊断信息落盘失败");
    }
}

/// Antigravity 账本凭证隔离摘要：优先取 `metadata.refresh_token`（跨 access 刷新稳定），
/// 静态 key 配置无 refresh_token 时回退 `provider.key`；两者均空返回空串。
fn ag_replay_credential_scope_for_provider(
    provider: &ccextra_core::route::ProviderConfig,
) -> String {
    provider
        .metadata
        .as_ref()
        .and_then(|m| m.get("refresh_token"))
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
        .or_else(|| Some(provider.key.trim()).filter(|s| !s.is_empty()))
        .map(crate::sse::antigravity_replay_cache::ag_replay_credential_scope)
        .unwrap_or_default()
}

/// Antigravity reasoning replay:有会话账本时注入并返回读账本快照;无会话仍做历史稳定 id 降级。
/// 配对无效本地 400(对齐 CPA statusErr 400)。
fn apply_antigravity_reasoning_replay(
    state: &AppState,
    route: &ccextra_core::route::RouteDecision,
    cc_session: Option<&str>,
    ag_replay_key: Option<&str>,
    original_claude_request: Option<&Value>,
    tool_names: Option<&HashMap<String, String>>,
    body_json: &mut Value,
) -> Result<crate::sse::antigravity_replay_cache::AgReplaySnapshot, AppError> {
    // 工具表（对齐 CPA toolSchemas 来源：原始 Claude 请求）+ 上游短名映射
    let replay_tools = ccextra_core::convert::AgReplayToolContext {
        original_request: original_claude_request,
        short_to_original: tool_names,
    };
    if let (Some(sess), Some(key)) = (cc_session, ag_replay_key) {
        use crate::sse::antigravity_replay_cache::AgReplayApplyOutcome;
        let (outcome, snapshot) = state.ag_replay_cache.apply_to_antigravity_body_with_tools(
            key,
            body_json,
            replay_tools,
        );
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
        Ok(snapshot)
    } else {
        // 无会话：无账本，但历史稳定 id 仍须降级（对齐 CPA 兜底不依赖 scope）；
        // 配对无效同样本地 400（见上，坏历史每轮重现）
        if let Some(message) =
            crate::sse::antigravity_replay_cache::prepare_antigravity_body_without_ledger(
                body_json,
                replay_tools,
            )
        {
            return Err(AppError::bad_request(message));
        }
        Ok(Default::default())
    }
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
                .request_encoded(prepared.upstream_request(base_url, user_agents))
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
        Out::Ok(resp) | Out::Fail(resp) => resp,
        // 网络错误已轮转完全部 base_url:直接返给客户端,不本地退避重试
        Out::Net(e) => return Err(e.into()),
    };

    if prepared.is_stream
        && matches!(
            prepared.route.protocol,
            Protocol::OpenAiChat | Protocol::OpenAiResponses
        )
        && upstream.status.is_success()
    {
        for attempt in 0..=1 {
            let status = upstream.status;
            let mut out = build_stream_relay(prepared, upstream);
            // 首帧只认业务帧:跳过心跳注释帧,防 keepalive 骗过错误检测提前提交 200
            match next_business_frame(&mut out).await {
                Some(Ok(frame)) if !is_initial_sse_error(&frame) => {
                    return Ok(ExecutedUpstream::Preloaded {
                        status,
                        stream: prepend_sse_frame(Ok(frame), out),
                    });
                }
                bad_first => {
                    if attempt == 0 {
                        tracing::warn!(
                            protocol = ?prepared.route.protocol,
                            retry_attempt = attempt + 1,
                            "首帧转换失败，重试上游请求"
                        );
                        upstream = prepared.send_retry(upstream_client, user_agents).await?;
                        if !upstream.status.is_success() {
                            return Ok(ExecutedUpstream::Raw(Box::new(upstream)));
                        }
                        continue;
                    }
                    // 二次仍失败:不提交 200,转 502 HTTP 错误交客户端整回合重试
                    let msg = match &bad_first {
                        Some(Ok(frame)) => initial_sse_error_message(frame),
                        Some(Err(e)) => e.to_string(),
                        None => "上游流在首帧前结束".to_string(),
                    };
                    return Err(AppError::with_status(
                        axum::http::StatusCode::BAD_GATEWAY,
                        format!("上游首帧失败: {msg}"),
                    ));
                }
            }
        }
    }

    Ok(ExecutedUpstream::Raw(Box::new(upstream)))
}

struct UpstreamErrorState {
    status: axum::http::StatusCode,
    bytes: Bytes,
    truncated: bool,
    retry_after: Option<std::time::Duration>,
}

impl UpstreamErrorState {
    async fn from_response(resp: UpstreamResponse) -> Result<Self, AppError> {
        let status = resp.status;
        // Retry-After 透传给客户端(对齐 codex 把 server retry advice 透出给上层):
        // 429 快速失败后由客户端按上游声明退避,代理不代等
        let retry_after = parse_retry_after(resp.body.headers());
        let (bytes, truncated) =
            crate::limits::read_error_body_or_anthropic(resp.body, status).await?;
        Ok(Self {
            status,
            bytes,
            truncated,
            retry_after,
        })
    }

    /// 发起一次恢复重试:2xx 直接返回成功响应,非 2xx 原地刷新错误快照供后续门禁检查。
    async fn retry(
        &mut self,
        prepared: &PreparedMessageRequest,
        upstream_client: &crate::upstream::UpstreamClient,
        user_agents: &crate::http::UserAgentSet,
    ) -> Result<Option<UpstreamResponse>, AppError> {
        let retry = prepared.send_retry(upstream_client, user_agents).await?;
        if retry.status.is_success() {
            return Ok(Some(retry));
        }
        *self = Self::from_response(retry).await?;
        Ok(None)
    }

    fn is_bad_request_body(&self) -> bool {
        !self.truncated && (self.status.as_u16() == 400 || self.status.as_u16() == 422)
    }
}

fn build_sse_response(
    status: axum::http::StatusCode,
    stream: SseStreamPin,
) -> Result<Response, AppError> {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from_stream(stream))
        .map_err(|e| AppError::new(anyhow::anyhow!("构造流式响应失败: {e}")))
}

/// 上游非 2xx 恢复链路:401 刷新重试 / unfit 自适应降级 / 账本清理 / encrypted reasoning 剥离。
/// 返回 `Ok(Ok(resp))` 表示重试已拿到 2xx;`Ok(Err(response))` 表示已构好最终 Anthropic 错误响应。
async fn recover_upstream_error(
    prepared: &mut PreparedMessageRequest,
    failed: UpstreamResponse,
    upstream_client: &crate::upstream::UpstreamClient,
    user_agents: &crate::http::UserAgentSet,
) -> Result<Result<UpstreamResponse, Response>, AppError> {
    let mut err = UpstreamErrorState::from_response(failed).await?;

    // 上游 401:OAuth 凭证强制刷新后重试一次;刷新失败硬失败,不得继续打旧 token
    if err.status.as_u16() == 401 && prepared.refresh_rejected_token().await? {
        tracing::info!(
            protocol = ?prepared.route.protocol,
            provider = %prepared.route.provider,
            "上游 401,凭证已刷新,重试一次"
        );
        prepared.replay.invalidate_for_retry();
        if let Some(ok) = err.retry(prepared, upstream_client, user_agents).await? {
            return Ok(Ok(ok));
        }
    }

    // Unfit 探测与自适应降级(对齐 magpie gateway.go:3440-3490)
    if err.is_bad_request_body() {
        let err_text = String::from_utf8_lossy(&err.bytes);
        if let Some(kind) =
            crate::http::unfit::detect_unfit_from_error(err.status.as_u16(), &err_text)
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
                prepared.replay.invalidate_for_retry();
                if let Some(ok) = err.retry(prepared, upstream_client, user_agents).await? {
                    return Ok(Ok(ok));
                }
            }
        }
    }

    if !err.truncated
        && should_clear_ag_replay_on_error(prepared.route.protocol, err.status.as_u16(), &err.bytes)
    {
        tracing::warn!(
            protocol = ?prepared.route.protocol,
            "antigravity thoughtSignature 无效,清空 replay 账本"
        );
        if let ReplayScope::Antigravity {
            cache,
            key,
            snapshot,
        } = &prepared.replay
        {
            // 对齐 CPA DeleteAntigravityReasoningReplayItemsIfUnchanged(scope.cacheSnapshot)
            cache.invalidate_if_unchanged(key, snapshot);
        }
    }

    // Kimi：注入后仍 400/422 → 清账本（对齐 CPA shouldClearKimiThinkingReplayAfterError）
    if err.is_bad_request_body()
        && let ReplayScope::Kimi {
            cache,
            key,
            applied: true,
        } = &prepared.replay
    {
        tracing::warn!(
            protocol = ?prepared.route.protocol,
            "kimi thinking replay 注入后上游 400/422,清空账本"
        );
        cache.invalidate(key);
    }

    if err.is_bad_request_body()
        && matches!(prepared.route.protocol, Protocol::OpenAiResponses)
        && is_thinking_signature_invalid(&err.bytes)
        && trim_encrypted_reasoning_items(&mut prepared.body_json)
    {
        tracing::warn!(
            protocol = ?prepared.route.protocol,
            "invalid_encrypted_content,剥离 reasoning 后重试一次"
        );
        prepared.reencode_body().await?;
        if let ReplayScope::Responses { cache, key, .. } = &prepared.replay {
            cache.invalidate(key);
        }
        if let Some(ok) = err.retry(prepared, upstream_client, user_agents).await? {
            return Ok(Ok(ok));
        }
    }

    // AG 封号:只打告警,不改凭证状态(provider 名含邮箱,不入日志)
    if !err.truncated
        && err.status.as_u16() == 403
        && matches!(prepared.route.protocol, Protocol::Antigravity)
        && let Some(appeal_url) = antigravity_tos_violation_appeal_url(&err.bytes)
    {
        tracing::warn!(
            appeal_url = %appeal_url,
            "Antigravity 上游 403 TOS_VIOLATION,账号疑似被封,需申诉后恢复"
        );
    }

    Ok(Err(build_upstream_error_response(
        prepared.route.protocol,
        err.status,
        &err.bytes,
        err.truncated,
        err.retry_after,
    )?))
}

pub(crate) async fn deliver_response(
    state: &AppState,
    prepared: &mut PreparedMessageRequest,
    executed: ExecutedUpstream,
    upstream_client: &crate::upstream::UpstreamClient,
    user_agents: &crate::http::UserAgentSet,
) -> Result<Response, AppError> {
    let upstream = match executed {
        ExecutedUpstream::Preloaded { status, stream } => {
            return build_sse_response(status, stream);
        }
        ExecutedUpstream::Raw(resp) if resp.status.is_success() => *resp,
        ExecutedUpstream::Raw(failed) => {
            match recover_upstream_error(prepared, *failed, upstream_client, user_agents).await? {
                Ok(recovered) => recovered,
                Err(error_response) => return Ok(error_response),
            }
        }
    };
    let status = upstream.status;

    // 响应转换
    if prepared.is_stream {
        let out = build_stream_relay(prepared, upstream);
        build_sse_response(status, out)
    } else {
        let body_bytes = crate::limits::read_success_body_or_anthropic(upstream.body).await?;
        // 成功 body 只解析一次,供 replay / AG / Kimi / drift / 协议转换复用
        let parsed: Option<Value> = serde_json::from_slice(&body_bytes).ok();
        if let Some(ref v) = parsed {
            observe_non_stream_success(prepared, v);
        }
        let converted = match parsed {
            Some(v) => {
                record_non_stream_input_tokens(state, prepared, &v);
                convert_non_stream_success(prepared, &v)
            }
            None => None,
        };
        // 对齐 CPA 7f2f17d7:转换层产出的 Claude api_error(如 MALFORMED)抬 502 供客户端重试
        if let Some(out) = converted.as_ref()
            && out.get("type").and_then(|t| t.as_str()) == Some("error")
        {
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
        // 非 Claude 协议必须转换:converted 为 None 表示转换层无输出(非法 JSON、
        // 缺 choices/response 等形状问题),不得原样透传上游字节(客户端收到非 Anthropic
        // 形状且状态码 200)。Gemini/AG 的 {"error":…} 由转换层产出空消息,不经此路径。
        let payload = if !matches!(prepared.route.protocol, Protocol::Claude) {
            match converted.and_then(|out| serde_json::to_vec(&out).ok()) {
                Some(bytes) => Bytes::from(bytes),
                None => {
                    let preview = String::from_utf8_lossy(&body_bytes[..body_bytes.len().min(200)]);
                    return Err(AppError::with_status(
                        axum::http::StatusCode::BAD_GATEWAY,
                        format!(
                            "上游返回 200 但响应无法转换为 Anthropic 形状(前 200 字节): {preview}"
                        ),
                    ));
                }
            }
        } else {
            converted
                .and_then(|out| serde_json::to_vec(&out).ok())
                .map(Bytes::from)
                .unwrap_or(body_bytes)
        };
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

    fn upstream_request<'a>(
        &'a self,
        base_url: &'a str,
        user_agents: &'a crate::http::UserAgentSet,
    ) -> crate::upstream::UpstreamRequest<'a> {
        crate::upstream::UpstreamRequest {
            base_url,
            api_key: &self.upstream_key,
            protocol: self.route.protocol,
            provider_proxy: self.upstream_proxy.as_deref(),
            body_bytes: self.encoded_body.clone(),
            body: &self.body_json,
            is_stream: self.is_stream,
            session_id: self.session_id.as_deref(),
            thread_id: self.thread_id.as_deref(),
            extra_headers: &self.extra_headers,
            user_agents,
            inbound_user_agent: self.inbound_user_agent.as_deref(),
            pool_refresh_token: self.pool_refresh_token.as_deref(),
            query: self.inbound_query.as_deref(),
        }
    }

    async fn send_retry(
        &self,
        client: &crate::upstream::UpstreamClient,
        user_agents: &crate::http::UserAgentSet,
    ) -> anyhow::Result<UpstreamResponse> {
        let base_url = self
            .active_base_url
            .as_deref()
            .unwrap_or(&self.upstream_base_urls[0]);
        client
            .request_encoded(self.upstream_request(base_url, user_agents))
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
    let started = std::time::Instant::now();
    let mut fields: Option<crate::http::access_log::AccessFields> = None;
    let result = async {
        // 同读锁捕获快照与 unfit 世代,与 /reload 写锁内 clear 成对:
        // prepare 中途 await 期间 reload 增代后,旧快照请求的 mark 必被丢弃。
        let (effective_snapshot, unfit_generation) = state.snapshot_and_unfit_generation().await;
        check_secret(&headers, &effective_snapshot.runtime.secret).await?;
        let official_enabled = effective_snapshot
            .providers
            .iter()
            .any(|p| p.protocol == Protocol::AnthropicOfficial);
        if official_enabled {
            crate::http::official::reject_compressed_inbound(&headers)?;
        }
        let bytes = crate::limits::read_inbound_body(body).await?;

        if official_enabled
            && let Some(probe) = crate::http::official::probe_inbound_body(&bytes)
            && let Ok(route) = resolve_route(&probe.model, &effective_snapshot.providers)
            && route.protocol == Protocol::AnthropicOfficial
        {
            fields = Some(crate::http::access_log::AccessFields::for_official(
                probe.model.clone(),
                route.provider.clone(),
                route.upstream_model.clone(),
                probe.stream,
            ));
            return crate::http::official::relay_messages(
                &effective_snapshot,
                &route,
                &headers,
                uri.query(),
                bytes,
                probe.stream,
            )
            .await;
        }

        let mut prepared = prepare_message_request(
            &state,
            &headers,
            &bytes,
            &effective_snapshot,
            uri.query(),
            unfit_generation,
        )
        .await?;
        fields = Some(crate::http::access_log::AccessFields::from_prepared(
            &prepared,
        ));

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
    .await;
    match result {
        Ok(resp) => Ok(crate::http::access_log::finish(resp, fields, started)),
        Err(e) => {
            crate::http::access_log::log_error(fields.as_ref(), e.status.as_u16(), started);
            Err(e)
        }
    }
}

/// 非流成功旁路:Responses/AG/Kimi replay 落账与 drift cache usage 观测(只读响应)。
fn observe_non_stream_success(prepared: &PreparedMessageRequest, v: &Value) {
    match &prepared.replay {
        ReplayScope::Responses {
            cache,
            key,
            fingerprint,
        } if v.get("object").and_then(|o| o.as_str()) == Some("response") => {
            let wrapped = json!({"response": v});
            cache.store_from_completed(key, &wrapped, fingerprint);
        }
        ReplayScope::Antigravity {
            cache,
            key,
            snapshot,
        } => {
            if let Some(request) = prepared.body_json.get("request") {
                cache.store_from_response(key, snapshot, request, v);
            }
        }
        ReplayScope::Kimi { cache, key, .. } => {
            cache.store_from_message_response(key, v);
        }
        ReplayScope::None | ReplayScope::Responses { .. } => {}
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
                    .or_else(|| v.pointer("/usage/prompt_tokens_details/cache_creation_tokens"))
                    .and_then(|t| t.as_i64())
                    .unwrap_or(0),
            ),
            Protocol::OpenAiResponses => (
                v.pointer("/usage/input_tokens_details/cached_tokens")
                    .and_then(|t| t.as_i64())
                    .unwrap_or(0),
                v.pointer("/usage/input_tokens_details/cache_write_tokens")
                    .or_else(|| v.pointer("/usage/input_tokens_details/cache_creation_tokens"))
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

/// 非流成功:按协议提取 input tokens 写会话缓存(供后续流式 message_start 占位)。
fn record_non_stream_input_tokens(state: &AppState, prepared: &PreparedMessageRequest, v: &Value) {
    if let Some(sid) = prepared.session_id.as_deref() {
        let input_tokens = match prepared.route.protocol {
            Protocol::OpenAiChat => v.pointer("/usage/prompt_tokens").and_then(|t| t.as_i64()),
            Protocol::OpenAiResponses => v.pointer("/usage/input_tokens").and_then(|t| t.as_i64()),
            Protocol::Gemini | Protocol::Antigravity => {
                let inner = if prepared.route.protocol == Protocol::Antigravity {
                    v.get("response").unwrap_or(v)
                } else {
                    v
                };
                inner
                    .pointer("/usageMetadata/promptTokenCount")
                    .and_then(|t| t.as_i64())
            }
            Protocol::Claude | Protocol::AnthropicOfficial => None,
        };
        if let Some(tokens) = input_tokens
            && tokens > 0
        {
            let _ = state
                .last_input_tokens
                .lock()
                .ok()
                .map(|mut cache| cache.insert(sid.to_string(), tokens as usize));
        }
    }
}

/// 非流成功:按协议把上游 JSON 转回 Anthropic message;Claude 直通返回 None。
fn convert_non_stream_success(prepared: &PreparedMessageRequest, v: &Value) -> Option<Value> {
    match prepared.route.protocol {
        Protocol::Claude => None,
        Protocol::AnthropicOfficial => {
            tracing::error!("AnthropicOfficial 不应进入 convert_non_stream_success");
            None
        }
        Protocol::OpenAiChat => crate::sse::non_stream::openai_chat_to_anthropic(v),
        Protocol::OpenAiResponses => {
            crate::sse::non_stream::responses_to_anthropic(v, prepared.tool_names.as_deref())
        }
        Protocol::Gemini => {
            use ccextra_core::convert::convert_gemini_response;
            Some(convert_gemini_response(
                v,
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

/// 上游最终失败:转 Anthropic error 形状,透传/补齐 Retry-After。
fn build_upstream_error_response(
    protocol: Protocol,
    final_status: axum::http::StatusCode,
    final_bytes: &[u8],
    final_truncated: bool,
    mut retry_after: Option<std::time::Duration>,
) -> Result<Response, AppError> {
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
        to_anthropic_error(final_bytes, final_status)
    };
    // Antigravity 429 / 503(容量不足)常不带 Retry-After 头,等待时长在 body 的 RetryInfo 等字段里;
    // 上游头优先(对齐 CPA antigravity executor ParseRetryDelay / ae3d80f7)
    if retry_after.is_none()
        && matches!(final_status.as_u16(), 429 | 503)
        && !final_truncated
        && matches!(protocol, Protocol::Antigravity)
    {
        retry_after = parse_google_retry_delay(final_bytes).map(retry_after_header_secs);
    }
    let mut builder = Response::builder()
        .status(final_status)
        .header(header::CONTENT_TYPE, "application/json");
    if let Some(ra) = retry_after {
        builder = builder.header("retry-after", ra.as_secs());
    }
    builder
        .body(Body::from(body))
        .map_err(|e| AppError::new(anyhow::anyhow!("构造错误响应失败: {e}")))
}

/// 流式成功:按协议挂 replay 旁路并接 SSE 状态机。
fn build_stream_relay(
    prepared: &PreparedMessageRequest,
    upstream: UpstreamResponse,
) -> SseStreamPin {
    relay_with_replay_tap(
        upstream.body.bytes_stream(),
        RelayParams {
            protocol: prepared.route.protocol,
            estimated_input_tokens: prepared.estimated_input_tokens,
            tool_names: prepared.tool_names.clone(),
            original_claude_request: prepared.original_claude_request.clone(),
            replay_tap: prepared.replay.stream_tap(&prepared.body_json),
            signature_model: prepared.signature_model.clone(),
            inbound_model: Some(Arc::clone(&prepared.inbound_model)),
            token_scope: prepared.token_scope.clone(),
            drift_usage_scope: prepared.drift_usage_scope.clone(),
        },
    )
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

    /// 快照仅保留 tools:响应侧回剥只读 tools,其余字段不进快照
    #[test]
    fn tools_only_snapshot_keeps_tools_and_drops_rest() {
        let body = serde_json::json!({
            "model": "m",
            "messages": [{"role": "user", "content": "long history"}],
            "tools": [{"name": "t", "input_schema": {"type": "object"}}],
        });
        let snapshot = tools_only_snapshot(&body);
        assert_eq!(snapshot.get("tools"), body.get("tools"));
        assert!(snapshot.get("messages").is_none());
        assert!(snapshot.get("model").is_none());
    }

    #[test]
    fn tools_only_snapshot_without_tools_is_empty_object() {
        let body = serde_json::json!({"messages": []});
        assert_eq!(tools_only_snapshot(&body), serde_json::json!({}));
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
            replay: ReplayScope::None,
            signature_model: None,
            inbound_model: Arc::from("gpt-test"),
            token_scope: None,
            drift_usage_scope: None,
            unfit_generation: 0,
        }
    }

    /// 访问日志只含路由元数据:上游 key、header、body 内容均不得出现。
    #[test]
    fn access_line_contains_route_but_no_secrets() {
        let mut prepared = stub_prepared_without_account_id();
        prepared.upstream_key = "sk-fixture-secret-key".into();
        prepared.body_json =
            json!({"model": "gpt-test", "input": [{"content": "secret-body-text"}]});
        prepared.extra_headers.insert(
            "authorization",
            "Bearer sk-fixture-header-token".parse().unwrap(),
        );
        prepared.is_stream = true;
        let fields = crate::http::access_log::AccessFields::from_prepared(&prepared);
        let line = crate::http::access_log::access_line(Some(&fields), 200, 12, 345);
        assert_eq!(
            line,
            "access model=gpt-test provider=codex protocol=OpenAiResponses \
             upstream_model=gpt-test stream=true status=200 first_byte_ms=12 total_ms=345"
        );
        for secret in ["sk-fixture", "secret-body", "Bearer", "authorization"] {
            assert!(!line.contains(secret), "泄露 {secret}: {line}");
        }
        assert_eq!(
            crate::http::access_log::access_line(None, 401, 3, 3),
            "access status=401 first_byte_ms=3 total_ms=3"
        );
    }

    /// AG 封号识别对齐 CPA parseAntigravityTOSViolation;申诉链接按 redactTOSAppealURL 脱敏。
    #[test]
    fn ag_tos_violation_detect_and_redact() {
        let body = br#"{"error":{"code":403,"status":"PERMISSION_DENIED","message":"banned","details":[
            {"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"TOS_VIOLATION",
             "metadata":{"appeal_url":" https://example.com/appeal?sig=abc#frag ","appeal_url_link_text":"Appeal"}}]}}"#;
        assert_eq!(
            antigravity_tos_violation_appeal_url(body).as_deref(),
            Some("https://example.com/appeal")
        );
        // 大小写 / 空白归一;无 metadata → 空串
        let no_url = br#"{"error":{"code":403,"status":" permission_denied ","details":[
            {"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"tos_violation"}]}}"#;
        assert_eq!(
            antigravity_tos_violation_appeal_url(no_url).as_deref(),
            Some("")
        );
    }

    #[test]
    fn ag_tos_violation_rejects_other_errors() {
        for body in [
            // 普通 403
            br#"{"error":{"code":403,"status":"PERMISSION_DENIED","details":[
                {"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"ACCESS_TOKEN_SCOPE_INSUFFICIENT"}]}}"#
                .as_slice(),
            // 429 配额(reason 偶然同名也不算)
            br#"{"error":{"code":429,"status":"RESOURCE_EXHAUSTED","details":[
                {"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"TOS_VIOLATION"}]}}"#
                .as_slice(),
            // status 不对
            br#"{"error":{"code":403,"status":"FORBIDDEN","details":[
                {"@type":"type.googleapis.com/google.rpc.ErrorInfo","reason":"TOS_VIOLATION"}]}}"#
                .as_slice(),
            // @type 不是 ErrorInfo
            br#"{"error":{"code":403,"status":"PERMISSION_DENIED","details":[
                {"@type":"type.googleapis.com/google.rpc.Help","reason":"TOS_VIOLATION"}]}}"#
                .as_slice(),
            // 无 details / 非 JSON
            br#"{"error":{"code":403,"status":"PERMISSION_DENIED"}}"#.as_slice(),
            b"forbidden".as_slice(),
        ] {
            assert_eq!(
                antigravity_tos_violation_appeal_url(body),
                None,
                "{}",
                String::from_utf8_lossy(body)
            );
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
        assert!(
            crate::http::unfit::detect_unfit_from_error(
                retry.as_u16(),
                r#"{"error":{"message":"unsupported response_format"}}"#
            )
            .is_some()
        );
        assert!(
            crate::http::unfit::detect_unfit_from_error(
                axum::http::StatusCode::UNAUTHORIZED.as_u16(),
                r#"{"error":{"message":"unsupported response_format"}}"#
            )
            .is_none()
        );
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

    #[test]
    fn ag_replay_credential_scope_prefers_refresh_token_and_falls_back_to_key() {
        let with_refresh = ccextra_core::route::ProviderConfig::new(
            "ag-oauth".into(),
            Protocol::Antigravity,
            vec!["https://daily-cloudcode-pa.googleapis.com".into()],
            "access-key-a".into(),
            None,
            false,
            vec![],
        )
        .with_metadata(HashMap::from([(
            "refresh_token".into(),
            "refresh-token-a".into(),
        )]));
        assert_eq!(
            ag_replay_credential_scope_for_provider(&with_refresh),
            crate::sse::antigravity_replay_cache::ag_replay_credential_scope("refresh-token-a")
        );

        let static_key = ccextra_core::route::ProviderConfig::new(
            "ag-static".into(),
            Protocol::Antigravity,
            vec!["https://daily-cloudcode-pa.googleapis.com".into()],
            "static-key-b".into(),
            None,
            false,
            vec![],
        );
        assert_eq!(
            ag_replay_credential_scope_for_provider(&static_key),
            crate::sse::antigravity_replay_cache::ag_replay_credential_scope("static-key-b")
        );

        let empty_both = ccextra_core::route::ProviderConfig::new(
            "ag-empty".into(),
            Protocol::Antigravity,
            vec!["https://daily-cloudcode-pa.googleapis.com".into()],
            "   ".into(),
            None,
            false,
            vec![],
        )
        .with_metadata(HashMap::from([("refresh_token".into(), " ".into())]));
        assert_eq!(ag_replay_credential_scope_for_provider(&empty_both), "");
    }
}
