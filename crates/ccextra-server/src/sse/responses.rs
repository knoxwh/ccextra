// OpenAI responses API SSE → Anthropic messages SSE 状态机
//
// 采用 ai-gateway 的简洁方案(相较 CPA 逐增量流式):
// respond 只处理 4 个事件:
//   - response.created        → message_start(身份 + usage)
//   - response.output_text.delta → text 块增量
//   - response.completed / response.incomplete → 从最终 response.output
//     一次性重构 reasoning + tool calls,发 message_delta + message_stop
//
// 精华:completed 事件自带完整 output 数组,无需逐块累积即可一次性
// 生成所有 tool_use / redacted_thinking content_block。

use async_stream::stream;
use bytes::Bytes;
use futures::Stream;
use futures::StreamExt;
use serde_json::{json, Value};

use super::parser::SseParser;
use super::SseStreamPin;

/// OpenAI responses → Anthropic 状态机
struct ResponsesRelay {
    message_started: bool,
    message_stop_sent: bool,
    text_block_index: i64,
    text_block_started: bool,
    next_block_index: i64,
    model: String,
    id: String,
}

impl ResponsesRelay {
    fn new() -> Self {
        Self {
            message_started: false,
            message_stop_sent: false,
            text_block_index: -1,
            text_block_started: false,
            next_block_index: 0,
            model: String::new(),
            id: String::new(),
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
            "response.created" => {
                let response = root.get("response");
                self.update_identity(response);
                self.ensure_started()
            }
            "response.output_text.delta" => {
                let delta = root.get("delta").and_then(|v| v.as_str()).unwrap_or("");
                if delta.is_empty() {
                    Vec::new()
                } else {
                    self.emit_text(delta)
                }
            }
            "response.completed" | "response.incomplete" => {
                let response = root.get("response");
                self.update_identity(response);
                let mut out = Vec::new();
                // 从最终 output 重构 reasoning + tool calls
                out.extend(self.emit_reasoning_blocks(response));
                out.extend(self.flush_tool_calls(response));
                out.extend(self.finalize(response));
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

    /// 确保 message_start 已发
    fn ensure_started(&mut self) -> Vec<Bytes> {
        if self.message_started {
            return Vec::new();
        }
        self.message_started = true;
        vec![sse(
            "message_start",
            &json!({
                "type": "message_start",
                "message": {
                    "id": self.id,
                    "type": "message",
                    "role": "assistant",
                    "model": self.model,
                    "content": [],
                    "stop_reason": null,
                    "stop_sequence": null,
                    "usage": {"input_tokens": 0, "output_tokens": 0}
                }
            }),
        )]
    }

    /// 发射 text 块增量(自动开块)
    fn emit_text(&mut self, delta: &str) -> Vec<Bytes> {
        let mut out = Vec::new();
        if !self.text_block_started {
            if self.text_block_index == -1 {
                self.text_block_index = self.next_block_index;
                self.next_block_index += 1;
            }
            out.push(sse(
                "content_block_start",
                &json!({
                    "type": "content_block_start",
                    "index": self.text_block_index,
                    "content_block": {"type": "text", "text": ""}
                }),
            ));
            self.text_block_started = true;
        }
        out.push(sse(
            "content_block_delta",
            &json!({
                "type": "content_block_delta",
                "index": self.text_block_index,
                "delta": {"type": "text_delta", "text": delta}
            }),
        ));
        out
    }

    /// 从 response.output 提取 reasoning items → redacted_thinking 块
    fn emit_reasoning_blocks(&mut self, response: Option<&Value>) -> Vec<Bytes> {
        let Some(output) = response.and_then(|r| r.get("output")).and_then(|v| v.as_array()) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for item in output {
            if item.get("type").and_then(|v| v.as_str()) != Some("reasoning") {
                continue;
            }
            let id = item.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let encrypted = item
                .get("encrypted_content")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if id.is_empty() || encrypted.is_empty() {
                continue;
            }
            out.extend(self.ensure_started());
            self.close_text(&mut out);
            let block_index = self.next_block_index;
            self.next_block_index += 1;
            out.push(sse(
                "content_block_start",
                &json!({
                    "type": "content_block_start",
                    "index": block_index,
                    "content_block": {
                        "type": "redacted_thinking",
                        "data": {"id": id, "encrypted_content": encrypted}
                    }
                }),
            ));
            out.push(sse(
                "content_block_stop",
                &json!({"type": "content_block_stop", "index": block_index}),
            ));
        }
        out
    }

    /// 从 response.output 提取 function_call items → tool_use 块
    fn flush_tool_calls(&mut self, response: Option<&Value>) -> Vec<Bytes> {
        let Some(output) = response.and_then(|r| r.get("output")).and_then(|v| v.as_array()) else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for item in output {
            let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
            if item_type != "function_call" && item_type != "tool_call" {
                continue;
            }
            let id = item.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let name = item
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let args_str = item
                .get("arguments")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if id.is_empty() || name.is_empty() {
                continue;
            }
            self.close_text(&mut out);
            let block_index = self.next_block_index;
            self.next_block_index += 1;
            // 解析 arguments JSON → input 对象
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

    /// 关闭 text 块 + 发 message_delta + message_stop
    fn finalize(&mut self, response: Option<&Value>) -> Vec<Bytes> {
        let mut out = Vec::new();
        self.close_text(&mut out);

        // usage
        let mut input_tokens = 0;
        let mut output_tokens = 0;
        let mut cached = 0;
        if let Some(usage) = response.and_then(|r| r.get("usage")) {
            input_tokens = usage.get("input_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
            output_tokens = usage.get("output_tokens").and_then(|v| v.as_i64()).unwrap_or(0);
            cached = usage
                .get("input_tokens_details")
                .and_then(|d| d.get("cached_tokens"))
                .and_then(|v| v.as_i64())
                .unwrap_or(0);
        }

        // finish_reason
        let stop_reason = responses_finish_reason(response);

        let mut event = json!({
            "type": "message_delta",
            "delta": {"stop_reason": stop_reason, "stop_sequence": null},
            "usage": {"input_tokens": input_tokens, "output_tokens": output_tokens}
        });
        if cached > 0 {
            event["usage"]["cache_read_input_tokens"] = json!(cached);
        }
        out.push(sse("message_delta", &event));

        if !self.message_stop_sent {
            out.push(sse("message_stop", &json!({"type": "message_stop"})));
            self.message_stop_sent = true;
        }
        out
    }

    fn close_text(&mut self, out: &mut Vec<Bytes>) {
        if !self.text_block_started {
            return;
        }
        out.push(sse(
            "content_block_stop",
            &json!({"type": "content_block_stop", "index": self.text_block_index}),
        ));
        self.text_block_started = false;
        self.text_block_index = -1;
    }
}

/// responses 的 finish_reason → anthropic stop_reason
fn responses_finish_reason(response: Option<&Value>) -> &'static str {
    let Some(r) = response else {
        return "end_turn";
    };
    // 状态 incomplete → max_tokens
    if r.get("status").and_then(|v| v.as_str()) == Some("incomplete") {
        return "max_tokens";
    }
    // 从 output items 找 finish_reason
    if let Some(output) = r.get("output").and_then(|v| v.as_array()) {
        for item in output {
            if let Some(fr) = item
                .get("finish_reason")
                .or_else(|| item.get("stop_reason"))
                .and_then(|v| v.as_str())
            {
                return match fr {
                    "tool_calls" | "function_call" | "tool_use" => "tool_use",
                    "length" | "max_tokens" => "max_tokens",
                    "stop" => "end_turn",
                    _ => "end_turn",
                };
            }
            let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
            let status = item.get("status").and_then(|v| v.as_str()).unwrap_or("");
            if matches!(item_type, "function_call" | "tool_call") && status != "incomplete" {
                return "tool_use";
            }
        }
    }
    // 顶层 finish_reason
    if let Some(fr) = r.get("finish_reason").and_then(|v| v.as_str()) {
        return match fr {
            "tool_calls" | "function_call" | "tool_use" => "tool_use",
            "length" | "max_tokens" => "max_tokens",
            _ => "end_turn",
        };
    }
    "end_turn"
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
                    yield Err(std::io::Error::other(e));
                    break;
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
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sse::parser::SseEvent;

    #[test]
    fn test_responses_text_stream() {
        let mut r = ResponsesRelay::new();
        // response.created
        let out1 = r.process(&SseEvent {
            event: Some("response.created".into()),
            data: r#"{"type":"response.created","response":{"id":"r1","model":"gpt-5"}}"#.into(),
        });
        assert!(out1.iter().any(|b| b.starts_with(b"event: message_start")));

        // output_text.delta
        let out2 = r.process(&SseEvent {
            event: Some("response.output_text.delta".into()),
            data: r#"{"type":"response.output_text.delta","delta":"hello"}"#.into(),
        });
        assert!(out2.iter().any(|b| b.starts_with(b"event: content_block_start")));
        assert!(out2.iter().any(|b| b.starts_with(b"event: content_block_delta")));
    }

    #[test]
    fn test_responses_completed_with_tool_call() {
        let mut r = ResponsesRelay::new();
        r.process(&SseEvent {
            event: Some("response.created".into()),
            data: r#"{"type":"response.created","response":{"id":"r1","model":"gpt-5"}}"#.into(),
        });

        let out = r.process(&SseEvent {
            event: Some("response.completed".into()),
            data: r#"{"type":"response.completed","response":{"id":"r1","output":[
                {"type":"function_call","id":"fc_1","name":"get_weather","arguments":"{\"city\":\"beijing\"}","status":"completed"}
            ],"usage":{"input_tokens":10,"output_tokens":5}}}"#.into(),
        });
        assert!(out.iter().any(|b| b.starts_with(b"event: content_block_start")));
        assert!(out.iter().any(|b| b.starts_with(b"event: content_block_stop")));
        assert!(out.iter().any(|b| b.starts_with(b"event: message_delta")));
        assert!(out.iter().any(|b| b.starts_with(b"event: message_stop")));
        // tool_use 顶层应带正确 name
        let delta = out.iter().find(|b| b.starts_with(b"event: content_block_start")).unwrap();
        assert!(String::from_utf8_lossy(delta).contains("get_weather"));
    }

    #[test]
    fn test_responses_finish_reason_tool_use() {
        let response = json!({"output": [{"type": "function_call", "status": "completed"}]});
        assert_eq!(responses_finish_reason(Some(&response)), "tool_use");
    }

    #[test]
    fn test_responses_incomplete_status() {
        let response = json!({"status": "incomplete"});
        assert_eq!(responses_finish_reason(Some(&response)), "max_tokens");
    }

    #[test]
    fn test_responses_finish_reason_from_top_level() {
        let response = json!({"finish_reason": "length"});
        assert_eq!(responses_finish_reason(Some(&response)), "max_tokens");
    }

    #[test]
    fn test_responses_no_response_defaults_end_turn() {
        assert_eq!(responses_finish_reason(None), "end_turn");
    }

    #[test]
    fn test_responses_empty_output() {
        let mut r = ResponsesRelay::new();
        let out = r.process(&SseEvent {
            event: Some("response.completed".into()),
            data: r#"{"type":"response.completed","response":{"id":"r1","output":[]}}"#.into(),
        });
        assert!(out.iter().any(|b| b.starts_with(b"event: message_stop")));
    }
}