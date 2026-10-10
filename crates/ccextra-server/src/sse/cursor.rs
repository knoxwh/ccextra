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
    // TurnEnded 上报的真实 input(未收到为 None)
    reported_input: Option<i64>,
    // 请求体字节/4 估算原值:续接回合 TurnEnded 缺 cache 字段时的假拆分基数
    estimated_input: i64,
    // cache_read:自校验通过时为真值,否则为假数据(基数的 99%)
    cache_read_tokens: i64,
    // cache_creation:自校验通过时为真值(field 4),否则恒 0
    cache_creation_tokens: i64,
    // 工具续接回合(多请求):TurnEnded 的 input/read 是整轮聚合值
    continuation: bool,
    next_index: i64,
    block: Option<Block>,
    started: bool,
    finished: bool,
    turn_ended: bool,
    has_text: bool,
    has_tool_use: bool,
}

impl CursorSse {
    pub fn new(id: &str, model: &str, input_tokens: usize, continuation: bool) -> Self {
        let estimate = i64::try_from(input_tokens).unwrap_or(i64::MAX);
        Self {
            id: id.into(),
            model: model.into(),
            // 未收到 TurnEnded 的流(工具边界提前收尾)按估算 1%/99% 假拆分:
            // context≈估算值,cache_read 累计近似真实缓存消耗
            input_tokens: estimate / 100,
            output_tokens: 0,
            reported_input: None,
            estimated_input: estimate,
            cache_read_tokens: estimate - estimate / 100,
            cache_creation_tokens: 0,
            continuation,
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
                // field 1 语义随回合形状不同:
                // - 单请求回合(新 user 消息起跑):input=本次总输入,read/write
                //   为子集,context=input(2026-10-09 活体样本:
                //   11292=11232+0+60、7952=1120+0+6832)。
                // - 工具续接回合(多请求):input=整轮各次输入总和,read=整轮
                //   累计命中(2026-10-10 活体样本:11 请求回合 input=176986
                //   read=157056,最终 context≈20k=input-read)。read 不可进
                //   context:最终行把 context(input-read)按 1%/99% 假拆分,
                //   read≈最终调用命中;中间请求的 cache_read 已由估算假拆分
                //   累计,近似整轮缓存消耗。
                // Anthropic 口径 input=非缓存部分,即 input-read-write;
                // 守卫 read+write<=input,越界回退 1%/99% 假拆分
                // (单请求按 input 真值,续接按请求体估算)。
                if let Some(input) = usage.input_tokens.filter(|tokens| *tokens > 0) {
                    self.reported_input = Some(input);
                    let read = usage.cache_read_tokens.filter(|v| *v >= 0);
                    // write 缺省视为 0:read 真值在场时不因 write 缺失丢拆分
                    let write = usage.cache_write_tokens.filter(|v| *v >= 0).or(Some(0));
                    match (read, write) {
                        // 非负守卫 + checked_add:varint 强转 i64 可能产生负值,
                        // 畸形值不得穿透进 usage
                        (Some(read), Some(write))
                            if read.checked_add(write).is_some_and(|sum| sum <= input) =>
                        {
                            if self.continuation {
                                // 续接回合:input=整轮总和,read=整轮累计命中,
                                // context=input-read(=Σfresh+write)。最终行把
                                // context 按 1%/99% 拆:read≈最终调用命中(前缀
                                // 缓存下命中占 context 绝大部分),input 取 1%。
                                // sanity:聚合语义的 context 不应远超请求体估算
                                // (R=0 等缓存异常形态会算出整轮总和),越界退估算。
                                let base = input - read - write;
                                // saturating_add:estimated_input 兜底 i64::MAX 时
                                // 普通加法会溢出(debug panic/release 回绕成负数)
                                if base
                                    <= self
                                        .estimated_input
                                        .saturating_add(self.estimated_input / 2)
                                {
                                    self.input_tokens = base / 100;
                                    self.cache_read_tokens = base - base / 100;
                                    self.cache_creation_tokens = write;
                                } else {
                                    self.input_tokens = self.estimated_input / 100;
                                    self.cache_read_tokens =
                                        self.estimated_input - self.estimated_input / 100;
                                }
                            } else {
                                self.input_tokens = input - read - write;
                                self.cache_read_tokens = read;
                                self.cache_creation_tokens = write;
                            }
                            tracing::info!(
                                "TurnEnded real cache split: input={input} read={read} write={write} continuation={}",
                                self.continuation
                            );
                        }
                        _ => {
                            let base = if self.continuation {
                                self.estimated_input
                            } else {
                                input
                            };
                            self.input_tokens = base / 100;
                            self.cache_read_tokens = base - base / 100;
                        }
                    }
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
                self.estimated_input,
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
        // input/cache 拆分:自校验通过为真值,否则 1%/99% 假数据;
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
            self.cache_creation_tokens,
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

    /// TurnEnded 上报的真实 input(未收到时 None)
    pub fn reported_input_tokens(&self) -> Option<i64> {
        self.reported_input
    }

    /// 导出 usage JSON（供非流式 JSON 响应使用；零值 cache 字段省略，
    /// 与 emit::message_delta 的下发规则一致）
    pub fn usage_json(&self) -> serde_json::Value {
        use serde_json::json;
        let mut usage = json!({
            "input_tokens": self.input_tokens,
            "output_tokens": self.output_tokens,
            "cache_read_input_tokens": self.cache_read_tokens
        });
        if self.cache_creation_tokens > 0 {
            usage["cache_creation_input_tokens"] = json!(self.cache_creation_tokens);
        }
        usage
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
        let mut state = CursorSse::new("message-1", "composer-2", 10, false);
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
    fn turn_ended_input_splits_into_fake_cache() {
        let mut state = CursorSse::new("message-3", "composer-2", 10, false);
        let mut frames = state.handle(&ServerMessage::TextDelta("answer".into()));
        frames.extend(state.handle(&ServerMessage::TokenDelta(3)));
        frames.extend(state.handle(&ServerMessage::TurnEnded(TurnUsage {
            input_tokens: Some(1000),
            output_tokens: Some(200),
            cache_read_tokens: None,
            cache_write_tokens: None,
        })));
        frames.extend(state.finish(false));
        // field 1 真值 1000:input 1% + cache_read 99% = 1000;output 用 TokenDelta
        let delta = event(&frames[frames.len() - 2]).1["usage"].clone();
        assert_eq!(delta["input_tokens"], 10);
        assert_eq!(delta["output_tokens"], 3);
        assert_eq!(delta["cache_read_input_tokens"], 990);
        assert!(delta.get("cache_creation_input_tokens").is_none());
        assert!(delta.get("output_tokens_details").is_none());
        assert_eq!(state.reported_input_tokens(), Some(1000));
    }

    #[test]
    fn turn_ended_real_cache_split_when_sum_matches_input() {
        let mut state = CursorSse::new("message-5", "composer-2", 10, false);
        let mut frames = state.handle(&ServerMessage::TextDelta("answer".into()));
        frames.extend(state.handle(&ServerMessage::TokenDelta(3)));
        // 文档实例形状:input 6759 = read 5120 + write 1639,非缓存部分为 0
        frames.extend(state.handle(&ServerMessage::TurnEnded(TurnUsage {
            input_tokens: Some(6759),
            output_tokens: Some(200),
            cache_read_tokens: Some(5120),
            cache_write_tokens: Some(1639),
        })));
        frames.extend(state.finish(false));
        let delta = event(&frames[frames.len() - 2]).1["usage"].clone();
        assert_eq!(delta["input_tokens"], 0);
        assert_eq!(delta["output_tokens"], 3);
        assert_eq!(delta["cache_read_input_tokens"], 5120);
        assert_eq!(delta["cache_creation_input_tokens"], 1639);
        // 非流式 JSON 路径与流式 message_delta 同源
        let usage = state.usage_json();
        assert_eq!(usage["input_tokens"], 0);
        assert_eq!(usage["cache_read_input_tokens"], 5120);
        assert_eq!(usage["cache_creation_input_tokens"], 1639);
    }

    #[test]
    fn turn_ended_real_cache_split_when_sum_below_input() {
        let mut state = CursorSse::new("message-7", "composer-2", 10, false);
        let mut frames = state.handle(&ServerMessage::TextDelta("answer".into()));
        frames.extend(state.handle(&ServerMessage::TokenDelta(3)));
        // 活体样本(2026-10-09 探针 E):input 11292 = read 11232 + write 0
        // + 非缓存 60;input 是总输入,Anthropic 口径取差值
        frames.extend(state.handle(&ServerMessage::TurnEnded(TurnUsage {
            input_tokens: Some(11292),
            output_tokens: Some(58),
            cache_read_tokens: Some(11232),
            cache_write_tokens: Some(0),
        })));
        frames.extend(state.finish(false));
        let delta = event(&frames[frames.len() - 2]).1["usage"].clone();
        assert_eq!(delta["input_tokens"], 60);
        assert_eq!(delta["output_tokens"], 3);
        assert_eq!(delta["cache_read_input_tokens"], 11232);
        // write 为 0 时省略 cache_creation,与 message_delta 形状一致
        assert!(delta.get("cache_creation_input_tokens").is_none());
        // 非流式 JSON 路径与流式 message_delta 同源
        let usage = state.usage_json();
        assert_eq!(usage["input_tokens"], 60);
        assert_eq!(usage["cache_read_input_tokens"], 11232);
        assert!(usage.get("cache_creation_input_tokens").is_none());
    }

    #[test]
    fn turn_ended_real_cache_split_when_all_fields_nonzero() {
        let mut state = CursorSse::new("message-8", "composer-2", 10, false);
        let mut frames = state.handle(&ServerMessage::TextDelta("answer".into()));
        frames.extend(state.handle(&ServerMessage::TokenDelta(3)));
        // 三项均非零:input 100 = read 60 + write 30 + 非缓存 10
        frames.extend(state.handle(&ServerMessage::TurnEnded(TurnUsage {
            input_tokens: Some(100),
            output_tokens: Some(40),
            cache_read_tokens: Some(60),
            cache_write_tokens: Some(30),
        })));
        frames.extend(state.finish(false));
        let delta = event(&frames[frames.len() - 2]).1["usage"].clone();
        assert_eq!(delta["input_tokens"], 10);
        assert_eq!(delta["cache_read_input_tokens"], 60);
        assert_eq!(delta["cache_creation_input_tokens"], 30);
    }

    #[test]
    fn turn_ended_falls_back_to_fake_split_on_negative_fields() {
        let mut state = CursorSse::new("message-9", "composer-2", 10, false);
        let mut frames = state.handle(&ServerMessage::TextDelta("answer".into()));
        frames.extend(state.handle(&ServerMessage::TokenDelta(3)));
        // varint 溢出成负 i64:负值穿透防御,回退假拆分
        frames.extend(state.handle(&ServerMessage::TurnEnded(TurnUsage {
            input_tokens: Some(1000),
            output_tokens: Some(200),
            cache_read_tokens: Some(-1),
            cache_write_tokens: Some(500),
        })));
        frames.extend(state.finish(false));
        let delta = event(&frames[frames.len() - 2]).1["usage"].clone();
        assert_eq!(delta["input_tokens"], 10);
        assert_eq!(delta["cache_read_input_tokens"], 990);
        assert!(delta.get("cache_creation_input_tokens").is_none());
    }

    #[test]
    fn turn_ended_falls_back_to_fake_split_when_sum_mismatches() {
        let mut state = CursorSse::new("message-6", "composer-2", 10, false);
        let mut frames = state.handle(&ServerMessage::TextDelta("answer".into()));
        frames.extend(state.handle(&ServerMessage::TokenDelta(3)));
        frames.extend(state.handle(&ServerMessage::TurnEnded(TurnUsage {
            input_tokens: Some(1000),
            output_tokens: Some(200),
            cache_read_tokens: Some(600),
            cache_write_tokens: Some(600),
        })));
        frames.extend(state.finish(false));
        // 600+600=1200 > 1000(累计语义为真):越界回退 1%/99% 假拆分
        let delta = event(&frames[frames.len() - 2]).1["usage"].clone();
        assert_eq!(delta["input_tokens"], 10);
        assert_eq!(delta["cache_read_input_tokens"], 990);
        assert!(delta.get("cache_creation_input_tokens").is_none());
    }

    #[test]
    fn continuation_turn_reports_turn_input_not_cumulative_usage() {
        let mut state = CursorSse::new("message-4", "composer-2", 40, false);
        let mut frames = state.handle(&ServerMessage::TextDelta("answer".into()));
        frames.extend(state.handle(&ServerMessage::TokenDelta(5)));
        frames.extend(state.handle(&ServerMessage::TurnEnded(TurnUsage {
            input_tokens: Some(366671),
            output_tokens: Some(29489),
            cache_read_tokens: None,
            cache_write_tokens: None,
        })));
        frames.extend(state.finish(false));
        // 上游 cache 字段不采用:usage 只由 field 1 拆分而来
        let delta = event(&frames[frames.len() - 2]).1["usage"].clone();
        assert_eq!(delta["input_tokens"], 3666);
        assert_eq!(delta["output_tokens"], 5);
        assert_eq!(delta["cache_read_input_tokens"], 363005);
        assert!(delta.get("output_tokens_details").is_none());
    }

    #[test]
    fn tool_boundary_closes_response_without_waiting_for_turn_end() {
        let mut state = CursorSse::new("message-2", "composer-2", 0, false);
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
    fn tool_boundary_usage_fake_splits_estimate() {
        let mut state = CursorSse::new("message-10", "composer-2", 1000, false);
        let mut frames = state.handle(&ServerMessage::TextDelta("checking".into()));
        frames.extend(state.tool_use("call-1", "lookup", r#"{"id":1}"#));
        frames.extend(state.finish(true));
        // 未收到 TurnEnded:估算 1000 按 1%/99% 假拆分,cache_read 累计近似缓存消耗
        let delta = event(&frames[frames.len() - 2]).1["usage"].clone();
        assert_eq!(delta["input_tokens"], 10);
        assert_eq!(delta["cache_read_input_tokens"], 990);
        // message_start 仍上报估算原值
        assert_eq!(
            event(&frames[0]).1["message"]["usage"]["input_tokens"],
            1000
        );
    }

    #[test]
    fn continuation_real_split_keeps_context_not_cumulative() {
        let mut state = CursorSse::new("message-11", "composer-2", 26000, true);
        let mut frames = state.handle(&ServerMessage::TextDelta("answer".into()));
        frames.extend(state.handle(&ServerMessage::TokenDelta(3)));
        // 活体样本(2026-10-10):11 请求回合 input=176986 read=157056 write=0,
        // input 是整轮总和,read 是整轮累计命中,context=input-read=19930;
        // 最终行按 1%/99% 拆 context,read≈最终调用命中
        frames.extend(state.handle(&ServerMessage::TurnEnded(TurnUsage {
            input_tokens: Some(176986),
            output_tokens: Some(58),
            cache_read_tokens: Some(157056),
            cache_write_tokens: Some(0),
        })));
        frames.extend(state.finish(false));
        let delta = event(&frames[frames.len() - 2]).1["usage"].clone();
        assert_eq!(delta["input_tokens"], 199);
        assert_eq!(delta["output_tokens"], 3);
        assert_eq!(delta["cache_read_input_tokens"], 19731);
        assert!(delta.get("cache_creation_input_tokens").is_none());
        // context = input + read = 19930,不是整轮总和 176986
        // 非流式 JSON 路径与流式 message_delta 同源
        let usage = state.usage_json();
        assert_eq!(usage["input_tokens"], 199);
        assert_eq!(usage["cache_read_input_tokens"], 19731);
    }

    #[test]
    fn continuation_cache_anomaly_falls_back_to_estimate() {
        let mut state = CursorSse::new("message-13", "composer-2", 26000, true);
        let mut frames = state.handle(&ServerMessage::TextDelta("answer".into()));
        frames.extend(state.handle(&ServerMessage::TokenDelta(3)));
        // R=0 的续接回合:input-read=整轮总和 176986,远超估算 26000,
        // 聚合语义不可信,退回估算假拆分
        frames.extend(state.handle(&ServerMessage::TurnEnded(TurnUsage {
            input_tokens: Some(176986),
            output_tokens: Some(58),
            cache_read_tokens: Some(0),
            cache_write_tokens: Some(0),
        })));
        frames.extend(state.finish(false));
        let delta = event(&frames[frames.len() - 2]).1["usage"].clone();
        assert_eq!(delta["input_tokens"], 260);
        assert_eq!(delta["cache_read_input_tokens"], 25740);
    }

    #[test]
    fn continuation_without_cache_fields_splits_estimate() {
        let mut state = CursorSse::new("message-12", "composer-2", 40000, true);
        let mut frames = state.handle(&ServerMessage::TextDelta("answer".into()));
        frames.extend(state.handle(&ServerMessage::TokenDelta(5)));
        // 续接回合缺 cache 字段:input 是整轮聚合值,不可直接拆,改用估算
        frames.extend(state.handle(&ServerMessage::TurnEnded(TurnUsage {
            input_tokens: Some(366671),
            output_tokens: Some(29489),
            cache_read_tokens: None,
            cache_write_tokens: None,
        })));
        frames.extend(state.finish(false));
        let delta = event(&frames[frames.len() - 2]).1["usage"].clone();
        assert_eq!(delta["input_tokens"], 400);
        assert_eq!(delta["cache_read_input_tokens"], 39600);
        assert_eq!(state.reported_input_tokens(), Some(366671));
    }

    #[test]
    fn eof_after_partial_output_emits_error_not_success() {
        let mut state = CursorSse::new("message-3", "composer-2", 4, false);
        let _ = state.handle(&ServerMessage::TextDelta("partial".into()));
        let frames = state.error("Cursor 连接提前关闭");
        assert_eq!(event(frames.last().unwrap()).0, "error");
        assert!(!frames.iter().any(|frame| event(frame).0 == "message_stop"));
        assert!(state.is_finished());
        assert!(state.finish(false).is_empty());
    }
}
