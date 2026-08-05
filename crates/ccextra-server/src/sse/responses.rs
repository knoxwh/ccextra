// OpenAI responses API SSE → Anthropic messages SSE 状态机
//
// 对齐 CPA codex_claude_response.go:
// - reasoning_summary_text.delta 流式转 thinking_delta(思考过程可见)
// - output_item.done(reasoning)用 encrypted_content 发 signature_delta 收尾,
//   下一轮请求侧把 thinking.signature 转回 reasoning.encrypted_content,闭环
// - tool calls 仍在 completed 时批量重构(规避并发交错,产物等价)
// - usage 扣减 cached_tokens;stop_reason 走 CPA 映射表;空轮次合成空 text 块

use async_stream::stream;
use bytes::Bytes;
use futures::Stream;
use futures::StreamExt;
use serde_json::{json, Value};

use super::parser::SseParser;
use super::SseStreamPin;

/// message_start 的 model 兜底(CPA 同默认值)
const FALLBACK_MODEL: &str = "claude-opus-4-1-20250805";
/// 同一 reasoning item 多个 summary part 的分隔符(CPA codexThinkingSummaryPartSeparator)
const SUMMARY_PART_SEPARATOR: &str = "\n\n";

/// OpenAI responses → Anthropic 状态机
struct ResponsesRelay {
    message_started: bool,
    finished: bool,
    model: String,
    id: String,
    next_block_index: i64,

    // text 块
    text_open: bool,
    text_index: i64,
    has_text_delta: bool,

    // thinking 块(每个 reasoning item 一个,output_item.done 才关)
    thinking_open: bool,
    thinking_index: i64,
    /// 待收尾的 encrypted_content(signature_delta 用)
    thinking_signature: String,
    thinking_summary_seen: bool,

    // tools
    has_emitted_tool_use: bool,
}

impl ResponsesRelay {
    fn new() -> Self {
        Self {
            message_started: false,
            finished: false,
            model: String::new(),
            id: String::new(),
            next_block_index: 0,
            text_open: false,
            text_index: -1,
            has_text_delta: false,
            thinking_open: false,
            thinking_index: -1,
            thinking_signature: String::new(),
            thinking_summary_seen: false,
            has_emitted_tool_use: false,
        }
    }

    /// 处理一个 SSE 事件,产出 anthropic 字节事件
    fn process(&mut self, ev: &super::parser::SseEvent) -> Vec<Bytes> {
        let root: Value = match serde_json::from_str(&ev.data) {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        };
        let event_type = root.get("type").and_then(|v| v.as_str()).unwrap_or("");

        match event_type {
            "error" => vec![stream_error_frame(&root)],
            "response.created" => {
                self.update_identity(root.get("response"));
                self.ensure_started()
            }
            "response.reasoning_summary_part.added" => {
                let mut out = self.stop_text();
                // Codex 一个 reasoning item 拆多个 summary part,块保持打开,
                // part 之间空行分隔,signature 只在 output_item.done 发一次
                if self.thinking_open {
                    out.extend(self.thinking_delta(SUMMARY_PART_SEPARATOR));
                } else {
                    out.extend(self.start_thinking());
                }
                self.thinking_summary_seen = true;
                out
            }
            "response.reasoning_summary_text.delta" => {
                let delta = root.get("delta").and_then(|v| v.as_str()).unwrap_or("");
                let mut out = self.stop_text();
                out.extend(self.start_thinking());
                out.extend(self.thinking_delta(delta));
                out
            }
            // 不关 thinking 块:等 output_item.done 带最终 encrypted_content
            "response.reasoning_summary_part.done" => Vec::new(),
            "response.content_part.added" => {
                let mut out = self.finalize_thinking();
                if root.pointer("/part/type").and_then(|v| v.as_str()) == Some("output_text") {
                    out.extend(self.start_text());
                }
                out
            }
            "response.output_text.delta" => {
                let delta = root.get("delta").and_then(|v| v.as_str()).unwrap_or("");
                if delta.is_empty() {
                    return Vec::new();
                }
                self.has_text_delta = true;
                let mut out = self.finalize_thinking();
                out.extend(self.start_text());
                out.extend(self.text_delta(delta));
                out
            }
            "response.content_part.done" => {
                if root.pointer("/part/type").and_then(|v| v.as_str()) == Some("output_text") {
                    self.stop_text()
                } else {
                    Vec::new()
                }
            }
            "response.output_item.added" => {
                let item = root.get("item");
                let item_type = item
                    .and_then(|i| i.get("type"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if item_type != "reasoning" {
                    return Vec::new();
                }
                let mut out = self.stop_text();
                // 上一个没 done 的 reasoning item 不得泄漏未关块
                out.extend(self.finalize_thinking());
                self.thinking_summary_seen = false;
                // 兜底快照:仅当 output_item.done 不带 encrypted_content 时用
                self.thinking_signature = item
                    .and_then(|i| i.get("encrypted_content"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                out
            }
            "response.output_item.done" => self.output_item_done(&root),
            "response.completed" | "response.incomplete" => {
                let response = root.get("response");
                self.update_identity(response);
                let mut out = self.finalize_thinking();
                out.extend(self.stop_text());
                out.extend(self.flush_tool_calls(response));
                out.extend(self.finalize(response));
                out
            }
            _ => Vec::new(),
        }
    }

    /// output_item.done 分派(message 文本兜底 / reasoning 收尾)
    fn output_item_done(&mut self, root: &Value) -> Vec<Bytes> {
        let item = match root.get("item") {
            Some(i) => i,
            None => return Vec::new(),
        };
        let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match item_type {
            "message" => {
                if self.has_text_delta {
                    return Vec::new();
                }
                // 无 delta 流时从 item.content 补发文本(CPA 同兜底)
                let mut text = String::new();
                if let Some(parts) = item.get("content").and_then(|v| v.as_array()) {
                    for part in parts {
                        if part.get("type").and_then(|v| v.as_str()) == Some("output_text") {
                            if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                                text.push_str(t);
                            }
                        }
                    }
                }
                if text.is_empty() {
                    return Vec::new();
                }
                let mut out = self.finalize_thinking();
                out.extend(self.start_text());
                out.extend(self.text_delta(&text));
                out.extend(self.stop_text());
                self.has_text_delta = true;
                out
            }
            "reasoning" => {
                let mut out = self.stop_text();
                if let Some(sig) = item.get("encrypted_content").and_then(|v| v.as_str()) {
                    if !sig.is_empty() {
                        self.thinking_signature = sig.to_string();
                    }
                }
                if self.thinking_summary_seen {
                    out.extend(self.finalize_thinking());
                } else {
                    out.extend(self.finalize_signature_only_thinking());
                }
                self.thinking_signature.clear();
                self.thinking_summary_seen = false;
                out
            }
            _ => Vec::new(),
        }
    }

    fn update_identity(&mut self, response: Option<&Value>) {
        if let Some(r) = response {
            if let Some(id) = r.get("id").and_then(|v| v.as_str()) {
                if !id.is_empty() {
                    self.id = id.to_string();
                }
            }
            if let Some(model) = r.get("model").and_then(|v| v.as_str()) {
                if !model.is_empty() {
                    self.model = model.to_string();
                }
            }
        }
    }

    /// 确保 message_start 已发(model 空时兜底,对齐 CPA)
    fn ensure_started(&mut self) -> Vec<Bytes> {
        if self.message_started {
            return Vec::new();
        }
        self.message_started = true;
        let model = if self.model.is_empty() {
            FALLBACK_MODEL
        } else {
            self.model.as_str()
        };
        vec![sse(
            "message_start",
            &json!({
                "type": "message_start",
                "message": {
                    "id": self.id,
                    "type": "message",
                    "role": "assistant",
                    "model": model,
                    "content": [],
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": {"input_tokens": 0, "output_tokens": 0}
                }
            }),
        )]
    }

    fn start_text(&mut self) -> Vec<Bytes> {
        if self.text_open {
            return Vec::new();
        }
        self.text_index = self.next_block_index;
        self.next_block_index += 1;
        self.text_open = true;
        vec![sse(
            "content_block_start",
            &json!({
                "type": "content_block_start",
                "index": self.text_index,
                "content_block": {"type": "text", "text": ""}
            }),
        )]
    }

    fn stop_text(&mut self) -> Vec<Bytes> {
        if !self.text_open {
            return Vec::new();
        }
        self.text_open = false;
        vec![sse(
            "content_block_stop",
            &json!({"type": "content_block_stop", "index": self.text_index}),
        )]
    }

    fn text_delta(&self, text: &str) -> Vec<Bytes> {
        vec![sse(
            "content_block_delta",
            &json!({
                "type": "content_block_delta",
                "index": self.text_index,
                "delta": {"type": "text_delta", "text": text}
            }),
        )]
    }

    fn start_thinking(&mut self) -> Vec<Bytes> {
        if self.thinking_open {
            return Vec::new();
        }
        self.thinking_index = self.next_block_index;
        self.next_block_index += 1;
        self.thinking_open = true;
        vec![sse(
            "content_block_start",
            &json!({
                "type": "content_block_start",
                "index": self.thinking_index,
                "content_block": {"type": "thinking", "thinking": ""}
            }),
        )]
    }

    fn thinking_delta(&self, text: &str) -> Vec<Bytes> {
        if text.is_empty() || !self.thinking_open {
            return Vec::new();
        }
        vec![sse(
            "content_block_delta",
            &json!({
                "type": "content_block_delta",
                "index": self.thinking_index,
                "delta": {"type": "thinking_delta", "thinking": text}
            }),
        )]
    }

    /// 关闭 thinking 块:有 signature 先发 signature_delta(加密内容回放闭环)
    fn finalize_thinking(&mut self) -> Vec<Bytes> {
        if !self.thinking_open {
            return Vec::new();
        }
        let mut out = Vec::new();
        if !self.thinking_signature.is_empty() {
            out.push(sse(
                "content_block_delta",
                &json!({
                    "type": "content_block_delta",
                    "index": self.thinking_index,
                    "delta": {"type": "signature_delta", "signature": self.thinking_signature}
                }),
            ));
        }
        out.push(sse(
            "content_block_stop",
            &json!({"type": "content_block_stop", "index": self.thinking_index}),
        ));
        self.thinking_open = false;
        out
    }

    /// 无 summary 只有 encrypted_content 的 reasoning item:开块即收尾
    fn finalize_signature_only_thinking(&mut self) -> Vec<Bytes> {
        if self.thinking_signature.is_empty() {
            return Vec::new();
        }
        let mut out = self.start_thinking();
        out.extend(self.finalize_thinking());
        out
    }

    /// 从 response.output 批量提取 function_call items → tool_use 块
    fn flush_tool_calls(&mut self, response: Option<&Value>) -> Vec<Bytes> {
        let Some(output) = response.and_then(|r| r.get("output")).and_then(|v| v.as_array())
        else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for item in output {
            let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
            if item_type != "function_call" && item_type != "tool_call" {
                continue;
            }
            // CPA:call_id 优先(codexFunctionCallID),id 兜底
            let raw_id = item
                .get("call_id")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
                .or_else(|| item.get("id").and_then(|v| v.as_str()))
                .unwrap_or("");
            let name = item.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let args_str = item.get("arguments").and_then(|v| v.as_str()).unwrap_or("");
            if raw_id.is_empty() || name.is_empty() {
                continue;
            }
            let id = sanitize_tool_id(raw_id);
            let block_index = self.next_block_index;
            self.next_block_index += 1;
            self.has_emitted_tool_use = true;
            let input = serde_json::from_str::<Value>(args_str)
                .ok()
                .filter(|v| v.is_object())
                .unwrap_or_else(|| json!({}));
            out.push(sse(
                "content_block_start",
                &json!({
                    "type": "content_block_start",
                    "index": block_index,
                    "content_block": {"type": "tool_use", "id": id, "name": name, "input": input}
                }),
            ));
            out.push(sse(
                "content_block_stop",
                &json!({"type": "content_block_stop", "index": block_index}),
            ));
        }
        out
    }

    /// 空轮次/纯思考轮次合成空 text 块
    /// (CPA synthesizeCodexEmptyTextBlock:Claude 客户端遇零块消息报
    /// "Content block not found")
    fn synthesize_empty_text_block(&mut self) -> Vec<Bytes> {
        if self.text_open || self.has_text_delta || self.has_emitted_tool_use || self.thinking_open
        {
            return Vec::new();
        }
        let mut out = self.start_text();
        out.extend(self.stop_text());
        out
    }

    /// message_delta + message_stop(usage 扣 cached,stop_reason 走 CPA 映射)
    fn finalize(&mut self, response: Option<&Value>) -> Vec<Bytes> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        let mut out = self.ensure_started();
        // synthesize 内部已含 start+stop;此处前一个 stop_text 关已开 text 块
        out.extend(self.finalize_thinking());
        out.extend(self.stop_text());
        out.extend(self.synthesize_empty_text_block());

        // usage(CPA extractResponsesUsage:cached 从 input 扣除)
        let mut input_tokens = 0;
        let mut output_tokens = 0;
        let mut cached = 0;
        if let Some(usage) = response.and_then(|r| r.get("usage")) {
            input_tokens = usage.get("input_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
            output_tokens = usage.get("output_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
            cached = usage
                .pointer("/input_tokens_details/cached_tokens")
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
            if cached > 0 {
                input_tokens = (input_tokens - cached).max(0);
            }
        }

        let stop_seq = response.and_then(stop_sequence);
        let raw_reason = response.map(codex_stop_reason).unwrap_or_default();
        let stop_reason = map_stop_reason(&raw_reason, self.has_emitted_tool_use);

        let mut event = json!({
            "type": "message_delta",
            "delta": {
                "stop_reason": stop_reason,
                "stop_sequence": stop_seq.clone().map(Value::String).unwrap_or(Value::Null)
            },
            "usage": {"input_tokens": input_tokens, "output_tokens": output_tokens}
        });
        if cached > 0 {
            event["usage"]["cache_read_input_tokens"] = json!(cached);
        }
        out.push(sse("message_delta", &event));
        out.push(sse("message_stop", &json!({"type": "message_stop"})));
        out
    }

    /// EOF 兜底:上游没发 completed 也保证完整收尾
    fn finish(&mut self) -> Vec<Bytes> {
        if self.finished {
            return Vec::new();
        }
        self.finalize(None)
    }

    /// 上游流中断:发 anthropic error 事件收尾(对齐 CPA code_handlers)
    fn stream_error(&mut self, message: &str) -> Vec<Bytes> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        let mut frames = self.ensure_started();
        frames.push(sse(
            "error",
            &json!({
                "type": "error",
                "error": {"type": "api_error", "message": message}
            }),
        ));
        frames
    }
}

/// 流内 error 事件 → anthropic error(对齐 CPA codexStreamErrorToClaudeError)
fn stream_error_frame(root: &Value) -> Bytes {
    let error = root.get("error");
    let mut err_type = error
        .and_then(|e| e.get("type"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if err_type.is_empty() {
        err_type = root
            .get("error_type")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
    }
    if err_type.is_empty() {
        err_type = "api_error".to_string();
    }
    let code = error
        .and_then(|e| e.get("code"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let mut message = error
        .and_then(|e| e.get("message"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if message.is_empty() {
        message = root
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
    }
    if message.is_empty() {
        message = code.clone();
    }
    if message.is_empty() {
        message = err_type.clone();
    }
    if code == "cyber_policy" || err_type == "invalid_request" {
        err_type = "invalid_request_error".to_string();
    }
    sse(
        "error",
        &json!({"type": "error", "error": {"type": err_type, "message": message}}),
    )
}

/// stop_reason 提取(对齐 CPA codexStopReason):
/// stop_reason > incomplete_details.reason > stop_sequence 推断
fn codex_stop_reason(r: &Value) -> String {
    if let Some(sr) = r.get("stop_reason").and_then(|v| v.as_str()) {
        if !sr.is_empty() {
            if sr == "stop" && stop_sequence(r).is_some() {
                return "stop_sequence".to_string();
            }
            return sr.to_string();
        }
    }
    if let Some(reason) = r.pointer("/incomplete_details/reason").and_then(|v| v.as_str()) {
        if !reason.is_empty() {
            return reason.to_string();
        }
    }
    if stop_sequence(r).is_some() {
        return "stop_sequence".to_string();
    }
    String::new()
}

fn stop_sequence(r: &Value) -> Option<String> {
    r.get("stop_sequence")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// stop_reason → anthropic(对齐 CPA mapCodexStopReasonToClaude)
fn map_stop_reason(stop_reason: &str, has_tool_use: bool) -> String {
    if has_tool_use {
        return "tool_use".to_string();
    }
    match stop_reason {
        "" | "stop" | "completed" => "end_turn".to_string(),
        "max_tokens" | "max_output_tokens" => "max_tokens".to_string(),
        // 无工具调用时 CPA 把 tool 类原因映射为 end_turn
        "tool_use" | "tool_calls" | "function_call" => "end_turn".to_string(),
        "content_filter" => "refusal".to_string(),
        "end_turn" | "stop_sequence" | "pause_turn" | "refusal"
        | "model_context_window_exceeded" => stop_reason.to_string(),
        _ => "end_turn".to_string(),
    }
}

/// tool id 清洗:非法字符 → _,超 64 截断
/// (对齐 CPA SanitizeClaudeToolID + shortenCodexCallIDIfNeeded)
fn sanitize_tool_id(id: &str) -> String {
    let mut out: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.len() > 64 {
        out.truncate(64);
    }
    out
}

/// 序列化一个 SSE 事件
fn sse(event: &str, data: &Value) -> Bytes {
    let mut s = String::from("event: ");
    s.push_str(event);
    s.push('\n');
    s.push_str("data: ");
    s.push_str(&data.to_string());
    s.push_str("\n\n");
    Bytes::from(s)
}

/// OpenAI responses → Anthropic SSE 状态机(realtime)
pub fn relay_responses_to_anthropic<S>(stream: S) -> SseStreamPin
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
{
    let mut stream = Box::pin(stream);
    let mut parser = SseParser::new();
    let mut relay = ResponsesRelay::new();

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
        for ev in parser.finish() {
            for out in relay.process(&ev) {
                yield Ok(out);
            }
        }
        // EOF 兜底:上游没发 completed 也保证 message_delta + message_stop
        for out in relay.finish() {
            yield Ok(out);
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sse::parser::SseEvent;

    fn ev(data: &str) -> SseEvent {
        SseEvent {
            event: None,
            data: data.into(),
        }
    }

    fn created() -> SseEvent {
        ev(r#"{"type":"response.created","response":{"id":"r1","model":"gpt-5"}}"#)
    }

    /// 从 SSE 帧提取 data JSON
    fn frame_data(frame: &Bytes) -> Value {
        let s = String::from_utf8_lossy(frame);
        let (_, rest) = s.split_once('\n').unwrap();
        serde_json::from_str(rest.strip_prefix("data: ").unwrap().trim()).unwrap()
    }

    #[test]
    fn test_responses_text_stream() {
        let mut r = ResponsesRelay::new();
        let out1 = r.process(&created());
        assert!(out1.iter().any(|b| b.starts_with(b"event: message_start")));

        let out2 = r.process(&ev(
            r#"{"type":"response.output_text.delta","delta":"hello"}"#,
        ));
        assert!(out2.iter().any(|b| b.starts_with(b"event: content_block_start")));
        assert!(out2.iter().any(|b| b.starts_with(b"event: content_block_delta")));
    }

    #[test]
    fn test_responses_completed_with_tool_call() {
        let mut r = ResponsesRelay::new();
        r.process(&created());

        let out = r.process(&ev(
            r#"{"type":"response.completed","response":{"id":"r1","output":[
                {"type":"function_call","id":"fc_1","call_id":"call_9","name":"get_weather","arguments":"{\"city\":\"beijing\"}","status":"completed"}
            ],"usage":{"input_tokens":10,"output_tokens":5}}}"#,
        ));
        assert!(out.iter().any(|b| b.starts_with(b"event: message_delta")));
        assert!(out.iter().any(|b| b.starts_with(b"event: message_stop")));
        // call_id 优先于 id(CPA codexFunctionCallID)
        let start = out
            .iter()
            .find(|b| b.starts_with(b"event: content_block_start"))
            .unwrap();
        let s = String::from_utf8_lossy(start);
        assert!(s.contains("get_weather"));
        assert!(s.contains("call_9"));
        assert!(!s.contains("fc_1"));
        // 有工具调用 → stop_reason tool_use
        let delta = out
            .iter()
            .find(|b| b.starts_with(b"event: message_delta"))
            .unwrap();
        assert!(String::from_utf8_lossy(delta).contains("tool_use"));
    }

    #[test]
    fn test_reasoning_replay_streaming_with_signature() {
        // 完整闭环:summary 流式可见 + encrypted_content 走 signature_delta
        let mut r = ResponsesRelay::new();
        r.process(&created());

        let out1 = r.process(&ev(
            r#"{"type":"response.reasoning_summary_text.delta","delta":"让我想想"}"#,
        ));
        let bufs1 = out1.concat();
        let s1 = String::from_utf8_lossy(&bufs1);
        assert!(s1.contains("content_block_start"));
        assert!(s1.contains("thinking_delta"));
        assert!(s1.contains("让我想想"));

        let out2 = r.process(&ev(
            r#"{"type":"response.output_item.done","item":{"type":"reasoning","encrypted_content":"ENC123"}}"#,
        ));
        let bufs2 = out2.concat();
        let s2 = String::from_utf8_lossy(&bufs2);
        assert!(s2.contains("signature_delta"));
        assert!(s2.contains("ENC123"));
        assert!(s2.contains("content_block_stop"));
    }

    #[test]
    fn test_reasoning_signature_only() {
        // 无 summary 的 reasoning item:开块即收尾,仍带 signature
        let mut r = ResponsesRelay::new();
        r.process(&created());
        r.process(&ev(
            r#"{"type":"response.output_item.added","item":{"type":"reasoning"}}"#,
        ));
        let out = r.process(&ev(
            r#"{"type":"response.output_item.done","item":{"type":"reasoning","encrypted_content":"ENC9"}}"#,
        ));
        let bufs = out.concat();
        let s = String::from_utf8_lossy(&bufs);
        assert!(s.contains("\"type\":\"thinking\""));
        assert!(s.contains("signature_delta"));
        assert!(s.contains("ENC9"));
    }

    #[test]
    fn test_reasoning_summary_parts_separated() {
        let mut r = ResponsesRelay::new();
        r.process(&created());
        r.process(&ev(r#"{"type":"response.reasoning_summary_part.added"}"#));
        r.process(&ev(
            r#"{"type":"response.reasoning_summary_text.delta","delta":"A"}"#,
        ));
        let out = r.process(&ev(r#"{"type":"response.reasoning_summary_part.added"}"#));
        // 第二个 part:块保持打开,发空行分隔而非新块
        let bufs = out.concat();
        let s = String::from_utf8_lossy(&bufs);
        assert!(s.contains("thinking_delta"));
        assert!(!s.contains("content_block_start"));
    }

    #[test]
    fn test_usage_subtracts_cached_tokens() {
        let mut r = ResponsesRelay::new();
        r.process(&created());
        let out = r.process(&ev(
            r#"{"type":"response.completed","response":{"id":"r1","output":[],
                "usage":{"input_tokens":100,"output_tokens":5,
                         "input_tokens_details":{"cached_tokens":80}}}}"#,
        ));
        let delta = out
            .iter()
            .find(|b| b.starts_with(b"event: message_delta"))
            .unwrap();
        let v = frame_data(delta);
        assert_eq!(v["usage"]["input_tokens"], 20);
        assert_eq!(v["usage"]["cache_read_input_tokens"], 80);
    }

    #[test]
    fn test_empty_output_synthesizes_text_block() {
        // 空轮次合成空 text 块(CPA synthesizeCodexEmptyTextBlock)
        let mut r = ResponsesRelay::new();
        let out = r.process(&ev(
            r#"{"type":"response.completed","response":{"id":"r1","output":[]}}"#,
        ));
        let bufs = out.concat();
        let s = String::from_utf8_lossy(&bufs);
        assert!(s.contains("message_stop"));
        assert!(s.contains("\"type\":\"text\""));
    }

    #[test]
    fn test_thinking_only_turn_synthesizes_text_block() {
        let mut r = ResponsesRelay::new();
        r.process(&created());
        r.process(&ev(
            r#"{"type":"response.output_item.done","item":{"type":"reasoning","encrypted_content":"E"}}"#,
        ));
        let out = r.process(&ev(
            r#"{"type":"response.completed","response":{"id":"r1","output":[]}}"#,
        ));
        let bufs = out.concat();
        let s = String::from_utf8_lossy(&bufs);
        assert!(s.contains("\"type\":\"text\""));
    }

    #[test]
    fn test_stream_error_event() {
        let mut r = ResponsesRelay::new();
        let out = r.process(&ev(
            r#"{"type":"error","error":{"type":"invalid_request","code":"cyber_policy","message":"blocked"}}"#,
        ));
        let bufs = out.concat();
        let s = String::from_utf8_lossy(&bufs);
        assert!(s.starts_with("event: error"));
        assert!(s.contains("invalid_request_error"));
        assert!(s.contains("blocked"));
    }

    #[test]
    fn test_stop_reason_mappings() {
        assert_eq!(map_stop_reason("content_filter", false), "refusal");
        assert_eq!(map_stop_reason("max_output_tokens", false), "max_tokens");
        assert_eq!(map_stop_reason("", false), "end_turn");
        assert_eq!(map_stop_reason("stop", true), "tool_use");
        assert_eq!(map_stop_reason("pause_turn", false), "pause_turn");
        let r = json!({"status":"incomplete","incomplete_details":{"reason":"max_output_tokens"}});
        assert_eq!(codex_stop_reason(&r), "max_output_tokens");
        let r = json!({"stop_reason":"stop","stop_sequence":"END"});
        assert_eq!(codex_stop_reason(&r), "stop_sequence");
    }

    #[test]
    fn test_stop_sequence_in_message_delta() {
        let mut r = ResponsesRelay::new();
        let out = r.process(&ev(
            r#"{"type":"response.completed","response":{"id":"r1","output":[],
                "stop_reason":"stop","stop_sequence":"END"}}"#,
        ));
        let delta = out
            .iter()
            .find(|b| b.starts_with(b"event: message_delta"))
            .unwrap();
        let s = String::from_utf8_lossy(delta);
        assert!(s.contains("stop_sequence"));
        assert!(s.contains("END"));
    }

    #[test]
    fn test_model_fallback() {
        let mut r = ResponsesRelay::new();
        let out = r.process(&ev(r#"{"type":"response.created","response":{"id":"r1"}}"#));
        let bufs = out.concat();
        let s = String::from_utf8_lossy(&bufs);
        assert!(s.contains(FALLBACK_MODEL));
    }

    #[test]
    fn test_message_item_text_fallback() {
        // 无 delta 流时从 output_item.done(message) 的 content 补发
        let mut r = ResponsesRelay::new();
        r.process(&created());
        let out = r.process(&ev(
            r#"{"type":"response.output_item.done","item":{"type":"message",
                "content":[{"type":"output_text","text":"补发文本"}]}}"#,
        ));
        let bufs = out.concat();
        let s = String::from_utf8_lossy(&bufs);
        assert!(s.contains("补发文本"));
        assert!(s.contains("content_block_stop"));
    }

    #[test]
    fn test_finish_eof_fallback() {
        let mut r = ResponsesRelay::new();
        r.process(&created());
        let out = r.finish();
        let bufs = out.concat();
        let s = String::from_utf8_lossy(&bufs);
        assert!(s.contains("message_delta"));
        assert!(s.contains("message_stop"));
        // 二次 finish 幂等
        assert!(r.finish().is_empty());
    }

    #[test]
    fn test_sanitize_tool_id() {
        assert_eq!(sanitize_tool_id("call_abc-123"), "call_abc-123");
        assert_eq!(sanitize_tool_id("a.b/c"), "a_b_c");
        let long = "x".repeat(100);
        assert_eq!(sanitize_tool_id(&long).len(), 64);
    }
}
