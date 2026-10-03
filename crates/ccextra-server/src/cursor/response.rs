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
    /// 本请求是工具续接(多轮回合):TurnEnded 是整回合累计口径,
    /// 不能当单请求 usage 直报,退回请求体字节估算
    continuation: bool,
    pub checkpoint: Option<Vec<u8>>,
    pub pending: Vec<ExecRequest>,
    pub finished: bool,
    pub emitted: bool,
}

impl CursorReply {
    /// input_tokens 是请求体字节/4 估算(对齐 Plus);单轮回合的
    /// TurnEnded 真实 usage 到达后覆盖,不估算
    pub fn new(id: String, model: String, input_tokens: usize, continuation: bool) -> Self {
        Self {
            sse: CursorSse::new(&id, &model, input_tokens, continuation),
            id,
            model,
            content: Vec::new(),
            input_tokens,
            output_tokens: 0,
            turn_usage: None,
            continuation,
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

    /// 上报给客户端的 input:单轮回合取 TurnEnded 真实值(Anthropic 语义,
    /// 已扣 cache);多轮回合无单请求真实数,用请求体估算;未上报为 None
    pub fn real_usage_input(&self) -> Option<i64> {
        let usage = self.turn_usage?;
        if self.continuation {
            return None;
        }
        let cache_read = usage.cache_read_tokens.unwrap_or(0);
        let cache_write = usage.cache_write_tokens.unwrap_or(0);
        usage
            .input_tokens
            .map(|tokens| (tokens - cache_read - cache_write).max(0))
    }

    /// 请求体字节/4 估算的 input(settle 写 token cache 的兜底值)
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
        // 单轮回合 TurnEnded 是真实单请求用量;多轮回合是整回合累计口径,
        // 直报会撑爆上下文/成本显示,退回请求体估算(input)与本响应
        // TokenDelta 累计(output),rd/wr 置 0(单请求真实值不存在)
        let usage = self.turn_usage.unwrap_or_default();
        let (input_tokens, cache_read, cache_write) = if self.continuation {
            (self.input_tokens as i64, 0, 0)
        } else {
            let cache_read = usage.cache_read_tokens.unwrap_or(0);
            let cache_write = usage.cache_write_tokens.unwrap_or(0);
            let input = usage
                .input_tokens
                .map(|tokens| (tokens - cache_read - cache_write).max(0))
                .unwrap_or(self.input_tokens as i64);
            (input, cache_read, cache_write)
        };
        let output_tokens = self.output_tokens;
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
