use axum::{
    body::{to_bytes, Body},
    extract::State,
    http::{header, HeaderMap},
    response::Response,
};
use bytes::Bytes;
use ccextra_core::cache_stabilization::drift_detector::derive_session_key as drift_derive_session_key;
use ccextra_core::cache_stabilization::drift_detector::{
    compute_structural_hash, is_ancillary_request, observe_drift, ApiKind as DriftApiKind,
    DriftState,
};
use ccextra_core::convert::{
    clamp_passthrough_effort, convert_passthrough, convert_to_antigravity_with,
    convert_to_gemini_with_registry, convert_to_openai_chat_with, convert_to_openai_responses_with,
    is_thinking_signature_invalid, sanitize_gpt_reasoning_items, sanitize_passthrough_prompt,
    trim_encrypted_reasoning_items,
};
use ccextra_core::normalize::{
    normalize_anthropic_full, normalize_anthropic_pretransform, normalize_target_post, TargetShape,
};
use ccextra_core::prompt_cache::inject_prompt_cache_key;
use ccextra_core::route::{resolve_route, Protocol, ProviderConfig};
use ccextra_core::session::{extract_claude_code_session, extract_claude_code_thread};
use futures::StreamExt;
use globset::Glob;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use crate::http::auth::check_secret;
use crate::http::claude_relay::{claude_inbound_user_agent, claude_relay_headers};
use crate::http::error::{to_anthropic_error, AppError};
use crate::http::retry::{compute_retry_delay, parse_retry_after};
use crate::http::{AppState, PayloadRule};
use crate::sse::replay_cache::StreamReplayExtractor;
use crate::sse::SseStreamPin;
use crate::upstream::{is_gpt_model, is_grok_model, UpstreamResponse};

/// 诊断日志请求序号:与毫秒时间戳组合,避免并发请求覆盖同一文件。
static UPSTREAM_LOG_SEQUENCE: AtomicU64 = AtomicU64::new(0);

/// 按 provider 名查找 provider 配置
pub(crate) fn find_provider<'a>(
    providers: &'a [ProviderConfig],
    name: &str,
) -> Option<&'a ProviderConfig> {
    providers.iter().find(|p| p.name == name)
}

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
pub(crate) fn observe_drift_for(
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

pub(crate) struct PreparedMessageRequest {
    pub route: ccextra_core::route::RouteDecision,
    pub body_json: Value,
    pub is_stream: bool,
    pub upstream_base_urls: Vec<String>,
    pub upstream_key: String,
    pub upstream_proxy: Option<String>,
    pub session_id: Option<String>,
    pub thread_id: Option<String>,
    pub extra_headers: HeaderMap,
    pub inbound_user_agent: Option<String>,
    pub estimated_input_tokens: Option<usize>,
    pub tool_names: Option<Arc<HashMap<String, String>>>,
    pub replay_scope: Option<(crate::sse::replay_cache::ReplayCache, String, String)>,
    pub signature_model: Option<Arc<str>>,
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
) -> Result<PreparedMessageRequest, AppError> {
    let normalize_enabled = config_snapshot.runtime.normalize.enabled;
    let normalize_drift_detector = config_snapshot.runtime.normalize.drift_detector;
    let log_request_body = config_snapshot.runtime.logging.request_body;
    let thinking_registry = &config_snapshot.runtime.thinking_registry;

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

    // 3. 归一化第一遍(按协议:claude 直通全量 / openai 转换前精简)
    if normalize_enabled {
        match route.protocol {
            Protocol::Claude => {
                let counts = normalize_anthropic_full(&mut body_json);
                tracing::debug!(?counts, "normalize_anthropic_full");
                observe_drift_for(
                    &state.drift,
                    headers,
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
    let mut request_fingerprint = String::new();

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
        }
        Protocol::OpenAiChat => {
            convert_to_openai_chat_with(&mut body_json, &route.upstream_model, thinking_registry)?;
            if normalize_enabled {
                normalize_target_post(&mut body_json, TargetShape::OpenAiChat);
                observe_drift_for(
                    &state.drift,
                    headers,
                    &body_json,
                    DriftApiKind::OpenAiChat,
                    normalize_drift_detector,
                );
            }
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
            if normalize_enabled {
                normalize_target_post(&mut body_json, TargetShape::OpenAiResponses);
            }
        }
        Protocol::Gemini => {
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
            let project_id = {
                let provider = find_provider(providers, &route.provider);
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
            );
            body_json = antigravity_body;
            if !short_to_original.is_empty() {
                tool_names = Some(Arc::new(short_to_original));
            }

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

    if normalize_enabled && matches!(route.protocol, Protocol::OpenAiResponses) {
        observe_drift_for(
            &state.drift,
            headers,
            &body_json,
            DriftApiKind::OpenAiResponses,
            normalize_drift_detector,
        );
    }

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
        upstream_proxy,
        provider_prompt_cache_key,
        antigravity_refresh,
        xai_refresh,
        codex_refresh,
    ) = {
        let provider = find_provider(providers, &route.provider)
            .ok_or_else(|| AppError::new(anyhow::anyhow!("provider 未找到: {}", route.provider)))?;
        let meta = provider.metadata.as_ref();
        let antigravity_refresh = if matches!(route.protocol, Protocol::Antigravity) {
            match (
                meta.and_then(|m| m.get("auth_dir")).cloned(),
                meta.and_then(|m| m.get("email")).cloned(),
            ) {
                (Some(auth_dir), Some(email)) => Some((auth_dir, email)),
                _ => None,
            }
        } else {
            None
        };
        let xai_refresh = meta
            .filter(|m| m.get("provider_type").map(|s| s.as_str()) == Some("xai"))
            .and_then(|m| {
                let auth_dir = m.get("auth_dir")?.clone();
                let email = m.get("email").cloned().unwrap_or_default();
                let sub = m.get("sub").cloned().unwrap_or_default();
                Some((auth_dir, email, sub))
            });
        let codex_refresh = meta
            .filter(|m| m.get("provider_type").map(|s| s.as_str()) == Some("codex"))
            .and_then(|m| {
                let auth_dir = m.get("auth_dir")?.clone();
                let email = m.get("email").cloned().unwrap_or_default();
                let account_id = m.get("account_id").cloned().unwrap_or_default();
                Some((auth_dir, email, account_id))
            });
        (
            provider.base_urls().to_vec(),
            provider.key.clone(),
            provider.proxy_url.clone(),
            provider.prompt_cache_key,
            antigravity_refresh,
            xai_refresh,
            codex_refresh,
        )
    };

    // Antigravity 运行时 token 校验与自动刷新
    if let Some((auth_dir_str, email)) = antigravity_refresh {
        let auth_dir = std::path::Path::new(&auth_dir_str);
        match crate::antigravity::ensure_credential_fresh(
            auth_dir,
            &email,
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

    // xAI 运行时 token 校验与自动刷新
    if let Some((auth_dir_str, email, sub)) = xai_refresh {
        let auth_dir = std::path::Path::new(&auth_dir_str);
        let id = if !email.is_empty() { &email } else { &sub };
        match crate::xai::ensure_credential_fresh(auth_dir, id, upstream_proxy.as_deref()).await {
            Ok(fresh_cred) => {
                upstream_key = fresh_cred.access_token;
            }
            Err(e) => {
                tracing::warn!(email = %email, sub = %sub, "xAI 凭证运行时刷新失败: {e}");
            }
        }
    }

    // Codex 运行时 token 校验与自动刷新;account_id 供 Chatgpt-Account-Id 头
    let mut codex_account_id: Option<String> = None;
    if let Some((auth_dir_str, email, account_id)) = codex_refresh {
        let auth_dir = std::path::Path::new(&auth_dir_str);
        let id = if !email.is_empty() {
            &email
        } else {
            &account_id
        };
        match crate::codex::ensure_credential_fresh(auth_dir, id, upstream_proxy.as_deref()).await {
            Ok(fresh_cred) => {
                upstream_key = fresh_cred.access_token;
                if !fresh_cred.account_id.is_empty() {
                    codex_account_id = Some(fresh_cred.account_id);
                }
            }
            Err(e) => {
                tracing::warn!(email = %email, "Codex 凭证运行时刷新失败: {e}");
                // 刷新失败仍用落盘 account_id 发请求
                if !account_id.is_empty() {
                    codex_account_id = Some(account_id);
                }
            }
        }
    }

    // prompt_cache_key 注入
    if should_inject_prompt_cache_key(provider_prompt_cache_key, route.protocol, &outbound_model)
        && inject_prompt_cache_key(&mut body_json, cc_session.as_deref())
    {
        tracing::debug!("prompt_cache_key 已注入");
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
        let _ = std::fs::create_dir_all("logs");
        let dump_obj = json!({
            "inbound_headers": inbound_headers_json(headers),
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
        claude_inbound_user_agent(headers)
    } else {
        None
    };
    let extra_headers = if matches!(route.protocol, Protocol::Claude) {
        claude_relay_headers(headers)
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
    } else if is_grok && matches!(route.protocol, Protocol::OpenAiChat) {
        (cc_session.as_deref(), None)
    } else {
        (None, None)
    };

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

    let signature_model: Option<Arc<str>> = matches!(route.protocol, Protocol::Antigravity)
        .then(|| Arc::from(route.upstream_model.as_str()));

    Ok(PreparedMessageRequest {
        route,
        body_json,
        is_stream,
        upstream_base_urls,
        upstream_key,
        upstream_proxy,
        session_id: session_id.map(str::to_string),
        thread_id,
        extra_headers,
        inbound_user_agent: inbound_user_agent.map(str::to_string),
        estimated_input_tokens,
        tool_names,
        replay_scope,
        signature_model,
    })
}

pub(crate) async fn execute_upstream_request(
    prepared: &mut PreparedMessageRequest,
    upstream_client: &crate::upstream::UpstreamClient,
    user_agents: &crate::http::UserAgentSet,
) -> Result<ExecutedUpstream, AppError> {
    let mut upstream: Option<UpstreamResponse> = None;
    let mut last_fail: Option<UpstreamResponse> = None;
    let mut last_err = None;
    let retry_started_at = std::time::Instant::now();
    let mut attempt: u32 = 0;
    loop {
        enum Out {
            Ok(UpstreamResponse),
            Fail(UpstreamResponse),
            Net(anyhow::Error),
        }
        let outcome = 'round: {
            for (idx, base_url) in prepared.upstream_base_urls.iter().enumerate() {
                match upstream_client
                    .request(
                        base_url,
                        &prepared.upstream_key,
                        prepared.route.protocol,
                        prepared.upstream_proxy.as_deref(),
                        &prepared.body_json,
                        prepared.is_stream,
                        prepared.session_id.as_deref(),
                        prepared.thread_id.as_deref(),
                        &prepared.extra_headers,
                        user_agents,
                        prepared.inbound_user_agent.as_deref(),
                    )
                    .await
                {
                    Ok(resp) => {
                        if resp.status.as_u16() == 429
                            && idx + 1 < prepared.upstream_base_urls.len()
                        {
                            tracing::debug!("上游 429,回退到下一个 base_url: {}", base_url);
                            continue;
                        }
                        if resp.status.is_success() {
                            break 'round Out::Ok(resp);
                        }
                        break 'round Out::Fail(resp);
                    }
                    Err(e) => {
                        if idx + 1 < prepared.upstream_base_urls.len() {
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
            // 429 不进退避重试(对齐 codex 传输层 retry_429: false):
            // 限流窗口远超 3s 预算,快速失败交客户端退避;多 base_url 回退不受影响
            Out::Fail(resp) if resp.status.is_server_error() => {
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
                prepared.route.protocol,
                current.body.bytes_stream(),
                prepared.estimated_input_tokens,
                prepared.tool_names.clone(),
                prepared.replay_scope.clone(),
                prepared.signature_model.clone(),
            );
            let first = out.next().await;
            let retry = match &first {
                Some(Ok(frame)) => is_initial_sse_error(frame),
                Some(Err(_)) | None => true,
            };
            if retry && attempt == 0 {
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
            let Some(first) = first else {
                return Err(AppError::new(anyhow::anyhow!("上游流在首帧前结束")));
            };
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

        if !final_truncated
            && (status.as_u16() == 400 || status.as_u16() == 422)
            && matches!(prepared.route.protocol, Protocol::OpenAiResponses)
            && is_thinking_signature_invalid(&final_bytes)
            && trim_encrypted_reasoning_items(&mut prepared.body_json)
        {
            tracing::warn!(
                protocol = ?prepared.route.protocol,
                "invalid_encrypted_content,剥离 reasoning 后重试一次"
            );
            if let Some((cache, key, _)) = prepared.replay_scope.as_ref() {
                cache.invalidate(key);
            }
            let retry = prepared.send_retry(upstream_client, user_agents).await?;
            final_status = retry.status;
            if retry.status.is_success() {
                status = retry.status;
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
                to_anthropic_error(&final_bytes)
            };
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
            relay_with_replay_tap(
                prepared.route.protocol,
                upstream.body.bytes_stream(),
                prepared.estimated_input_tokens,
                prepared.tool_names.clone(),
                prepared.replay_scope.clone(),
                prepared.signature_model.clone(),
            )
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
        if let Some((cache, key, fingerprint)) = prepared.replay_scope.as_ref() {
            if let Ok(v) = serde_json::from_slice::<Value>(&body_bytes) {
                match prepared.route.protocol {
                    Protocol::OpenAiResponses => {
                        if v.get("object").and_then(|o| o.as_str()) == Some("response") {
                            let wrapped = json!({"response": v});
                            cache.store_from_completed(key, &wrapped, fingerprint);
                        }
                    }
                    Protocol::Antigravity => {
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

impl PreparedMessageRequest {
    async fn send_retry(
        &self,
        client: &crate::upstream::UpstreamClient,
        user_agents: &crate::http::UserAgentSet,
    ) -> anyhow::Result<UpstreamResponse> {
        client
            .request(
                &self.upstream_base_urls[0],
                &self.upstream_key,
                self.route.protocol,
                self.upstream_proxy.as_deref(),
                &self.body_json,
                self.is_stream,
                self.session_id.as_deref(),
                self.thread_id.as_deref(),
                &self.extra_headers,
                user_agents,
                self.inbound_user_agent.as_deref(),
            )
            .await
    }
}

pub async fn handle_messages(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, AppError> {
    // 只取得一次快照；认证、转换和响应交付始终使用同一版本。
    let effective_snapshot = Arc::clone(&*state.config.read().await);
    check_secret(&headers, &effective_snapshot.runtime.secret)?;
    let bytes = to_bytes(body, crate::limits::INBOUND_BODY_LIMIT)
        .await
        .map_err(|e| AppError::new(anyhow::anyhow!("读请求体失败: {e}")))?;

    let mut prepared =
        prepare_message_request(&state, &headers, &bytes, &effective_snapshot).await?;

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
