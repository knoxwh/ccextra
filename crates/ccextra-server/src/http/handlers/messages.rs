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
use crate::http::retry::compute_retry_delay;
use crate::http::{AppState, PayloadRule};
use crate::sse::replay_cache::StreamReplayExtractor;
use crate::sse::SseStreamPin;
use crate::upstream::{is_gpt_model, is_grok_model, UpstreamResponse};

/// 诊断日志请求序号:与毫秒时间戳组合,避免并发请求覆盖同一文件。
static UPSTREAM_LOG_SEQUENCE: AtomicU64 = AtomicU64::new(0);


/// 按 provider 名查找 provider 配置
pub(crate) fn find_provider<'a>(providers: &'a [ProviderConfig], name: &str) -> Option<&'a ProviderConfig> {
    providers.iter().find(|p| p.name == name)
}

/// 解析 payload 后最终模型。仅 OpenAI 的无效覆盖回退路由模型并同步写回 body,
/// 确保 Grok 判定与 UpstreamClient 读取的 model 一致；其余协议保留 payload 原语义。
pub(crate) fn resolve_outbound_model(body: &mut Value, fallback: &str, protocol: Protocol) -> String {
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

pub async fn handle_messages(
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
        thinking_registry,
    ) = {
        let rt = state.runtime.read().await;
        (
            rt.secret.clone(),
            rt.logging.request_body,
            rt.normalize.enabled,
            rt.normalize.drift_detector,
            rt.upstream.clone(),
            rt.user_agents.clone(),
            rt.thinking_registry.clone(),
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
    // 精简子集(跳过 dateline 归一化 / volatile 告警 / drift——dateline 和
    // drift 在转换后 openai handler 处理;cache_control 由转换器丢弃)
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
            // 非 Claude 模型的 system 清洗(剥离计费指纹、Claude 身份与触发块;
            // `*claude*` 模型保持逐字节直通)
            if sanitize_passthrough_prompt(&mut body_json, &route.upstream_model) {
                tracing::debug!(
                    model = %route.upstream_model,
                    "claude 直通 system 已清洗"
                );
            }
            // 非 Claude 模型的越档 effort 钳制(百炼等上游对越档值 400);
            // `*claude*` 模型与 thinking disabled 跳过,值不变不写回
            if clamp_passthrough_effort(&mut body_json, &route.upstream_model, &thinking_registry) {
                tracing::debug!(
                    model = %route.upstream_model,
                    "claude 直通 effort 已钳制到模型支持档"
                );
            }
        }
        Protocol::OpenAiChat => {
            convert_to_openai_chat_with(&mut body_json, &route.upstream_model, &thinking_registry)?;
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
            let rev = convert_to_openai_responses_with(
                &mut body_json,
                &route.upstream_model,
                &thinking_registry,
            )?;
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
            let (gemini_body, short_to_original) = convert_to_gemini_with_registry(
                &body_json,
                &route.upstream_model,
                ccextra_core::convert::gemini::SchemaFlavor::Gemini,
                &thinking_registry,
            );
            body_json = gemini_body;
            if !short_to_original.is_empty() {
                tool_names = Some(Arc::new(short_to_original));
            }
        }
        Protocol::Antigravity => {
            // Antigravity 使用包裹后的 Gemini 格式
            // 从 provider metadata 中提取 project_id
            let project_id = {
                let provider = find_provider(&providers, &route.provider);
                provider
                    .and_then(|p| p.metadata.as_ref())
                    .and_then(|m| m.get("project_id"))
                    .map(|s| s.as_str())
            };

            let (antigravity_body, short_to_original) = convert_to_antigravity_with(
                &body_json,
                &route.upstream_model,
                project_id,
                &thinking_registry,
            );
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

