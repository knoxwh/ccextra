// Gemini SSE 流 → Anthropic SSE 转换

use super::emit;
use super::parser::{SseEvent, SseParser};
use super::SseStreamPin;
use bytes::{BufMut, Bytes, BytesMut};
use ccextra_core::convert::{
    convert_gemini_stream_chunk, finalize_gemini_stream, force_finalize_gemini_stream,
    GeminiStreamState,
};
use futures::{Stream, StreamExt};
use std::collections::HashMap;
use std::sync::Arc;

#[derive(Default)]
struct GeminiRelayState {
    stream: GeminiStreamState,
    message_started: bool,
    finished: bool,
    upstream_failed: bool,
    saw_payload: bool,
    /// 仅 Antigravity 对缺失 finishReason 的 clean EOF 合成终态。
    synthesize_missing_finish: bool,
}

fn finish_gemini_relay(relay: &mut GeminiRelayState) -> Vec<Bytes> {
    let mut output = Vec::new();
    if !relay.saw_payload || !relay.message_started {
        output.push(emit::error_event(
            "upstream stream ended before response start",
        ));
        relay.upstream_failed = true;
        relay.finished = true;
        return output;
    }

    if relay.synthesize_missing_finish && !relay.stream.has_finish_reason() {
        for event in force_finalize_gemini_stream(&mut relay.stream) {
            let event_type = event["type"].as_str().unwrap_or("");
            output.push(emit::sse(event_type, &event));
        }
    }
    output.push(emit::message_stop());
    relay.finished = true;
    output
}

fn process_gemini_events(
    events: Vec<SseEvent>,
    relay: &mut GeminiRelayState,
    input_tokens: i64,
    tool_map: &HashMap<String, String>,
) -> Vec<Bytes> {
    let mut output = Vec::new();
    for event in events {
        if relay.finished {
            continue;
        }
        if event.event.as_deref() == Some("error") {
            // 上游错误事件直接转发。
            let mut buf = BytesMut::new();
            buf.put(b"event: error\ndata: " as &[u8]);
            buf.put(event.data.as_bytes());
            buf.put(b"\n\n" as &[u8]);
            output.push(buf.freeze());
            relay.upstream_failed = true;
            relay.finished = true;
            continue;
        }

        if event.data.trim() == "[DONE]" {
            output.extend(finish_gemini_relay(relay));
            continue;
        }

        let chunk: serde_json::Value = match serde_json::from_str(&event.data) {
            Ok(value) => value,
            Err(error) => {
                tracing::warn!("Gemini SSE 解析失败: {} data={}", error, event.data);
                continue;
            }
        };

        // Antigravity 信封解包;usageMetadata 可能在信封根或 cpaUsageMetadata。
        let mut chunk = match chunk.get("response") {
            Some(inner) => {
                let mut inner = inner.clone();
                if inner.get("usageMetadata").is_none() {
                    if let Some(usage) = chunk
                        .get("usageMetadata")
                        .or_else(|| chunk.get("cpaUsageMetadata"))
                    {
                        inner["usageMetadata"] = usage.clone();
                    }
                }
                inner
            }
            None => chunk,
        };
        if chunk.get("usageMetadata").is_none() {
            if let Some(usage) = chunk.get("cpaUsageMetadata").cloned() {
                chunk["usageMetadata"] = usage;
            }
        }

        // 空信封、空 candidates 不算已开始响应;usage 对象可单独作为有效 payload。
        let has_payload = chunk
            .get("candidates")
            .and_then(|c| c.as_array())
            .is_some_and(|candidates| !candidates.is_empty())
            || chunk
                .get("usageMetadata")
                .and_then(|u| u.as_object())
                .is_some_and(|usage| !usage.is_empty());
        if !has_payload {
            continue;
        }
        relay.saw_payload = true;

        // Gemini 流式响应格式与 OpenAI 类似,但字段名和结构不同:
        if !relay.message_started {
            let msg_id = chunk
                .get("responseId")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            let model = chunk
                .get("modelVersion")
                .and_then(|value| value.as_str())
                .unwrap_or("");
            output.push(emit::message_start(msg_id, model, input_tokens, 0, true));
            relay.message_started = true;
        }

        for event in convert_gemini_stream_chunk(&chunk, &mut relay.stream, tool_map) {
            let event_type = event["type"].as_str().unwrap_or("");
            output.push(emit::sse(event_type, &event));
        }
        for event in finalize_gemini_stream(&chunk, &mut relay.stream) {
            let event_type = event["type"].as_str().unwrap_or("");
            output.push(emit::sse(event_type, &event));
        }
    }
    output
}

/// Gemini SSE 流转 Anthropic SSE。
///
/// 直连 Gemini 不对缺失 finishReason 的流合成终态,仅转发 message_stop。
pub fn relay_gemini_to_anthropic<S>(
    stream: S,
    estimated_input_tokens: Option<usize>,
    tool_names: Option<Arc<HashMap<String, String>>>,
) -> SseStreamPin
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
{
    relay_gemini_like_to_anthropic(stream, estimated_input_tokens, tool_names, false)
}

/// Antigravity SSE 流转 Anthropic SSE。
///
/// 缺失 finishReason 时按 CPA 在 clean EOF/[DONE] 合成终态 chunk。
pub fn relay_antigravity_to_anthropic<S>(
    stream: S,
    estimated_input_tokens: Option<usize>,
    tool_names: Option<Arc<HashMap<String, String>>>,
) -> SseStreamPin
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
{
    relay_gemini_like_to_anthropic(stream, estimated_input_tokens, tool_names, true)
}

fn relay_gemini_like_to_anthropic<S>(
    stream: S,
    estimated_input_tokens: Option<usize>,
    tool_names: Option<Arc<HashMap<String, String>>>,
    synthesize_missing_finish: bool,
) -> SseStreamPin
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
{
    let tool_map = tool_names.unwrap_or_default();

    Box::pin(async_stream::stream! {
        let mut parser = SseParser::new();
        let mut relay = GeminiRelayState {
            synthesize_missing_finish,
            ..Default::default()
        };

        let input_tokens = estimated_input_tokens.unwrap_or(1);

        let mut stream = Box::pin(stream);
        while let Some(result) = stream.next().await {
            match result {
                Ok(chunk_bytes) => {
                    for output in process_gemini_events(
                        parser.push(&chunk_bytes),
                        &mut relay,
                        input_tokens as i64,
                        &tool_map,
                    ) {
                        yield Ok(output);
                    }
                }
                Err(e) => {
                    if relay.finished {
                        break;
                    }
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
                    relay.upstream_failed = true;
                    break;
                }
            }
        }

        // flush 没有末尾空行的 SSE 事件。
        if !relay.finished && !relay.upstream_failed {
            for output in process_gemini_events(
                parser.finish(),
                &mut relay,
                input_tokens as i64,
                &tool_map,
            ) {
                yield Ok(output);
            }
        }

        // clean EOF 可收尾;read error 或空流不得伪造成功完成。
        if !relay.finished && !relay.upstream_failed {
            for output in finish_gemini_relay(&mut relay) {
                yield Ok(output);
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::stream;

    async fn collect_output<S>(stream: S) -> String
    where
        S: Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
    {
        relay_gemini_to_anthropic(stream, None, None)
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .map(|item| String::from_utf8_lossy(&item.unwrap()).into_owned())
            .collect()
    }

    async fn collect_antigravity_output<S>(stream: S) -> String
    where
        S: Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
    {
        relay_antigravity_to_anthropic(stream, None, None)
            .collect::<Vec<_>>()
            .await
            .into_iter()
            .map(|item| String::from_utf8_lossy(&item.unwrap()).into_owned())
            .collect()
    }

    #[tokio::test]
    async fn test_empty_gemini_response_is_error() {
        let output = collect_output(stream::iter(vec![Ok(Bytes::from(
            "data: {\"response\":{}}\n\ndata: [DONE]\n\n",
        ))]))
        .await;
        assert!(output.contains("event: error"));
        assert!(!output.contains("event: message_start"));
        assert!(!output.contains("event: message_stop"));
    }

    #[tokio::test]
    async fn test_gemini_done_without_finish_does_not_synthesize_terminal() {
        let output = collect_output(stream::iter(vec![Ok(Bytes::from(
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hi\"}]} }],\"usageMetadata\":{\"promptTokenCount\":3}}\n\ndata: [DONE]\n\n",
        ))]))
        .await;
        assert_eq!(output.matches("event: message_delta").count(), 0);
        assert_eq!(output.matches("event: message_stop").count(), 1);
    }

    #[tokio::test]
    async fn test_antigravity_done_without_finish_synthesizes_terminal() {
        let output = collect_antigravity_output(stream::iter(vec![Ok(Bytes::from(
            "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hi\"}]}}],\"usageMetadata\":{\"promptTokenCount\":3}}}\n\ndata: [DONE]\n\n",
        ))]))
        .await;
        assert_eq!(output.matches("event: message_delta").count(), 1);
        assert_eq!(output.matches("event: message_stop").count(), 1);
    }

    #[tokio::test]
    async fn test_antigravity_done_with_finish_does_not_force_finalize() {
        let output = collect_antigravity_output(stream::iter(vec![Ok(Bytes::from(
            "data: {\"response\":{\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hi\"}]},\"finishReason\":\"STOP\"}]}}\n\ndata: [DONE]\n\n",
        ))]))
        .await;
        assert_eq!(output.matches("event: message_delta").count(), 0);
        assert_eq!(output.matches("event: message_stop").count(), 1);
    }

    #[tokio::test]
    async fn test_clean_eof_with_payload_is_finalized() {
        let output = collect_output(stream::iter(vec![Ok(Bytes::from(
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hi\"}]}}]}\n\n",
        ))]))
        .await;
        assert!(output.contains("event: message_start"));
        assert!(output.contains("event: message_stop"));
        assert!(!output.contains("event: error"));
    }

    #[tokio::test]
    async fn test_finish_flushes_payload_without_trailing_blank_line() {
        let output = collect_output(stream::iter(vec![Ok(Bytes::from(
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hi\"}]}}]}\n",
        ))]))
        .await;
        assert!(output.contains("event: message_start"));
        assert!(output.contains("event: message_stop"));
        assert!(!output.contains("event: error"));
    }

    #[tokio::test]
    async fn test_finish_flushes_done_without_trailing_blank_line() {
        let output = collect_output(stream::iter(vec![Ok(Bytes::from(
            "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hi\"}]}}]}\n\ndata: [DONE]\n",
        ))]))
        .await;
        assert_eq!(output.matches("event: message_stop").count(), 1);
        assert!(!output.contains("event: error"));
    }

    #[tokio::test]
    async fn test_usage_only_payload_is_finalized() {
        let output = collect_output(stream::iter(vec![Ok(Bytes::from(
            "data: {\"usageMetadata\":{\"promptTokenCount\":3}}\n\n",
        ))]))
        .await;
        assert!(output.contains("event: message_start"));
        assert!(output.contains("event: message_stop"));
        assert!(!output.contains("event: error"));
    }
    #[tokio::test]
    async fn test_read_error_does_not_finalize_successfully() {
        let request_error = reqwest::Client::new()
            .get("not a url")
            .build()
            .expect_err("invalid URL must produce reqwest error");
        let output = collect_output(stream::iter(vec![
            Ok(Bytes::from(
                "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"hi\"}]}}]}\n\n",
            )),
            Err(request_error),
        ]))
        .await;
        assert!(output.contains("event: error"));
        assert!(!output.contains("event: message_stop"));
    }
}
