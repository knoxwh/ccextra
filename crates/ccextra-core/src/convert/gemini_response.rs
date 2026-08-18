// Gemini 响应 → Anthropic 格式转换

use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use super::tool_id::claude_tool_id_for;

/// 全局工具使用 ID 计数器
static TOOL_USE_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// 响应类型状态
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum ResponseType {
    #[default]
    None = 0,
    Content = 1,
    Thinking = 2,
    Function = 3,
}

/// Gemini 流式响应状态机
#[derive(Debug, Default)]
pub struct GeminiStreamState {
    /// 当前响应类型
    pub response_type: ResponseType,
    /// 当前内容块索引
    pub response_index: usize,
    /// 是否输出过任何内容
    pub has_content: bool,
    /// 是否见过工具调用
    pub saw_tool_call: bool,
    /// 最终事件是否已发(对齐 CPA HasFinalEvents,防重复 finalize)
    pub final_events_sent: bool,
}

/// 转换 Gemini 流式事件为 Anthropic SSE 格式
///
/// 实现状态机处理内容块转换：
/// - ResponseType::None → 初始状态
/// - ResponseType::Content → 文本内容块
/// - ResponseType::Thinking → 思考内容块
/// - ResponseType::Function → 工具调用块
///
/// 状态转换时会发送 content_block_stop 和 content_block_start 事件
pub fn convert_gemini_stream_chunk(
    chunk: &Value,
    state: &mut GeminiStreamState,
    short_to_original: &HashMap<String, String>,
) -> Vec<Value> {
    let mut events = Vec::new();

    // 处理 candidates[0].content.parts
    if let Some(parts) = chunk
        .get("candidates")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("content"))
        .and_then(|c| c.get("parts"))
        .and_then(|p| p.as_array())
    {
        for part in parts {
            let text_result = part.get("text");
            let function_call_result = part.get("functionCall");
            let thought_signature = part
                .get("thoughtSignature")
                .or_else(|| part.get("thought_signature"))
                .and_then(|s| s.as_str())
                .unwrap_or("");
            let has_thought_signature = !thought_signature.is_empty();

            // 只有 thoughtSignature 的情况
            if has_thought_signature && text_result.is_none() && function_call_result.is_none() {
                if state.response_type == ResponseType::Thinking {
                    events.push(json!({
                        "type": "content_block_delta",
                        "index": state.response_index,
                        "delta": {
                            "type": "signature_delta",
                            "signature": thought_signature
                        }
                    }));
                    state.has_content = true;
                }
                continue;
            }

            // 处理文本内容
            if let Some(text) = text_result.and_then(|t| t.as_str()) {
                let is_thought = part
                    .get("thought")
                    .and_then(|t| t.as_bool())
                    .unwrap_or(false)
                    || has_thought_signature;

                if is_thought {
                    // 空文本只有签名的情况
                    if has_thought_signature && text.is_empty() {
                        if state.response_type == ResponseType::Thinking {
                            events.push(json!({
                                "type": "content_block_delta",
                                "index": state.response_index,
                                "delta": {
                                    "type": "signature_delta",
                                    "signature": thought_signature
                                }
                            }));
                            state.has_content = true;
                        }
                        continue;
                    }

                    // Thinking 内容
                    if state.response_type == ResponseType::Thinking {
                        // 继续现有 thinking 块
                        events.push(json!({
                            "type": "content_block_delta",
                            "index": state.response_index,
                            "delta": {
                                "type": "thinking_delta",
                                "thinking": text
                            }
                        }));
                        state.has_content = true;
                    } else {
                        // 状态转换到 thinking
                        if state.response_type != ResponseType::None {
                            events.push(json!({
                                "type": "content_block_stop",
                                "index": state.response_index
                            }));
                            state.response_index += 1;
                        }

                        // 开始新的 thinking 块
                        events.push(json!({
                            "type": "content_block_start",
                            "index": state.response_index,
                            "content_block": {
                                "type": "thinking",
                                "thinking": ""
                            }
                        }));
                        events.push(json!({
                            "type": "content_block_delta",
                            "index": state.response_index,
                            "delta": {
                                "type": "thinking_delta",
                                "thinking": text
                            }
                        }));
                        state.response_type = ResponseType::Thinking;
                        state.has_content = true;
                    }

                    // 发送签名
                    if has_thought_signature {
                        events.push(json!({
                            "type": "content_block_delta",
                            "index": state.response_index,
                            "delta": {
                                "type": "signature_delta",
                                "signature": thought_signature
                            }
                        }));
                    }
                } else {
                    // 普通文本内容
                    if state.response_type == ResponseType::Content {
                        // 继续现有文本块
                        events.push(json!({
                            "type": "content_block_delta",
                            "index": state.response_index,
                            "delta": {
                                "type": "text_delta",
                                "text": text
                            }
                        }));
                        state.has_content = true;
                    } else {
                        // 状态转换到文本
                        if state.response_type != ResponseType::None {
                            events.push(json!({
                                "type": "content_block_stop",
                                "index": state.response_index
                            }));
                            state.response_index += 1;
                        }

                        // 开始新的文本块
                        events.push(json!({
                            "type": "content_block_start",
                            "index": state.response_index,
                            "content_block": {
                                "type": "text",
                                "text": ""
                            }
                        }));
                        events.push(json!({
                            "type": "content_block_delta",
                            "index": state.response_index,
                            "delta": {
                                "type": "text_delta",
                                "text": text
                            }
                        }));
                        state.response_type = ResponseType::Content;
                        state.has_content = true;
                    }
                }
            } else if let Some(function_call) = function_call_result {
                // 处理工具调用
                state.saw_tool_call = true;
                let upstream_tool_name = function_call
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("");

                // 处理流式分块：名称为空表示续传
                if state.response_type == ResponseType::Function && upstream_tool_name.is_empty() {
                    if let Some(args) = function_call.get("args") {
                        events.push(json!({
                            "type": "content_block_delta",
                            "index": state.response_index,
                            "delta": {
                                "type": "input_json_delta",
                                "partial_json": args.to_string()
                            }
                        }));
                    }
                    continue;
                }

                // 关闭现有工具调用块
                if state.response_type == ResponseType::Function {
                    events.push(json!({
                        "type": "content_block_stop",
                        "index": state.response_index
                    }));
                    state.response_index += 1;
                    state.response_type = ResponseType::None;
                }

                // 关闭其他类型块
                if state.response_type != ResponseType::None {
                    events.push(json!({
                        "type": "content_block_stop",
                        "index": state.response_index
                    }));
                    state.response_index += 1;
                }

                // 还原原始工具名
                let client_tool_name = short_to_original
                    .get(upstream_tool_name)
                    .map(|s| s.as_str())
                    .unwrap_or(upstream_tool_name);

                // 生成唯一工具使用 ID(对齐 CPA:{name}-{counter},可被请求侧反解)
                let tool_use_id = claude_tool_id_for(
                    upstream_tool_name,
                    TOOL_USE_ID_COUNTER.fetch_add(1, Ordering::SeqCst),
                );

                // 开始新工具使用块
                events.push(json!({
                    "type": "content_block_start",
                    "index": state.response_index,
                    "content_block": {
                        "type": "tool_use",
                        "id": tool_use_id,
                        "name": client_tool_name,
                        "input": {}
                    }
                }));

                // 发送参数增量
                if let Some(args) = function_call.get("args") {
                    events.push(json!({
                        "type": "content_block_delta",
                        "index": state.response_index,
                        "delta": {
                            "type": "input_json_delta",
                            "partial_json": args.to_string()
                        }
                    }));
                }

                state.response_type = ResponseType::Function;
                state.has_content = true;
            }
        }
    }

    events
}

/// 处理 Gemini 响应的完成和使用统计
///
/// 当检测到 finishReason 时调用，生成 content_block_stop 和 message_delta 事件
pub fn finalize_gemini_stream(chunk: &Value, state: &mut GeminiStreamState) -> Vec<Value> {
    let mut events = Vec::new();

    // 检查是否有 finishReason
    let has_finish_reason = chunk
        .get("candidates")
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("finishReason"))
        .is_some();

    let has_usage = chunk.get("usageMetadata").is_some();

    // 只有在有 finishReason 且有内容输出时才发送最终事件;且只发一次
    // (对齐 CPA HasFinalEvents:上游可能在多个非终帧重复带 usage/finishReason)
    if has_finish_reason && has_usage && state.has_content && !state.final_events_sent {
        state.final_events_sent = true;
        // 关闭当前内容块
        if state.response_type != ResponseType::None {
            events.push(json!({
                "type": "content_block_stop",
                "index": state.response_index
            }));
            state.response_type = ResponseType::None;
        }

        // 确定 stop_reason
        let stop_reason = if state.saw_tool_call {
            "tool_use"
        } else if let Some("MAX_TOKENS") = chunk
            .get("candidates")
            .and_then(|c| c.get(0))
            .and_then(|c| c.get("finishReason"))
            .and_then(|f| f.as_str())
        {
            "max_tokens"
        } else {
            "end_turn"
        };

        // 计算 token 使用
        let usage = chunk.get("usageMetadata");
        let prompt_tokens = usage
            .and_then(|u| u.get("promptTokenCount"))
            .and_then(|t| t.as_i64())
            .unwrap_or(0);
        let candidates_tokens = usage
            .and_then(|u| u.get("candidatesTokenCount"))
            .and_then(|t| t.as_i64())
            .unwrap_or(0);
        let thoughts_tokens = usage
            .and_then(|u| u.get("thoughtsTokenCount"))
            .and_then(|t| t.as_i64())
            .unwrap_or(0);

        let total_output_tokens = candidates_tokens + thoughts_tokens;

        events.push(json!({
            "type": "message_delta",
            "delta": {
                "stop_reason": stop_reason,
                "stop_sequence": null
            },
            "usage": {
                "input_tokens": prompt_tokens,
                "output_tokens": total_output_tokens
            }
        }));
    }

    events
}

/// 转换 Gemini 非流式响应为 Anthropic 格式(对齐 CPA gemini_claude_response.go)
pub fn convert_gemini_response(
    response: &Value,
    short_to_original: &HashMap<String, String>,
) -> Value {
    let mut anthropic = json!({
        "id": "",
        "type": "message",
        "role": "assistant",
        "content": [],
        "model": "",
        "stop_reason": null,
        "stop_sequence": null,
        "usage": {
            "input_tokens": 0,
            "output_tokens": 0
        }
    });

    if let Some(response_id) = response.get("responseId").and_then(|r| r.as_str()) {
        anthropic["id"] = json!(response_id);
    }
    if let Some(model) = response.get("modelVersion").and_then(|m| m.as_str()) {
        anthropic["model"] = json!(model);
    }

    let mut saw_tool_call = false;
    if let Some(candidate) = response
        .get("candidates")
        .and_then(|c| c.as_array())
        .and_then(|c| c.first())
    {
        if let Some(parts) = candidate
            .pointer("/content/parts")
            .and_then(|p| p.as_array())
        {
            let mut content_blocks = Vec::new();
            for part in parts {
                // 文本/思考块:空文本跳过(对齐 CPA builders)
                if let Some(text) = part.get("text").and_then(|t| t.as_str()) {
                    if text.is_empty() {
                        continue;
                    }
                    let is_thought = part
                        .get("thought")
                        .and_then(|t| t.as_bool())
                        .unwrap_or(false);
                    if is_thought {
                        let mut block = json!({"type": "thinking", "thinking": text});
                        if let Some(sig) = part
                            .get("thoughtSignature")
                            .or_else(|| part.get("thought_signature"))
                            .and_then(|s| s.as_str())
                        {
                            if !sig.is_empty() {
                                block["signature"] = json!(sig);
                            }
                        }
                        content_blocks.push(block);
                    } else {
                        content_blocks.push(json!({"type": "text", "text": text}));
                    }
                }

                // 工具调用:id 自生成(对齐 CPA,不透传上游 id),name 还原原名
                if let Some(function_call) = part.get("functionCall") {
                    saw_tool_call = true;
                    let name = function_call
                        .get("name")
                        .and_then(|n| n.as_str())
                        .unwrap_or("");
                    let original_name = short_to_original
                        .get(name)
                        .map(|s| s.as_str())
                        .unwrap_or(name);
                    // args 仅接受对象,否则兜底 {}(对齐 CPA gjson.Valid && IsObject)
                    let args = match function_call.get("args") {
                        Some(a) if a.is_object() => a.clone(),
                        _ => json!({}),
                    };
                    let tool_id = claude_tool_id_for(
                        name,
                        TOOL_USE_ID_COUNTER.fetch_add(1, Ordering::SeqCst),
                    );
                    content_blocks.push(json!({
                        "type": "tool_use",
                        "id": tool_id,
                        "name": original_name,
                        "input": args
                    }));
                }
            }
            anthropic["content"] = json!(content_blocks);
        }

        // finishReason 映射:有工具调用一律 tool_use(对齐 CPA hasToolCall 优先)
        if let Some(finish_reason) = candidate.get("finishReason").and_then(|f| f.as_str()) {
            let stop_reason = if saw_tool_call {
                "tool_use"
            } else {
                match finish_reason {
                    "MAX_TOKENS" => "max_tokens",
                    _ => "end_turn",
                }
            };
            anthropic["stop_reason"] = json!(stop_reason);
        }
    }

    // 使用统计:output = candidates + thoughts(对齐 CPA)
    match response.get("usageMetadata") {
        Some(usage) => {
            let prompt_tokens = usage
                .get("promptTokenCount")
                .and_then(|t| t.as_i64())
                .unwrap_or(0);
            let candidates_tokens = usage
                .get("candidatesTokenCount")
                .and_then(|t| t.as_i64())
                .unwrap_or(0);
            let thoughts_tokens = usage
                .get("thoughtsTokenCount")
                .and_then(|t| t.as_i64())
                .unwrap_or(0);
            anthropic["usage"]["input_tokens"] = json!(prompt_tokens);
            anthropic["usage"]["output_tokens"] = json!(candidates_tokens + thoughts_tokens);
        }
        // 对齐 CPA:无 usageMetadata 且全零时删除 usage 键
        None => {
            anthropic.as_object_mut().map(|m| m.remove("usage"));
        }
    }

    anthropic
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_convert_gemini_response_basic() {
        let gemini = json!({
            "candidates": [{
                "content": {
                    "parts": [
                        {"text": "Hello, world!"}
                    ],
                    "role": "model"
                },
                "finishReason": "STOP"
            }],
            "usageMetadata": {
                "promptTokenCount": 10,
                "candidatesTokenCount": 20
            },
            "modelVersion": "gemini-2.0",
            "responseId": "resp_123"
        });

        let tool_map = HashMap::new();
        let anthropic = convert_gemini_response(&gemini, &tool_map);

        assert_eq!(anthropic["type"], "message");
        assert_eq!(anthropic["role"], "assistant");
        assert_eq!(anthropic["model"], "gemini-2.0");
        assert_eq!(anthropic["id"], "resp_123");
        assert_eq!(anthropic["stop_reason"], "end_turn");

        let content = anthropic["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "text");
        assert_eq!(content[0]["text"], "Hello, world!");

        assert_eq!(anthropic["usage"]["input_tokens"], 10);
        assert_eq!(anthropic["usage"]["output_tokens"], 20);
    }

    #[test]
    fn test_convert_gemini_response_with_tool() {
        let gemini = json!({
            "candidates": [{
                "content": {
                    "parts": [
                        {
                            "functionCall": {
                                "name": "Rd",
                                "id": "call_123",
                                "args": {
                                    "path": "/file.txt"
                                }
                            }
                        }
                    ],
                    "role": "model"
                },
                "finishReason": "STOP"
            }],
            "modelVersion": "gemini-2.0"
        });

        let mut tool_map = HashMap::new();
        tool_map.insert("Rd".to_string(), "Read".to_string());

        let anthropic = convert_gemini_response(&gemini, &tool_map);

        let content = anthropic["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "tool_use");
        assert_eq!(content[0]["name"], "Read"); // 还原了原名
                                                // 对齐 CPA:id 自生成 {name}-{counter},不透传上游 id
        let id = content[0]["id"].as_str().unwrap();
        assert!(id.starts_with("Rd-"), "id={}", id);
        assert_eq!(content[0]["input"]["path"], "/file.txt");
        // 有工具调用时 stop_reason 一律 tool_use(即使 finishReason=STOP)
        assert_eq!(anthropic["stop_reason"], "tool_use");
    }

    #[test]
    fn test_convert_gemini_response_thoughts_tokens() {
        let gemini = json!({
            "candidates": [{
                "content": {"parts": [{"text": "hi"}], "role": "model"},
                "finishReason": "STOP"
            }],
            "usageMetadata": {
                "promptTokenCount": 10,
                "candidatesTokenCount": 20,
                "thoughtsTokenCount": 30
            }
        });
        let anthropic = convert_gemini_response(&gemini, &HashMap::new());
        // output = candidates + thoughts(对齐 CPA)
        assert_eq!(anthropic["usage"]["output_tokens"], 50);
    }

    #[test]
    fn test_convert_gemini_response_no_usage_metadata() {
        let gemini = json!({
            "candidates": [{
                "content": {"parts": [{"text": "hi"}], "role": "model"},
                "finishReason": "STOP"
            }]
        });
        let anthropic = convert_gemini_response(&gemini, &HashMap::new());
        // 对齐 CPA:无 usageMetadata 时删除 usage
        assert!(anthropic.get("usage").is_none());
        assert!(anthropic.get("stop_sequence").is_some());
    }

    #[test]
    fn test_convert_gemini_response_args_non_object_fallback() {
        let gemini = json!({
            "candidates": [{
                "content": {
                    "parts": [{"functionCall": {"name": "Read", "args": "oops"}}],
                    "role": "model"
                },
                "finishReason": "STOP"
            }]
        });
        let anthropic = convert_gemini_response(&gemini, &HashMap::new());
        assert_eq!(anthropic["content"][0]["input"], json!({}));
    }

    #[test]
    fn test_finalize_gemini_stream_only_once() {
        let chunk = json!({
            "candidates": [{"finishReason": "STOP"}],
            "usageMetadata": {"promptTokenCount": 1, "candidatesTokenCount": 2}
        });
        let mut state = GeminiStreamState {
            has_content: true,
            ..Default::default()
        };
        let first = finalize_gemini_stream(&chunk, &mut state);
        assert!(!first.is_empty());
        // 第二帧重复 finishReason+usage 不再发最终事件(对齐 HasFinalEvents)
        let second = finalize_gemini_stream(&chunk, &mut state);
        assert!(second.is_empty());
    }

    #[test]
    fn test_convert_gemini_stream_chunk_text() {
        let chunk = json!({
            "candidates": [{
                "content": {
                    "parts": [
                        {"text": "Hello"}
                    ]
                }
            }]
        });

        let mut state = GeminiStreamState::default();
        let tool_map = HashMap::new();
        let events = convert_gemini_stream_chunk(&chunk, &mut state, &tool_map);

        // 新状态机：第一个块会发送 content_block_start + content_block_delta
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["type"], "content_block_start");
        assert_eq!(events[0]["content_block"]["type"], "text");
        assert_eq!(events[1]["type"], "content_block_delta");
        assert_eq!(events[1]["delta"]["type"], "text_delta");
        assert_eq!(events[1]["delta"]["text"], "Hello");
    }
}
