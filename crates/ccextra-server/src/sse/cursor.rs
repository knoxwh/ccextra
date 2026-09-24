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
            input_tokens: i64::try_from(input_tokens).unwrap_or(i64::MAX).max(1),
            output_tokens: 0,
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
            ServerMessage::TurnEnded => self.turn_ended = true,
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
        frames.push(emit::message_delta(
            if tool_use || self.has_tool_use {
                "tool_use"
            } else {
                "end_turn"
            },
            None,
            self.input_tokens,
            self.output_tokens,
            0,
            0,
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
}

#[cfg(test)]
mod tests {
    use super::*;

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
        frames.extend(state.handle(&ServerMessage::TurnEnded));
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
    fn tool_boundary_closes_response_without_waiting_for_turn_end() {
        let mut state = CursorSse::new("message-2", "composer-2", 0);
        let mut frames = state.handle(&ServerMessage::TextDelta("checking".into()));
        frames.extend(state.tool_use("call-1", "lookup", r#"{"id":1}"#));
        frames.extend(state.finish(true));
        assert_eq!(event(&frames[0]).1["message"]["usage"]["input_tokens"], 1);
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
