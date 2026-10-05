use super::constants::DEFAULT_CLIENT_VERSION;
use super::drive::{CursorDrive, CursorEvent};
use super::error::CursorFailure;
use super::journal::ReplayStatus;
use super::response::CursorReply;
use super::session::{
    compute_tool_catalog_fingerprint, compute_turn_digest, BeginOutcome, ConsumerGuard,
    CursorSessions, InflightRun, RunOutcome, TakeError, ToolResult,
};
use super::{provider, refresh, store, stream::CursorStream};
use crate::http::error::AppError;
use crate::http::handlers::messages::PreparedMessageRequest;
use crate::http::publish_refreshed_providers;
use crate::http::retry::compute_retry_delay;
use crate::http::{AppState, ConfigSnapshot};
use axum::{
    body::Body,
    http::{header, HeaderMap, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use ccextra_core::convert::cursor::{build_run_request_with, conversation_id};
use serde_json::Value;
use std::time::{Duration, Instant};

fn uuid() -> Result<String, AppError> {
    let mut random = [0u8; 16];
    getrandom::getrandom(&mut random).map_err(|err| AppError::new(anyhow::anyhow!(err)))?;
    random[6] = (random[6] & 0x0f) | 0x40;
    random[8] = (random[8] & 0x3f) | 0x80;
    let digits = format!("{:032x}", u128::from_be_bytes(random));
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &digits[..8],
        &digits[8..12],
        &digits[12..16],
        &digits[16..20],
        &digits[20..]
    ))
}

async fn consumer_response(
    is_stream: bool,
    turn_digest: String,
    inflight: InflightRun,
    guard: ConsumerGuard,
) -> Result<Response, AppError> {
    if is_stream {
        Ok(consumer_stream_response(
            guard.sessions.clone(),
            turn_digest,
            inflight,
            guard,
        ))
    } else {
        consumer_json_response(guard, inflight).await
    }
}

/// 请求体字节/4 的 input 估算(对齐 Plus setInputEstimate);
/// 序列化失败退 1,保证占位非零
fn input_estimate(prepared: &PreparedMessageRequest) -> usize {
    serde_json::to_vec(&prepared.body_json)
        .map(|bytes| (bytes.len() / 4).max(1))
        .unwrap_or(1)
}

fn failure_response(failure: CursorFailure) -> Response {
    let mut response = AppError::with_status(failure.status, failure.message).into_response();
    if let Some(value) = failure.retry_after {
        response.headers_mut().insert("retry-after", value);
    }
    response
}

#[allow(clippy::too_many_arguments)]
fn handle_producer_error(
    sessions: &CursorSessions,
    conversation: &str,
    identity: &str,
    generation: u64,
    turn_digest: &str,
    reply: &mut CursorReply,
    inflight: &InflightRun,
    err_msg: String,
) {
    let err_frames = reply.error(&err_msg);
    sessions.publish_if_current(conversation, identity, generation, |journal| {
        for frame in err_frames {
            journal.record(turn_digest, frame.clone());
            let _ = inflight.event_tx.send(frame);
        }
    });
    let outcome = RunOutcome::Failure(err_msg.clone());
    *inflight.outcome.lock().unwrap() = Some(outcome.clone());
    let _ = inflight.notify.send(outcome);
    sessions.record_failure(conversation, identity, generation);
    sessions.broadcast_outcome(
        conversation,
        identity,
        generation,
        RunOutcome::Failure(err_msg),
    );
}

#[allow(clippy::too_many_arguments)]
fn handle_producer_settle(
    sessions: &CursorSessions,
    conversation: &str,
    identity: &str,
    generation: u64,
    drive: CursorDrive,
    reply: &mut CursorReply,
    stable: bool,
    token_cache: &SessionTokenCacheHandle,
    session_id: Option<&str>,
) -> Result<(), CursorFailure> {
    if stable {
        if let Some(raw) = reply.checkpoint.as_ref() {
            sessions.record_checkpoint(
                conversation,
                identity,
                generation,
                raw.clone(),
                drive.blob_store(),
            );
        }
    }
    if stable && !reply.pending.is_empty() {
        sessions
            .park(
                conversation,
                identity,
                generation,
                drive,
                reply.pending.clone(),
            )
            .map_err(CursorFailure::from_transport)?;
    } else {
        sessions.finish(conversation, identity, generation);
    }
    let snapshot = serde_json::to_vec(&reply.json()).map_err(CursorFailure::from_transport)?;
    // input 固定为请求体字节/4(count_tokens 与下轮占位复用)
    let settled_input = reply.estimated_input();
    if let (Some(sid), true) = (session_id, settled_input > 0) {
        let _ = token_cache
            .lock()
            .ok()
            .map(|mut cache| cache.insert(sid.to_string(), settled_input as usize));
    }
    sessions.broadcast_outcome(
        conversation,
        identity,
        generation,
        RunOutcome::Success(snapshot),
    );
    Ok(())
}

struct SetupGuard {
    sessions: CursorSessions,
    conversation: String,
    identity: String,
    generation: u64,
    handed_off: bool,
}

impl Drop for SetupGuard {
    fn drop(&mut self) {
        if !self.handed_off {
            self.sessions.broadcast_outcome(
                &self.conversation,
                &self.identity,
                self.generation,
                RunOutcome::Failure("Cursor 请求在启动阶段中断".into()),
            );
            self.sessions
                .cancel(&self.conversation, &self.identity, self.generation);
        }
    }
}

type SessionTokenCacheHandle =
    std::sync::Arc<std::sync::Mutex<crate::http::session_tokens::SessionTokenCache>>;

#[allow(clippy::too_many_arguments)]
fn spawn_cursor_producer(
    sessions: CursorSessions,
    conversation: String,
    identity: String,
    generation: u64,
    turn_digest: String,
    mut drive: CursorDrive,
    mut reply: CursorReply,
    inflight: InflightRun,
    mut cancellation: Option<tokio::sync::watch::Receiver<bool>>,
    stable: bool,
    token_cache: SessionTokenCacheHandle,
    session_id: Option<String>,
) {
    tokio::spawn(async move {
        let mut cancelled = false;
        while !reply.finished {
            let event = tokio::select! {
                event = drive.next_event() => event,
                _ = async {
                    if let Some(rx) = cancellation.as_mut() {
                        if !*rx.borrow() {
                            let _ = rx.changed().await;
                        }
                    } else {
                        futures::future::pending::<()>().await;
                    }
                } => {
                    cancelled = true;
                    break;
                }
            };

            let event = match event {
                Ok(ev) => ev,
                Err(err) => {
                    handle_producer_error(
                        &sessions,
                        &conversation,
                        &identity,
                        generation,
                        &turn_digest,
                        &mut reply,
                        &inflight,
                        err.message,
                    );
                    return;
                }
            };

            let frames = match accept_event(&mut drive, &mut reply, event).await {
                Ok(f) => f,
                Err(err) => {
                    handle_producer_error(
                        &sessions,
                        &conversation,
                        &identity,
                        generation,
                        &turn_digest,
                        &mut reply,
                        &inflight,
                        err.message,
                    );
                    return;
                }
            };

            if !sessions.publish_if_current(&conversation, &identity, generation, |journal| {
                for frame in frames {
                    journal.record(&turn_digest, frame.clone());
                    let _ = inflight.event_tx.send(frame);
                }
            }) {
                cancelled = true;
                break;
            }

            if !reply.pending.is_empty() {
                break;
            }
        }

        if cancelled {
            handle_producer_error(
                &sessions,
                &conversation,
                &identity,
                generation,
                &turn_digest,
                &mut reply,
                &inflight,
                "Cursor 会话已被取消".into(),
            );
            return;
        }

        if let Err(error) = handle_producer_settle(
            &sessions,
            &conversation,
            &identity,
            generation,
            drive,
            &mut reply,
            stable,
            &token_cache,
            session_id.as_deref(),
        ) {
            handle_producer_error(
                &sessions,
                &conversation,
                &identity,
                generation,
                &turn_digest,
                &mut reply,
                &inflight,
                error.message,
            );
        }
    });
}

fn consumer_stream_response(
    sessions: CursorSessions,
    turn_digest: String,
    inflight: InflightRun,
    guard: ConsumerGuard,
) -> Response {
    let mut event_rx = inflight.event_tx.subscribe();
    let mut outcome_rx = inflight.notify.subscribe();
    let output = async_stream::stream! {
        let _guard = guard;
        let mut cursor = 0;
        let mut emitted_error = false;
        let mut ticker = tokio::time::interval_at(
            tokio::time::Instant::now() + Duration::from_secs(10),
            Duration::from_secs(10),
        );
        loop {
            let outcome = inflight.outcome.lock().unwrap().clone();
            match sessions.journal().replay_status(&turn_digest) {
                ReplayStatus::Available(frames) => {
                    for frame in frames.into_iter().skip(cursor) {
                        let terminal = frame.starts_with(b"event: message_delta\n")
                            || frame.starts_with(b"event: message_stop\n");
                        // 工具驻留或收尾成功后，才允许客户端看见成功终态。
                        if terminal && outcome.is_none() {
                            break;
                        }
                        cursor += 1;
                        if terminal && matches!(outcome, Some(RunOutcome::Failure(_))) {
                            continue;
                        }
                        emitted_error |= frame.starts_with(b"event: error\n");
                        yield Ok::<Bytes, std::io::Error>(frame);
                    }
                }
                ReplayStatus::Evicted => {
                    yield Ok(crate::sse::emit::error_event("cursor_replay_unavailable"));
                    break;
                }
                // 失败但 journal 无记录(流未 message_start 就断了,error 帧进不了
                // journal):放行到下方 Failure 分支,把真实错误发给客户端,
                // 不用 replay 丢失的笼统文案掩盖
                ReplayStatus::NotFound if matches!(outcome, Some(RunOutcome::Success(_))) => {
                    yield Ok(crate::sse::emit::error_event("cursor_replay_unavailable"));
                    break;
                }
                ReplayStatus::NotFound => {}
            }
            if let Some(outcome) = outcome {
                if let RunOutcome::Failure(message) = outcome {
                    if !emitted_error {
                        yield Ok(crate::sse::emit::error_event(&message));
                    }
                }
                break;
            }
            tokio::select! {
                _ = ticker.tick() => yield Ok(Bytes::from_static(b": keepalive\n\n")),
                _ = outcome_rx.recv() => {},
                _ = event_rx.recv() => {},
            }
        }
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from_stream(output))
        .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

async fn consumer_json_response(
    _guard: ConsumerGuard,
    inflight: InflightRun,
) -> Result<Response, AppError> {
    let mut outcome_rx = inflight.notify.subscribe();
    let cached = inflight.outcome.lock().unwrap().clone();
    let outcome = match cached {
        Some(outcome) => Ok(outcome),
        None => outcome_rx.recv().await,
    };
    match outcome {
        Ok(RunOutcome::Success(snapshot)) => {
            let json_body: serde_json::Value = serde_json::from_slice(&snapshot)
                .map_err(|err| AppError::new(anyhow::anyhow!("解析 JSON 快照失败: {err}")))?;
            Ok((
                StatusCode::OK,
                [(header::CONTENT_TYPE, "application/json")],
                axum::Json(json_body),
            )
                .into_response())
        }
        Ok(RunOutcome::Failure(err)) => Err(AppError::with_status(StatusCode::BAD_GATEWAY, err)),
        Err(_) => Err(AppError::with_status(
            StatusCode::BAD_GATEWAY,
            "上游请求未产生有效响应",
        )),
    }
}

fn tool_results(body: &Value) -> Result<Vec<ToolResult>, AppError> {
    // Claude Code 会在 tool_result 后追加 system reminder,末条常是 system;
    // 对齐 input.rs last_conversation:跳过尾部 system 找真正的用户消息
    let Some(message) = body
        .get("messages")
        .and_then(Value::as_array)
        .and_then(|list| {
            list.iter()
                .rev()
                .find(|message| message.get("role").and_then(Value::as_str) != Some("system"))
        })
    else {
        return Ok(Vec::new());
    };
    if message.get("role").and_then(Value::as_str) != Some("user") {
        return Ok(Vec::new());
    }
    let Some(parts) = message.get("content").and_then(Value::as_array) else {
        return Ok(Vec::new());
    };
    let mut results = Vec::new();
    for part in parts {
        if part.get("type").and_then(Value::as_str) != Some("tool_result") {
            continue;
        }
        let id = part
            .get("tool_use_id")
            .and_then(Value::as_str)
            .filter(|id| !id.is_empty())
            .ok_or_else(|| AppError::bad_request("Cursor tool_result 缺少 tool_use_id"))?;
        let content = match part.get("content") {
            Some(Value::String(text)) => text.clone(),
            Some(Value::Array(blocks)) => {
                let mut text = Vec::new();
                for block in blocks {
                    if block.get("type").and_then(Value::as_str) != Some("text") {
                        return Err(AppError::bad_request("Cursor tool_result 仅支持文本块"));
                    }
                    text.push(
                        block
                            .get("text")
                            .and_then(Value::as_str)
                            .unwrap_or("")
                            .to_string(),
                    );
                }
                text.join("\n")
            }
            _ => return Err(AppError::bad_request("Cursor tool_result 内容必须是文本")),
        };
        results.push(ToolResult {
            tool_call_id: id.to_string(),
            content,
            is_error: part
                .get("is_error")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        });
    }
    Ok(results)
}

async fn publish_token(
    state: &AppState,
    provider_name: &str,
    identity: &str,
    token: &str,
    auth_dir: &std::path::Path,
) {
    for _ in 0..8 {
        if store::load(auth_dir).is_ok_and(|credential| credential.access_token != token) {
            return;
        }
        let snapshot = std::sync::Arc::clone(&*state.config.read().await);
        let mut providers = snapshot.providers.clone();
        let Some(provider) = providers.iter_mut().find(|p| p.name == provider_name) else {
            return;
        };
        if provider
            .metadata
            .as_ref()
            .and_then(|m| m.get("credential_id"))
            .map(String::as_str)
            != Some(identity)
        {
            return;
        }
        if provider.key == token {
            return;
        }
        provider.key = token.to_string();
        match publish_refreshed_providers(&state.config, snapshot.version, providers).await {
            Ok(true) => return,
            Ok(false) => continue,
            Err(error) => {
                tracing::warn!("Cursor 更新内存 token 失败: {error}");
                return;
            }
        }
    }
    tracing::warn!("Cursor 更新内存 token 遇到持续配置冲突");
}

pub(crate) async fn handle_cursor(
    state: &AppState,
    snapshot: &ConfigSnapshot,
    prepared: PreparedMessageRequest,
) -> Result<Response, AppError> {
    let provider = snapshot
        .providers
        .iter()
        .find(|p| p.name == prepared.route.provider)
        .ok_or_else(|| AppError::new(anyhow::anyhow!("Cursor provider 已失效")))?;
    let metadata = provider
        .metadata
        .as_ref()
        .ok_or_else(|| AppError::new(anyhow::anyhow!("Cursor provider 缺少凭证元数据")))?;
    let auth_dir = metadata
        .get("auth_dir")
        .ok_or_else(|| AppError::new(anyhow::anyhow!("Cursor provider 缺少 auth_dir")))?;
    let identity = metadata
        .get("credential_id")
        .ok_or_else(|| AppError::new(anyhow::anyhow!("Cursor provider 缺少 credential_id")))?;
    // Run 出站代理:provider 覆盖优先,缺省回退全局(区域受限模型如
    // muse-spark-1.3 需代理出口,直连会被上游 resource_exhausted 拒绝)
    let run_proxy = prepared
        .upstream_proxy
        .as_deref()
        .or_else(|| snapshot.runtime.upstream.global_proxy());
    let auth_dir = std::path::Path::new(auth_dir);
    let current = match store::load(auth_dir) {
        Ok(credential) => credential,
        Err(error) => {
            state.cursor_sessions.retain_identity(None);
            return Err(AppError::unauthorized(error.to_string()));
        }
    };
    let current_identity = provider::credential_fingerprint(&current);
    state
        .cursor_sessions
        .retain_identity(Some(&current_identity));
    if current_identity != *identity {
        return Err(AppError::unauthorized(
            "Cursor 凭证已换号,请 reload 模型目录",
        ));
    }
    let credential = refresh::ensure_credential_fresh(auth_dir, run_proxy, None)
        .await
        .unwrap_or_else(|err| {
            tracing::warn!("Cursor 请求前刷新失败,尝试当前 token: {err}");
            current
        });
    let refreshed_identity = provider::credential_fingerprint(&credential);
    if refreshed_identity != *identity {
        state
            .cursor_sessions
            .retain_identity(Some(&refreshed_identity));
        return Err(AppError::unauthorized(
            "Cursor 凭证已换号,请 reload 模型目录",
        ));
    }
    let mut token = credential.access_token;
    publish_token(state, &prepared.route.provider, identity, &token, auth_dir).await;
    let client_version = metadata
        .get("client_version")
        .map(String::as_str)
        .unwrap_or(DEFAULT_CLIENT_VERSION);
    let base_url = prepared
        .upstream_base_urls
        .first()
        .ok_or_else(|| AppError::new(anyhow::anyhow!("Cursor base_url 为空")))?;
    let session = prepared.session_id.as_deref();
    let conversation = match session {
        Some(id) => conversation_id(identity, id),
        None => uuid()?,
    };
    let message_id = uuid()?;
    let response_id = format!("msg_{}", uuid()?.replace('-', ""));
    let inbound_model = prepared
        .body_json
        .get("model")
        .and_then(Value::as_str)
        .unwrap_or("")
        .to_string();
    let results = tool_results(&prepared.body_json)?;
    let has_results = !results.is_empty();
    let stable = session.is_some();
    if has_results && !stable {
        return Err(AppError::bad_request(
            "cursor_session_lost: 工具续接需要稳定会话 ID",
        ));
    }

    let system = prepared
        .body_json
        .get("system")
        .cloned()
        .unwrap_or(serde_json::Value::Null);
    let tools = prepared
        .body_json
        .get("tools")
        .cloned()
        .unwrap_or(serde_json::Value::Array(vec![]));
    let tool_catalog_fingerprint = compute_tool_catalog_fingerprint(&tools);
    let messages = prepared
        .body_json
        .get("messages")
        .cloned()
        .unwrap_or(serde_json::Value::Array(vec![]));
    let model_params = serde_json::json!({
        "max_tokens": prepared.body_json.get("max_tokens"),
        "temperature": prepared.body_json.get("temperature"),
        "stream": prepared.body_json.get("stream"),
        "metadata": prepared.body_json.get("metadata"),
        "thinking": prepared.body_json.get("thinking"),
        // effort 输入必须进 digest:家族模型把钳制后的 effort 拼进上游
        // model id,同消息不同 effort 是不同回合,不能 singleflight 合流或回放
        "reasoning_effort": prepared.body_json.get("reasoning_effort"),
        "output_config": prepared.body_json.get("output_config"),
    });
    let upstream_model = prepared.route.upstream_model.clone();
    let turn_digest = compute_turn_digest(
        &conversation,
        &upstream_model,
        &model_params,
        &system,
        &messages,
        &tools,
    );

    // 先检查是否已完成（Completed 状态）
    if let Some(snapshot) =
        state
            .cursor_sessions
            .try_get_completed(&conversation, identity, &turn_digest)
    {
        if !prepared.is_stream {
            if let Ok(json_val) = serde_json::from_slice::<serde_json::Value>(&snapshot) {
                return Ok((
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "application/json")],
                    axum::Json(json_val),
                )
                    .into_response());
            }
            return Err(AppError::with_status(
                StatusCode::GONE,
                "cursor_replay_unavailable",
            ));
        }
        match state.cursor_sessions.journal().replay_status(&turn_digest) {
            ReplayStatus::Available(frames) if !frames.is_empty() => {
                let body_stream =
                    futures::stream::iter(frames.into_iter().map(Ok::<_, std::io::Error>));
                let mut response = Response::new(Body::from_stream(body_stream));
                *response.status_mut() = StatusCode::OK;
                response.headers_mut().insert(
                    header::CONTENT_TYPE,
                    HeaderValue::from_static("text/event-stream"),
                );
                response
                    .headers_mut()
                    .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-cache"));
                return Ok(response);
            }
            _ => {
                return Err(AppError::with_status(
                    StatusCode::GONE,
                    "cursor_replay_unavailable",
                ));
            }
        }
    }

    // 检查是否有正在运行的同 digest 请求（singleflight / 断线重连）
    if let Some((inflight, guard)) =
        state
            .cursor_sessions
            .try_join_inflight(&conversation, identity, &turn_digest)
    {
        return consumer_response(prepared.is_stream, turn_digest, inflight, guard).await;
    }

    if stable && has_results {
        let continuation_digest = turn_digest.clone();
        let resumed = match state.cursor_sessions.take(
            &conversation,
            identity,
            results,
            &upstream_model,
            &tool_catalog_fingerprint,
            continuation_digest.clone(),
        ) {
            Ok(resumed) => Some(resumed),
            Err(TakeError::Invalid(reason)) => {
                // 工具结果形状错误是客户端问题,必须 fail closed
                return Err(AppError::bad_request(format!(
                    "continuation 失败: {reason}"
                )));
            }
            Err(TakeError::Lost(reason)) => {
                // 驻留会话丢失(进程重启/TTL 过期/模型或工具目录变更):对齐 Plus,
                // 冷分支 flatten 全量 transcript(含 tool_use/tool_result 文本)重新起跑
                tracing::warn!("Cursor 工具续接会话丢失,回退 flatten: {reason}");
                None
            }
        };
        if let Some(resumed) = resumed {
            let (generation, drive_receiver, matched, inflight) = match resumed {
                super::session::ResumedSession::Resumed(generation, drive, matched, run) => {
                    (generation, drive, matched, run)
                }
                super::session::ResumedSession::Joined(generation, run) => {
                    let guard = state.cursor_sessions.create_consumer_guard(
                        &conversation,
                        identity,
                        generation,
                        &run,
                    );
                    return consumer_response(prepared.is_stream, turn_digest, run, guard).await;
                }
            };
            let mut setup = SetupGuard {
                sessions: state.cursor_sessions.clone(),
                conversation: conversation.clone(),
                identity: identity.clone(),
                generation,
                handed_off: false,
            };
            let guard = state.cursor_sessions.create_consumer_guard(
                &conversation,
                identity,
                generation,
                &inflight,
            );
            let cancellation =
                state
                    .cursor_sessions
                    .cancellation(&conversation, identity, generation);

            let drive = match drive_receiver.await {
                Ok(d) => d,
                Err(_) => return Err(AppError::bad_request("Cursor 会话已被取消或通道关闭")),
            };
            for (exec, result) in matched {
                if let Err(err) = drive
                    .send_tool_result(&exec, &result.content, result.is_error)
                    .await
                {
                    return Ok(failure_response(err));
                }
            }
            let reply = CursorReply::new(response_id, inbound_model, input_estimate(&prepared));
            setup.handed_off = true;
            spawn_cursor_producer(
                state.cursor_sessions.clone(),
                conversation,
                identity.to_string(),
                generation,
                continuation_digest.clone(),
                drive,
                reply,
                inflight.clone(),
                cancellation,
                stable,
                state.last_input_tokens.clone(),
                prepared.session_id.clone(),
            );
            return consumer_response(prepared.is_stream, continuation_digest, inflight, guard)
                .await;
        }
    }
    let checkpoint = if stable && !has_results {
        state
            .cursor_sessions
            .take_checkpoint(&conversation, identity)
    } else {
        None
    };

    // 在打开上游之前，原子注册/加入请求（同 digest 加入 singleflight，不同 digest 且 attached 拒绝）
    let (generation, inflight) = match state.cursor_sessions.try_begin(
        &conversation,
        identity,
        prepared.route.upstream_model.clone(),
        tool_catalog_fingerprint,
        turn_digest.clone(),
    ) {
        Ok(BeginOutcome::Started(gen, inf)) => (gen, inf),
        Ok(BeginOutcome::AlreadyRunning(generation, inf)) => {
            let guard = state.cursor_sessions.create_consumer_guard(
                &conversation,
                identity,
                generation,
                &inf,
            );
            return consumer_response(prepared.is_stream, turn_digest, inf, guard).await;
        }
        Err(err) => {
            return Err(AppError::with_status(StatusCode::BAD_REQUEST, err));
        }
    };

    let mut setup = SetupGuard {
        sessions: state.cursor_sessions.clone(),
        conversation: conversation.clone(),
        identity: identity.clone(),
        generation,
        handed_off: false,
    };
    let guard =
        state
            .cursor_sessions
            .create_consumer_guard(&conversation, identity, generation, &inflight);
    state.cursor_sessions.cleanup_turn_journal(&turn_digest);
    let mut retried_auth = false;
    let started = Instant::now();
    let mut attempt = 0;
    loop {
        let request = build_run_request_with(
            &prepared.body_json,
            &prepared.route.upstream_model,
            &conversation,
            &message_id,
            checkpoint.as_ref().map(|(raw, _)| raw.as_slice()),
            &snapshot.runtime.thinking_registry,
        )
        .map_err(|err| AppError::bad_request(err.to_string()))?;
        let request_id = uuid()?;
        let opened = CursorStream::open(
            base_url,
            &token,
            client_version,
            &request_id,
            run_proxy,
            &request.payload,
        )
        .await
        .map_err(CursorFailure::from_transport);
        let result = match opened {
            Ok(stream) if stream.status().is_success() => {
                let mut drive = CursorDrive::new(stream, request);
                if let Some((_, blobs)) = checkpoint.as_ref() {
                    drive.seed_blobs(blobs.clone());
                }
                let mut reply = CursorReply::new(
                    response_id.clone(),
                    inbound_model.clone(),
                    input_estimate(&prepared),
                );
                let first = async {
                    loop {
                        let event = drive.next_event().await?;
                        let frames = accept_event(&mut drive, &mut reply, event).await?;
                        if !frames.is_empty() || reply.finished {
                            return Ok::<_, CursorFailure>(frames);
                        }
                    }
                }
                .await;
                if let Err(mut error) = first {
                    // 首帧循环已收到增量(token 计数或已产帧)后不再重试:
                    // 重试会丢弃已收增量并重复计费,直接把失败交给客户端
                    if reply.progressed() {
                        error.progressed = true;
                    }
                    Err(error)
                } else {
                    if !state.cursor_sessions.publish_if_current(
                        &conversation,
                        identity,
                        generation,
                        |journal| {
                            for frame in first.unwrap() {
                                journal.record(&turn_digest, frame.clone());
                                let _ = inflight.event_tx.send(frame);
                            }
                        },
                    ) {
                        return Err(AppError::bad_request("Cursor 会话 owner 已更换"));
                    }
                    let cancellation =
                        state
                            .cursor_sessions
                            .cancellation(&conversation, identity, generation);
                    setup.handed_off = true;
                    spawn_cursor_producer(
                        state.cursor_sessions.clone(),
                        conversation.clone(),
                        identity.to_string(),
                        generation,
                        turn_digest.clone(),
                        drive,
                        reply,
                        inflight.clone(),
                        cancellation,
                        stable,
                        state.last_input_tokens.clone(),
                        prepared.session_id.clone(),
                    );
                    return consumer_response(prepared.is_stream, turn_digest, inflight, guard)
                        .await;
                }
            }
            Ok(stream) => Err(CursorFailure::from_http(stream.status(), stream.headers())),
            Err(error) => Err(error),
        };
        let failure = match result {
            Ok(response) => return Ok(response),
            Err(error) => error,
        };
        if failure.status == StatusCode::UNAUTHORIZED && !retried_auth && !failure.progressed {
            retried_auth = true;
            match refresh::ensure_credential_fresh(auth_dir, run_proxy, Some(&token)).await {
                Ok(fresh) if provider::credential_fingerprint(&fresh) == *identity => {
                    token = fresh.access_token;
                    publish_token(state, &prepared.route.provider, identity, &token, auth_dir)
                        .await;
                    continue;
                }
                Ok(_) => {
                    return Err(AppError::unauthorized(
                        "Cursor 凭证已换号,请 reload 模型目录",
                    ))
                }
                Err(error) => tracing::warn!("Cursor 401 刷新失败: {error}"),
            }
        }
        if !failure.retryable() || failure.progressed {
            state.cursor_sessions.broadcast_outcome(
                &conversation,
                identity,
                generation,
                RunOutcome::Failure(failure.message.clone()),
            );
            state
                .cursor_sessions
                .cancel(&conversation, identity, generation);
            return Ok(failure_response(failure));
        }
        let mut headers = HeaderMap::new();
        if let Some(value) = failure.retry_after.as_ref() {
            headers.insert("retry-after", value.clone());
        }
        let Some(delay) = compute_retry_delay(attempt, started, &headers, Some(failure.status))
        else {
            state.cursor_sessions.broadcast_outcome(
                &conversation,
                identity,
                generation,
                RunOutcome::Failure(failure.message.clone()),
            );
            state
                .cursor_sessions
                .cancel(&conversation, identity, generation);
            return Ok(failure_response(failure));
        };
        attempt += 1;
        tokio::time::sleep(delay).await;
    }
}

async fn accept_event(
    drive: &mut CursorDrive,
    reply: &mut CursorReply,
    event: CursorEvent,
) -> Result<Vec<Bytes>, CursorFailure> {
    let mut saw_turn_ended = matches!(event, CursorEvent::TurnEnded(_));
    let mut frames = reply.accept(event);
    if !reply.pending.is_empty() && !reply.finished {
        // 同一批工具:缓冲里的事件一次排空。TurnEnded 结束等待,但不把回复标成已结束。
        // 没有 TurnEnded 时,缓冲空后最多再等 16ms。
        let deadline = tokio::time::Instant::now() + Duration::from_millis(16);
        loop {
            let event = match drive.next_ready_event().await? {
                Some(event) => event,
                None if saw_turn_ended => break,
                None => match tokio::time::timeout_at(deadline, drive.next_event()).await {
                    Ok(event) => event?,
                    Err(_) => break,
                },
            };
            saw_turn_ended |= matches!(event, CursorEvent::TurnEnded(_));
            frames.extend(reply.accept(event));
            if reply.finished {
                break;
            }
        }
        if reply.finished {
            return Err(CursorFailure::from_transport("Cursor 在工具结果前结束"));
        }
        frames.extend(reply.tool_boundary());
    }
    Ok(frames)
}
