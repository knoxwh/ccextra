use super::drive::CursorEvent;
use crate::sse::cursor::CursorSse;
use bytes::Bytes;
use ccextra_core::convert::cursor::proto::{ExecKind, ExecRequest, ServerMessage, TurnUsage};
use serde_json::{json, Value};

pub struct CursorReply {
    sse: CursorSse,
    id: String,
    model: String,
    content: Vec<Value>,
    input_tokens: usize,
    output_tokens: i64,
    turn_usage: Option<TurnUsage>,
    pub checkpoint: Option<Vec<u8>>,
    pub pending: Vec<ExecRequest>,
    pub finished: bool,
    pub emitted: bool,
}

impl CursorReply {
    pub fn new(id: String, model: String, input_tokens: usize) -> Self {
        Self {
            sse: CursorSse::new(&id, &model, input_tokens),
            id,
            model,
            content: Vec::new(),
            input_tokens: input_tokens.max(1),
            output_tokens: 0,
            turn_usage: None,
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
            CursorEvent::TurnEnded(usage) => {
                self.turn_usage = Some(usage);
                self.sse.handle(&ServerMessage::TurnEnded(usage))
            }
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
        // TurnEnded 上报全量用量时覆盖估算值;input 含 cache 部分,按 Anthropic 语义拆出
        let usage = self.turn_usage.unwrap_or_default();
        let cache_read = usage.cache_read_tokens.unwrap_or(0);
        let cache_write = usage.cache_write_tokens.unwrap_or(0);
        let input_tokens = usage
            .input_tokens
            .map(|tokens| (tokens - cache_read - cache_write).max(0))
            .unwrap_or(self.input_tokens as i64);
        let output_tokens = usage.output_tokens.unwrap_or(self.output_tokens);
        json!({
            "id": self.id, "type": "message", "role": "assistant", "model": self.model,
            "content": content,
            "stop_reason": if self.pending.is_empty() { "end_turn" } else { "tool_use" },
            "stop_sequence": null,
            "usage": {
                "input_tokens": input_tokens,
                "output_tokens": output_tokens,
                "cache_read_input_tokens": cache_read,
                "cache_creation_input_tokens": cache_write
            }
        })
    }
}
