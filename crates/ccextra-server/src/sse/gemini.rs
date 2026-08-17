// Gemini SSE 流 → Anthropic SSE 转换

use super::emit;
use super::parser::SseParser;
use super::SseStreamPin;
use bytes::{BufMut, Bytes, BytesMut};
use ccextra_core::convert::{convert_gemini_stream_chunk, finalize_gemini_stream, GeminiStreamState};
use futures::{Stream, StreamExt};
use std::collections::HashMap;
use std::sync::Arc;

/// Gemini SSE 流 → Anthropic SSE
///
/// Gemini 流式响应格式与 OpenAI 类似,但字段名和结构不同:
/// - data: {...} JSON 对象,包含 candidates、usageMetadata
/// - candidates[].content.parts[] 包含 text、functionCall、thought
/// - finishReason 映射: STOP→end_turn, MAX_TOKENS→max_tokens
pub fn relay_gemini_to_anthropic<S>(
    stream: S,
    estimated_input_tokens: Option<usize>,
    tool_names: Option<Arc<HashMap<String, String>>>,
) -> SseStreamPin
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
{
    let tool_map = tool_names.unwrap_or_default();

    Box::pin(async_stream::stream! {
        let mut parser = SseParser::new();
        let mut state = GeminiStreamState::default();
        let mut message_started = false;

        let input_tokens = estimated_input_tokens.unwrap_or(1);

        let mut stream = Box::pin(stream);
        while let Some(result) = stream.next().await {
            match result {
                Ok(chunk_bytes) => {
                    let events = parser.push(&chunk_bytes);

                    for event in events {
                        if event.event.as_deref() == Some("error") {
                            // 上游错误事件直接转发
                            let mut buf = BytesMut::new();
                            buf.put(b"event: error\ndata: " as &[u8]);
                            buf.put(event.data.as_bytes());
                            buf.put(b"\n\n" as &[u8]);
                            yield Ok(buf.freeze());
                            continue;
                        }

                        if event.data == "[DONE]" {
                            // 只在有内容时发送 message_stop
                            if state.has_content {
                                let stop_event = emit::sse("message_stop", &serde_json::json!({"type": "message_stop"}));
                                yield Ok(stop_event);
                            }
                            continue;
                        }

                        // 解析 Gemini chunk
                        let chunk: serde_json::Value = match serde_json::from_str(&event.data) {
                            Ok(v) => v,
                            Err(e) => {
                                tracing::warn!("Gemini SSE 解析失败: {} data={}", e, event.data);
                                continue;
                            }
                        };

                        // message_start 必须第一帧发送
                        if !message_started {
                            let start_event = emit::message_start("", "", input_tokens as i64, 0, true);
                            yield Ok(start_event);
                            message_started = true;
                        }

                        // 转换 chunk 为 Anthropic 事件
                        let anthropic_events = convert_gemini_stream_chunk(&chunk, &mut state, &tool_map);

                        for event in anthropic_events {
                            let event_type = event["type"].as_str().unwrap_or("");
                            let event_bytes = emit::sse(event_type, &event);
                            yield Ok(event_bytes);
                        }

                        // 检查是否需要发送最终事件
                        let finalize_events = finalize_gemini_stream(&chunk, &mut state);
                        for event in finalize_events {
                            let event_type = event["type"].as_str().unwrap_or("");
                            let event_bytes = emit::sse(event_type, &event);
                            yield Ok(event_bytes);
                        }
                    }
                }
                Err(e) => {
                    tracing::error!("Gemini 流读取错误: {}", e);
                    // 发送结构化错误事件
                    let error_event = serde_json::json!({
                        "type": "error",
                        "error": {
                            "type": "api_error",
                            "message": format!("上游流中断: {}", e)
                        }
                    });
                    let error_bytes = emit::sse("error", &error_event);
                    yield Ok(error_bytes);
                    break;
                }
            }
        }

        // 流结束: 只在有内容时发送 message_stop
        if message_started && state.has_content {
            let stop_event = emit::sse("message_stop", &serde_json::json!({"type": "message_stop"}));
            yield Ok(stop_event);
        }
    })
}

/// 从 Gemini usage 提取三元组 (input, output, cached)
///
/// Gemini usageMetadata 格式:
/// - promptTokenCount
/// - candidatesTokenCount
/// - cachedContentTokenCount (如果有缓存)
pub fn extract_usage_gemini(usage: &serde_json::Value) -> (i64, i64, i64) {
    let mut input = usage
        .get("promptTokenCount")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let output = usage
        .get("candidatesTokenCount")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let cached = usage
        .get("cachedContentTokenCount")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);

    if cached > 0 {
        input = (input - cached).max(0);
    }

    (input, output, cached)
}
