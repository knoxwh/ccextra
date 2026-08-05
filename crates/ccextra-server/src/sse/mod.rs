// SSE 解析与响应转换
//
// - parser:   手写 SSE 解析器(跨 chunk 累积)
// - chat:     OpenAI chat SSE → Anthropic SSE 状态机
// - responses: OpenAI responses SSE → Anthropic SSE 状态机
// - relay:    按协议分派响应流

pub mod chat;
pub mod non_stream;
pub mod parser;
pub mod responses;

use bytes::Bytes;
use ccextra_core::route::Protocol;
use futures::Stream;
use std::collections::HashMap;
use std::io;
use std::pin::Pin;
use std::sync::Arc;

/// 统一响应流类型:可 Send 的固定字节流
pub type SseStreamPin = Pin<Box<dyn Stream<Item = Result<Bytes, io::Error>> + Send>>;

/// 按入站协议分派响应流
///
/// `estimated_input_tokens`:入站 body 的本地估算输入 token(对齐 CPA
/// ClaudeInputTokenState)。上游流未回真实 usage 时,message_start 用它填充,
/// 避免 context 记账显示 0。claude 直通不经过状态机,传 None。
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
    match protocol {
        Protocol::Claude => chat::relay_claude_passthrough(stream),
        Protocol::OpenAiChat => {
            chat::relay_openai_chat_to_anthropic(stream, estimated_input_tokens)
        }
        Protocol::OpenAiResponses => {
            responses::relay_responses_to_anthropic(stream, estimated_input_tokens, tool_names)
        }
    }
}
