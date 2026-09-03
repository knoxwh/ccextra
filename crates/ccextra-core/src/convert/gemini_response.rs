// Gemini 响应 → Anthropic 格式转换

use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use super::signature::{format_claude_signature_value, model_group};
use super::tool_id::claude_tool_id_for;

/// 全局工具使用 ID 计数器
static TOOL_USE_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// 出站 thoughtSignature 归一化。
///
/// `signature_model` 为 Some(上游模型名)时走 antigravity 路径,按 CPA
/// formatClaudeSignatureValue 处理(claude 组解回原生 E 形,其余组原样);
/// plain gemini 路径 CPA 不做归一化,传 None 原样透传。
fn outbound_signature(signature_model: Option<&str>, raw: &str) -> String {
    match signature_model {
        Some(model) => format_claude_signature_value(model, raw),
        None => raw.to_string(),
    }
}

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
    /// 是否见过 usageMetadata(对齐 CPA HasUsageMetadata)
    has_usage: bool,
    /// 是否见过 finishReason(对齐 CPA HasFinishReason)
    has_finish: bool,
    /// 流式 input = prompt - cached(对齐 CPA Params.PromptTokenCount)
    prompt_tokens: i64,
    output_tokens: i64,
    cache_read: i64,
    /// 缓存的 finishReason,供 [DONE] force 使用
    finish_reason: String,
}

impl GeminiStreamState {
    /// 是否观察到非空 finishReason(供 Antigravity [DONE] 收尾判断)。
    pub fn has_finish_reason(&self) -> bool {
        self.has_finish
    }
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
    signature_model: Option<&str>,
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
            let signature_value = outbound_signature(signature_model, thought_signature);

            // 只有 thoughtSignature 的情况
            if has_thought_signature && text_result.is_none() && function_call_result.is_none() {
                if state.response_type == ResponseType::Thinking {
                    events.push(json!({
                        "type": "content_block_delta",
                        "index": state.response_index,
                        "delta": {
                            "type": "signature_delta",
                            "signature": signature_value.as_str()
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
                                    "signature": signature_value.as_str()
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
                                "signature": signature_value.as_str()
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

fn i64_field(v: &Value, key: &str) -> i64 {
    v.get(key).and_then(|t| t.as_i64()).unwrap_or(0)
}

/// usage JSON:cached>0 才写 cache_read_input_tokens(对齐 CPA)
fn usage_object(input: i64, output: i64, cache_read: i64) -> Value {
    let mut usage = json!({"input_tokens": input, "output_tokens": output});
    if cache_read > 0 {
        usage["cache_read_input_tokens"] = json!(cache_read);
    }
    usage
}

fn cache_chunk_metadata(chunk: &Value, state: &mut GeminiStreamState) {
    if let Some(candidates) = chunk.get("candidates").and_then(|c| c.as_array()) {
        for (index, candidate) in candidates.iter().enumerate() {
            let Some(reason) = candidate
                .get("finishReason")
                .and_then(|value| value.as_str())
                .filter(|reason| !reason.is_empty())
            else {
                continue;
            };
            state.has_finish = true;
            // CPA 下游 stop_reason 只消费 candidates[0],但 Antigravity
            // [DONE] 判定需扫描所有 candidate 是否见过终态。
            if index == 0 && state.finish_reason.is_empty() {
                state.finish_reason = reason.to_string();
            }
        }
    }
    if let Some(usage) = chunk.get("usageMetadata") {
        // 流式:input = prompt - cached,负数钳 0(对齐 CPA Params.PromptTokenCount)
        let cached = i64_field(usage, "cachedContentTokenCount");
        state.has_usage = true;
        state.prompt_tokens = (i64_field(usage, "promptTokenCount") - cached).max(0);
        state.output_tokens =
            i64_field(usage, "candidatesTokenCount") + i64_field(usage, "thoughtsTokenCount");
        state.cache_read = cached;
    }
}

fn stop_reason_of(state: &GeminiStreamState) -> &'static str {
    if state.saw_tool_call {
        "tool_use"
    } else if state.finish_reason == "MAX_TOKENS" {
        "max_tokens"
    } else {
        "end_turn"
    }
}

/// 关块 + message_delta(对齐 CPA appendFinalEvents)
fn append_final_events(state: &mut GeminiStreamState, events: &mut Vec<Value>) {
    if state.final_events_sent || !state.has_content {
        return;
    }
    state.final_events_sent = true;
    if state.response_type != ResponseType::None {
        events.push(json!({
            "type": "content_block_stop",
            "index": state.response_index
        }));
        state.response_type = ResponseType::None;
    }
    events.push(json!({
        "type": "message_delta",
        "delta": {
            "stop_reason": stop_reason_of(state),
            "stop_sequence": null
        },
        "usage": usage_object(state.prompt_tokens, state.output_tokens, state.cache_read)
    }));
}

/// 处理 Gemini 响应的完成和使用统计
///
/// 非 force:finishReason + usage + 有内容才发,且只发一次
/// (对齐 CPA HasFinalEvents:上游可能在多个非终帧重复带 usage/finishReason)
pub fn finalize_gemini_stream(chunk: &Value, state: &mut GeminiStreamState) -> Vec<Value> {
    cache_chunk_metadata(chunk, state);
    let mut events = Vec::new();
    if state.has_finish && state.has_usage && state.has_content && !state.final_events_sent {
        append_final_events(state, &mut events);
    }
    events
}

/// [DONE]/EOF force 收尾(对齐 CPA appendFinalEvents(force=true)):
/// 无内容先补空 text 块;然后关块 + message_delta(缺 usage 用已缓存值,默认 0)
pub fn force_finalize_gemini_stream(state: &mut GeminiStreamState) -> Vec<Value> {
    if state.final_events_sent {
        return Vec::new();
    }
    let mut events = Vec::new();
    if !state.has_content {
        events.push(json!({
            "type": "content_block_start",
            "index": state.response_index,
            "content_block": {"type": "text", "text": ""}
        }));
        events.push(json!({
            "type": "content_block_delta",
            "index": state.response_index,
            "delta": {"type": "text_delta", "text": ""}
        }));
        state.response_type = ResponseType::Content;
        state.has_content = true;
    }
    append_final_events(state, &mut events);
    events
}

/// 转换 Gemini 非流式响应为 Anthropic 格式(对齐 CPA gemini_claude_response.go)
pub fn convert_gemini_response(
    response: &Value,
    short_to_original: &HashMap<String, String>,
    signature_model: Option<&str>,
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
                                block["signature"] =
                                    json!(outbound_signature(signature_model, sig));
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
                    let mut tool_block = json!({
                        "type": "tool_use",
                        "id": tool_id,
                        "name": original_name,
                        "input": args
                    });
                    // tool_use 签名只在 antigravity claude 组下发(对齐 CPA isClaudeTarget);
                    // 非 claude 组 CPA 把该签名挂到前置 thinking 块或独立 carrier 块,
                    // carrier 机制未移植,故此处不发。
                    if let Some(model) = signature_model.filter(|m| model_group(m) == "claude") {
                        let sig = part
                            .get("thoughtSignature")
                            .or_else(|| part.get("thought_signature"))
                            .and_then(|s| s.as_str())
                            .unwrap_or("");
                        if !sig.is_empty() {
                            tool_block["signature"] =
                                json!(format_claude_signature_value(model, sig));
                        }
                    }
                    content_blocks.push(tool_block);
                }
            }
            anthropic["content"] = json!(content_blocks);
        }

        // finishReason 缺失时按 CPA 非流转换默认补 STOP。
        let finish_reason = candidate
            .get("finishReason")
            .and_then(|f| f.as_str())
            .filter(|reason| !reason.is_empty())
            .unwrap_or("STOP");
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

    // 使用统计:非流 input = prompt - cached(负数钳 0);cached>0 写 cache_read(对齐 CPA 非流)
    // Antigravity 非流可能暂用 cpaUsageMetadata,先兼容两种字段名。
    match response
        .get("usageMetadata")
        .or_else(|| response.get("cpaUsageMetadata"))
    {
        Some(usage) => {
            let cached = i64_field(usage, "cachedContentTokenCount");
            let input = (i64_field(usage, "promptTokenCount") - cached).max(0);
            anthropic["usage"] = usage_object(
                input,
                i64_field(usage, "candidatesTokenCount") + i64_field(usage, "thoughtsTokenCount"),
                cached,
            );
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
        let anthropic = convert_gemini_response(&gemini, &tool_map, None);

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

        let anthropic = convert_gemini_response(&gemini, &tool_map, None);

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
        let anthropic = convert_gemini_response(&gemini, &HashMap::new(), None);
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
        let anthropic = convert_gemini_response(&gemini, &HashMap::new(), None);
        // 对齐 CPA:无 usageMetadata 时删除 usage
        assert!(anthropic.get("usage").is_none());
        assert!(anthropic.get("stop_sequence").is_some());
    }

    #[test]
    fn test_convert_gemini_response_accepts_cpa_usage_metadata() {
        let gemini = json!({
            "candidates": [{
                "content": {"parts": [{"text": "hi"}]}
            }],
            "cpaUsageMetadata": {
                "promptTokenCount": 4,
                "candidatesTokenCount": 2
            }
        });
        let anthropic = convert_gemini_response(&gemini, &HashMap::new(), None);
        assert_eq!(anthropic["usage"]["input_tokens"], 4);
        assert_eq!(anthropic["usage"]["output_tokens"], 2);
    }

    #[test]
    fn test_convert_gemini_response_defaults_missing_finish_reason() {
        let gemini = json!({
            "candidates": [{
                "content": {"parts": [{"text": "hi"}], "role": "model"}
            }]
        });
        let anthropic = convert_gemini_response(&gemini, &HashMap::new(), None);
        assert_eq!(anthropic["stop_reason"], "end_turn");
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
        let anthropic = convert_gemini_response(&gemini, &HashMap::new(), None);
        assert_eq!(anthropic["content"][0]["input"], json!({}));
    }

    #[test]
    fn test_finalize_gemini_stream_scans_all_candidates_for_finish_reason() {
        let chunk = json!({
            "candidates": [
                {"content": {"parts": [{"text": "hi"}]}},
                {"finishReason": "STOP"}
            ],
            "usageMetadata": {"promptTokenCount": 1, "candidatesTokenCount": 2}
        });
        let mut state = GeminiStreamState {
            has_content: true,
            ..Default::default()
        };
        let events = finalize_gemini_stream(&chunk, &mut state);
        assert!(!events.is_empty());
        assert!(state.has_finish_reason());
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
        let events = convert_gemini_stream_chunk(&chunk, &mut state, &tool_map, None);

        // 新状态机：第一个块会发送 content_block_start + content_block_delta
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["type"], "content_block_start");
        assert_eq!(events[0]["content_block"]["type"], "text");
        assert_eq!(events[1]["type"], "content_block_delta");
        assert_eq!(events[1]["delta"]["type"], "text_delta");
        assert_eq!(events[1]["delta"]["text"], "Hello");
    }

    #[test]
    fn test_convert_gemini_response_emits_cache_read() {
        // 对齐 CPA 非流:input = prompt - cached;cached>0 写 cache_read
        let gemini = json!({
            "candidates": [{
                "content": {"parts": [{"text": "hi"}], "role": "model"},
                "finishReason": "STOP"
            }],
            "usageMetadata": {
                "promptTokenCount": 100,
                "candidatesTokenCount": 20,
                "cachedContentTokenCount": 40
            }
        });
        let anthropic = convert_gemini_response(&gemini, &HashMap::new(), None);
        assert_eq!(anthropic["usage"]["input_tokens"], 60);
        assert_eq!(anthropic["usage"]["output_tokens"], 20);
        assert_eq!(anthropic["usage"]["cache_read_input_tokens"], 40);
    }

    #[test]
    fn test_convert_gemini_response_clamps_negative_input() {
        // 对齐 CPA:prompt < cached 时 input 钳 0
        let gemini = json!({
            "candidates": [{
                "content": {"parts": [{"text": "hi"}], "role": "model"},
                "finishReason": "STOP"
            }],
            "usageMetadata": {
                "promptTokenCount": 5,
                "candidatesTokenCount": 3,
                "cachedContentTokenCount": 40
            }
        });
        let anthropic = convert_gemini_response(&gemini, &HashMap::new(), None);
        assert_eq!(anthropic["usage"]["input_tokens"], 0);
        assert_eq!(anthropic["usage"]["cache_read_input_tokens"], 40);
    }

    #[test]
    fn test_convert_gemini_response_zero_cached_omits_cache_read() {
        let gemini = json!({
            "candidates": [{
                "content": {"parts": [{"text": "hi"}], "role": "model"},
                "finishReason": "STOP"
            }],
            "usageMetadata": {
                "promptTokenCount": 10,
                "candidatesTokenCount": 5,
                "cachedContentTokenCount": 0
            }
        });
        let anthropic = convert_gemini_response(&gemini, &HashMap::new(), None);
        assert_eq!(anthropic["usage"]["input_tokens"], 10);
        assert!(anthropic["usage"].get("cache_read_input_tokens").is_none());
    }

    #[test]
    fn test_finalize_gemini_stream_subtracts_cached() {
        let chunk = json!({
            "candidates": [{"finishReason": "STOP"}],
            "usageMetadata": {
                "promptTokenCount": 100,
                "candidatesTokenCount": 8,
                "thoughtsTokenCount": 2,
                "cachedContentTokenCount": 40
            }
        });
        let mut state = GeminiStreamState {
            has_content: true,
            ..Default::default()
        };
        let events = finalize_gemini_stream(&chunk, &mut state);
        let delta = events
            .iter()
            .find(|e| e["type"] == "message_delta")
            .unwrap();
        assert_eq!(delta["usage"]["input_tokens"], 60);
        assert_eq!(delta["usage"]["output_tokens"], 10);
        assert_eq!(delta["usage"]["cache_read_input_tokens"], 40);
    }

    #[test]
    fn test_force_finalize_on_done_without_usage() {
        // [DONE] force:有内容无 finish/usage 仍发 message_delta(usage 全 0)
        let mut state = GeminiStreamState {
            has_content: true,
            response_type: ResponseType::Content,
            ..Default::default()
        };
        let events = force_finalize_gemini_stream(&mut state);
        assert!(state.final_events_sent);
        assert_eq!(events[0]["type"], "content_block_stop");
        let delta = events
            .iter()
            .find(|e| e["type"] == "message_delta")
            .unwrap();
        assert_eq!(delta["delta"]["stop_reason"], "end_turn");
        assert_eq!(delta["usage"]["input_tokens"], 0);
        assert_eq!(delta["usage"]["output_tokens"], 0);
        assert!(delta["usage"].get("cache_read_input_tokens").is_none());
        assert!(force_finalize_gemini_stream(&mut state).is_empty());
    }

    #[test]
    fn test_force_finalize_synthesizes_empty_text_when_no_content() {
        let mut state = GeminiStreamState::default();
        let events = force_finalize_gemini_stream(&mut state);
        assert!(state.has_content);
        assert_eq!(events[0]["type"], "content_block_start");
        assert_eq!(events[0]["content_block"]["type"], "text");
        assert_eq!(events[0]["content_block"]["text"], "");
        assert_eq!(events[1]["type"], "content_block_delta");
        assert_eq!(events[1]["delta"]["text"], "");
        assert_eq!(events[2]["type"], "content_block_stop");
        assert_eq!(events[3]["type"], "message_delta");
    }

    #[test]
    fn test_force_finalize_uses_cached_usage() {
        // usage 在非终帧出现,force 时用缓存值
        let usage_chunk = json!({
            "usageMetadata": {
                "promptTokenCount": 50,
                "candidatesTokenCount": 3,
                "cachedContentTokenCount": 10
            }
        });
        let mut state = GeminiStreamState {
            has_content: true,
            ..Default::default()
        };
        assert!(finalize_gemini_stream(&usage_chunk, &mut state).is_empty());
        let events = force_finalize_gemini_stream(&mut state);
        let delta = events
            .iter()
            .find(|e| e["type"] == "message_delta")
            .unwrap();
        assert_eq!(delta["usage"]["input_tokens"], 40);
        assert_eq!(delta["usage"]["output_tokens"], 3);
        assert_eq!(delta["usage"]["cache_read_input_tokens"], 10);
    }

    #[test]
    fn test_antigravity_claude_response_emits_native_signatures() {
        // 对齐 CPA EmitsNativeSignaturesWithoutProviderPrefixes:
        // claude 组 thinking / tool_use 签名解回原生 E 形,无前缀
        use crate::convert::signature::fixtures::{
            claude_native_signature, claude_upstream_signature,
        };
        let native1 = claude_native_signature(12, Some(2), "claude-sonnet-4-6", true);
        let native2 = claude_native_signature(13, Some(2), "claude-opus-4-6", true);
        let upstream1 = claude_upstream_signature(&native1);
        let upstream2 = claude_upstream_signature(&native2);
        let response = json!({
            "candidates": [{
                "content": {"parts": [
                    {"text": "thought content", "thought": true, "thoughtSignature": upstream1},
                    {"functionCall": {"name": "run_command", "args": {"command": "true"}},
                     "thoughtSignature": upstream2}
                ]},
                "finishReason": "STOP"
            }],
            "usageMetadata": {"promptTokenCount": 1, "candidatesTokenCount": 1},
            "modelVersion": "claude-sonnet-4-6-thinking",
            "responseId": "resp-claude-native-sigs"
        });

        let anthropic =
            convert_gemini_response(&response, &HashMap::new(), Some("claude-sonnet-4-6"));
        let content = anthropic["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["type"], "thinking");
        assert_eq!(content[0]["signature"], native1);
        assert_eq!(content[1]["type"], "tool_use");
        assert_eq!(content[1]["name"], "run_command");
        assert_eq!(content[1]["signature"], native2);
    }

    #[test]
    fn test_plain_gemini_response_passes_signature_through() {
        // plain gemini 路径 CPA 不做归一化:thinking 签名原样,tool_use 不发签名
        let sig = "gemini-native-opaque";
        let response = json!({
            "candidates": [{
                "content": {"parts": [
                    {"text": "thought", "thought": true, "thoughtSignature": sig},
                    {"functionCall": {"name": "run_command", "args": {}}, "thoughtSignature": sig}
                ]},
                "finishReason": "STOP"
            }],
            "usageMetadata": {"promptTokenCount": 1, "candidatesTokenCount": 1},
            "modelVersion": "gemini-2.5-pro"
        });

        let anthropic = convert_gemini_response(&response, &HashMap::new(), None);
        let content = anthropic["content"].as_array().unwrap();
        assert_eq!(content[0]["signature"], sig);
        assert!(content[1].get("signature").is_none());
    }

    #[test]
    fn test_antigravity_claude_stream_signature_delta_is_native() {
        // 流式 signature_delta 同样解回 E 形
        use crate::convert::signature::fixtures::{
            claude_native_default, claude_upstream_signature,
        };
        let native = claude_native_default();
        let chunk = json!({
            "candidates": [{
                "content": {"parts": [
                    {"text": "thinking", "thought": true,
                     "thoughtSignature": claude_upstream_signature(&native)}
                ]}
            }]
        });
        let mut state = GeminiStreamState::default();
        let events = convert_gemini_stream_chunk(
            &chunk,
            &mut state,
            &HashMap::new(),
            Some("claude-sonnet-4-6"),
        );
        let delta = events
            .iter()
            .find(|e| e["delta"]["type"] == "signature_delta")
            .expect("signature_delta 应存在");
        assert_eq!(delta["delta"]["signature"], native);
    }
}
