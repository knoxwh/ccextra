// SSE 解析与响应转换
//
// - parser:   手写 SSE 解析器(跨 chunk 累积)
// - chat:     OpenAI chat SSE → Anthropic SSE 状态机
// - responses: OpenAI responses SSE → Anthropic SSE 状态机
// - emit:     无状态帧构造函数(chat/responses 共用)
// - relay:    按协议分派响应流

pub mod chat;
pub mod emit;
pub mod gemini;
pub mod non_stream;
pub mod parser;
pub mod replay_cache;
pub mod responses;

use bytes::Bytes;
use ccextra_core::route::Protocol;
use futures::Stream;
use futures::StreamExt;
use std::collections::HashMap;
use std::io;
use std::pin::Pin;
use std::sync::Arc;

/// 统一响应流类型:可 Send 的固定字节流
pub type SseStreamPin = Pin<Box<dyn Stream<Item = Result<Bytes, io::Error>> + Send>>;

/// 空闲心跳间隔与心跳帧。
/// 长 reasoning 期间上游可能几十秒不出 delta,客户端/中间 LB 会掐空闲连接;
/// 官方 API 自带兜底,转换路径须等价提供。claude 直通同样注入。
/// 帧用 SSE 注释行 `: keepalive`(对齐 sub2api openai_compact_sse_keepalive):
/// eventsource 解析层直接忽略,不进入客户端事件流,任何协议下游都可见字节。
const KEEPALIVE_INTERVAL: std::time::Duration = std::time::Duration::from_secs(10);
const KEEPALIVE_FRAME: &str = ": keepalive\n\n";

/// 空闲超时心跳:每 interval 无上游字节则发一帧注释行占位。
/// 任意真实字节(含转发帧)重置计时器;空字节块不发不重置。
pub fn with_idle_keepalive(stream: SseStreamPin) -> SseStreamPin {
    with_idle_keepalive_after(stream, KEEPALIVE_INTERVAL)
}

/// 指定间隔版本(测试可注短间隔)。间隔为 0 时禁用心跳直通。
fn with_idle_keepalive_after<S>(stream: S, interval: std::time::Duration) -> SseStreamPin
where
    S: Stream<Item = Result<Bytes, io::Error>> + Unpin + Send + 'static,
{
    if interval.is_zero() {
        let inner: SseStreamPin = Box::pin(stream);
        return inner;
    }
    Box::pin(futures::stream::unfold(
        Some((stream, None::<std::pin::Pin<Box<tokio::time::Sleep>>>)),
        move |state| async move {
            let (mut inner, mut timer) = state?;
            // 到期或数据到达即返回一帧;状态机由 unfold 重入延续
            if timer.is_none() {
                timer = Some(Box::pin(tokio::time::sleep(interval)));
            }
            let t = timer.as_mut().expect("timer 已初始化");
            tokio::select! {
                _ = t.as_mut() => {
                    // 到期无数据:发一帧注释行心跳并重新武装计时器
                    Some((
                        Ok(Bytes::from_static(KEEPALIVE_FRAME.as_bytes())),
                        Some((inner, None)),
                    ))
                }
                item = inner.next() => match item {
                    Some(Ok(bytes)) => {
                        if !bytes.is_empty() {
                            timer = None;
                        }
                        Some((Ok(bytes), Some((inner, timer))))
                    }
                    Some(Err(e)) => Some((Err(e), Some((inner, timer)))),
                    None => None,
                },
            }
        },
    ))
}

/// 按入站协议分派响应流
///
/// `estimated_input_tokens`:入站 body 的本地估算输入 token(对齐
/// ClaudeInputTokenState)。流中真实 usage 通常只出现在流尾,message_start
/// 又必须第一帧发,故用它占位,让 cc context 过程中接近真实而非跳 1;
/// 流尾 message_delta 以真实 usage 覆盖。claude 直通不经过状态机,传 None。
///
/// `tool_names`:short→original 工具名还原表(仅 responses 协议用,请求转换侧
/// 产出;stream 需 'static,内部用 Arc 共享)。
pub fn relay<S>(
    protocol: Protocol,
    stream: S,
    estimated_input_tokens: Option<usize>,
    tool_names: Option<Arc<HashMap<String, String>>>,
) -> SseStreamPin
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
{
    // 空闲心跳在分派后统一注入:所有转换路径(含 claude 直通)共享
    let inner: SseStreamPin = match protocol {
        Protocol::Claude => chat::relay_claude_passthrough(stream),
        Protocol::OpenAiChat => {
            chat::relay_openai_chat_to_anthropic(stream, estimated_input_tokens)
        }
        Protocol::OpenAiResponses => {
            responses::relay_responses_to_anthropic(stream, estimated_input_tokens, tool_names)
        }
        Protocol::Gemini => {
            gemini::relay_gemini_to_anthropic(stream, estimated_input_tokens, tool_names)
        }
        Protocol::Antigravity => {
            gemini::relay_antigravity_to_anthropic(stream, estimated_input_tokens, tool_names)
        }
    };
    with_idle_keepalive(inner)
}

/// 从 OpenAI Chat usage 提取三元组(input, output, cached),cached 已从 input 扣除。
///
/// 对齐 extractOpenAIUsage:prompt_tokens - cached (if > 0)。
pub fn extract_usage_chat(usage: &serde_json::Value) -> (i64, i64, i64) {
    let mut input = usage
        .get("prompt_tokens")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let output = usage
        .get("completion_tokens")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let cached = usage
        .pointer("/prompt_tokens_details/cached_tokens")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    if cached > 0 {
        input = (input - cached).max(0);
    }
    (input, output, cached)
}

/// 从 OpenAI Responses usage 提取三元组(input, output, cached),cached 已从 input 扣除。
///
/// 对齐 extractResponsesUsage:input_tokens - cached (if > 0)。
pub fn extract_usage_responses(usage: &serde_json::Value) -> (i64, i64, i64) {
    let mut input = usage
        .get("input_tokens")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let output = usage
        .get("output_tokens")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    let cached = usage
        .pointer("/input_tokens_details/cached_tokens")
        .and_then(|v| v.as_i64())
        .unwrap_or(0);
    if cached > 0 {
        input = (input - cached).max(0);
    }
    (input, output, cached)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 稳定产出 n 个字节块,块间 pause 间隔(模拟慢上游)
    fn slow_stream(
        n: usize,
        pause: std::time::Duration,
    ) -> Pin<Box<dyn Stream<Item = Result<Bytes, io::Error>> + Send>> {
        Box::pin(futures::stream::unfold(0usize, move |i| async move {
            if i >= n {
                return None;
            }
            tokio::time::sleep(pause).await;
            let payload = format!("d{i}");
            Some((Ok(Bytes::from(payload)), i + 1))
        }))
    }

    /// 收干流为字符串帧序列(保持顺序)
    async fn drain(mut out: SseStreamPin) -> Vec<String> {
        let mut frames = Vec::new();
        while let Some(item) = out.next().await {
            frames.push(String::from_utf8(item.unwrap().to_vec()).unwrap());
        }
        frames
    }

    #[tokio::test]
    async fn keeps_pacing_when_data_flows_faster_than_interval() {
        // 数据每 30ms 一块,心跳间隔 200ms:全程不该插心跳帧
        let out = with_idle_keepalive_after(
            slow_stream(5, std::time::Duration::from_millis(30)),
            std::time::Duration::from_millis(200),
        );
        assert_eq!(drain(out).await.join(""), "d0d1d2d3d4");
    }

    #[tokio::test]
    async fn injects_keepalive_during_silence_and_resumes_in_order() {
        // 首 5 块每 40ms,然后静默 210ms 再来一块(与间隔 100ms 无公倍
        // 撞点);静默期应插 ≥1 心跳注释行
        let first = slow_stream(5, std::time::Duration::from_millis(40));
        let second = futures::stream::once(async {
            tokio::time::sleep(std::time::Duration::from_millis(360)).await;
            Ok(Bytes::from("after"))
        });
        let merged: Pin<Box<dyn Stream<Item = Result<Bytes, io::Error>> + Send>> =
            Box::pin(futures::stream::select(first, second));
        let out = with_idle_keepalive_after(merged, std::time::Duration::from_millis(100));
        let frames = drain(out).await;
        // 静默 360ms-40ms×5=160ms 起,间隔 100ms → 至少 1 帧注释行
        let beats = frames.iter().filter(|f| f.contains(": keepalive")).count();
        assert!(beats >= 1, "静默期应插入心跳注释行: {frames:?}");
        // d4 必须先于 after(心跳只准插在空档)
        let d4 = frames.iter().position(|f| f == "d4").unwrap();
        let after = frames.iter().position(|f| f == "after").unwrap();
        assert!(d4 < after, "顺序不得被心跳打乱: {frames:?}");
    }

    #[test]
    fn test_extract_usage_chat_top_level() {
        let chunk = json!({
            "usage": {
                "prompt_tokens": 100,
                "completion_tokens": 50,
                "prompt_tokens_details": {"cached_tokens": 20}
            }
        });
        let (input, output, cached) = extract_usage_chat(&chunk["usage"]);
        assert_eq!(input, 80);  // 100 - 20
        assert_eq!(output, 50);
        assert_eq!(cached, 20);
    }

    #[test]
    fn test_extract_usage_chat_choices_fallback() {
        // Moonshot 部分版本将 usage 置于 choices[0]
        let chunk = json!({
            "choices": [{
                "delta": {"content": "hi"},
                "usage": {
                    "prompt_tokens": 150,
                    "completion_tokens": 30,
                    "prompt_tokens_details": {"cached_tokens": 50}
                }
            }]
        });
        // 注意:当前 extract_usage_chat 接收已提取的 usage 对象,
        // 双路径逻辑需在 chat.rs 的 process 方法实现
        // 此测试验证提取器本身处理 choices 内 usage 的能力
        let (input, output, cached) = extract_usage_chat(&chunk["choices"][0]["usage"]);
        assert_eq!(input, 100);  // 150 - 50
        assert_eq!(output, 30);
        assert_eq!(cached, 50);
    }
}
