use super::drive::CursorEvent;
use crate::sse::cursor::CursorSse;
use bytes::Bytes;
use ccextra_core::convert::cursor::proto::{ExecKind, ExecRequest, ServerMessage};
use serde_json::{json, Value};

pub struct CursorReply {
    sse: CursorSse,
    id: String,
    model: String,
    content: Vec<Value>,
    input_tokens: usize,
    output_tokens: i64,
    pub checkpoint: Option<Vec<u8>>,
    pub pending: Vec<ExecRequest>,
    pub finished: bool,
    pub emitted: bool,
}

impl CursorReply {
    /// 创建新回复（input_tokens 是请求体字节/4 估算）
    ///
    /// Cursor TurnEnded 不携带用量。策略对齐 CLIProxyAPIPlus：
    /// - message_start.input_tokens: 估算值（请求体字节/4）
    /// - message_start.output_tokens: 0（TokenDelta 流式到达时才累加）
    /// - message_delta.output_tokens: 本响应 TokenDelta 累计真实值
    ///
    /// Claude Code 应读取 message_delta 获得最终准确 usage。
    pub fn new(id: String, model: String, input_tokens: usize) -> Self {
        Self {
            sse: CursorSse::new(&id, &model, input_tokens),
            id,
            model,
            content: Vec::new(),
            input_tokens,
            output_tokens: 0,
            checkpoint: None,
            pending: Vec::new(),
            finished: false,
            emitted: false,
        }
    }

    fn append_text(&mut self, kind: &str, text: String) {
        if text.is_empty() {
            return;
        }
        if let Some(last) = self.content.last_mut() {
            if last.get("type").and_then(Value::as_str) == Some(kind) {
                let key = if kind == "thinking" {
                    "thinking"
                } else {
                    "text"
                };
                if let Some(value) = last.get_mut(key).and_then(|value| value.as_str()) {
                    let mut joined = value.to_string();
                    joined.push_str(&text);
                    last[key] = Value::String(joined);
                    return;
                }
            }
        }
        self.content.push(if kind == "thinking" {
            json!({ "type": "thinking", "thinking": text })
        } else {
            json!({ "type": "text", "text": text })
        });
    }

    pub fn accept(&mut self, event: CursorEvent) -> Vec<Bytes> {
        if self.finished {
            return Vec::new();
        }
        let frames = match event {
            CursorEvent::Text(text) => {
                let frames = self.sse.handle(&ServerMessage::TextDelta(text.clone()));
                self.append_text("text", text);
                frames
            }
            CursorEvent::Thinking(text) => {
                let frames = self.sse.handle(&ServerMessage::ThinkingDelta(text.clone()));
                self.append_text("thinking", text);
                frames
            }
            CursorEvent::Tokens(delta) => {
                self.output_tokens = self.output_tokens.saturating_add(delta.max(0));
                self.sse.handle(&ServerMessage::TokenDelta(delta))
            }
            CursorEvent::TurnEnded(usage) => self.sse.handle(&ServerMessage::TurnEnded(usage)),
            CursorEvent::Checkpoint(raw) => {
                self.checkpoint = Some(raw);
                Vec::new()
            }
            CursorEvent::ToolUse { exec, input } => self.tool_use(exec, input),
            CursorEvent::End => {
                self.finished = true;
                self.sse.finish(false)
            }
        };
        self.emitted |= !frames.is_empty();
        frames
    }

    fn tool_use(&mut self, exec: ExecRequest, input: Value) -> Vec<Bytes> {
        let ExecKind::Mcp {
            name, tool_call_id, ..
        } = &exec.kind
        else {
            return Vec::new();
        };
        let id = if tool_call_id.is_empty() {
            format!("cursor_{}_{}", exec.exec_msg_id, exec.exec_id)
        } else {
            tool_call_id.clone()
        };
        let frames = self.sse.tool_use(&id, name, &input.to_string());
        self.content
            .push(json!({ "type": "tool_use", "id": id, "name": name, "input": input }));
        self.pending.push(exec);
        frames
    }

    pub fn tool_boundary(&mut self) -> Vec<Bytes> {
        self.finished = true;
        let frames = self.sse.finish(true);
        self.emitted |= !frames.is_empty();
        frames
    }

    /// 请求体字节/4 估算的 input(settle 写 token cache)
    pub fn estimated_input(&self) -> i64 {
        self.input_tokens as i64
    }

    /// 回合是否已向上游推进(已产帧或已收 token 增量);失败后不可安全重试
    pub fn progressed(&self) -> bool {
        self.emitted || self.output_tokens > 0
    }

    pub fn error(&mut self, message: &str) -> Vec<Bytes> {
        self.finished = true;
        self.sse.error(message)
    }

    pub fn json(&self) -> Value {
        let content = if self.content.is_empty() {
            vec![json!({ "type": "text", "text": "" })]
        } else {
            self.content.clone()
        };
        // TurnEnded 现已解析 cache token。input 用请求体估算,output 用 TokenDelta 累计。
        let usage = self.sse.usage_json();
        json!({
            "id": self.id, "type": "message", "role": "assistant", "model": self.model,
            "content": content,
            "stop_reason": if self.pending.is_empty() { "end_turn" } else { "tool_use" },
            "stop_sequence": null,
            "usage": usage
        })
    }
}
