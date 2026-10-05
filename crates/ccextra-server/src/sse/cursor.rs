use bytes::Bytes;
use ccextra_core::convert::cursor::proto::ServerMessage;

use super::emit;

#[derive(Clone, Copy, PartialEq, Eq)]
enum Block {
    Thinking,
    Text,
}

pub struct CursorSse {
    id: String,
    model: String,
    input_tokens: i64,
    output_tokens: i64,
    cache_read_tokens: i64,
    cache_write_tokens: i64,
    next_index: i64,
    block: Option<Block>,
    started: bool,
    finished: bool,
    turn_ended: bool,
    has_text: bool,
    has_tool_use: bool,
}

impl CursorSse {
    pub fn new(id: &str, model: &str, input_tokens: usize) -> Self {
        Self {
            id: id.into(),
            model: model.into(),
            input_tokens: i64::try_from(input_tokens).unwrap_or(i64::MAX),
            output_tokens: 0,
            cache_read_tokens: 0,
            cache_write_tokens: 0,
            next_index: 0,
            block: None,
            started: false,
            finished: false,
            turn_ended: false,
            has_text: false,
            has_tool_use: false,
        }
    }

    pub fn handle(&mut self, message: &ServerMessage) -> Vec<Bytes> {
        if self.finished {
            return Vec::new();
        }
        let mut frames = Vec::new();
        match message {
            ServerMessage::TextDelta(text) if !text.is_empty() => {
                self.start_block(Block::Text, &mut frames);
                self.has_text = true;
                frames.push(emit::content_block_delta_text(self.next_index, text));
            }
            ServerMessage::ThinkingDelta(text) if !text.is_empty() => {
                self.start_block(Block::Thinking, &mut frames);
                frames.push(emit::content_block_delta_thinking(self.next_index, text));
            }
            ServerMessage::TokenDelta(delta) if *delta > 0 => {
                self.output_tokens = self.output_tokens.saturating_add(*delta);
            }
            ServerMessage::TurnEnded(usage) => {
                self.turn_ended = true;
                // 提取 TurnEnded 携带的 cache token
                if let Some(cache_read) = usage.cache_read_tokens {
                    self.cache_read_tokens = cache_read;
                }
                if let Some(cache_write) = usage.cache_write_tokens {
                    self.cache_write_tokens = cache_write;
                }
                // output_tokens 优先用 TokenDelta 累计值，TurnEnded 作为兜底
                if self.output_tokens == 0 {
                    if let Some(output) = usage.output_tokens {
                        self.output_tokens = output;
                    }
                }
            }
            _ => {}
        }
        frames
    }

    fn ensure_started(&mut self, frames: &mut Vec<Bytes>) {
        if !self.started {
            self.started = true;
            frames.push(emit::message_start(
                &self.id,
                &self.model,
                self.input_tokens,
                0,
                true,
            ));
        }
    }

    fn close_block(&mut self, frames: &mut Vec<Bytes>) {
        if self.block.take().is_some() {
            frames.push(emit::content_block_stop(self.next_index));
            self.next_index += 1;
        }
    }

    fn start_block(&mut self, block: Block, frames: &mut Vec<Bytes>) {
        self.ensure_started(frames);
        if self.block == Some(block) {
            return;
        }
        self.close_block(frames);
        let start = match block {
            Block::Text => emit::content_block_start_text(self.next_index),
            Block::Thinking => emit::content_block_start_thinking(self.next_index),
        };
        frames.push(start);
        self.block = Some(block);
    }

    pub fn tool_use(&mut self, id: &str, name: &str, input_json: &str) -> Vec<Bytes> {
        if self.finished {
            return Vec::new();
        }
        let mut frames = Vec::new();
        self.ensure_started(&mut frames);
        self.close_block(&mut frames);
        frames.push(emit::content_block_start_tool_use(
            self.next_index,
            id,
            name,
        ));
        frames.push(emit::content_block_delta_input_json(
            self.next_index,
            input_json,
        ));
        frames.push(emit::content_block_stop(self.next_index));
        self.next_index += 1;
        self.has_tool_use = true;
        frames
    }

    pub fn finish(&mut self, tool_use: bool) -> Vec<Bytes> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        let mut frames = Vec::new();
        self.ensure_started(&mut frames);
        self.close_block(&mut frames);
        if !self.has_text && !self.has_tool_use {
            frames.push(emit::content_block_start_text(self.next_index));
            frames.push(emit::content_block_stop(self.next_index));
        }
        // TurnEnded 现已解析 cache token。input 仍用请求体估算，
        // output 用 TokenDelta 累计（TurnEnded 作兜底）。
        frames.push(emit::message_delta(
            if tool_use || self.has_tool_use {
                "tool_use"
            } else {
                "end_turn"
            },
            None,
            self.input_tokens,
            self.output_tokens,
            self.cache_read_tokens,
            self.cache_write_tokens,
            -1,
        ));
        frames.push(emit::message_stop());
        frames
    }

    pub fn error(&mut self, message: &str) -> Vec<Bytes> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        let mut frames = Vec::new();
        self.close_block(&mut frames);
        frames.push(emit::error_event(message));
        frames
    }

    pub fn is_finished(&self) -> bool {
        self.finished
    }
    pub fn turn_ended(&self) -> bool {
        self.turn_ended
    }

    /// 导出 usage JSON（供非流式 JSON 响应使用）
    pub fn usage_json(&self) -> serde_json::Value {
        use serde_json::json;
        json!({
            "input_tokens": self.input_tokens,
            "output_tokens": self.output_tokens,
            "cache_read_input_tokens": self.cache_read_tokens,
            "cache_creation_input_tokens": self.cache_write_tokens
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ccextra_core::convert::cursor::proto::TurnUsage;

    fn event(bytes: &Bytes) -> (String, serde_json::Value) {
        let text = std::str::from_utf8(bytes).unwrap();
        let (name, body) = text.trim_end().split_once("\ndata: ").unwrap();
        (
            name.trim_start_matches("event: ").to_string(),
            serde_json::from_str(body).unwrap(),
        )
    }

    #[test]
    fn text_and_thinking_finish_only_after_trailer() {
        let mut state = CursorSse::new("message-1", "composer-2", 10);
        let mut frames = state.handle(&ServerMessage::ThinkingDelta("plan".into()));
        frames.extend(state.handle(&ServerMessage::TextDelta("answer".into())));
        frames.extend(state.handle(&ServerMessage::TokenDelta(3)));
        frames.extend(state.handle(&ServerMessage::TurnEnded(Default::default())));
        assert!(!state.is_finished());
        frames.extend(state.finish(false));
        let names: Vec<_> = frames.iter().map(|frame| event(frame).0).collect();
        assert_eq!(
            names,
            [
                "message_start",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "content_block_start",
                "content_block_delta",
                "content_block_stop",
                "message_delta",
                "message_stop"
            ]
        );
        assert_eq!(event(&frames[0]).1["message"]["usage"]["input_tokens"], 10);
        assert_eq!(event(&frames[2]).1["delta"]["type"], "thinking_delta");
        assert_eq!(event(&frames[5]).1["delta"]["type"], "text_delta");
        assert_eq!(event(&frames[7]).1["usage"]["output_tokens"], 3);
        assert_eq!(event(&frames[7]).1["delta"]["stop_reason"], "end_turn");
        assert!(state
            .handle(&ServerMessage::TextDelta("late".into()))
            .is_empty());
        assert!(state.finish(false).is_empty());
    }

    #[test]
    fn turn_ended_does_not_override_estimate() {
        let mut state = CursorSse::new("message-3", "composer-2", 10);
        let mut frames = state.handle(&ServerMessage::TextDelta("answer".into()));
        frames.extend(state.handle(&ServerMessage::TokenDelta(3)));
        frames.extend(state.handle(&ServerMessage::TurnEnded(TurnUsage {
            input_tokens: Some(1000),
            output_tokens: Some(200),
            cache_read_tokens: Some(600),
            cache_write_tokens: Some(50),
            reasoning_tokens: Some(30),
        })));
        frames.extend(state.finish(false));
        // TurnEnded 现已解析 cache token。input 用估算,output 用 TokenDelta。
        let delta = event(&frames[frames.len() - 2]).1["usage"].clone();
        assert_eq!(delta["input_tokens"], 10);
        assert_eq!(delta["output_tokens"], 3);
        assert_eq!(delta["cache_read_input_tokens"], 600);
        assert_eq!(delta["cache_creation_input_tokens"], 50);
        assert!(delta.get("output_tokens_details").is_none());
    }

    #[test]
    fn continuation_turn_reports_estimate_not_cumulative_usage() {
        let mut state = CursorSse::new("message-4", "composer-2", 40);
        let mut frames = state.handle(&ServerMessage::TextDelta("answer".into()));
        frames.extend(state.handle(&ServerMessage::TokenDelta(5)));
        frames.extend(state.handle(&ServerMessage::TurnEnded(TurnUsage {
            input_tokens: Some(366671),
            output_tokens: Some(29489),
            cache_read_tokens: Some(1637760),
            cache_write_tokens: Some(1000),
            reasoning_tokens: Some(26578),
        })));
        frames.extend(state.finish(false));
        // TurnEnded 现已解析 cache token
        let delta = event(&frames[frames.len() - 2]).1["usage"].clone();
        assert_eq!(delta["input_tokens"], 40);
        assert_eq!(delta["output_tokens"], 5);
        assert_eq!(delta["cache_read_input_tokens"], 1637760);
        assert_eq!(delta["cache_creation_input_tokens"], 1000);
        assert!(delta.get("output_tokens_details").is_none());
    }

    #[test]
    fn tool_boundary_closes_response_without_waiting_for_turn_end() {
        let mut state = CursorSse::new("message-2", "composer-2", 0);
        let mut frames = state.handle(&ServerMessage::TextDelta("checking".into()));
        frames.extend(state.tool_use("call-1", "lookup", r#"{"id":1}"#));
        frames.extend(state.finish(true));
        // input 占位为构造时传入的估算
        assert_eq!(event(&frames[0]).1["message"]["usage"]["input_tokens"], 0);
        assert_eq!(event(&frames[4]).1["content_block"]["type"], "tool_use");
        assert_eq!(event(&frames[5]).1["delta"]["partial_json"], r#"{"id":1}"#);
        assert_eq!(event(&frames[7]).1["delta"]["stop_reason"], "tool_use");
        assert_eq!(event(frames.last().unwrap()).0, "message_stop");
    }

    #[test]
    fn eof_after_partial_output_emits_error_not_success() {
        let mut state = CursorSse::new("message-3", "composer-2", 4);
        let _ = state.handle(&ServerMessage::TextDelta("partial".into()));
        let frames = state.error("Cursor 连接提前关闭");
        assert_eq!(event(frames.last().unwrap()).0, "error");
        assert!(!frames.iter().any(|frame| event(frame).0 == "message_stop"));
        assert!(state.is_finished());
        assert!(state.finish(false).is_empty());
    }
}
