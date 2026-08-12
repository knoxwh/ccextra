// SSE 帧构造公共函数
//
// 从 chat.rs/responses.rs 提取的无状态帧构造器,消除约 200 行重复。

use bytes::Bytes;
use serde_json::{json, Value};

/// 序列化一个 SSE 事件(Anthropic 格式)
pub fn sse(event: &str, data: &Value) -> Bytes {
    let mut s = String::from("event: ");
    s.push_str(event);
    s.push('\n');
    s.push_str("data: ");
    s.push_str(&data.to_string());
    s.push_str("\n\n");
    Bytes::from(s)
}

/// message_start 事件
///
/// `usage` 策略(对齐 chat/responses 两路径):
/// - 真实 usage 可用时传入(input 已扣 cached,output 恒 0)
/// - 否则用 estimated_input 占位(缺失时兜底 1),cache 归零
pub fn message_start(
    id: &str,
    model: &str,
    input_tokens: i64,
    cache_read: i64,
    is_estimated: bool,
) -> Bytes {
    let usage = if is_estimated {
        json!({
            "input_tokens": input_tokens,
            "output_tokens": 0,
            "cache_read_input_tokens": 0,
            "cache_creation_input_tokens": 0
        })
    } else {
        json!({
            "input_tokens": input_tokens,
            "output_tokens": 0,
            "cache_read_input_tokens": cache_read,
            "cache_creation_input_tokens": 0
        })
    };
    sse(
        "message_start",
        &json!({
            "type": "message_start",
            "message": {
                "id": id,
                "type": "message",
                "role": "assistant",
                "model": model,
                "content": [],
                "stop_reason": null,
                "stop_sequence": null,
                "usage": usage
            }
        }),
    )
}

/// message_delta 事件(流尾 usage 覆盖 + stop_reason)
pub fn message_delta(
    stop_reason: &str,
    stop_sequence: Option<&str>,
    input_tokens: i64,
    output_tokens: i64,
    cache_read: i64,
) -> Bytes {
    let mut event = json!({
        "type": "message_delta",
        "delta": {"stop_reason": stop_reason, "stop_sequence": stop_sequence},
        "usage": {"input_tokens": input_tokens, "output_tokens": output_tokens}
    });
    if cache_read > 0 {
        event["usage"]["cache_read_input_tokens"] = json!(cache_read);
    }
    sse("message_delta", &event)
}

/// message_stop 事件
pub fn message_stop() -> Bytes {
    sse("message_stop", &json!({"type": "message_stop"}))
}

/// content_block_start 事件(text 块)
pub fn content_block_start_text(index: i64) -> Bytes {
    sse(
        "content_block_start",
        &json!({
            "type": "content_block_start",
            "index": index,
            "content_block": {"type": "text", "text": ""}
        }),
    )
}

/// content_block_start 事件(thinking 块)
pub fn content_block_start_thinking(index: i64) -> Bytes {
    sse(
        "content_block_start",
        &json!({
            "type": "content_block_start",
            "index": index,
            "content_block": {"type": "thinking", "thinking": ""}
        }),
    )
}

/// content_block_start 事件(tool_use 块)
pub fn content_block_start_tool_use(index: i64, id: &str, name: &str) -> Bytes {
    sse(
        "content_block_start",
        &json!({
            "type": "content_block_start",
            "index": index,
            "content_block": {"type": "tool_use", "id": id, "name": name}
        }),
    )
}

/// content_block_start 事件(server_tool_use 块,responses 专用)
pub fn content_block_start_server_tool_use(index: i64, id: &str, name: &str) -> Bytes {
    sse(
        "content_block_start",
        &json!({
            "type": "content_block_start",
            "index": index,
            "content_block": {"type": "server_tool_use", "id": id, "name": name, "input": {}}
        }),
    )
}

/// content_block_start 事件(web_search_tool_result 块,responses 专用)
pub fn content_block_start_web_search_result(
    index: i64,
    tool_use_id: &str,
    content: &Value,
) -> Bytes {
    let mut start = json!({
        "type": "content_block_start",
        "index": index,
        "content_block": {"type": "web_search_tool_result", "tool_use_id": tool_use_id, "content": []}
    });
    if let Value::Array(arr) = content {
        if !arr.is_empty() {
            start["content_block"]["content"] = content.clone();
        }
    }
    sse("content_block_start", &start)
}

/// content_block_delta 事件(text_delta)
pub fn content_block_delta_text(index: i64, text: &str) -> Bytes {
    sse(
        "content_block_delta",
        &json!({
            "type": "content_block_delta",
            "index": index,
            "delta": {"type": "text_delta", "text": text}
        }),
    )
}

/// content_block_delta 事件(thinking_delta)
pub fn content_block_delta_thinking(index: i64, thinking: &str) -> Bytes {
    sse(
        "content_block_delta",
        &json!({
            "type": "content_block_delta",
            "index": index,
            "delta": {"type": "thinking_delta", "thinking": thinking}
        }),
    )
}

/// content_block_delta 事件(signature_delta,responses 协议专用)
pub fn content_block_delta_signature(index: i64, signature: &str) -> Bytes {
    sse(
        "content_block_delta",
        &json!({
            "type": "content_block_delta",
            "index": index,
            "delta": {"type": "signature_delta", "signature": signature}
        }),
    )
}

/// content_block_delta 事件(input_json_delta)
pub fn content_block_delta_input_json(index: i64, partial_json: &str) -> Bytes {
    sse(
        "content_block_delta",
        &json!({
            "type": "content_block_delta",
            "index": index,
            "delta": {"type": "input_json_delta", "partial_json": partial_json}
        }),
    )
}

/// content_block_stop 事件
pub fn content_block_stop(index: i64) -> Bytes {
    sse(
        "content_block_stop",
        &json!({"type": "content_block_stop", "index": index}),
    )
}

/// error 事件(流内中断)
pub fn error_event(message: &str) -> Bytes {
    sse(
        "error",
        &json!({
            "type": "error",
            "error": {"type": "api_error", "message": message}
        }),
    )
}

/// error 事件(自定义错误类型,responses 协议专用)
pub fn error_event_typed(err_type: &str, message: &str) -> Bytes {
    sse(
        "error",
        &json!({"type": "error", "error": {"type": err_type, "message": message}}),
    )
}
