use super::error::CursorFailure;
use super::stream::CursorStream;
use ccextra_core::convert::cursor::proto::{
    decode_agent_server_message, parse_connect_end_stream, reply, ConnectFrame,
    ConnectFrameDecoder, ExecKind, ExecRequest, ServerMessage, CONNECT_END_STREAM_FLAG,
    DEFAULT_MAX_FRAME_SIZE,
};
use ccextra_core::convert::cursor::{decode_mcp_args, CursorRunRequest};
use serde_json::Value;
use std::collections::{HashMap, VecDeque};

pub enum CursorEvent {
    Text(String),
    Thinking(String),
    Tokens(i64),
    TurnEnded,
    Checkpoint(Vec<u8>),
    ToolUse { exec: ExecRequest, input: Value },
    End,
}

pub struct CursorDrive {
    stream: CursorStream,
    decoder: ConnectFrameDecoder,
    pending: VecDeque<ServerMessage>,
    frames: VecDeque<ConnectFrame>,
    blobs: HashMap<String, Vec<u8>>,
    tools: Vec<ccextra_core::convert::cursor::proto::generated::McpToolDefinition>,
    ended: bool,
}

impl CursorDrive {
    pub fn new(stream: CursorStream, request: CursorRunRequest) -> Self {
        Self {
            stream,
            decoder: ConnectFrameDecoder::new(DEFAULT_MAX_FRAME_SIZE),
            pending: VecDeque::new(),
            frames: VecDeque::new(),
            blobs: request.blob_store,
            tools: request.mcp_tools,
            ended: false,
        }
    }

    pub fn seed_blobs(&mut self, blobs: HashMap<String, Vec<u8>>) {
        self.blobs.extend(blobs);
    }

    pub fn blob_store(&self) -> HashMap<String, Vec<u8>> {
        self.blobs.clone()
    }

    pub async fn next_event(&mut self) -> Result<CursorEvent, CursorFailure> {
        loop {
            if let Some(event) = self.next_ready_event().await? {
                return Ok(event);
            }
            self.read_chunk().await?;
        }
    }

    pub async fn read_chunk(&mut self) -> Result<(), CursorFailure> {
        match self
            .stream
            .next_chunk()
            .await
            .map_err(CursorFailure::from_transport)?
        {
            Some(data) => self.frames.extend(
                self.decoder
                    .push(&data)
                    .map_err(CursorFailure::from_transport)?,
            ),
            None => {
                return Err(CursorFailure::from_transport(
                    "缺少 Connect end-stream trailer",
                ))
            }
        }
        Ok(())
    }

    pub async fn next_ready_event(&mut self) -> Result<Option<CursorEvent>, CursorFailure> {
        if self.ended {
            return Err(CursorFailure::from_transport("Cursor 已结束"));
        }
        loop {
            if let Some(message) = self.pending.pop_front() {
                if let Some(event) = self.handle_message(message).await? {
                    return Ok(Some(event));
                }
                continue;
            }
            let Some(frame) = self.frames.pop_front() else {
                return Ok(None);
            };
            if frame.flags == CONNECT_END_STREAM_FLAG {
                self.ended = true;
                if let Some(error) = parse_connect_end_stream(&frame.payload)
                    .map_err(CursorFailure::from_transport)?
                {
                    let mut failure = CursorFailure::from_connect(error);
                    failure.retry_after = self.stream.headers().get("retry-after").cloned();
                    return Err(failure);
                }
                if !self.frames.is_empty() || !self.decoder.is_empty() {
                    return Err(CursorFailure::from_transport("Connect trailer 后还有数据"));
                }
                return Ok(Some(CursorEvent::End));
            }
            if frame.flags & !1 != 0 {
                return Err(CursorFailure::from_transport("无效 Connect 帧 flags"));
            }
            let data = frame
                .decoded_payload(DEFAULT_MAX_FRAME_SIZE)
                .map_err(CursorFailure::from_transport)?;
            self.pending
                .extend(decode_agent_server_message(&data).map_err(CursorFailure::from_transport)?);
        }
    }

    async fn handle_message(
        &mut self,
        message: ServerMessage,
    ) -> Result<Option<CursorEvent>, CursorFailure> {
        let event = match message {
            ServerMessage::TextDelta(text) => Some(CursorEvent::Text(text)),
            ServerMessage::ThinkingDelta(text) => Some(CursorEvent::Thinking(text)),
            ServerMessage::TokenDelta(tokens) => Some(CursorEvent::Tokens(tokens)),
            ServerMessage::TurnEnded => Some(CursorEvent::TurnEnded),
            ServerMessage::Checkpoint(raw) => Some(CursorEvent::Checkpoint(raw.0)),
            ServerMessage::KvGet { id, blob_id } => {
                let payload = reply::encode_kv_get(
                    id,
                    self.blobs.get(&hex::encode(blob_id)).map(Vec::as_slice),
                );
                self.stream
                    .send_message(&payload)
                    .await
                    .map_err(CursorFailure::from_transport)?;
                None
            }
            ServerMessage::KvSet { id, blob_id, data } => {
                let key = hex::encode(blob_id);
                let size: usize = self
                    .blobs
                    .iter()
                    .filter(|(stored, _)| stored.as_str() != key.as_str())
                    .map(|(_, value)| value.len())
                    .sum();
                if size.saturating_add(data.len()) > 64 * 1024 * 1024 {
                    return Err(CursorFailure::from_transport(
                        "Cursor blob store 超过 64 MiB",
                    ));
                }
                self.blobs.insert(key, data);
                self.stream
                    .send_message(&reply::encode_kv_set(id))
                    .await
                    .map_err(CursorFailure::from_transport)?;
                None
            }
            ServerMessage::Exec(mut exec) => match exec.kind.clone() {
                ExecKind::RequestContext => {
                    let payload = reply::encode_request_context_result(
                        exec.exec_msg_id,
                        &exec.exec_id,
                        self.tools.clone(),
                    );
                    self.stream
                        .send_message(&payload)
                        .await
                        .map_err(CursorFailure::from_transport)?;
                    None
                }
                ExecKind::Builtin { field_number } => {
                    let payload = reply::encode_builtin_rejection(
                        exec.exec_msg_id,
                        &exec.exec_id,
                        field_number,
                    )
                    .ok_or_else(|| {
                        CursorFailure::from_transport(format!(
                            "Cursor 内置工具字段 {field_number} 不支持"
                        ))
                    })?;
                    self.stream
                        .send_message(&payload)
                        .await
                        .map_err(CursorFailure::from_transport)?;
                    None
                }
                ExecKind::Mcp { args, .. } => {
                    if let ExecKind::Mcp { tool_call_id, .. } = &mut exec.kind {
                        if tool_call_id.is_empty() {
                            *tool_call_id = format!("cursor_{}_{}", exec.exec_msg_id, exec.exec_id);
                        }
                    }
                    let input = decode_mcp_args(&args).map_err(CursorFailure::from_transport)?;
                    Some(CursorEvent::ToolUse { exec, input })
                }
            },
            ServerMessage::ThinkingCompleted | ServerMessage::Heartbeat => None,
        };
        Ok(event)
    }

    pub async fn send_tool_result(
        &self,
        exec: &ExecRequest,
        content: &str,
        is_error: bool,
    ) -> Result<(), CursorFailure> {
        let payload = reply::encode_mcp_result(exec.exec_msg_id, &exec.exec_id, content, is_error);
        self.stream
            .send_message(&payload)
            .await
            .map_err(CursorFailure::from_transport)
    }
}
