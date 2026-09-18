// OpenAI responses API SSE → Anthropic messages SSE 状态机
//
// 对齐响应转换语义:
// - reasoning_summary_text.delta 流式转 thinking_delta(思考过程可见)
// - output_item.done(reasoning)用 encrypted_content 发 signature_delta 收尾,
//   下一轮请求侧把 thinking.signature 转回 reasoning.encrypted_content,闭环
// - function_call 完整流式状态机(对齐 codexFunctionCallStream:index 队列、
//   ActiveFunctionCall 串行、arguments delta 缓冲、并行调用 defer 防交错)
// - web_search_call → server_tool_use + web_search_tool_result
// - 工具名还原(请求侧超长名缩短,响应侧 buildReverseMap 还原原名)
// - usage 扣减 cached_tokens;stop_reason 走统一映射表;空轮次合成空 text 块

pub mod compensations;
pub mod function_call;
pub mod state_machine;
pub mod thinking;
pub mod web_search;

#[cfg(test)]
mod tests;

use async_stream::stream;
use bytes::Bytes;
use futures::Stream;
use std::collections::HashMap;
use std::sync::Arc;

use super::parser::SseParser;
use super::SseStreamPin;
use state_machine::ResponsesRelay;

// Re-exports for server crate (including non_stream.rs)
pub use compensations::{
    codex_stop_reason, is_terminal_empty_incomplete, map_stop_reason, sanitize_tool_id,
    stop_sequence,
};
pub use web_search::web_search_result_content;

/// message_start 的 model 兜底(同默认值)
const FALLBACK_MODEL: &str = "claude-opus-4-1-20250805";
/// 同一 reasoning item 多个 summary part 的分隔符(对齐 codexThinkingSummaryPartSeparator)
const SUMMARY_PART_SEPARATOR: &str = "\n\n";
/// Responses reasoning item 携带 Claude redacted_thinking 载荷的前缀
/// (对齐 ClaudeResponsesRedactedThinkingPrefix)
///
/// Responses 无 redacted reasoning 类型，Anthropic 要求 redacted_thinking 必须原样回放，
/// 故 redacted_thinking.data 骑在 encrypted_content 后带此前缀，回程侧还原。
/// 该前缀对任何提供商都不是合法签名，外部上游会丢弃而非回放无效值。
const CLAUDE_RESPONSES_REDACTED_THINKING_PREFIX: &str = "claude-redacted-thinking:";

/// OpenAI responses → Anthropic SSE 状态机(realtime)
pub fn relay_responses_to_anthropic<S>(
    stream: S,
    estimated_input_tokens: Option<usize>,
    tool_names: Option<Arc<HashMap<String, String>>>,
) -> SseStreamPin
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
{
    let mut stream = Box::pin(stream);
    let mut parser = SseParser::new();
    let mut relay = ResponsesRelay::new(estimated_input_tokens).with_tool_names(tool_names);

    Box::pin(stream! {
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
        }
        for ev in parser.finish() {
            for out in relay.process(&ev) {
                yield Ok(out);
            }
        }
        // EOF 兜底:未见 Responses 终态的残缺流由状态机转 error。
        for out in relay.finish() {
            yield Ok(out);
        }
    })
}
