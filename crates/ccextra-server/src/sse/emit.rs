// SSE 帧构造公共函数
//
// 从 chat.rs/responses.rs 提取的无状态帧构造器,消除约 200 行重复。

use bytes::Bytes;
use serde_json::{json, Value};
use std::io::Write;

/// 序列化一个 SSE 事件(Anthropic 格式)
pub fn sse(event: &str, data: &Value) -> Bytes {
    let mut buf = Vec::with_capacity(32 + 128);
    buf.extend_from_slice(b"event: ");
    buf.extend_from_slice(event.as_bytes());
    buf.extend_from_slice(b"\ndata: ");
    serde_json::to_writer(&mut buf, data).expect("序列化 SSE 数据失败");
    buf.extend_from_slice(b"\n\n");
    Bytes::from(buf)
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
    cache_write: i64,
    thinking_tokens: i64,
) -> Bytes {
    let mut event = json!({
        "type": "message_delta",
        "delta": {"stop_reason": stop_reason, "stop_sequence": stop_sequence},
        "usage": {"input_tokens": input_tokens, "output_tokens": output_tokens}
    });
    if cache_read > 0 {
        event["usage"]["cache_read_input_tokens"] = json!(cache_read);
    }
    if cache_write > 0 {
        event["usage"]["cache_creation_input_tokens"] = json!(cache_write);
    }
    // 对齐 CPA e365ab0c:thinking_tokens ≥ 0 时写入
    if thinking_tokens >= 0 {
        event["usage"]["output_tokens_details"] = json!({"thinking_tokens": thinking_tokens});
    }
    sse("message_delta", &event)
}

/// 流中断终止事件:保留 Anthropic 终态结构,不伪造 stop_reason
pub fn message_delta_incomplete() -> Bytes {
    sse(
        "message_delta",
        &json!({
            "type": "message_delta",
            "delta": {"stop_reason": null, "stop_sequence": null},
            "usage": {"output_tokens": 0}
        }),
    )
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
            "content_block": {"type": "thinking", "thinking": "", "signature": ""}
        }),
    )
}

/// content_block_start 事件(redacted_thinking 块)
pub fn content_block_start_redacted_thinking(index: i64) -> Bytes {
    sse(
        "content_block_start",
        &json!({
            "type": "content_block_start",
            "index": index,
            "content_block": {"type": "redacted_thinking", "data": ""}
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

/// 直写 content_block_delta 格式帧(热路径免去 json! 宏与 Value 分配)
#[inline]
fn emit_delta(index: i64, delta_type: &str, field_key: &str, value: &str) -> Bytes {
    let mut buf = Vec::with_capacity(96 + value.len());
    buf.extend_from_slice(b"event: content_block_delta\ndata: {\"type\":\"content_block_delta\",\"index\":");
    let _ = write!(&mut buf, "{index}");
    buf.extend_from_slice(b",\"delta\":{\"type\":\"");
    buf.extend_from_slice(delta_type.as_bytes());
    buf.extend_from_slice(b"\",\"");
    buf.extend_from_slice(field_key.as_bytes());
    buf.extend_from_slice(b"\":");
    serde_json::to_writer(&mut buf, value).expect("序列化 delta 字段失败");
    buf.extend_from_slice(b"}}\n\n");
    Bytes::from(buf)
}

/// content_block_delta 事件(text_delta)
pub fn content_block_delta_text(index: i64, text: &str) -> Bytes {
    emit_delta(index, "text_delta", "text", text)
}

/// content_block_delta 事件(thinking_delta)
pub fn content_block_delta_thinking(index: i64, thinking: &str) -> Bytes {
    emit_delta(index, "thinking_delta", "thinking", thinking)
}

/// content_block_delta 事件(signature_delta,responses 协议专用)
pub fn content_block_delta_signature(index: i64, signature: &str) -> Bytes {
    emit_delta(index, "signature_delta", "signature", signature)
}

/// content_block_delta 事件(redacted_thinking data,responses 协议专用)
pub fn content_block_delta_redacted_thinking_data(index: i64, data: &str) -> Bytes {
    emit_delta(index, "redacted_thinking_data", "data", data)
}

/// content_block_delta 事件(input_json_delta)
pub fn content_block_delta_input_json(index: i64, partial_json: &str) -> Bytes {
    emit_delta(index, "input_json_delta", "partial_json", partial_json)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_emit_delta_matches_json_serialization() {
        let text_cases = [
            "hello",
            "line1\nline2\r\nline3\t\"quoted\"",
            "Unicode: 🦀 日本語 \u{1F600}",
            r#"{"nested": "json string"}"#,
            "",
        ];

        for (i, case) in text_cases.iter().enumerate() {
            let actual = content_block_delta_text(i as i64, case);
            let expected = sse(
                "content_block_delta",
                &json!({
                    "type": "content_block_delta",
                    "index": i as i64,
                    "delta": {"type": "text_delta", "text": case}
                }),
            );
            assert_eq!(actual, expected, "case {} failed", case);
        }

        let thinking_actual = content_block_delta_thinking(0, "thought \n \"quotes\"");
        let thinking_expected = sse(
            "content_block_delta",
            &json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {"type": "thinking_delta", "thinking": "thought \n \"quotes\""}
            }),
        );
        assert_eq!(thinking_actual, thinking_expected);
    }
}

