// OpenAI chat/completions SSE → Anthropic messages SSE 状态机
//
// 采用"单 active block"模型(相比三独立 flag 更简洁):
// - 任意时刻只有一个 active block(text 或 thinking),切换类型时先 close 再开
// - tool 调用独立 pending map,不占 active block,完工时 flush 完整 input_json
// - finish 时统一 finalize(close 全部 + message_delta + message_stop)
//
// 核心难点:工具调用 index 一致。OpenAI 的 tool_calls[N] 用 index 标识,
// Anthropic 的 content_block 用连续 index 分配。两者需映射。

use std::collections::HashMap;

use async_stream::stream;
use bytes::Bytes;
use futures::Stream;
use futures::StreamExt;
use serde_json::Value;

use super::emit;
use super::parser::SseParser;
use super::SseStreamPin;
use ccextra_core::convert::fix_json_quotes;

/// active block 类型
#[derive(Clone, Copy, PartialEq, Eq)]
enum BlockType {
    Text,
    Thinking,
}

/// 单条工具调用累积状态
struct ToolCall {
    id: String,
    name: String,
    arguments: String,
    block_index: i64,
    started: bool,
    start_emitted: bool,
    closed: bool,
}

/// OpenAI chat → Anthropic 状态机
struct ChatRelay {
    started: bool,
    finished: bool,
    model: String,
    id: String,
    finish_reason: String,

    // 单 active block
    active_block: Option<(BlockType, i64)>,
    next_block_index: i64,

    // 待发 tool calls(按 OpenAI index 索引)
    tool_calls: HashMap<usize, ToolCall>,
    saw_tool_call: bool,

    // usage
    usage_seen: bool,
    usage_input: i64,
    usage_output: i64,
    usage_cached: i64,
    // 已发射的 thinking 片段(去重)
    thinking_parts: Vec<String>,
    // 本回合是否发过 text(兜底判定用)
    has_text: bool,
    // 空 id 兜底合成计数(SanitizeClaudeToolID 同角色)
    synthetic_tool_ids: u64,
    // 入站 body 本地估算输入 token(http.rs 计算;上游流未回真实 usage 时占位)
    estimated_input: Option<usize>,
}

impl ChatRelay {
    fn new(estimated_input: Option<usize>) -> Self {
        Self {
            started: false,
            finished: false,
            model: String::new(),
            id: String::new(),
            finish_reason: String::new(),
            active_block: None,
            next_block_index: 0,
            tool_calls: HashMap::new(),
            saw_tool_call: false,
            usage_seen: false,
            usage_input: 0,
            usage_output: 0,
            usage_cached: 0,
            thinking_parts: Vec::new(),
            has_text: false,
            synthetic_tool_ids: 0,
            estimated_input,
        }
    }

    /// 处理一个 SSE 事件,产出 anthropic 字节事件
    fn process(&mut self, ev: &super::parser::SseEvent) -> Vec<Bytes> {
        if self.finished {
            return Vec::new();
        }
        // [DONE] 标记
        if ev.data.trim() == "[DONE]" {
            return self.finish_done_marker();
        }

        let root: Value = match serde_json::from_str(&ev.data) {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        };
        if let Some(error) = root.get("error").filter(|error| error.is_object()) {
            let message = error
                .get("message")
                .and_then(|value| value.as_str())
                .or_else(|| root.get("message").and_then(|value| value.as_str()))
                .unwrap_or("upstream returned an error");
            return self.stream_error(message);
        }

        // 身份 + usage(任意 chunk 缓存)
        if !self.started {
            if let Some(id) = root.get("id").and_then(|v| v.as_str()) {
                if !id.is_empty() {
                    self.id = id.to_string();
                }
            }
        }
        if let Some(model) = root.get("model").and_then(|v| v.as_str()) {
            if !model.is_empty() {
                self.model = model.to_string();
            }
        }
        // usage 双路径提取(对齐 Moonshot):优先顶层 usage,回退 choices[0].usage
        if let Some(usage) = root.get("usage").filter(|u| !u.is_null()) {
            self.cache_usage(usage);
        } else if let Some(usage) = root.pointer("/choices/0/usage").filter(|u| !u.is_null()) {
            self.cache_usage(usage);
        }

        let Some(delta) = root.pointer("/choices/0/delta") else {
            return Vec::new();
        };

        let mut frames = self.ensure_started();

        // reasoning → thinking
        for text in collect_reasoning_texts(delta) {
            if self.thinking_distinct(&text) {
                frames.extend(self.emit_content_delta(BlockType::Thinking, &text));
            }
        }

        // content → text
        if let Some(text) = delta.get("content").and_then(|v| v.as_str()) {
            if !text.is_empty() {
                self.has_text = true;
                frames.extend(self.emit_content_delta(BlockType::Text, text));
            }
        }

        // tool_calls
        if let Some(tool_calls) = delta.get("tool_calls").and_then(|v| v.as_array()) {
            frames.extend(self.collect_tool_calls(tool_calls));
        }

        // finish_reason
        if let Some(fr) = root
            .pointer("/choices/0/finish_reason")
            .and_then(|v| v.as_str())
        {
            if !fr.is_empty() {
                self.finish_reason = if self.saw_tool_call {
                    "tool_calls".to_string()
                } else if fr == "tool_calls" {
                    "stop".to_string()
                } else {
                    fr.to_string()
                };
            }
        }

        // finish_reason 且 usage 可用 → finalize
        let usage_in_chunk = root.get("usage").map(|u| !u.is_null()).unwrap_or(false);
        if !self.finish_reason.is_empty() && (usage_in_chunk || self.usage_seen) {
            frames.extend(self.finalize());
        }

        frames
    }

    /// 缓存 usage 并做 cached 减法(与 extractOpenAIUsage 一致)
    fn cache_usage(&mut self, usage: &Value) {
        let (input, output, cached) = super::extract_usage_chat(usage);
        self.usage_input = input;
        self.usage_output = output;
        self.usage_cached = cached;
        self.usage_seen = true;
    }

    /// [DONE] 是 Chat 的显式终态;已开始消息即使缺 finish_reason 也按 end_turn 收尾。
    fn finish_done_marker(&mut self) -> Vec<Bytes> {
        if self.finished {
            return Vec::new();
        }
        if !self.started {
            return self.stream_error("upstream stream ended before response start");
        }
        self.finalize()
    }

    /// EOF 兜底:未见 Chat 终态时显式报 error,不把半个回答包装成正常完成。
    fn finish_eof(&mut self) -> Vec<Bytes> {
        if self.finished {
            return Vec::new();
        }
        if self.finish_reason.is_empty() {
            return self.stream_error("upstream stream ended before completion");
        }
        self.finalize()
    }

    /// 上游流中断:发 Anthropic error 事件,不伪造 message_start。
    fn stream_error(&mut self, message: &str) -> Vec<Bytes> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        vec![emit::error_event(message)]
    }

    /// 统一收尾:close active + flush tool calls + message_delta + message_stop
    fn finalize(&mut self) -> Vec<Bytes> {
        if self.finished {
            return Vec::new();
        }
        let mut frames = self.ensure_started();
        self.close_active_block(&mut frames);
        // reasoning-only 空回合兜底:上游只回 reasoning、无 text 也无工具
        // (如 DeepSeek 推理回合),把 thinking 文本转成 text 块,避免 Claude
        // Code 收到零可见内容回合(对齐 sub2api chatMessageToAnthropicBlocks:
        // text=="" 且无 tool_calls 时 text=reasoning)。
        if !self.has_text && !self.saw_tool_call && !self.thinking_parts.is_empty() {
            let content = self.thinking_parts.join("");
            frames.extend(self.emit_content_delta(BlockType::Text, &content));
            self.close_active_block(&mut frames);
        }
        self.flush_pending_tool_calls(&mut frames);

        frames.push(emit::message_delta(
            map_finish_reason(&self.finish_reason),
            None,
            self.usage_input,
            self.usage_output,
            self.usage_cached,
        ));
        frames.push(emit::message_stop());

        self.finished = true;
        frames
    }

    /// 确保 message_start 已发
    ///
    /// usage 填充策略:process 开头已解析过任意 chunk 的 usage(部分上游首
    /// chunk 即携带),有真实值用真实净值,cache 如实上报(全缓存回合净值
    /// 可为 0,由 cache_read 撑起 context);否则用入站 body 本地估算占位
    /// (占位兜底 1),让 cc context 过程中接近真实而非跳 1。流尾
    /// message_delta 以真实 usage 覆盖(上游全程沉默属残缺上游,delta 如实
    /// 报 0,不臆造)。cache 字段显式给出(0 或真实),不缺失,避免 Claude
    /// 把 undefined 当未知叠加。
    fn ensure_started(&mut self) -> Vec<Bytes> {
        if self.started {
            return Vec::new();
        }
        self.started = true;
        if self.usage_seen && (self.usage_input > 0 || self.usage_cached > 0) {
            // 真实 usage 已可用:input 为净值(已扣 cached),cache 如实上报。
            vec![emit::message_start(
                &self.id,
                &self.model,
                self.usage_input,
                self.usage_cached,
                false,
            )]
        } else {
            // 上游未回真实 usage(主流上游 usage 只在流尾):入站估算占位,
            // 量级接近真实;估算缺失时兜底 1。
            vec![emit::message_start(
                &self.id,
                &self.model,
                self.estimated_input.unwrap_or(1) as i64,
                0,
                true,
            )]
        }
    }

    /// 发 content_block_delta,自动开块(切换类型先 close 当前)
    fn emit_content_delta(&mut self, block_type: BlockType, content: &str) -> Vec<Bytes> {
        // 发新内容前关掉待发 tool calls(它们不占 active block)
        let mut frames = self.close_pending_tool_calls();
        frames.extend(self.ensure_block(block_type));
        let Some((_, index)) = self.active_block else {
            return frames;
        };
        frames.push(match block_type {
            BlockType::Text => emit::content_block_delta_text(index, content),
            BlockType::Thinking => emit::content_block_delta_thinking(index, content),
        });
        frames
    }

    /// 确保 active block 与目标类型一致;不一致则 close 当前再开
    fn ensure_block(&mut self, block_type: BlockType) -> Vec<Bytes> {
        let mut frames = self.ensure_started();
        if let Some((active, _)) = self.active_block {
            if active == block_type {
                return frames;
            }
        }
        self.close_active_block(&mut frames);
        let index = self.next_block_index;
        self.next_block_index += 1;
        frames.push(match block_type {
            BlockType::Text => emit::content_block_start_text(index),
            BlockType::Thinking => emit::content_block_start_thinking(index),
        });
        self.active_block = Some((block_type, index));
        frames
    }

    /// close 当前 active block
    fn close_active_block(&mut self, frames: &mut Vec<Bytes>) {
        if let Some((_, index)) = self.active_block.take() {
            frames.push(emit::content_block_stop(index));
        }
    }

    /// 累积 tool_calls 增量
    fn collect_tool_calls(&mut self, tool_calls: &[Value]) -> Vec<Bytes> {
        let mut frames = Vec::new();
        for (array_index, tc) in tool_calls.iter().enumerate() {
            // 缺 index 用数组下标(对齐 CPA be1763e5);字段存在则走 Int(),非法值当 0
            let index = if tc.get("index").is_none() {
                array_index
            } else {
                tc.get("index").and_then(|v| v.as_u64()).unwrap_or(0) as usize
            };
            {
                let call = self.tool_calls.entry(index).or_insert_with(|| ToolCall {
                    id: String::new(),
                    name: String::new(),
                    arguments: String::new(),
                    block_index: -1,
                    started: false,
                    start_emitted: false,
                    closed: false,
                });

                if let Some(id) = tc.get("id").and_then(|v| v.as_str()) {
                    if !id.is_empty() {
                        call.id = id.to_string();
                    }
                }

                if let Some(function) = tc.get("function") {
                    if let Some(name) = function.get("name").and_then(|v| v.as_str()) {
                        if !name.is_empty() {
                            call.name = name.to_string();
                        }
                    }
                    if let Some(args) = function.get("arguments") {
                        match args {
                            Value::String(s) if !s.is_empty() => call.arguments.push_str(s),
                            Value::Object(_) | Value::Array(_) => {
                                call.arguments = args.to_string();
                            }
                            _ => {}
                        }
                    }
                }
            }

            // name+id 齐了 → 发 start
            let (started, ready) = match self.tool_calls.get(&index) {
                Some(c) => (c.started, !c.name.is_empty() && !c.id.is_empty()),
                None => continue,
            };
            if !started && ready {
                self.emit_tool_use_start(index, &mut frames);
            }
        }
        frames
    }

    /// 分配 tool_use 的 block index(帧在 flush 时发)
    fn emit_tool_use_start(&mut self, openai_index: usize, frames: &mut Vec<Bytes>) {
        let started = self
            .tool_calls
            .get(&openai_index)
            .map(|c| c.started)
            .unwrap_or(true);
        if started {
            return;
        }
        // 工具调用开始前关闭 active text/thinking 块(对齐 emitToolUseStart),
        // 否则该块永远收不到 content_block_stop
        self.close_active_block(frames);
        let call = self.tool_calls.get_mut(&openai_index).unwrap();
        call.block_index = self.next_block_index;
        self.next_block_index += 1;
        call.started = true;
        self.saw_tool_call = true;
    }

    /// 发一条 tool_use 的 content_block_start 帧(幂等)
    fn tool_use_start_frame(&mut self, openai_index: usize, frames: &mut Vec<Bytes>) {
        let call = self.tool_calls.get_mut(&openai_index).unwrap();
        if call.start_emitted {
            return;
        }
        if call.id.is_empty() {
            // 空 id 兜底(对齐 SanitizeClaudeToolID 合成 id),避免发非法块
            self.synthetic_tool_ids += 1;
            call.id = format!("toolu_ccextra_{}", self.synthetic_tool_ids);
        }
        let id = call.id.clone();
        let name = call.name.clone();
        let index = call.block_index;
        call.start_emitted = true;
        frames.push(emit::content_block_start_tool_use(index, &id, &name));
    }

    /// 关闭所有待发 tool calls(发完整 input_json + stop)
    fn flush_pending_tool_calls(&mut self, frames: &mut Vec<Bytes>) {
        let mut indexes: Vec<usize> = self.tool_calls.keys().copied().collect();
        indexes.sort_unstable();
        for index in indexes {
            let (closed, started, name_empty) = match self.tool_calls.get(&index) {
                Some(c) => (c.closed, c.started, c.name.is_empty()),
                None => continue,
            };
            if closed {
                continue;
            }
            if !started {
                // 有 name 但没 id:兜底补分配
                if name_empty {
                    continue;
                }
                self.emit_tool_use_start(index, frames);
            }
            self.tool_use_start_frame(index, frames);
            let call = self.tool_calls.get(&index).unwrap();
            if !call.arguments.is_empty() {
                frames.push(emit::content_block_delta_input_json(
                    call.block_index,
                    &fix_json_quotes(&call.arguments),
                ));
            }
            frames.push(emit::content_block_stop(call.block_index));
            self.tool_calls.get_mut(&index).unwrap().closed = true;
        }
    }

    /// 关闭待发 tool calls(发新 text/thinking 前调用)
    fn close_pending_tool_calls(&mut self) -> Vec<Bytes> {
        let mut frames = Vec::new();
        let mut indexes: Vec<usize> = self.tool_calls.keys().copied().collect();
        indexes.sort_unstable();
        for index in indexes {
            let (closed, started) = match self.tool_calls.get(&index) {
                Some(c) => (c.closed, c.started),
                None => continue,
            };
            if closed || !started {
                continue;
            }
            self.tool_use_start_frame(index, &mut frames);
            // 关闭前补发累积的 arguments(对齐 flush),否则工具 input 变空
            let (block_index, arguments) = {
                let call = self.tool_calls.get(&index).unwrap();
                (call.block_index, call.arguments.clone())
            };
            if !arguments.is_empty() {
                frames.push(emit::content_block_delta_input_json(
                    block_index,
                    &fix_json_quotes(&arguments),
                ));
            }
            frames.push(emit::content_block_stop(block_index));
            self.tool_calls.get_mut(&index).unwrap().closed = true;
        }
        frames
    }

    /// thinking 去重:空白或重复片段跳过
    /// 对齐 appendReasoningTextIfDistinct:片段等于任一已发片段、
    /// 或等于全部已发拼接(累积快照式 provider)都丢弃
    fn thinking_distinct(&mut self, text: &str) -> bool {
        let trimmed = text.trim();
        if trimmed.is_empty() {
            return false;
        }
        let joined = self.thinking_parts.join("");
        if joined.trim() == trimmed {
            return false;
        }
        if self.thinking_parts.iter().any(|p| p.trim() == trimmed) {
            return false;
        }
        self.thinking_parts.push(text.to_string());
        true
    }
}

/// 从 delta 的多种字段提取 reasoning 文本
/// (对齐 collectOpenAIReasoningTexts:reasoning_content /
/// reasoning_details[] / reasoning / thinking 多种供应商拼写)
fn collect_reasoning_texts(delta: &Value) -> Vec<String> {
    let mut texts = Vec::new();
    if let Some(v) = delta.get("reasoning_content") {
        collect_reasoning_value(v, &mut texts);
    }
    if let Some(Value::Array(items)) = delta.get("reasoning_details") {
        for item in items {
            // 跳过仅加密项(同语义)
            if item.get("encrypted_content").is_some() || item.get("data").is_some() {
                continue;
            }
            for field in ["text", "reasoning", "thinking", "summary"] {
                if let Some(v) = item.get(field) {
                    collect_reasoning_value(v, &mut texts);
                    break;
                }
            }
        }
    }
    if let Some(v) = delta.get("reasoning") {
        collect_reasoning_value(v, &mut texts);
    }
    if let Some(v) = delta.get("thinking") {
        collect_reasoning_value(v, &mut texts);
    }
    texts
}

fn collect_reasoning_value(node: &Value, out: &mut Vec<String>) {
    match node {
        Value::String(s) if !s.is_empty() => out.push(s.clone()),
        Value::Array(arr) => {
            for item in arr {
                collect_reasoning_value(item, out);
            }
        }
        Value::Object(obj) => {
            if let Some(Value::String(s)) = obj.get("text") {
                if !s.is_empty() {
                    out.push(s.clone());
                }
            }
        }
        _ => {}
    }
}

/// OpenAI finish_reason → Anthropic stop_reason
pub(super) fn map_finish_reason(reason: &str) -> &'static str {
    match reason {
        "stop" => "end_turn",
        "length" => "max_tokens",
        "tool_calls" => "tool_use",
        "function_call" => "tool_use",
        "content_filter" => "refusal",
        _ => "end_turn",
    }
}

/// Claude 直通:字节级转发,不解析
pub fn relay_claude_passthrough<S>(stream: S) -> SseStreamPin
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
{
    Box::pin(stream.map(|chunk| chunk.map_err(std::io::Error::other)))
}

/// OpenAI chat → Anthropic SSE 状态机(realtime)
pub fn relay_openai_chat_to_anthropic<S>(
    stream: S,
    estimated_input_tokens: Option<usize>,
) -> SseStreamPin
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
{
    let mut stream = Box::pin(stream);
    let mut parser = SseParser::new();
    let mut relay = ChatRelay::new(estimated_input_tokens);

    Box::pin(stream! {
        loop {
            let Some(chunk) = stream.next().await else { break };
            let chunk = match chunk {
                Ok(c) => c,
                Err(e) => {
                    // 上游中断:发结构化 error 事件,不裸断流
                    for out in relay.stream_error(&e.to_string()) {
                        yield Ok(out);
                    }
                    return;
                }
            };
            for ev in parser.push(&chunk) {
                for out in relay.process(&ev) {
                    yield Ok(out);
                }
            }
        }
        // 流结束 flush
        for ev in parser.finish() {
            for out in relay.process(&ev) {
                yield Ok(out);
            }
        }
        // EOF 兜底:未见终态的残缺流由状态机转 error。
        for out in relay.finish_eof() {
            yield Ok(out);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_sse_format() {
        let b = emit::sse("message_start", &json!({"type": "message_start"}));
        assert_eq!(
            b,
            Bytes::from("event: message_start\ndata: {\"type\":\"message_start\"}\n\n")
        );
    }

    #[test]
    fn test_message_start_uses_estimated_when_usage_silent() {
        // 上游未回真实 usage 时,message_start 用入站估算占位(context 不跳 1)
        let mut r = ChatRelay::new(Some(567));
        let out = r.process(&super::super::parser::SseEvent {
            event: Some("chat.completion.chunk".into()),
            data: r#"{"id":"1","choices":[{"delta":{"content":"hi"}}]}"#.into(),
        });
        let start = out
            .iter()
            .find(|b| b.starts_with(b"event: message_start"))
            .expect("message_start 应存在");
        let s = String::from_utf8_lossy(start);
        let v: Value = serde_json::from_str(s.split_once("data: ").unwrap().1.trim()).unwrap();
        assert_eq!(v["message"]["usage"]["input_tokens"], 567);
        assert_eq!(v["message"]["usage"]["cache_read_input_tokens"], 0);
    }

    #[test]
    fn test_message_start_prefers_real_usage_when_present() {
        // 上游首 chunk 即带真实 usage 时,message_start 用真实净值 + cache,
        // 不用估算占位(占位可能偏离上游 tokenizer)。
        let mut r = ChatRelay::new(Some(99999));
        let out = r.process(&super::super::parser::SseEvent {
            event: Some("chat.completion.chunk".into()),
            data: r#"{"id":"1","choices":[{"delta":{"content":"hi"},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":100,"completion_tokens":20,
                "prompt_tokens_details":{"cached_tokens":70}}}"#
                .into(),
        });
        let start = out
            .iter()
            .find(|b| b.starts_with(b"event: message_start"))
            .expect("message_start 应存在");
        let s = String::from_utf8_lossy(start);
        let v: Value = serde_json::from_str(s.split_once("data: ").unwrap().1.trim()).unwrap();
        let usage = &v["message"]["usage"];
        // 真实净值 = prompt - cached = 30,不用占位
        assert_eq!(usage["input_tokens"], 30);
        assert_eq!(usage["cache_read_input_tokens"], 70);
        assert_eq!(usage["cache_creation_input_tokens"], 0);
    }

    #[test]
    fn test_message_start_all_cached_uses_real_net_and_read() {
        // 全缓存回合:净值 0 但 cache_read 有值,仍走真实分支(cache 撑起 context)
        let mut r = ChatRelay::new(None);
        let out = r.process(&super::super::parser::SseEvent {
            event: Some("chat.completion.chunk".into()),
            data: r#"{"id":"1","choices":[{"delta":{"content":"hi"},"finish_reason":"stop"}],
                "usage":{"prompt_tokens":70,"completion_tokens":20,
                "prompt_tokens_details":{"cached_tokens":70}}}"#
                .into(),
        });
        let start = out
            .iter()
            .find(|b| b.starts_with(b"event: message_start"))
            .expect("message_start 应存在");
        let s = String::from_utf8_lossy(start);
        let v: Value = serde_json::from_str(s.split_once("data: ").unwrap().1.trim()).unwrap();
        let usage = &v["message"]["usage"];
        assert_eq!(usage["input_tokens"], 0);
        assert_eq!(usage["cache_read_input_tokens"], 70);
    }

    #[test]
    fn test_text_stream() {
        let mut r = ChatRelay::new(None);
        let ev1 = super::super::parser::SseEvent {
            event: Some("chat.completion.chunk".into()),
            data: r#"{"id":"1","choices":[{"delta":{"content":"hi "}}]}"#.into(),
        };
        let out1 = r.process(&ev1);
        // 首 chunk 应发 message_start + content_block_start + content_block_delta
        assert!(out1.iter().any(|b| b.starts_with(b"event: message_start")));
        assert!(out1
            .iter()
            .any(|b| b.starts_with(b"event: content_block_start")));
        assert!(out1
            .iter()
            .any(|b| b.starts_with(b"event: content_block_delta")));

        let ev2 = super::super::parser::SseEvent {
            event: Some("chat.completion.chunk".into()),
            data: r#"{"id":"1","choices":[{"delta":{"content":"there"},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":5}}"#.into(),
        };
        let out2 = r.process(&ev2);
        // 应发 content_block_stop + message_delta + message_stop
        assert!(out2
            .iter()
            .any(|b| b.starts_with(b"event: content_block_stop")));
        assert!(out2.iter().any(|b| b.starts_with(b"event: message_delta")));
        assert!(out2.iter().any(|b| b.starts_with(b"event: message_stop")));
    }

    #[test]
    fn test_tool_call_stream() {
        let mut r = ChatRelay::new(None);
        // chunk1: tool_calls 第一个片段,id+name(ai 模型下仅累积,finish 才 flush 发 start)
        let out1 = r.process(&super::super::parser::SseEvent {
            event: Some("c".into()),
            data: r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"get_weather","arguments":""}}]}}]}"#.into(),
        });
        // 会发 message_start,但工具 start 帧延迟到 finish 才发
        assert!(out1.iter().any(|b| b.starts_with(b"event: message_start")));
        assert!(!out1
            .iter()
            .any(|b| b.starts_with(b"event: content_block_start")));

        // chunk2: arguments 增量(累积,不在流中逐段发)
        let out2 = r.process(&super::super::parser::SseEvent {
            event: Some("c".into()),
            data: r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{\"city\":"}}]}}]}"#.into(),
        });
        assert!(out2.is_empty());

        // chunk3: finish_reason + usage → 关闭块 + 发完整 input_json_delta
        let out3 = r.process(&super::super::parser::SseEvent {
            event: Some("c".into()),
            data: r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":10,"completion_tokens":5}}"#.into(),
        });
        assert!(out3
            .iter()
            .any(|b| b.starts_with(b"event: content_block_stop")));
        assert!(out3
            .iter()
            .any(|b| b.starts_with(b"event: content_block_delta")));
        assert!(out3.iter().any(|b| b.starts_with(b"event: message_delta")));
    }

    #[test]
    fn test_omitted_tool_call_index_preserves_parallel_calls() {
        // 对齐 CPA be1763e5:同 chunk 并行 tool_calls 缺 index 时按数组下标累积
        let mut r = ChatRelay::new(None);
        let out = r.process(&super::super::parser::SseEvent {
            event: Some("c".into()),
            data: r#"{"id":"c1","model":"m","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"id":"call_weather","type":"function","function":{"name":"get_weather","arguments":"{\"city\":\"Paris\"}"}},{"id":"call_time","type":"function","function":{"name":"get_time","arguments":"{\"tz\":\"UTC\"}"}}]},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":10,"completion_tokens":5}}"#.into(),
        });
        let buf = out.concat();
        let s = String::from_utf8_lossy(&buf);
        let mut starts = Vec::new();
        let mut deltas = Vec::new();
        let mut stops = 0;
        for block in s.split("event: ") {
            if block.starts_with("content_block_start") {
                if let Some(data) = block.split_once("data: ").map(|(_, rest)| rest) {
                    let json = data.split("\n\n").next().unwrap_or(data).trim();
                    if let Ok(v) = serde_json::from_str::<Value>(json) {
                        if v["content_block"]["type"] == "tool_use" {
                            starts.push(v);
                        }
                    }
                }
            } else if block.starts_with("content_block_delta") {
                if let Some(data) = block.split_once("data: ").map(|(_, rest)| rest) {
                    let json = data.split("\n\n").next().unwrap_or(data).trim();
                    if let Ok(v) = serde_json::from_str::<Value>(json) {
                        if v["delta"]["type"] == "input_json_delta" {
                            deltas.push(v);
                        }
                    }
                }
            } else if block.starts_with("content_block_stop") {
                stops += 1;
            }
        }
        assert_eq!(starts.len(), 2, "应有两个 tool_use start,实际: {s}");
        assert_eq!(starts[0]["content_block"]["id"], "call_weather");
        assert_eq!(starts[0]["content_block"]["name"], "get_weather");
        assert_eq!(starts[1]["content_block"]["id"], "call_time");
        assert_eq!(starts[1]["content_block"]["name"], "get_time");
        assert_eq!(deltas.len(), 2, "应有两个 input_json_delta,实际: {s}");
        let first: Value =
            serde_json::from_str(deltas[0]["delta"]["partial_json"].as_str().unwrap()).unwrap();
        let second: Value =
            serde_json::from_str(deltas[1]["delta"]["partial_json"].as_str().unwrap()).unwrap();
        assert_eq!(first["city"], "Paris");
        assert_eq!(second["tz"], "UTC");
        assert_eq!(stops, 2, "应有两个 content_block_stop,实际: {s}");
        assert!(s.contains("\"stop_reason\":\"tool_use\""), "实际: {s}");
    }

    #[test]
    fn test_tool_call_single_quote_arguments_fixed() {
        // 上游增量输出单引号参数(非标准 JSON),flush 时应修复为双引号
        let mut r = ChatRelay::new(None);
        r.process(&super::super::parser::SseEvent {
            event: Some("c".into()),
            data: r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"get_weather","arguments":""}}]}}]}"#.into(),
        });
        r.process(&super::super::parser::SseEvent {
            event: Some("c".into()),
            data: r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"{'city': 'beij"}}]}}]}"#.into(),
        });
        r.process(&super::super::parser::SseEvent {
            event: Some("c".into()),
            data: r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"function":{"arguments":"ing'}"}}]}}]}"#.into(),
        });
        let out = r.process(&super::super::parser::SseEvent {
            event: Some("c".into()),
            data: r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":10,"completion_tokens":5}}"#.into(),
        });
        let joined = out
            .iter()
            .map(|b| String::from_utf8_lossy(b).into_owned())
            .collect::<Vec<_>>()
            .join("");
        assert!(
            joined.contains(r#"\"city\": \"beijing\""#),
            "单引号参数应被修复,实际: {joined}"
        );
        assert!(!joined.contains('\''), "修复后不应残留单引号");
    }

    #[test]
    fn test_reasoning_deduplication() {
        let mut r = ChatRelay::new(None);
        // 第一次发 reasoning
        let out1 = r.process(&super::super::parser::SseEvent {
            event: Some("c".into()),
            data: r#"{"id":"1","choices":[{"delta":{"reasoning_content":"thinking A"}}]}"#.into(),
        });
        assert!(out1
            .iter()
            .any(|b| String::from_utf8_lossy(b).contains("thinking A")));

        // 重复发同样的 reasoning(去重)
        let out2 = r.process(&super::super::parser::SseEvent {
            event: Some("c".into()),
            data: r#"{"choices":[{"delta":{"reasoning_content":"thinking A"}}]}"#.into(),
        });
        assert!(out2.is_empty());

        // 新 reasoning 应发出
        let out3 = r.process(&super::super::parser::SseEvent {
            event: Some("c".into()),
            data: r#"{"choices":[{"delta":{"reasoning_content":"thinking B"}}]}"#.into(),
        });
        assert!(out3
            .iter()
            .any(|b| String::from_utf8_lossy(b).contains("thinking B")));
    }

    #[test]
    fn test_reasoning_only_fallback_text() {
        // 上游只回 reasoning(如 DeepSeek 推理回合):finalize 时 thinking 转 text
        let mut r = ChatRelay::new(None);
        r.process(&super::super::parser::SseEvent {
            event: Some("c".into()),
            data: r#"{"id":"1","choices":[{"delta":{"reasoning_content":"think step 1"}}]}"#.into(),
        });
        r.process(&super::super::parser::SseEvent {
            event: Some("c".into()),
            data: r#"{"choices":[{"delta":{"reasoning_content":" think step 2"}}]}"#.into(),
        });
        let out = r.process(&super::super::parser::SseEvent {
            event: Some("c".into()),
            data: r#"{"choices":[{"delta":{},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":5}}"#.into(),
        });
        let buf = out.concat();
        let s = String::from_utf8_lossy(&buf);
        // 兜底 text 块输出拼接后的 thinking 内容
        assert!(s.contains("think step 1 think step 2"), "实际: {s}");
        let text_deltas = s.matches("text_delta").count();
        assert!(text_deltas >= 1, "应有 text_delta,实际: {s}");
        assert!(s.contains("message_stop"), "实际: {s}");
    }

    #[test]
    fn test_reasoning_only_fallback_skipped_with_text() {
        // 有 text 的正常回合:不触发兜底,text 只发一次
        let mut r = ChatRelay::new(None);
        let out1 = r.process(&super::super::parser::SseEvent {
            event: Some("c".into()),
            data: r#"{"id":"1","choices":[{"delta":{"reasoning_content":"think"}}]}"#.into(),
        });
        let buf1 = out1.concat();
        let s1 = String::from_utf8_lossy(&buf1);
        assert!(
            s1.contains("thinking_delta"),
            "thinking 应正常发出,实际: {s1}"
        );
        let out = r.process(&super::super::parser::SseEvent {
            event: Some("c".into()),
            data: r#"{"choices":[{"delta":{"content":"answer"},"finish_reason":"stop"}],"usage":{"prompt_tokens":10,"completion_tokens":5}}"#.into(),
        });
        let buf = out.concat();
        let s = String::from_utf8_lossy(&buf);
        assert!(s.contains("answer"), "实际: {s}");
        assert!(
            s.matches("text_delta").count() <= 1,
            "有 text 时不得重复输出兜底,实际: {s}"
        );
    }

    #[test]
    fn test_reasoning_only_fallback_skipped_with_tool() {
        // reasoning + 工具调用回合:不得兜底成 text(工具回合无可见文本正常)
        let mut r = ChatRelay::new(None);
        r.process(&super::super::parser::SseEvent {
            event: Some("c".into()),
            data: r#"{"id":"1","choices":[{"delta":{"reasoning_content":"think"}}]}"#.into(),
        });
        r.process(&super::super::parser::SseEvent {
            event: Some("c".into()),
            data: r#"{"choices":[{"delta":{"tool_calls":[{"index":0,"id":"call_1","function":{"name":"get_weather","arguments":"{}"}}]}}]}"#.into(),
        });
        let out = r.process(&super::super::parser::SseEvent {
            event: Some("c".into()),
            data: r#"{"choices":[{"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":10,"completion_tokens":5}}"#.into(),
        });
        let buf = out.concat();
        let s = String::from_utf8_lossy(&buf);
        assert!(!s.contains("text_delta"), "工具回合不得兜底 text,实际: {s}");
        assert!(s.contains("tool_use"), "应有 tool_use 块,实际: {s}");
    }

    #[test]
    fn test_done_marker_without_message_errors() {
        // 空流仅收到 [DONE]:没有有效消息可收尾,显式报 error。
        let mut r = ChatRelay::new(None);
        let out = r.process(&super::super::parser::SseEvent {
            event: None,
            data: "[DONE]".into(),
        });
        let buf = out.concat();
        let s = String::from_utf8_lossy(&buf);
        assert!(s.contains("event: error"), "残缺 DONE 应发 error,实际: {s}");
        assert!(
            !s.contains("message_start"),
            "残缺流不得伪造 message_start,实际: {s}"
        );
        assert!(
            !s.contains("message_stop"),
            "残缺流不得发 message_stop,实际: {s}"
        );
    }

    #[test]
    fn test_done_marker_after_content_without_finish_reason_finalizes() {
        let mut r = ChatRelay::new(None);
        r.process(&super::super::parser::SseEvent {
            event: None,
            data: r#"{"id":"1","model":"m","choices":[{"delta":{"content":"hi"}}]}"#.into(),
        });
        let out = r.process(&super::super::parser::SseEvent {
            event: None,
            data: "[DONE]".into(),
        });
        let buf = out.concat();
        let s = String::from_utf8_lossy(&buf);
        assert!(
            s.contains("event: message_delta"),
            "[DONE] 应完成消息,实际: {s}"
        );
        assert!(
            s.contains("event: message_stop"),
            "[DONE] 应停止消息,实际: {s}"
        );
        assert!(!s.contains("event: error"), "[DONE] 不应转 error,实际: {s}");
    }

    #[test]
    fn test_eof_after_content_without_terminal_errors() {
        let mut r = ChatRelay::new(None);
        r.process(&super::super::parser::SseEvent {
            event: None,
            data: r#"{"id":"1","model":"m","choices":[{"delta":{"content":"partial"}}]}"#.into(),
        });
        let out = r.finish_eof();
        let buf = out.concat();
        let s = String::from_utf8_lossy(&buf);
        assert!(s.contains("event: error"), "残缺 EOF 应发 error,实际: {s}");
        assert!(
            !s.contains("message_delta"),
            "残缺 EOF 不得发正常 message_delta,实际: {s}"
        );
        assert!(
            !s.contains("message_stop"),
            "残缺 EOF 不得发正常 message_stop,实际: {s}"
        );
    }

    #[test]
    fn test_error_terminal_ignores_followup_chunk() {
        let mut r = ChatRelay::new(None);
        r.process(&super::super::parser::SseEvent {
            event: None,
            data: "[DONE]".into(),
        });
        let out = r.process(&super::super::parser::SseEvent {
            event: None,
            data: r#"{"id":"1","model":"m","choices":[{"delta":{"content":"late"}}]}"#.into(),
        });
        assert!(out.is_empty(), "error 后续 chunk 应忽略,实际: {out:?}");
    }

    #[test]
    fn test_top_level_error_is_forwarded() {
        let mut r = ChatRelay::new(None);
        let out = r.process(&super::super::parser::SseEvent {
            event: None,
            data: r#"{"error":{"message":"upstream overloaded"}}"#.into(),
        });
        let buf = out.concat();
        let s = String::from_utf8_lossy(&buf);
        assert!(
            s.starts_with("event: error"),
            "顶层 error 应立即转发,实际: {s}"
        );
        assert!(
            s.contains("upstream overloaded"),
            "应保留上游错误消息,实际: {s}"
        );
    }

    #[test]
    fn test_done_marker_after_finish_reason_stops() {
        // 正常完成:finish_reason 已由 process 收尾,后续 [DONE] 幂等为空
        let mut r = ChatRelay::new(None);
        r.process(&super::super::parser::SseEvent {
            event: None,
            data: r#"{"id":"1","model":"m","usage":{"prompt_tokens":1,"completion_tokens":1},
                "choices":[{"delta":{"content":"hi"},"finish_reason":"stop"}]}"#
                .into(),
        });
        let out = r.process(&super::super::parser::SseEvent {
            event: None,
            data: "[DONE]".into(),
        });
        assert!(out.is_empty(), "已收尾后 [DONE] 应为空,实际: {out:?}");
    }

    #[test]
    fn test_finish_reason_mapping() {
        assert_eq!(map_finish_reason("stop"), "end_turn");
        assert_eq!(map_finish_reason("length"), "max_tokens");
        assert_eq!(map_finish_reason("tool_calls"), "tool_use");
        assert_eq!(map_finish_reason("function_call"), "tool_use");
        assert_eq!(map_finish_reason("unknown"), "end_turn");
    }

    #[test]
    fn test_chat_relay_usage_from_choices() {
        use crate::sse::parser::SseEvent;

        let mut relay = ChatRelay::new(None);

        // 模拟 Moonshot 流式响应:usage 在 choices[0]
        let ev = SseEvent {
            event: None,
            data: r#"{"id":"chat-1","model":"moonshot-v1","choices":[{"delta":{"content":"hi"},"usage":{"prompt_tokens":100,"completion_tokens":20}}]}"#.to_string(),
        };

        relay.process(&ev);

        // 验证 usage 被正确提取
        assert_eq!(relay.usage_input, 100);
        assert_eq!(relay.usage_output, 20);
        assert!(relay.usage_seen);
    }
}
