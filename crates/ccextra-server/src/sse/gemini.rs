// Gemini SSE 流 → Anthropic SSE 转换

use super::emit;
use super::parser::SseParser;
use super::SseStreamPin;
use bytes::{BufMut, Bytes, BytesMut};
use ccextra_core::convert::{
    convert_gemini_stream_chunk, finalize_gemini_stream, force_finalize_gemini_stream,
    GeminiStreamState,
};
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
        let mut finished = false;

        let input_tokens = estimated_input_tokens.unwrap_or(1);

        let mut stream = Box::pin(stream);
        while let Some(result) = stream.next().await {
            match result {
                Ok(chunk_bytes) => {
                    let events = parser.push(&chunk_bytes);

                    for event in events {
                        if finished {
                            continue;
                        }
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
                            // 对齐 CPA:[DONE] force finalize;无内容补空 text;再 message_stop
                            if message_started {
                                for ev in force_finalize_gemini_stream(&mut state) {
                                    let event_type = ev["type"].as_str().unwrap_or("");
                                    yield Ok(emit::sse(event_type, &ev));
                                }
                                if state.has_content {
                                    let stop_event = emit::sse("message_stop", &serde_json::json!({"type": "message_stop"}));
                                    yield Ok(stop_event);
                                }
                            }
                            finished = true;
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

                        // Antigravity 信封解包:{"response": {...}} → 内层;
                        // 对齐 CPA:usageMetadata 可能在信封根,补回内层
                        let chunk = match chunk.get("response") {
                            Some(inner) => {
                                let mut inner = inner.clone();
                                if inner.get("usageMetadata").is_none() {
                                    if let Some(u) = chunk.get("usageMetadata") {
                                        inner["usageMetadata"] = u.clone();
                                    }
                                }
                                inner
                            }
                            None => chunk,
                        };

                        // message_start 必须第一帧发送;id/model 取首 chunk
                        // 的 responseId/modelVersion(对齐 CPA)
                        if !message_started {
                            let msg_id = chunk
                                .get("responseId")
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            let model = chunk
                                .get("modelVersion")
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            let start_event =
                                emit::message_start(msg_id, model, input_tokens as i64, 0, true);
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

        // 流结束无 [DONE]:与 [DONE] 同 force,防双 message_stop
        if message_started && !finished {
            for ev in force_finalize_gemini_stream(&mut state) {
                let event_type = ev["type"].as_str().unwrap_or("");
                yield Ok(emit::sse(event_type, &ev));
            }
            if state.has_content {
                let stop_event = emit::sse("message_stop", &serde_json::json!({"type": "message_stop"}));
                yield Ok(stop_event);
            }
        }
    })
}
