use super::constants::DEFAULT_CLIENT_VERSION;
use super::drive::{CursorDrive, CursorEvent};
use super::error::CursorFailure;
use super::response::CursorReply;
use super::session::{CursorSessions, ToolResult};
use super::{provider, refresh, store, stream::CursorStream};
use crate::http::error::AppError;
use crate::http::handlers::messages::PreparedMessageRequest;
use crate::http::publish_refreshed_providers;
use crate::http::retry::compute_retry_delay;
use crate::http::{AppState, ConfigSnapshot};
use axum::{
    body::Body,
    http::{header, HeaderMap, StatusCode},
    response::{IntoResponse, Response},
};
use bytes::Bytes;
use ccextra_core::convert::cursor::{build_run_request, conversation_id};
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

fn failure_response(failure: CursorFailure) -> Response {
    let mut response = AppError::with_status(failure.status, failure.message).into_response();
    if let Some(value) = failure.retry_after {
        response.headers_mut().insert("retry-after", value);
    }
    response
}

struct SessionLease {
    sessions: CursorSessions,
    conversation: String,
    identity: String,
    generation: u64,
    stable: bool,
    settled: bool,
    cancelled: Option<tokio::sync::watch::Receiver<bool>>,
}

impl SessionLease {
    async fn wait_cancelled(&mut self) {
        if let Some(receiver) = &mut self.cancelled {
            if !*receiver.borrow() {
                let _ = receiver.changed().await;
            }
        }
    }

    fn checkpoint(&self, reply: &CursorReply, drive: &CursorDrive) {
        if self.stable {
            if let Some(raw) = reply.checkpoint.as_ref() {
                self.sessions.record_checkpoint(
                    &self.conversation,
                    &self.identity,
                    self.generation,
                    raw.clone(),
                    drive.blob_store(),
                );
            }
        }
    }

    fn park(
        &mut self,
        drive: CursorDrive,
        pending: Vec<ccextra_core::convert::cursor::proto::ExecRequest>,
    ) -> Result<(), CursorFailure> {
        if self.stable {
            self.sessions
                .park(
                    &self.conversation,
                    &self.identity,
                    self.generation,
                    drive,
                    pending,
                )
                .map_err(CursorFailure::from_transport)?;
            self.settled = true;
        }
        Ok(())
    }

    fn finish(&mut self) {
        self.sessions
            .finish(&self.conversation, &self.identity, self.generation);
        self.settled = true;
    }
}

impl Drop for SessionLease {
    fn drop(&mut self) {
        if !self.settled {
            self.sessions
                .cancel(&self.conversation, &self.identity, self.generation);
        }
    }
}

fn tool_results(body: &Value) -> Result<Vec<ToolResult>, AppError> {
    let Some(message) = body
        .get("messages")
        .and_then(Value::as_array)
        .and_then(|list| list.last())
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
    let credential =
        refresh::ensure_credential_fresh(auth_dir, prepared.upstream_proxy.as_deref(), None)
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
    if stable && has_results {
        let resumed = state
            .cursor_sessions
            .take(&conversation, identity, results)
            .map_err(AppError::bad_request)?;
        if let Some((generation, drive_receiver, matched)) = resumed {
            let mut lease = SessionLease {
                sessions: state.cursor_sessions.clone(),
                conversation: conversation.clone(),
                identity: identity.clone(),
                generation,
                stable,
                settled: false,
                cancelled: state
                    .cursor_sessions
                    .cancellation(&conversation, identity, generation),
            };
            let drive = tokio::select! {
                drive = drive_receiver => drive.map_err(CursorFailure::from_transport),
                _ = lease.wait_cancelled() => Err(CursorFailure::from_transport("Cursor 会话 owner 已更换")),
            };
            let drive = match drive {
                Ok(drive) => drive,
                Err(error) => return Ok(failure_response(error)),
            };
            for (exec, result) in matched {
                let sent = tokio::select! {
                    sent = drive.send_tool_result(&exec, &result.content, result.is_error) => sent,
                    _ = lease.wait_cancelled() => Err(CursorFailure::from_transport("Cursor 会话 owner 已更换")),
                };
                if let Err(error) = sent {
                    return Ok(failure_response(error));
                }
            }
            let reply = CursorReply::new(
                response_id,
                inbound_model,
                (prepared.body_json.to_string().len() / 4).max(1),
            );
            let result = if prepared.is_stream {
                stream_response(drive, reply, lease).await
            } else {
                json_response(drive, reply, &mut lease).await
            };
            return Ok(result.unwrap_or_else(failure_response));
        }
    }
    let checkpoint = if stable && !has_results {
        state.cursor_sessions.checkpoint(&conversation, identity)
    } else {
        None
    };
    let mut retried_auth = false;
    let started = Instant::now();
    let mut attempt = 0;
    loop {
        let request = build_run_request(
            &prepared.body_json,
            &prepared.route.upstream_model,
            &conversation,
            &message_id,
            checkpoint.as_ref().map(|(raw, _)| raw.as_slice()),
        )
        .map_err(|err| AppError::bad_request(err.to_string()))?;
        let input_tokens = (request.payload.len() / 4).max(1);
        let request_id = uuid()?;
        let opened = CursorStream::open(
            base_url,
            &token,
            client_version,
            &request_id,
            prepared.upstream_proxy.as_deref(),
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
                let generation = state.cursor_sessions.begin(&conversation, identity);
                let mut lease = SessionLease {
                    sessions: state.cursor_sessions.clone(),
                    conversation: conversation.clone(),
                    identity: identity.clone(),
                    generation,
                    stable,
                    settled: false,
                    cancelled: state.cursor_sessions.cancellation(
                        &conversation,
                        identity,
                        generation,
                    ),
                };
                let reply =
                    CursorReply::new(response_id.clone(), inbound_model.clone(), input_tokens);
                if prepared.is_stream {
                    stream_response(drive, reply, lease).await
                } else {
                    json_response(drive, reply, &mut lease).await
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
            match refresh::ensure_credential_fresh(
                auth_dir,
                prepared.upstream_proxy.as_deref(),
                Some(&token),
            )
            .await
            {
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
            return Ok(failure_response(failure));
        }
        let mut headers = HeaderMap::new();
        if let Some(value) = failure.retry_after.as_ref() {
            headers.insert("retry-after", value.clone());
        }
        let Some(delay) = compute_retry_delay(attempt, started, &headers, Some(failure.status))
        else {
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
    let mut frames = reply.accept(event);
    if !reply.pending.is_empty() && !reply.finished {
        let deadline = tokio::time::Instant::now() + Duration::from_millis(16);
        loop {
            let event = match drive.next_ready_event().await? {
                Some(event) => event,
                None => match tokio::time::timeout_at(deadline, drive.next_event()).await {
                    Ok(event) => event?,
                    Err(_) => break,
                },
            };
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

async fn next_output(
    drive: &mut CursorDrive,
    reply: &mut CursorReply,
    lease: &mut SessionLease,
) -> Result<Vec<Bytes>, CursorFailure> {
    loop {
        let event = tokio::select! {
            event = drive.next_event() => event,
            _ = lease.wait_cancelled() => Err(CursorFailure::from_transport("Cursor 会话 owner 已更换")),
        }.map_err(|mut error| {
            error.progressed = reply.emitted;
            error
        })?;
        let frames = accept_event(drive, reply, event)
            .await
            .map_err(|mut error| {
                error.progressed = reply.emitted;
                error
            })?;
        if !frames.is_empty() || reply.finished {
            return Ok(frames);
        }
    }
}

async fn json_response(
    mut drive: CursorDrive,
    mut reply: CursorReply,
    lease: &mut SessionLease,
) -> Result<Response, CursorFailure> {
    while !reply.finished {
        next_output(&mut drive, &mut reply, lease).await?;
    }
    settle(lease, &reply, drive).map_err(|mut error| {
        error.progressed = reply.emitted;
        error
    })?;
    Ok((
        [(header::CONTENT_TYPE, "application/json")],
        axum::Json(reply.json()),
    )
        .into_response())
}

fn settle(
    lease: &mut SessionLease,
    reply: &CursorReply,
    drive: CursorDrive,
) -> Result<(), CursorFailure> {
    lease.checkpoint(reply, &drive);
    if reply.pending.is_empty() {
        lease.finish();
        Ok(())
    } else {
        lease.park(drive, reply.pending.clone())
    }
}

async fn stream_response(
    mut drive: CursorDrive,
    mut reply: CursorReply,
    mut lease: SessionLease,
) -> Result<Response, CursorFailure> {
    let first = next_output(&mut drive, &mut reply, &mut lease).await?;
    let output = async_stream::stream! {
        if reply.finished {
            match settle(&mut lease, &reply, drive) {
                Ok(()) => {
                    for frame in first { yield Ok::<Bytes, std::io::Error>(frame); }
                }
                Err(error) => yield Ok(crate::sse::emit::error_event(&error.message)),
            }
            return;
        }
        for frame in first { yield Ok(frame); }
        let mut ticker = tokio::time::interval_at(
            tokio::time::Instant::now() + Duration::from_secs(10),
            Duration::from_secs(10),
        );
        loop {
            let event = tokio::select! {
                event = drive.next_event() => event,
                _ = lease.wait_cancelled() => Err(CursorFailure::from_transport("Cursor 会话 owner 已更换")),
                _ = ticker.tick() => {
                    yield Ok(Bytes::from_static(b": keepalive\n\n"));
                    continue;
                }
            };
            let frames = match event {
                Ok(event) => accept_event(&mut drive, &mut reply, event).await,
                Err(error) => Err(error),
            };
            match frames {
                Ok(frames) if reply.finished => {
                    match settle(&mut lease, &reply, drive) {
                        Ok(()) => {
                            for frame in frames { yield Ok(frame); }
                        }
                        Err(error) => yield Ok(crate::sse::emit::error_event(&error.message)),
                    }
                    break;
                }
                Ok(frames) => {
                    for frame in frames { yield Ok(frame); }
                }
                Err(error) => {
                    for frame in reply.error(&error.message) { yield Ok(frame); }
                    break;
                }
            }
        }
    };
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from_stream(output))
        .map_err(CursorFailure::from_transport)
}
