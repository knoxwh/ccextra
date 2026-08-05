// SSE 解析与响应转换
//
// - parser:   手写 SSE 解析器(跨 chunk 累积)
// - chat:     OpenAI chat SSE → Anthropic SSE 状态机
// - responses: OpenAI responses SSE → Anthropic SSE 状态机
// - relay:    按协议分派响应流

pub mod chat;
pub mod parser;
pub mod responses;

use bytes::Bytes;
use ccextra_core::route::Protocol;
use futures::Stream;
use std::io;
use std::pin::Pin;

/// 统一响应流类型:可 Send 的固定字节流
pub type SseStreamPin = Pin<Box<dyn Stream<Item = Result<Bytes, io::Error>> + Send>>;

/// 按入站协议分派响应流
pub fn relay<S>(
    protocol: Protocol,
    stream: S,
) -> SseStreamPin
where
    S: Stream<Item = Result<Bytes, reqwest::Error>> + Send + 'static,
{
    match protocol {
        Protocol::Claude => chat::relay_claude_passthrough(stream),
        Protocol::OpenAiChat => chat::relay_openai_chat_to_anthropic(stream),
        Protocol::OpenAiResponses => responses::relay_responses_to_anthropic(stream),
    }
}