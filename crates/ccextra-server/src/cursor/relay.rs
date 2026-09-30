// relay.rs:Cursor sidecar SSE → Anthropic SSE 状态机 + 非流聚合
//
// sidecar 事件已归一(text_delta / thinking_delta / tool_use / usage /
// turn_end / error),此处只做 Anthropic 事件映射:
// - text_delta → text 块;thinking_delta → thinking 块
// - tool_use → content_block_start + 单次 input_json_delta + stop
// - usage → 流尾覆盖 + 正数 input_tokens 写 session token cache
// - turn_end → message_delta + message_stop 后立即结束,不等 EOF
// - error → Anthropic error 事件收尾

use std::pin::Pin;

use bytes::Bytes;
use futures::{Stream, StreamExt};
use serde_json::{json, Value};

use crate::http::error::AppError;
use crate::http::session_tokens::TokenCacheScope;
use crate::sse::emit;
use crate::sse::parser::SseParser;
use crate::sse::SseStreamPin;

/// sidecar 原始字节流(由 Response::bytes_stream() 装箱)
pub type CursorSdkStream = Pin<Box<dyn Stream<Item = Result<Bytes, reqwest::Error>> + Send>>;

/// relay 元数据:model 进 message_start;usage 解析成功后写 token cache
pub struct CursorRelayMeta {
    pub model: String,
    pub estimated_input_tokens: i64,
    pub token_scope: Option<TokenCacheScope>,
}

/// 正数 input_tokens 写入 session token cache(流式/非流共享)
pub(crate) fn record_input_tokens(scope: &TokenCacheScope, tokens: usize) {
    if let Ok(mut cache) = scope.0.lock() {
        cache.insert(scope.1.clone(), tokens);
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum BlockType {
    Text,
    Thinking,
}

struct CursorRelay {
    started: bool,
    finished: bool,
    model: String,
    id: String,
    active_block: Option<(BlockType, i64)>,
    next_block_index: i64,
    usage_input: i64,
    usage_output: i64,
    usage_cached: i64,
    usage_seen: bool,
    estimated_input: i64,
    token_scope: Option<TokenCacheScope>,
}

impl CursorRelay {
    fn new(meta: CursorRelayMeta) -> Self {
        Self {
            started: false,
            finished: false,
            model: meta.model,
            id: generate_message_id(),
            active_block: None,
            next_block_index: 0,
            usage_input: 0,
            usage_output: 0,
            usage_cached: 0,
            usage_seen: false,
            estimated_input: meta.estimated_input_tokens,
            token_scope: meta.token_scope,
        }
    }

    /// 处理一个 sidecar SSE 事件,产出 Anthropic 字节事件
    fn process(&mut self, ev: &crate::sse::parser::SseEvent) -> Vec<Bytes> {
        if self.finished {
            return Vec::new();
        }
        let Ok(root) = serde_json::from_str::<Value>(&ev.data) else {
            return Vec::new();
        };
        let event_type = root.get("type").and_then(|t| t.as_str()).unwrap_or("");
        match event_type {
            "text_delta" => {
                let text = root.get("text").and_then(|t| t.as_str()).unwrap_or("");
                if !text.is_empty() {
                    return self.emit_content_delta(BlockType::Text, text);
                }
            }
            "thinking_delta" => {
                let text = root.get("text").and_then(|t| t.as_str()).unwrap_or("");
                if !text.is_empty() {
                    return self.emit_content_delta(BlockType::Thinking, text);
                }
            }
            "tool_use" => return self.emit_tool_use(&root),
            "usage" => self.cache_usage(&root),
            "turn_end" => {
                let stop_reason = root
                    .get("stop_reason")
                    .and_then(|s| s.as_str())
                    .unwrap_or("end_turn");
                return self.finalize(stop_reason);
            }
            "error" => {
                let message = root
                    .get("message")
                    .and_then(|m| m.as_str())
                    .unwrap_or("cursor sidecar error");
                return self.stream_error(message);
            }
            _ => {}
        }
        Vec::new()
    }

    /// 缓存 usage;正数 input_tokens 写 session cache
    fn cache_usage(&mut self, root: &Value) {
        let input = root
            .get("input_tokens")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let output = root
            .get("output_tokens")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        let cached = root
            .get("cache_read_input_tokens")
            .and_then(|v| v.as_i64())
            .unwrap_or(0);
        self.usage_input = input;
        self.usage_output = output;
        self.usage_cached = cached;
        self.usage_seen = true;
        if input > 0 {
            if let Some(scope) = self.token_scope.as_ref() {
                if let Ok(tokens) = usize::try_from(input) {
                    record_input_tokens(scope, tokens);
                }
            }
        }
    }

    /// tool_use:关当前块 → start → 单次完整 input_json_delta → stop
    fn emit_tool_use(&mut self, root: &Value) -> Vec<Bytes> {
        let mut frames = self.ensure_started();
        self.close_active_block(&mut frames);
        let id = root.get("id").and_then(|v| v.as_str()).unwrap_or("");
        let name = root.get("name").and_then(|v| v.as_str()).unwrap_or("");
        let input = root.get("input").cloned().unwrap_or(json!({}));
        let index = self.next_block_index;
        self.next_block_index += 1;
        frames.push(emit::content_block_start_tool_use(index, id, name));
        frames.push(emit::content_block_delta_input_json(
            index,
            &input.to_string(),
        ));
        frames.push(emit::content_block_stop(index));
        frames
    }

    /// 统一收尾:close active + message_delta + message_stop
    fn finalize(&mut self, stop_reason: &str) -> Vec<Bytes> {
        if self.finished {
            return Vec::new();
        }
        let mut frames = self.ensure_started();
        self.close_active_block(&mut frames);
        frames.push(emit::message_delta(
            stop_reason,
            None,
            self.usage_input,
            self.usage_output,
            self.usage_cached,
            0,
            -1,
        ));
        frames.push(emit::message_stop());
        self.finished = true;
        frames
    }

    fn stream_error(&mut self, message: &str) -> Vec<Bytes> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        vec![emit::error_event(message)]
    }

    /// EOF 兜底:未见 turn_end 的残缺流转 error
    fn finish_eof(&mut self) -> Vec<Bytes> {
        if self.finished {
            return Vec::new();
        }
        self.stream_error("cursor sidecar stream ended without turn_end")
    }

    /// 确保 message_start 已发(usage 未到时用入站估算占位)
    fn ensure_started(&mut self) -> Vec<Bytes> {
        if self.started {
            return Vec::new();
        }
        self.started = true;
        if self.usage_seen && (self.usage_input > 0 || self.usage_cached > 0) {
            vec![emit::message_start(
                &self.id,
                &self.model,
                self.usage_input,
                self.usage_cached,
                false,
            )]
        } else {
            vec![emit::message_start(
                &self.id,
                &self.model,
                self.estimated_input.max(1),
                0,
                true,
            )]
        }
    }

    /// 发 content_block_delta,自动开块(切换类型先 close)
    fn emit_content_delta(&mut self, block_type: BlockType, text: &str) -> Vec<Bytes> {
        let mut frames = self.ensure_started();
        let need_open = !matches!(self.active_block, Some((current, _)) if current == block_type);
        if need_open {
            self.close_active_block(&mut frames);
            let index = self.next_block_index;
            self.next_block_index += 1;
            let start = match block_type {
                BlockType::Text => emit::content_block_start_text(index),
                BlockType::Thinking => emit::content_block_start_thinking(index),
            };
            frames.push(start);
            self.active_block = Some((block_type, index));
        }
        let (_, index) = self.active_block.expect("block just opened");
        let delta = match block_type {
            BlockType::Text => emit::content_block_delta_text(index, text),
            BlockType::Thinking => emit::content_block_delta_thinking(index, text),
        };
        frames.push(delta);
        frames
    }

    fn close_active_block(&mut self, frames: &mut Vec<Bytes>) {
        if let Some((_, index)) = self.active_block.take() {
            frames.push(emit::content_block_stop(index));
        }
    }
}

/// 随机 message id:8 字节 hex
fn generate_message_id() -> String {
    let mut bytes = [0u8; 8];
    let _ = getrandom::getrandom(&mut bytes);
    format!("msg_{}", hex::encode(bytes))
}

/// 流式 relay:sidecar SSE → Anthropic SSE
///
/// turn_end 终态事件完整下发后立即结束,不等上游 EOF
pub fn relay_cursor_sdk_to_anthropic(
    stream: CursorSdkStream,
    meta: CursorRelayMeta,
) -> SseStreamPin {
    let mut stream = stream;
    let mut parser = SseParser::new();
    let mut relay = CursorRelay::new(meta);

    Box::pin(async_stream::stream! {
        loop {
            let chunk = match crate::sse::next_upstream_chunk(&mut stream).await {
                Ok(Some(Ok(c))) => c,
                Ok(Some(Err(e))) => {
                    for out in relay.stream_error(&e.to_string()) {
                        yield Ok(out);
                    }
                    return;
                }
                Ok(None) => break,
                Err(msg) => {
                    for out in relay.stream_error(msg) {
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
            if relay.finished {
                return;
            }
        }
        for ev in parser.finish() {
            for out in relay.process(&ev) {
                yield Ok(out);
            }
        }
        for out in relay.finish_eof() {
            yield Ok(out);
        }
    })
}

/// 非流聚合:sidecar SSE → Anthropic message JSON
///
/// 聚合 text / thinking / tool_use / usage / terminal;error 事件与
/// 缺失 turn_end 的残缺流都返回 AppError
pub async fn collect_cursor_sdk_response(stream: CursorSdkStream) -> Result<Value, AppError> {
    let mut stream = stream;
    let mut parser = SseParser::new();
    let mut content: Vec<Value> = Vec::new();
    let mut text_parts: Vec<String> = Vec::new();
    let mut thinking_parts: Vec<String> = Vec::new();
    let mut usage = json!({"input_tokens": 0, "output_tokens": 0});
    let mut stop_reason = None;
    let id = generate_message_id();

    loop {
        let chunk = match stream.next().await {
            Some(Ok(c)) => c,
            Some(Err(e)) => {
                return Err(AppError::with_status(
                    axum::http::StatusCode::BAD_GATEWAY,
                    format!("cursor sidecar stream failed: {e}"),
                ));
            }
            None => break,
        };
        for ev in parser.push(&chunk) {
            let Ok(root) = serde_json::from_str::<Value>(&ev.data) else {
                continue;
            };
            match root.get("type").and_then(|t| t.as_str()).unwrap_or("") {
                "text_delta" => {
                    if let Some(text) = root.get("text").and_then(|t| t.as_str()) {
                        if !text.is_empty() {
                            text_parts.push(text.to_string());
                        }
                    }
                }
                "thinking_delta" => {
                    if let Some(text) = root.get("text").and_then(|t| t.as_str()) {
                        if !text.is_empty() {
                            thinking_parts.push(text.to_string());
                        }
                    }
                }
                "tool_use" => {
                    let id = root.get("id").and_then(|v| v.as_str()).unwrap_or("");
                    let name = root.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    let input = root.get("input").cloned().unwrap_or(json!({}));
                    content.push(json!({
                        "type": "tool_use", "id": id, "name": name, "input": input,
                    }));
                }
                "usage" => {
                    usage = json!({
                        "input_tokens": root.get("input_tokens").and_then(|v| v.as_i64()).unwrap_or(0),
                        "output_tokens": root.get("output_tokens").and_then(|v| v.as_i64()).unwrap_or(0),
                    });
                    if let Some(cached) =
                        root.get("cache_read_input_tokens").and_then(|v| v.as_i64())
                    {
                        if cached > 0 {
                            usage["cache_read_input_tokens"] = json!(cached);
                        }
                    }
                }
                "turn_end" => {
                    stop_reason = Some(
                        root.get("stop_reason")
                            .and_then(|s| s.as_str())
                            .unwrap_or("end_turn")
                            .to_string(),
                    );
                }
                "error" => {
                    let message = root
                        .get("message")
                        .and_then(|m| m.as_str())
                        .unwrap_or("cursor sidecar error");
                    return Err(AppError::with_status(
                        axum::http::StatusCode::BAD_GATEWAY,
                        message.to_string(),
                    ));
                }
                _ => {}
            }
        }
        if stop_reason.is_some() {
            break;
        }
    }
    let Some(stop_reason) = stop_reason else {
        return Err(AppError::with_status(
            axum::http::StatusCode::BAD_GATEWAY,
            "cursor sidecar stream ended without turn_end".to_string(),
        ));
    };
    if !thinking_parts.is_empty() {
        content.insert(
            0,
            json!({"type": "thinking", "thinking": thinking_parts.concat()}),
        );
    }
    if !text_parts.is_empty() {
        content.insert(0, json!({"type": "text", "text": text_parts.concat()}));
    }
    Ok(json!({
        "id": id,
        "type": "message",
        "role": "assistant",
        "model": "",
        "content": content,
        "stop_reason": stop_reason,
        "stop_sequence": Value::Null,
        "usage": usage,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::StreamExt;

    fn frame_stream(frames: &[&str]) -> CursorSdkStream {
        let chunks: Vec<Result<Bytes, reqwest::Error>> = frames
            .iter()
            .map(|frame| Ok(Bytes::from(format!("data: {frame}\n\n"))))
            .collect();
        Box::pin(futures::stream::iter(chunks))
    }

    fn meta() -> CursorRelayMeta {
        CursorRelayMeta {
            model: "auto".into(),
            estimated_input_tokens: 0,
            token_scope: None,
        }
    }

    async fn drain(stream: SseStreamPin) -> String {
        let mut output = String::new();
        let mut stream = stream;
        while let Some(frame) = stream.next().await {
            output.push_str(&String::from_utf8_lossy(&frame.unwrap()));
        }
        output
    }

    #[tokio::test]
    async fn cursor_tool_event_emits_anthropic_tool_block_and_terminal() {
        let frames = [
            r#"{"type":"tool_use","id":"call-1","name":"Read","input":{"path":"a"}}"#,
            r#"{"type":"turn_end","stop_reason":"tool_use"}"#,
        ];
        let stream = frame_stream(&frames);
        let output = drain(relay_cursor_sdk_to_anthropic(stream, meta())).await;
        assert!(output.contains("\"type\":\"tool_use\""));
        assert!(output.contains("\"stop_reason\":\"tool_use\""));
        assert!(output.contains("event: message_stop"));
    }

    #[tokio::test]
    async fn text_and_thinking_keep_arrival_order() {
        let frames = [
            r#"{"type":"thinking_delta","text":"ponder"}"#,
            r#"{"type":"text_delta","text":"hello"}"#,
            r#"{"type":"text_delta","text":" world"}"#,
            r#"{"type":"turn_end","stop_reason":"end_turn"}"#,
        ];
        let stream = frame_stream(&frames);
        let output = drain(relay_cursor_sdk_to_anthropic(stream, meta())).await;
        let thinking_pos = output.find("\"thinking_delta\"").expect("thinking delta");
        let text_pos = output.find("\"text_delta\"").expect("text delta");
        assert!(thinking_pos < text_pos);
        assert!(output.contains("hello"));
        assert!(output.contains(" world"));
        assert!(output.contains("\"stop_reason\":\"end_turn\""));
    }

    #[tokio::test]
    async fn parallel_tool_uses_emit_sequential_blocks() {
        let frames = [
            r#"{"type":"tool_use","id":"call-1","name":"Read","input":{"path":"a"}}"#,
            r#"{"type":"tool_use","id":"call-2","name":"Bash","input":{"command":"ls"}}"#,
            r#"{"type":"turn_end","stop_reason":"tool_use"}"#,
        ];
        let stream = frame_stream(&frames);
        let output = drain(relay_cursor_sdk_to_anthropic(stream, meta())).await;
        assert!(output.contains("\"id\":\"call-1\""));
        assert!(output.contains("\"id\":\"call-2\""));
        assert!(output.contains("\"name\":\"Bash\""));
        // 两个块各自完整 start/delta/stop
        assert_eq!(output.matches("event: content_block_start").count(), 2);
        assert_eq!(output.matches("event: content_block_stop").count(), 2);
    }

    #[tokio::test]
    async fn usage_writes_session_token_cache() {
        use crate::http::session_tokens::SessionTokenCache;
        use std::sync::{Arc, Mutex};

        let cache = Arc::new(Mutex::new(SessionTokenCache::new()));
        let scope: TokenCacheScope = (cache.clone(), "session-1".into());
        let frames = [
            r#"{"type":"text_delta","text":"hi"}"#,
            r#"{"type":"usage","input_tokens":42,"output_tokens":7}"#,
            r#"{"type":"turn_end","stop_reason":"end_turn"}"#,
        ];
        let stream = frame_stream(&frames);
        let meta = CursorRelayMeta {
            model: "auto".into(),
            estimated_input_tokens: 0,
            token_scope: Some(scope),
        };
        let output = drain(relay_cursor_sdk_to_anthropic(stream, meta)).await;
        assert!(output.contains("\"input_tokens\":42"));
        assert_eq!(cache.lock().unwrap().get("session-1"), Some(42));
    }

    #[tokio::test]
    async fn stream_error_event_closes_with_anthropic_error() {
        let frames = [
            r#"{"type":"text_delta","text":"partial"}"#,
            r#"{"type":"error","code":"cursor_sdk_pump_failed","message":"boom"}"#,
        ];
        let stream = frame_stream(&frames);
        let output = drain(relay_cursor_sdk_to_anthropic(stream, meta())).await;
        assert!(output.contains("event: error"));
        assert!(output.contains("boom"));
        // error 后不得追加成功事件
        assert!(!output.contains("event: message_stop"));
    }

    #[tokio::test]
    async fn eof_without_turn_end_becomes_error() {
        let frames = [r#"{"type":"text_delta","text":"partial"}"#];
        let stream = frame_stream(&frames);
        let output = drain(relay_cursor_sdk_to_anthropic(stream, meta())).await;
        assert!(output.contains("event: error"));
        assert!(output.contains("without turn_end"));
    }

    #[tokio::test]
    async fn terminal_ends_stream_without_waiting_eof() {
        // turn_end 之后仍有帧:终态已收尾,后续帧必须被忽略
        let frames = [
            r#"{"type":"text_delta","text":"done"}"#,
            r#"{"type":"turn_end","stop_reason":"end_turn"}"#,
            r#"{"type":"text_delta","text":"late"}"#,
        ];
        let stream = frame_stream(&frames);
        let output = drain(relay_cursor_sdk_to_anthropic(stream, meta())).await;
        assert!(output.contains("event: message_stop"));
        assert!(!output.contains("late"));
    }

    #[tokio::test]
    async fn collect_aggregates_text_thinking_tool_usage() {
        let frames = [
            r#"{"type":"thinking_delta","text":"ponder"}"#,
            r#"{"type":"text_delta","text":"hello"}"#,
            r#"{"type":"tool_use","id":"call-1","name":"Read","input":{"path":"a"}}"#,
            r#"{"type":"usage","input_tokens":10,"output_tokens":5,"cache_read_input_tokens":3}"#,
            r#"{"type":"turn_end","stop_reason":"tool_use"}"#,
        ];
        let message = collect_cursor_sdk_response(frame_stream(&frames))
            .await
            .unwrap();
        assert_eq!(message["type"], "message");
        assert_eq!(message["role"], "assistant");
        assert_eq!(message["stop_reason"], "tool_use");
        let content = message["content"].as_array().unwrap();
        assert_eq!(content.len(), 3);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "hello");
        assert_eq!(content[1]["type"], "thinking");
        assert_eq!(content[1]["thinking"], "ponder");
        assert_eq!(content[2]["type"], "tool_use");
        assert_eq!(content[2]["id"], "call-1");
        assert_eq!(message["usage"]["input_tokens"], 10);
        assert_eq!(message["usage"]["cache_read_input_tokens"], 3);
    }

    #[tokio::test]
    async fn collect_error_event_returns_app_error() {
        let frames = [r#"{"type":"error","code":"x","message":"sidecar boom"}"#];
        let err = collect_cursor_sdk_response(frame_stream(&frames))
            .await
            .unwrap_err();
        assert!(err.err.to_string().contains("sidecar boom"));
    }

    #[tokio::test]
    async fn collect_missing_terminal_returns_app_error() {
        let frames = [r#"{"type":"text_delta","text":"partial"}"#];
        let err = collect_cursor_sdk_response(frame_stream(&frames))
            .await
            .unwrap_err();
        assert!(err.err.to_string().contains("without turn_end"));
    }
}
