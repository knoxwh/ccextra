// Gemini 响应 → Anthropic 格式转换

use serde_json::{Value, json};
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};

use super::antigravity_tools::antigravity_upstream_tool_name_to_client;
use super::claude_tool_args::{ToolArgStreamState, clean_claude_tool_arguments};
use super::signature::{format_claude_signature_value, model_group};
use super::tool_id::{claude_tool_id_for, gemini_claude_tool_use_id};

/// 全局工具使用 ID 计数器
static TOOL_USE_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// 合成 Claude message id 的进程级计数器(对齐 CPA claudeResponseIDCounter)
static CLAUDE_MESSAGE_ID_COUNTER: AtomicU64 = AtomicU64::new(0);

/// Claude message id(对齐 CPA da4396fd resolveAntigravityClaudeMessageID):
/// 上游 `responseId` 去空白后非空则沿用;缺失或空白时合成 `msg_{unix 纳秒十六进制}_{计数}`,
/// Claude Code 靠 message id 归并对话记录,不能为空。
pub fn resolve_claude_message_id(response_id: Option<&str>) -> String {
    if let Some(id) = response_id.map(str::trim).filter(|id| !id.is_empty()) {
        return id.to_string();
    }
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_nanos());
    let seq = CLAUDE_MESSAGE_ID_COUNTER.fetch_add(1, Ordering::Relaxed) + 1;
    format!("msg_{nanos:x}_{seq}")
}

/// Claude 侧 tool_use id(对齐 CPA antigravityClaudeToolUseID):
/// AG Gemini 族(与 reasoning replay 同门禁)且上游给了非空原生 `functionCall.id` 时,
/// 用 (原生 id, name, args) 派生稳定 id,回放时据账本换回原生 id;
/// 否则(含 plain Gemini、AG claude 族、原生 id 为空)保持 `{name}-{counter}`。
fn claude_tool_use_id(
    signature_model: Option<&str>,
    function_call: &Value,
    fallback_name: &str,
) -> String {
    if signature_model.is_some_and(crate::antigravity_uses_reasoning_replay) {
        let stable = gemini_claude_tool_use_id(
            function_call
                .get("id")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
            function_call
                .get("name")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
            function_call.get("args"),
        );
        if !stable.is_empty() {
            return stable;
        }
    }
    claude_tool_id_for(
        fallback_name,
        TOOL_USE_ID_COUNTER.fetch_add(1, Ordering::SeqCst),
    )
}

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
    /// 是否见过可见输出:非空白文本或非空 functionCall,thinking 不算(对齐 CPA cf07c78a HasVisibleOutput)
    pub has_visible_output: bool,
    /// 最终事件是否已发(对齐 CPA HasFinalEvents,防重复 finalize)
    pub final_events_sent: bool,
    /// 已发 MALFORMED api_error(对齐 CPA 7f2f17d7;SSE 层据此跳过 message_stop)
    pub api_error_emitted: bool,
    /// 工具参数清洗流状态(对齐 CPA CurrentTool*)
    pub tool_args: ToolArgStreamState,
    /// 续传块带来的签名:关 tool 块后再发 carrier(对齐 CPA 3a353935 CurrentToolSignature)
    pending_tool_signature: String,
    /// 是否见过 usageMetadata(对齐 CPA HasUsageMetadata)
    has_usage: bool,
    /// 是否见过 finishReason(对齐 CPA HasFinishReason)
    has_finish: bool,
    /// 流式 input = prompt - cached(对齐 CPA Params.PromptTokenCount)
    prompt_tokens: i64,
    output_tokens: i64,
    cache_read: i64,
    /// 缓存的 finishReason,供 `[DONE]` force 使用
    finish_reason: String,
}

impl GeminiStreamState {
    /// 是否观察到非空 finishReason(供 Antigravity `[DONE]` 收尾判断)。
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
/// 对齐 CPA appendCarrierThinkingBlock:独立空 thinking 块承载签名后立即关闭。
fn append_carrier_thinking_block(
    events: &mut Vec<Value>,
    state: &mut GeminiStreamState,
    signature: &str,
) {
    if signature.is_empty() {
        return;
    }
    if state.response_type != ResponseType::None {
        push_content_block_stop(events, state);
        state.response_index += 1;
        state.response_type = ResponseType::None;
    }
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
            "type": "signature_delta",
            "signature": signature
        }
    }));
    push_content_block_stop(events, state);
    state.response_index += 1;
    state.response_type = ResponseType::None;
    state.has_content = true;
}

fn append_signature_delta(events: &mut Vec<Value>, state: &mut GeminiStreamState, signature: &str) {
    events.push(json!({
        "type": "content_block_delta",
        "index": state.response_index,
        "delta": {
            "type": "signature_delta",
            "signature": signature
        }
    }));
    state.has_content = true;
}

/// 关块前冲刷延迟的空 `{}` / 补开放对象的 `}`(对齐 CPA e1fb2521、3a353935 flushPendingToolInput);
/// 关的是 tool 块且有延后的续传签名时,紧接着发独立 carrier(占用下一 index,调用方照常 +1)。
fn push_content_block_stop(events: &mut Vec<Value>, state: &mut GeminiStreamState) {
    let closing_tool = state.response_type == ResponseType::Function;
    if let Some(partial) = state.tool_args.flush_on_block_close() {
        events.push(json!({
            "type": "content_block_delta",
            "index": state.response_index,
            "delta": {
                "type": "input_json_delta",
                "partial_json": partial
            }
        }));
    }
    events.push(json!({
        "type": "content_block_stop",
        "index": state.response_index
    }));
    if closing_tool && !state.pending_tool_signature.is_empty() {
        let signature = std::mem::take(&mut state.pending_tool_signature);
        state.response_index += 1;
        events.push(json!({
            "type": "content_block_start",
            "index": state.response_index,
            "content_block": {"type": "thinking", "thinking": ""}
        }));
        events.push(json!({
            "type": "content_block_delta",
            "index": state.response_index,
            "delta": {"type": "signature_delta", "signature": signature}
        }));
        events.push(json!({
            "type": "content_block_stop",
            "index": state.response_index
        }));
        state.has_content = true;
    }
}

pub fn convert_gemini_stream_chunk(
    chunk: &Value,
    state: &mut GeminiStreamState,
    short_to_original: &HashMap<String, String>,
    signature_model: Option<&str>,
    original_request: Option<&Value>,
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
            // 对齐 CPA cf07c78a:非续传的空 functionCall 跳过(视为无 FC,签名走签名-only 分支)
            let function_call_result = part.get("functionCall").filter(|fc| {
                is_function_call_continuation(state, fc) || is_non_empty_function_call(fc)
            });
            let thought_signature = part
                .get("thoughtSignature")
                .or_else(|| part.get("thought_signature"))
                .and_then(|s| s.as_str())
                .unwrap_or("");
            let has_thought_signature = !thought_signature.is_empty();
            let signature_value = outbound_signature(signature_model, thought_signature);
            let text_str = text_result.and_then(|t| t.as_str());
            let text_empty = text_str.is_none_or(|t| t.is_empty());
            let is_thought = part
                .get("thought")
                .and_then(|t| t.as_bool())
                .unwrap_or(false);

            // 签名-only(无 text / 空 text、无 FC):在 thinking 内挂签名,否则 carrier
            if has_thought_signature && text_empty && function_call_result.is_none() {
                if state.response_type == ResponseType::Thinking {
                    append_signature_delta(&mut events, state, &signature_value);
                } else {
                    append_carrier_thinking_block(&mut events, state, &signature_value);
                }
                continue;
            }

            // 仅 thought==true 视为 thinking(对齐 CPA;签名不再单独标记)
            if is_thought {
                if has_thought_signature && text_empty {
                    if state.response_type == ResponseType::Thinking {
                        append_signature_delta(&mut events, state, &signature_value);
                    } else {
                        append_carrier_thinking_block(&mut events, state, &signature_value);
                    }
                    continue;
                }

                let text = text_str.unwrap_or("");
                if state.response_type == ResponseType::Thinking {
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
                    if state.response_type != ResponseType::None {
                        push_content_block_stop(&mut events, state);
                        state.response_index += 1;
                    }
                    events.push(json!({
                        "type": "content_block_start",
                        "index": state.response_index,
                        "content_block": {
                            "type": "thinking",
                            "thinking": "",
                            "signature": ""
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
                if has_thought_signature {
                    append_signature_delta(&mut events, state, &signature_value);
                }
                continue;
            }

            // 工具调用
            if let Some(function_call) = function_call_result {
                state.saw_tool_call = true;
                if is_non_empty_function_call(function_call) {
                    state.has_visible_output = true;
                }
                let upstream_tool_name = function_call
                    .get("name")
                    .and_then(|n| n.as_str())
                    .unwrap_or("");

                let restored_upstream_name = if signature_model.is_some() {
                    antigravity_upstream_tool_name_to_client(upstream_tool_name)
                } else {
                    upstream_tool_name.to_string()
                };

                // 流式分块:名称为空(含纯空白)表示续传
                if state.response_type == ResponseType::Function
                    && upstream_tool_name.trim().is_empty()
                {
                    if let Some(args) = function_call.get("args").filter(|v| v.is_object()) {
                        let tool_name = state.tool_args.current_tool_name.clone();
                        if let Some(partial) = state.tool_args.clean_emitted_args(
                            &args.to_string(),
                            &tool_name,
                            original_request,
                        ) {
                            events.push(json!({
                                "type": "content_block_delta",
                                "index": state.response_index,
                                "delta": {
                                    "type": "input_json_delta",
                                    "partial_json": partial
                                }
                            }));
                        }
                    }
                    // 对齐 CPA 3a353935:续传签名延后到 tool 块关闭后再发 carrier,不提前切断参数流
                    if has_thought_signature {
                        state.pending_tool_signature = signature_value;
                    }
                    continue;
                }

                if has_thought_signature {
                    append_carrier_thinking_block(&mut events, state, &signature_value);
                }

                if state.response_type == ResponseType::Function {
                    push_content_block_stop(&mut events, state);
                    state.response_index += 1;
                    state.response_type = ResponseType::None;
                }

                if state.response_type != ResponseType::None {
                    push_content_block_stop(&mut events, state);
                    state.response_index += 1;
                }

                let client_tool_name = short_to_original
                    .get(&restored_upstream_name)
                    .map(|s| s.as_str())
                    .unwrap_or(&restored_upstream_name);

                state
                    .tool_args
                    .begin_tool(client_tool_name, original_request);

                let tool_use_id =
                    claude_tool_use_id(signature_model, function_call, &restored_upstream_name);

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

                if let Some(args) = function_call.get("args").filter(|v| v.is_object())
                    && let Some(partial) = state.tool_args.clean_emitted_args(
                        &args.to_string(),
                        client_tool_name,
                        original_request,
                    )
                {
                    events.push(json!({
                        "type": "content_block_delta",
                        "index": state.response_index,
                        "delta": {
                            "type": "input_json_delta",
                            "partial_json": partial
                        }
                    }));
                }

                state.response_type = ResponseType::Function;
                state.has_content = true;
                continue;
            }

            // 普通可见文本(签名不单独标 thinking)
            if let Some(text) = text_str {
                if has_thought_signature && text.is_empty() {
                    if state.response_type == ResponseType::Thinking {
                        append_signature_delta(&mut events, state, &signature_value);
                    } else {
                        append_carrier_thinking_block(&mut events, state, &signature_value);
                    }
                    continue;
                }
                if has_thought_signature {
                    append_carrier_thinking_block(&mut events, state, &signature_value);
                }
                if !text.trim().is_empty() {
                    state.has_visible_output = true;
                }

                if state.response_type == ResponseType::Content {
                    events.push(json!({
                        "type": "content_block_delta",
                        "index": state.response_index,
                        "delta": {
                            "type": "text_delta",
                            "text": text
                        }
                    }));
                    state.has_content = true;
                } else if !text.is_empty() {
                    // 对齐 CPA fd3e6623:只在非空时转换,防止空 text 关闭活动块
                    if state.response_type != ResponseType::None {
                        push_content_block_stop(&mut events, state);
                        state.response_index += 1;
                    }
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
        }
    }

    // 对齐 CPA e1fb2521:见 finishReason 即冲刷延迟空 input
    if state.has_finish
        && let Some(partial) = state.tool_args.flush_pending_empty()
    {
        events.push(json!({
            "type": "content_block_delta",
            "index": state.response_index,
            "delta": {
                "type": "input_json_delta",
                "partial_json": partial
            }
        }));
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

/// 无可见输出即按异常终止处理的 finishReason(对齐 CPA cf07c78a
/// IsGeminiZeroOutputAbnormalFinishReason)。
const ZERO_OUTPUT_ABNORMAL_FINISH_REASONS: [&str; 3] = [
    "MALFORMED_FUNCTION_CALL",
    "UNEXPECTED_TOOL_CALL",
    "MALFORMED_RESPONSE",
];

/// 无可见输出的异常终止 → Claude api_error(对齐 CPA cf07c78a:忽略大小写与首尾空白;
/// 可见输出 = 非空白文本或非空 functionCall)。有可见输出时沿用原 stop_reason 映射。
pub(crate) fn is_gemini_zero_output_abnormal_finish(
    finish_reason: &str,
    has_visible_output: bool,
) -> bool {
    let reason = finish_reason.trim();
    !has_visible_output
        && ZERO_OUTPUT_ABNORMAL_FINISH_REASONS
            .iter()
            .any(|r| reason.eq_ignore_ascii_case(r))
}

/// 非空 functionCall:name/id 非空白,或 args 为非空对象,或 partialArgs 为非空数组
/// (对齐 CPA cf07c78a isGeminiNonEmptyFunctionCall)。
fn is_non_empty_function_call(fc: &Value) -> bool {
    let Some(obj) = fc.as_object() else {
        return false;
    };
    let non_blank = |key: &str| {
        obj.get(key)
            .and_then(|v| v.as_str())
            .is_some_and(|s| !s.trim().is_empty())
    };
    if non_blank("name") || non_blank("id") {
        return true;
    }
    if ["partialArgs", "partial_args"].iter().any(|key| {
        obj.get(*key)
            .and_then(|v| v.as_array())
            .is_some_and(|a| !a.is_empty())
    }) {
        return true;
    }
    obj.get("args")
        .and_then(|v| v.as_object())
        .is_some_and(|m| !m.is_empty())
}

/// 流式续传块:当前在 tool 块内且 name 为空(含纯空白,与续传分支判定一致)。
fn is_function_call_continuation(state: &GeminiStreamState, fc: &Value) -> bool {
    state.response_type == ResponseType::Function
        && fc
            .get("name")
            .and_then(|n| n.as_str())
            .unwrap_or("")
            .trim()
            .is_empty()
}

/// 对齐 CPA GeminiZeroOutputErrorMessage(FormatClaude):`Gemini response finished with <REASON>`。
pub fn gemini_zero_output_error(finish_reason: &str) -> Value {
    json!({
        "type": "error",
        "error": {
            "type": "api_error",
            "message": format!(
                "Gemini response finished with {}",
                finish_reason.trim().to_ascii_uppercase()
            )
        }
    })
}

/// 对齐 CPA resolveGeminiClaudeStopReason (7f2f17d7:MALFORMED_FUNCTION_CALL 不再映射
/// refusal;3f5d3a16/24b04548:MALFORMED_RESPONSE 映射 refusal)
pub(crate) fn resolve_gemini_claude_stop_reason(
    finish_reason: &str,
    saw_tool_call: bool,
) -> &'static str {
    if saw_tool_call {
        return "tool_use";
    }
    match finish_reason {
        "MAX_TOKENS" => "max_tokens",
        "SAFETY" | "RECITATION" | "PROHIBITED_CONTENT" | "SPII" | "BLOCKLIST" | "IMAGE_SAFETY"
        | "MALFORMED_RESPONSE" => "refusal",
        "STOP" | "FINISH_REASON_UNSPECIFIED" | "UNKNOWN" | "" => "end_turn",
        _ => "end_turn",
    }
}

fn stop_reason_of(state: &GeminiStreamState) -> &'static str {
    resolve_gemini_claude_stop_reason(&state.finish_reason, state.saw_tool_call)
}

/// 关块后发零输出 api_error(对齐 CPA 7f2f17d7/cf07c78a,不发 message_delta/message_stop)
fn append_zero_output_api_error(state: &mut GeminiStreamState, events: &mut Vec<Value>) {
    if state.final_events_sent {
        return;
    }
    state.final_events_sent = true;
    state.api_error_emitted = true;
    if state.response_type != ResponseType::None {
        push_content_block_stop(events, state);
        state.response_type = ResponseType::None;
    }
    events.push(gemini_zero_output_error(&state.finish_reason));
}

/// 关块 + message_delta(对齐 CPA appendFinalEvents)
fn append_final_events(state: &mut GeminiStreamState, events: &mut Vec<Value>) {
    if state.final_events_sent || !state.has_content {
        return;
    }
    if is_gemini_zero_output_abnormal_finish(&state.finish_reason, state.has_visible_output) {
        append_zero_output_api_error(state, events);
        return;
    }
    state.final_events_sent = true;
    if state.response_type != ResponseType::None {
        push_content_block_stop(events, state);
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
/// MALFORMED 无 tool_call:见 finishReason 即发 api_error(不要求 usage/content,对齐 CPA 7f2f17d7)
pub fn finalize_gemini_stream(chunk: &Value, state: &mut GeminiStreamState) -> Vec<Value> {
    cache_chunk_metadata(chunk, state);
    let mut events = Vec::new();
    if state.final_events_sent {
        return events;
    }
    if state.has_finish
        && is_gemini_zero_output_abnormal_finish(&state.finish_reason, state.has_visible_output)
    {
        append_zero_output_api_error(state, &mut events);
        return events;
    }
    if state.has_finish && state.has_usage && state.has_content {
        append_final_events(state, &mut events);
    }
    events
}

/// `[DONE]`/EOF force 收尾(对齐 CPA appendFinalEvents(force=true)):
/// 无内容先补空 text 块;然后关块 + message_delta(缺 usage 用已缓存值,默认 0)
/// MALFORMED 无 tool_call:发 api_error,不补空 text / message_delta
pub fn force_finalize_gemini_stream(state: &mut GeminiStreamState) -> Vec<Value> {
    if state.final_events_sent {
        return Vec::new();
    }
    if is_gemini_zero_output_abnormal_finish(&state.finish_reason, state.has_visible_output) {
        let mut events = Vec::new();
        append_zero_output_api_error(state, &mut events);
        return events;
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
    original_request: Option<&Value>,
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

    anthropic["id"] = json!(resolve_claude_message_id(
        response.get("responseId").and_then(|r| r.as_str())
    ));
    if let Some(model) = response.get("modelVersion").and_then(|m| m.as_str()) {
        anthropic["model"] = json!(model);
    }

    let mut saw_tool_call = false;
    let mut has_visible_output = false;
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
                        // thinking 块必须包含 signature 字段（对齐 sub2api 412718edf）
                        let sig = part
                            .get("thoughtSignature")
                            .or_else(|| part.get("thought_signature"))
                            .and_then(|s| s.as_str())
                            .filter(|s| !s.is_empty())
                            .map_or_default(|s| outbound_signature(signature_model, s));
                        block["signature"] = json!(sig);
                        content_blocks.push(block);
                    } else {
                        if !text.trim().is_empty() {
                            has_visible_output = true;
                        }
                        content_blocks.push(json!({"type": "text", "text": text}));
                    }
                }

                // 工具调用:id 见 claude_tool_use_id(不直接透传上游 id),name 还原原名
                // 对齐 CPA cf07c78a:空 functionCall 跳过(非流无 carrier,签名随之丢弃)
                if let Some(function_call) = part
                    .get("functionCall")
                    .filter(|fc| is_non_empty_function_call(fc))
                {
                    saw_tool_call = true;
                    has_visible_output = true;
                    let upstream_name = function_call
                        .get("name")
                        .and_then(|n| n.as_str())
                        .unwrap_or("");

                    // 对齐 CPA 6a26e92a:Antigravity 响应侧还原工具名
                    let restored_name = if signature_model.is_some() {
                        antigravity_upstream_tool_name_to_client(upstream_name)
                    } else {
                        upstream_name.to_string()
                    };

                    let original_name = short_to_original
                        .get(&restored_name)
                        .map(|s| s.as_str())
                        .unwrap_or(&restored_name);
                    // args 仅接受对象,否则兜底 {}(对齐 CPA gjson.Valid && IsObject)
                    let args = match function_call.get("args") {
                        Some(a) if a.is_object() => {
                            if let Some(orig) = original_request {
                                clean_claude_tool_arguments(a, original_name, orig)
                            } else {
                                a.clone()
                            }
                        }
                        _ => json!({}),
                    };
                    let tool_id =
                        claude_tool_use_id(signature_model, function_call, &restored_name);
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
        // 对齐 CPA 7f2f17d7/cf07c78a:无可见输出的异常终止 → api_error(可重试)
        if is_gemini_zero_output_abnormal_finish(finish_reason, has_visible_output) {
            return gemini_zero_output_error(finish_reason);
        }
        anthropic["stop_reason"] = json!(resolve_gemini_claude_stop_reason(
            finish_reason,
            saw_tool_call
        ));
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
        let anthropic = convert_gemini_response(&gemini, &tool_map, None, None);

        assert_eq!(anthropic["type"], "message");
        assert_eq!(anthropic["role"], "assistant");
        assert_eq!(anthropic["model"], "gemini-2.0");
        assert_eq!(anthropic["id"], "resp_123");
        // 对齐 CPA da4396fd:缺失/空/空白 responseId 合成唯一 msg_ id
        for gemini in [
            json!({"candidates": []}),
            json!({"responseId": "", "candidates": []}),
            json!({"responseId": "   ", "candidates": []}),
        ] {
            let id1 = convert_gemini_response(&gemini, &tool_map, None, None)["id"].clone();
            let id2 = convert_gemini_response(&gemini, &tool_map, None, None)["id"].clone();
            let id1 = id1.as_str().unwrap();
            assert!(id1.starts_with("msg_") && id1.len() > 4, "{id1}");
            assert!(id2.as_str().unwrap().starts_with("msg_"));
            assert_ne!(id1, id2.as_str().unwrap());
        }
        assert_eq!(
            convert_gemini_response(
                &json!({"responseId": "  resp_trim  ", "candidates": []}),
                &tool_map,
                None,
                None
            )["id"],
            "resp_trim"
        );
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

        let anthropic = convert_gemini_response(&gemini, &tool_map, None, None);

        let content = anthropic["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "tool_use");
        assert_eq!(content[0]["name"], "Read"); // 还原了原名
        // 对齐 CPA:id 自生成 {name}-{counter},不透传上游 id
        let id = content[0]["id"].as_str().unwrap();
        assert!(id.starts_with("Rd-"), "id={id}");
        assert_eq!(content[0]["input"]["path"], "/file.txt");
        // 有工具调用时 stop_reason 一律 tool_use(即使 finishReason=STOP)
        assert_eq!(anthropic["stop_reason"], "tool_use");
    }

    #[test]
    fn test_convert_gemini_response_usage_cases() {
        for (usage, input_tokens, output_tokens, cache_read) in [
            (
                json!({"promptTokenCount": 10, "candidatesTokenCount": 20,
                    "thoughtsTokenCount": 30}),
                10,
                50,
                None,
            ),
            (
                json!({"promptTokenCount": 100, "candidatesTokenCount": 20,
                    "cachedContentTokenCount": 40}),
                60,
                20,
                Some(40),
            ),
            (
                json!({"promptTokenCount": 5, "candidatesTokenCount": 3,
                    "cachedContentTokenCount": 40}),
                0,
                3,
                Some(40),
            ),
            (
                json!({"promptTokenCount": 10, "candidatesTokenCount": 5,
                    "cachedContentTokenCount": 0}),
                10,
                5,
                None,
            ),
        ] {
            let gemini = json!({
                "candidates": [{
                    "content": {"parts": [{"text": "hi"}], "role": "model"},
                    "finishReason": "STOP"
                }],
                "usageMetadata": usage
            });
            let anthropic = convert_gemini_response(&gemini, &HashMap::new(), None, None);
            assert_eq!(anthropic["usage"]["input_tokens"], input_tokens, "{usage}");
            assert_eq!(
                anthropic["usage"]["output_tokens"], output_tokens,
                "{usage}"
            );
            assert_eq!(
                anthropic["usage"].get("cache_read_input_tokens").cloned(),
                cache_read.map(Value::from),
                "{usage}"
            );
        }
    }

    #[test]
    fn test_convert_gemini_response_no_usage_metadata() {
        let gemini = json!({
            "candidates": [{
                "content": {"parts": [{"text": "hi"}], "role": "model"},
                "finishReason": "STOP"
            }]
        });
        let anthropic = convert_gemini_response(&gemini, &HashMap::new(), None, None);
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
        let anthropic = convert_gemini_response(&gemini, &HashMap::new(), None, None);
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
        let anthropic = convert_gemini_response(&gemini, &HashMap::new(), None, None);
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
        let anthropic = convert_gemini_response(&gemini, &HashMap::new(), None, None);
        assert_eq!(anthropic["content"][0]["input"], json!({}));
    }

    #[test]
    fn test_convert_gemini_response_refusal_finish_reasons() {
        // 对齐 CPA 7f2f17d7:SAFETY/RECITATION 等仍为 refusal;MALFORMED_FUNCTION_CALL 单独走
        // api_error;对齐 CPA 3f5d3a16/24b04548:MALFORMED_RESPONSE 为 refusal
        for reason in [
            "SAFETY",
            "RECITATION",
            "PROHIBITED_CONTENT",
            "SPII",
            "BLOCKLIST",
            "IMAGE_SAFETY",
            "MALFORMED_RESPONSE",
        ] {
            let gemini = json!({
                "candidates": [{
                    "content": {"parts": [{"text": "blocked"}], "role": "model"},
                    "finishReason": reason
                }]
            });
            let anthropic = convert_gemini_response(&gemini, &HashMap::new(), None, None);
            assert_eq!(anthropic["stop_reason"], "refusal");
        }
    }

    #[test]
    fn test_convert_gemini_response_malformed_returns_api_error() {
        // 对齐 CPA 7f2f17d7 / TestConvertGeminiResponseToClaudeNonStream_MalformedFunctionCallReturnsAPIError
        let gemini = json!({
            "candidates": [{
                "content": {"parts": [], "role": "model"},
                "finishReason": "MALFORMED_FUNCTION_CALL"
            }],
            "usageMetadata": {
                "promptTokenCount": 120,
                "candidatesTokenCount": 37,
                "totalTokenCount": 157
            },
            "modelVersion": "m",
            "responseId": "resp-test"
        });
        let out = convert_gemini_response(&gemini, &HashMap::new(), None, None);
        assert_eq!(out["type"], "error");
        assert_eq!(out["error"]["type"], "api_error");
        assert!(
            out["error"]["message"]
                .as_str()
                .unwrap_or("")
                .contains("MALFORMED_FUNCTION_CALL"),
            "{out}"
        );
        assert!(out.get("stop_reason").is_none() || out["stop_reason"].is_null());
    }

    #[test]
    fn test_convert_gemini_response_malformed_with_tool_keeps_tool_use() {
        // 对齐 CPA TestConvertGeminiResponseToClaude_MalformedFunctionCallWithToolUsePreservesToolUse
        let gemini = json!({
            "candidates": [{
                "content": {
                    "role": "model",
                    "parts": [{"functionCall": {"name": "Bash", "args": {"command": "ls"}}}]
                },
                "finishReason": "MALFORMED_FUNCTION_CALL"
            }],
            "usageMetadata": {
                "promptTokenCount": 50,
                "candidatesTokenCount": 10,
                "totalTokenCount": 60
            },
            "modelVersion": "m",
            "responseId": "resp-tool"
        });
        let out = convert_gemini_response(&gemini, &HashMap::new(), None, None);
        assert_eq!(out["type"], "message");
        assert_eq!(out["stop_reason"], "tool_use");
        assert_eq!(out["content"][0]["type"], "tool_use");
    }

    #[test]
    fn test_finalize_gemini_stream_malformed_emits_api_error() {
        // 对齐 CPA PartlessMalformed / WithoutUsage:见 finishReason 即 error,无 message_delta
        let chunk = json!({
            "candidates": [{
                "content": {"role": "model", "parts": []},
                "finishReason": "MALFORMED_FUNCTION_CALL"
            }],
            "modelVersion": "m"
        });
        let mut state = GeminiStreamState::default();
        let events = finalize_gemini_stream(&chunk, &mut state);
        assert!(state.api_error_emitted);
        assert!(state.final_events_sent);
        assert!(
            events
                .iter()
                .any(|e| e["type"] == "error" && e["error"]["type"] == "api_error"),
            "{events:?}"
        );
        assert!(
            events.iter().all(|e| e["type"] != "message_delta"),
            "{events:?}"
        );

        // 重复终帧 / [DONE] 不再发
        let dup = json!({
            "candidates": [{
                "content": {"role": "model", "parts": [{"text": "late"}]},
                "finishReason": "MALFORMED_FUNCTION_CALL"
            }],
            "usageMetadata": {"promptTokenCount": 1, "candidatesTokenCount": 1}
        });
        assert!(finalize_gemini_stream(&dup, &mut state).is_empty());
        assert!(force_finalize_gemini_stream(&mut state).is_empty());
    }

    #[test]
    fn test_finalize_gemini_stream_malformed_after_thinking_closes_then_error() {
        let mut state = GeminiStreamState {
            response_type: ResponseType::Thinking,
            response_index: 0,
            has_content: true,
            ..Default::default()
        };
        let chunk = json!({
            "candidates": [{
                "content": {"role": "model", "parts": []},
                "finishReason": "MALFORMED_FUNCTION_CALL"
            }],
            "usageMetadata": {
                "promptTokenCount": 50,
                "candidatesTokenCount": 10,
                "thoughtsTokenCount": 313
            }
        });
        let events = finalize_gemini_stream(&chunk, &mut state);
        assert_eq!(events[0]["type"], "content_block_stop");
        assert_eq!(events[1]["type"], "error");
        assert_eq!(events[1]["error"]["type"], "api_error");
        assert!(events.iter().all(|e| e["type"] != "message_delta"));
    }

    #[test]
    fn test_finalize_gemini_stream_maps_refusal() {
        let chunk = json!({
            "candidates": [
                {
                    "content": {"parts": [{"text": "blocked"}]},
                    "finishReason": "SAFETY"
                }
            ],
            "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 5}
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
        assert_eq!(delta["delta"]["stop_reason"], "refusal");
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
        let events = convert_gemini_stream_chunk(&chunk, &mut state, &tool_map, None, None);

        // 新状态机：第一个块会发送 content_block_start + content_block_delta
        assert_eq!(events.len(), 2);
        assert_eq!(events[0]["type"], "content_block_start");
        assert_eq!(events[0]["content_block"]["type"], "text");
        assert_eq!(events[1]["type"], "content_block_delta");
        assert_eq!(events[1]["delta"]["type"], "text_delta");
        assert_eq!(events[1]["delta"]["text"], "Hello");
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
            convert_gemini_response(&response, &HashMap::new(), Some("claude-sonnet-4-6"), None);
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

        let anthropic = convert_gemini_response(&response, &HashMap::new(), None, None);
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
            None,
        );
        let delta = events
            .iter()
            .find(|e| e["delta"]["type"] == "signature_delta")
            .expect("signature_delta 应存在");
        assert_eq!(delta["delta"]["signature"], native);
    }

    #[test]
    fn test_gemini_stream_empty_text_keeps_thinking_block_open() {
        // 对齐 CPA fd3e6623:空 text parts 不关闭活动 thinking 块
        let chunk1 = json!({
            "candidates": [{
                "content": {"parts": [
                    {"text": "思考中", "thought": true}
                ]}
            }]
        });
        let chunk2 = json!({
            "candidates": [{
                "content": {"parts": [
                    {"text": ""}
                ]}
            }]
        });

        let mut state = GeminiStreamState::default();
        let events1 = convert_gemini_stream_chunk(&chunk1, &mut state, &HashMap::new(), None, None);
        assert_eq!(state.response_type, ResponseType::Thinking);
        let start = events1.iter().find(|e| e["type"] == "content_block_start");
        assert!(start.is_some());
        assert_eq!(start.unwrap()["content_block"]["type"], "thinking");

        let events2 = convert_gemini_stream_chunk(&chunk2, &mut state, &HashMap::new(), None, None);
        // 空 text 不应关闭 thinking 块
        assert!(events2.iter().all(|e| e["type"] != "content_block_stop"));
        assert_eq!(state.response_type, ResponseType::Thinking);
    }

    #[test]
    fn test_gemini_stream_signature_on_visible_text_emits_carrier_not_thinking() {
        // 对齐 CPA:签名 alone 不把可见文本标成 thinking
        let chunk = json!({
            "candidates": [{
                "content": {"parts": [
                    {"text": "visible answer", "thoughtSignature": "sig-final-answer"}
                ]}
            }]
        });
        let mut state = GeminiStreamState::default();
        let events = convert_gemini_stream_chunk(&chunk, &mut state, &HashMap::new(), None, None);

        let carrier_start = events
            .iter()
            .find(|e| {
                e["type"] == "content_block_start" && e["content_block"]["type"] == "thinking"
            })
            .expect("carrier thinking start");
        assert_eq!(carrier_start["content_block"]["thinking"], "");
        assert!(
            events
                .iter()
                .any(|e| e["delta"]["type"] == "signature_delta")
        );
        assert!(
            events
                .iter()
                .any(|e| e["delta"]["type"] == "text_delta"
                    && e["delta"]["text"] == "visible answer")
        );
        assert!(
            events
                .iter()
                .all(|e| e["delta"]["type"] != "thinking_delta")
        );
        assert_eq!(state.response_type, ResponseType::Content);
    }

    #[test]
    fn test_gemini_stream_signature_only_emits_carrier_when_not_thinking() {
        let chunk = json!({
            "candidates": [{
                "content": {"parts": [
                    {"thoughtSignature": "sig-only"}
                ]}
            }]
        });
        let mut state = GeminiStreamState::default();
        let events = convert_gemini_stream_chunk(&chunk, &mut state, &HashMap::new(), None, None);
        assert!(events.iter().any(
            |e| e["type"] == "content_block_start" && e["content_block"]["type"] == "thinking"
        ));
        assert!(
            events
                .iter()
                .any(|e| e["delta"]["type"] == "signature_delta")
        );
        assert!(events.iter().any(|e| e["type"] == "content_block_stop"));
        assert_eq!(state.response_type, ResponseType::None);
        assert!(state.has_content);
    }

    #[test]
    fn test_gemini_stream_thought_true_still_emits_thinking_delta() {
        let chunk = json!({
            "candidates": [{
                "content": {"parts": [
                    {"text": "reason", "thought": true, "thoughtSignature": "sig-t"}
                ]}
            }]
        });
        let mut state = GeminiStreamState::default();
        let events = convert_gemini_stream_chunk(&chunk, &mut state, &HashMap::new(), None, None);
        assert!(
            events
                .iter()
                .any(|e| e["delta"]["type"] == "thinking_delta"
                    && e["delta"]["thinking"] == "reason")
        );
        assert!(
            events
                .iter()
                .any(|e| e["delta"]["type"] == "signature_delta")
        );
        assert_eq!(state.response_type, ResponseType::Thinking);
    }

    #[test]
    fn test_gemini_stream_function_call_signature_emits_carrier_before_tool() {
        let chunk = json!({
            "candidates": [{
                "content": {"parts": [
                    {"functionCall": {"name": "run_command", "args": {"command": "true"}},
                     "thoughtSignature": "sig-tool"}
                ]}
            }]
        });
        let mut state = GeminiStreamState::default();
        let events = convert_gemini_stream_chunk(&chunk, &mut state, &HashMap::new(), None, None);

        let carrier_idx = events
            .iter()
            .position(|e| {
                e["type"] == "content_block_start" && e["content_block"]["type"] == "thinking"
            })
            .expect("carrier before tool");
        let tool_idx = events
            .iter()
            .position(|e| {
                e["type"] == "content_block_start" && e["content_block"]["type"] == "tool_use"
            })
            .expect("tool_use start");
        assert!(carrier_idx < tool_idx);
        assert_eq!(state.response_type, ResponseType::Function);
    }

    #[test]
    fn test_gemini_stream_non_empty_text_transitions_from_thinking() {
        // 非空普通 text 正常触发状态转换
        let chunk1 = json!({
            "candidates": [{
                "content": {"parts": [
                    {"text": "思考", "thought": true}
                ]}
            }]
        });
        let chunk2 = json!({
            "candidates": [{
                "content": {"parts": [
                    {"text": "回答"}
                ]}
            }]
        });

        let mut state = GeminiStreamState::default();
        convert_gemini_stream_chunk(&chunk1, &mut state, &HashMap::new(), None, None);
        assert_eq!(state.response_type, ResponseType::Thinking);

        let events2 = convert_gemini_stream_chunk(&chunk2, &mut state, &HashMap::new(), None, None);
        // 非空 text 应关闭 thinking 并开始 text 块
        let stop = events2.iter().find(|e| e["type"] == "content_block_stop");
        assert!(stop.is_some());
        let start = events2.iter().find(|e| e["type"] == "content_block_start");
        assert!(start.is_some());
        assert_eq!(start.unwrap()["content_block"]["type"], "text");
        assert_eq!(state.response_type, ResponseType::Content);
    }

    #[test]
    fn test_stream_parameterless_tool_args_normalized_to_empty() {
        // 对齐 CPA 202b8b8c:无参工具 reason 占位在响应侧归一为 {}
        let orig = json!({
            "tools": [{"name": "EnterPlanMode", "input_schema": {"type": "object", "properties": {}, "additionalProperties": false}}]
        });
        let chunk = json!({
            "candidates": [{
                "content": {
                    "parts": [{
                        "functionCall": {
                            "name": "EnterPlanMode",
                            "args": {"reason": "Switching to plan mode to inspect files"}
                        }
                    }]
                },
                "finishReason": "STOP"
            }]
        });
        let mut state = GeminiStreamState::default();
        let events =
            convert_gemini_stream_chunk(&chunk, &mut state, &HashMap::new(), None, Some(&orig));
        let partial: String = events
            .iter()
            .filter(|e| e["delta"]["type"] == "input_json_delta")
            .map(|e| {
                e["delta"]["partial_json"]
                    .as_str()
                    .unwrap_or("")
                    .to_string()
            })
            .collect();
        assert_eq!(partial, "{}");
    }

    #[test]
    fn test_nonstream_strips_injected_underscore() {
        let orig = json!({
            "tools": [{
                "name": "WrappedPlaceholder",
                "input_schema": {
                    "type": "object",
                    "properties": {"flag": {"type": "string"}},
                    "additionalProperties": false
                }
            }]
        });
        let response = json!({
            "candidates": [{
                "content": {
                    "parts": [{
                        "functionCall": {
                            "name": "WrappedPlaceholder",
                            "args": {"flag": "x", "_": true}
                        }
                    }]
                },
                "finishReason": "STOP"
            }]
        });
        let out = convert_gemini_response(&response, &HashMap::new(), None, Some(&orig));
        assert_eq!(out["content"][0]["input"], json!({"flag": "x"}));
    }

    #[test]
    fn test_stream_parameterless_continuation_does_not_duplicate_empty() {
        let orig = json!({
            "tools": [{"name": "EnterPlanMode", "input_schema": {"type": "object", "properties": {}, "additionalProperties": false}}]
        });
        let first = json!({
            "candidates": [{
                "content": {
                    "parts": [{
                        "functionCall": {
                            "name": "EnterPlanMode",
                            "args": {"reason": "Switching to plan mode"}
                        }
                    }]
                }
            }]
        });
        let cont = json!({
            "candidates": [{
                "content": {
                    "parts": [{
                        "functionCall": {
                            "args": {"reason": "more"}
                        }
                    }]
                },
                "finishReason": "STOP"
            }]
        });
        let mut state = GeminiStreamState::default();
        let e1 =
            convert_gemini_stream_chunk(&first, &mut state, &HashMap::new(), None, Some(&orig));
        let e2 = convert_gemini_stream_chunk(&cont, &mut state, &HashMap::new(), None, Some(&orig));
        let starts = e1
            .iter()
            .chain(e2.iter())
            .filter(|e| {
                e["type"] == "content_block_start" && e["content_block"]["type"] == "tool_use"
            })
            .count();
        assert_eq!(starts, 1);
        let partial: String = e1
            .iter()
            .chain(e2.iter())
            .filter(|e| e["delta"]["type"] == "input_json_delta")
            .map(|e| {
                e["delta"]["partial_json"]
                    .as_str()
                    .unwrap_or("")
                    .to_string()
            })
            .collect();
        assert_eq!(partial, "{}");
    }

    #[test]
    fn test_stream_implicit_open_continuation_ordering() {
        // 对齐 CPA e1fb2521 ImplicitOpenToolContinuationOrdering
        let orig = json!({
            "tools": [{"name": "echo", "input_schema": {"type": "object", "properties": {}}}]
        });
        for (first_args, cont_args, want) in [
            (json!({}), json!({"user": "v"}), r#"{"user":"v"}"#),
            (
                json!({"reason": "thinking"}),
                json!({"user": "v"}),
                r#"{"user":"v"}"#,
            ),
            (json!({"user": "v"}), json!({}), r#"{"user":"v"}"#),
            (
                json!({"user": "v"}),
                json!({"reason": "done"}),
                r#"{"user":"v"}"#,
            ),
        ] {
            let first = json!({
                "candidates": [{
                    "content": {"parts": [{"functionCall": {"name": "echo", "args": first_args}}]}
                }]
            });
            let cont = json!({
                "candidates": [{
                    "content": {"parts": [{"functionCall": {"args": cont_args}}]},
                    "finishReason": "STOP"
                }]
            });
            let mut state = GeminiStreamState::default();
            let e1 =
                convert_gemini_stream_chunk(&first, &mut state, &HashMap::new(), None, Some(&orig));
            let e2 =
                convert_gemini_stream_chunk(&cont, &mut state, &HashMap::new(), None, Some(&orig));
            // 对象参数的 `}` 在关块时补(对齐 CPA 3a353935)
            let e3 = force_finalize_gemini_stream(&mut state);
            let partial: String = e1
                .iter()
                .chain(e2.iter())
                .chain(e3.iter())
                .filter(|e| e["delta"]["type"] == "input_json_delta")
                .map(|e| {
                    e["delta"]["partial_json"]
                        .as_str()
                        .unwrap_or("")
                        .to_string()
                })
                .collect();
            assert_eq!(partial, want, "first={first_args} cont={cont_args}");
        }
    }

    fn stream_tool_use_ids(signature_model: Option<&str>, fc: Value) -> Vec<String> {
        let mut state = GeminiStreamState::default();
        let chunk = json!({"candidates": [{"content": {"parts": [{"functionCall": fc}]}}]});
        convert_gemini_stream_chunk(&chunk, &mut state, &HashMap::new(), signature_model, None)
            .iter()
            .filter(|e| {
                e["type"] == "content_block_start" && e["content_block"]["type"] == "tool_use"
            })
            .map(|e| e["content_block"]["id"].as_str().unwrap().to_string())
            .collect()
    }

    fn nonstream_tool_use_ids(signature_model: Option<&str>, fc: Value) -> Vec<String> {
        let response = json!({"candidates": [{
            "content": {"parts": [{"functionCall": fc}]},
            "finishReason": "STOP"
        }]});
        convert_gemini_response(&response, &HashMap::new(), signature_model, None)["content"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|b| b["type"] == "tool_use")
            .map(|b| b["id"].as_str().unwrap().to_string())
            .collect()
    }

    /// AG Gemini 族 + 原生 id → 稳定 id（流式 / 非流式一致）；无原生 id → `{name}-{counter}`。
    #[test]
    fn ag_gemini_native_call_id_emits_stable_tool_use_id() {
        use crate::convert::tool_id::{gemini_claude_tool_use_id, is_gemini_claude_tool_use_id};
        let fc = json!({"id": "native-1", "name": "Read", "args": {"file_path": "/a"}});
        let expected = gemini_claude_tool_use_id("native-1", "Read", Some(&fc["args"]));
        for ids in [
            stream_tool_use_ids(Some("gemini-3-flash"), fc.clone()),
            nonstream_tool_use_ids(Some("gemini-3-flash"), fc),
        ] {
            assert_eq!(ids, vec![expected.clone()]);
        }

        let no_id = json!({"name": "Read", "args": {"file_path": "/a"}});
        let empty_id = json!({"id": "  ", "name": "Read", "args": {"file_path": "/a"}});
        for ids in [
            stream_tool_use_ids(Some("gemini-3-flash"), no_id.clone()),
            nonstream_tool_use_ids(Some("gemini-3-flash"), no_id),
            stream_tool_use_ids(Some("gemini-3-flash"), empty_id.clone()),
            nonstream_tool_use_ids(Some("gemini-3-flash"), empty_id),
        ] {
            assert_eq!(ids.len(), 1);
            assert!(ids[0].starts_with("Read-"), "{ids:?}");
            assert!(!is_gemini_claude_tool_use_id(&ids[0]));
        }
    }

    /// 非 replay 路径（plain Gemini / AG claude 族）即使上游给了 id 也保持旧格式。
    #[test]
    fn non_replay_paths_keep_counter_tool_use_id() {
        let fc = json!({"id": "native-1", "name": "Read", "args": {"file_path": "/a"}});
        for model in [None, Some("claude-sonnet-4-6")] {
            for ids in [
                stream_tool_use_ids(model, fc.clone()),
                nonstream_tool_use_ids(model, fc.clone()),
            ] {
                assert_eq!(ids.len(), 1);
                assert!(ids[0].starts_with("Read-"), "{model:?} {ids:?}");
            }
        }
    }

    // ---- 对齐 CPA cf07c78a:MALFORMED 判定改「无可见输出」+ 空 functionCall 跳过 ----

    fn run_stream(chunks: &[Value]) -> (Vec<Value>, GeminiStreamState) {
        let mut state = GeminiStreamState::default();
        let mut events = Vec::new();
        for chunk in chunks {
            events.extend(convert_gemini_stream_chunk(
                chunk,
                &mut state,
                &HashMap::new(),
                None,
                None,
            ));
            events.extend(finalize_gemini_stream(chunk, &mut state));
        }
        (events, state)
    }

    fn malformed_terminal_chunk() -> Value {
        json!({
            "candidates": [{
                "content": {"role": "model", "parts": []},
                "finishReason": "MALFORMED_FUNCTION_CALL"
            }],
            "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 5}
        })
    }

    fn has_api_error(events: &[Value]) -> bool {
        events
            .iter()
            .any(|e| e["type"] == "error" && e["error"]["type"] == "api_error")
    }

    fn has_tool_use_start(events: &[Value]) -> bool {
        events
            .iter()
            .any(|e| e["type"] == "content_block_start" && e["content_block"]["type"] == "tool_use")
    }

    #[test]
    fn test_stream_malformed_after_visible_text_ends_normally() {
        let text =
            json!({"candidates": [{"content": {"role": "model", "parts": [{"text": "hi"}]}}]});
        let (events, state) = run_stream(&[text, malformed_terminal_chunk()]);
        assert!(state.has_visible_output);
        assert!(!state.api_error_emitted);
        assert!(!has_api_error(&events), "{events:?}");
        let delta = events
            .iter()
            .find(|e| e["type"] == "message_delta")
            .expect("message_delta");
        assert_eq!(delta["delta"]["stop_reason"], "end_turn");
    }

    #[test]
    fn test_stream_malformed_after_whitespace_text_emits_api_error() {
        let text =
            json!({"candidates": [{"content": {"role": "model", "parts": [{"text": "  \n"}]}}]});
        let (events, state) = run_stream(&[text, malformed_terminal_chunk()]);
        assert!(!state.has_visible_output);
        assert!(state.api_error_emitted);
        assert!(has_api_error(&events), "{events:?}");
        assert!(events.iter().all(|e| e["type"] != "message_delta"));
    }

    #[test]
    fn test_stream_malformed_after_thinking_only_emits_api_error() {
        let thinking = json!({"candidates": [{"content": {"role": "model", "parts": [
            {"text": "plan", "thought": true}
        ]}}]});
        let (events, state) = run_stream(&[thinking, malformed_terminal_chunk()]);
        assert!(!state.has_visible_output);
        assert!(state.api_error_emitted);
        let n = events.len();
        assert_eq!(events[n - 2]["type"], "content_block_stop", "{events:?}");
        assert_eq!(events[n - 1]["type"], "error", "{events:?}");
        assert!(events.iter().all(|e| e["type"] != "message_delta"));
    }

    #[test]
    fn test_stream_empty_function_call_skipped_then_malformed_errors() {
        let empty_fc = json!({"candidates": [{"content": {"role": "model", "parts": [
            {"functionCall": {}}
        ]}}]});
        let (events, state) = run_stream(&[empty_fc, malformed_terminal_chunk()]);
        assert!(!state.saw_tool_call);
        assert!(!state.has_visible_output);
        assert!(!has_tool_use_start(&events), "{events:?}");
        assert!(has_api_error(&events), "{events:?}");
    }

    #[test]
    fn test_stream_signed_empty_function_call_emits_carrier_not_tool_use() {
        let empty_fc = json!({"candidates": [{"content": {"role": "model", "parts": [
            {"thoughtSignature": "sig-empty-fc", "functionCall": {"args": {}}}
        ]}}]});
        let (events, state) = run_stream(&[empty_fc, malformed_terminal_chunk()]);
        assert!(!state.saw_tool_call);
        assert!(!has_tool_use_start(&events), "{events:?}");
        assert!(
            events
                .iter()
                .any(|e| e["type"] == "content_block_start"
                    && e["content_block"]["type"] == "thinking"),
            "{events:?}"
        );
        assert!(
            events
                .iter()
                .any(|e| e["delta"]["type"] == "signature_delta"
                    && e["delta"]["signature"] == "sig-empty-fc"),
            "{events:?}"
        );
        assert!(has_api_error(&events), "{events:?}");
    }

    #[test]
    fn test_malformed_finish_reason_case_insensitive() {
        assert!(is_gemini_zero_output_abnormal_finish(
            " malformed_function_call ",
            false
        ));
        assert!(!is_gemini_zero_output_abnormal_finish(
            "malformed_function_call",
            true
        ));
        let gemini = json!({
            "candidates": [{
                "content": {"role": "model", "parts": []},
                "finishReason": "malformed_function_call"
            }]
        });
        let out = convert_gemini_response(&gemini, &HashMap::new(), None, None);
        assert_eq!(out["type"], "error");
        assert_eq!(out["error"]["type"], "api_error");
    }

    #[test]
    fn test_convert_gemini_response_malformed_with_text_keeps_message() {
        let gemini = json!({
            "candidates": [{
                "content": {"role": "model", "parts": [{"text": "partial answer"}]},
                "finishReason": "MALFORMED_FUNCTION_CALL"
            }]
        });
        let out = convert_gemini_response(&gemini, &HashMap::new(), None, None);
        assert_eq!(out["type"], "message", "{out}");
        assert_eq!(out["stop_reason"], "end_turn");
        assert_eq!(out["content"][0]["text"], "partial answer");
    }

    #[test]
    fn test_convert_gemini_response_malformed_whitespace_text_returns_api_error() {
        let gemini = json!({
            "candidates": [{
                "content": {"role": "model", "parts": [{"text": " \n "}]},
                "finishReason": "MALFORMED_FUNCTION_CALL"
            }]
        });
        let out = convert_gemini_response(&gemini, &HashMap::new(), None, None);
        assert_eq!(out["type"], "error", "{out}");
    }

    #[test]
    fn test_convert_gemini_response_malformed_thinking_only_returns_api_error() {
        let gemini = json!({
            "candidates": [{
                "content": {"role": "model", "parts": [{"text": "plan", "thought": true}]},
                "finishReason": "MALFORMED_FUNCTION_CALL"
            }]
        });
        let out = convert_gemini_response(&gemini, &HashMap::new(), None, None);
        assert_eq!(out["type"], "error", "{out}");
        assert_eq!(out["error"]["type"], "api_error");
    }

    #[test]
    fn test_convert_gemini_response_empty_function_call_malformed_returns_api_error() {
        let gemini = json!({
            "candidates": [{
                "content": {"role": "model", "parts": [
                    {"thoughtSignature": "sig-empty-fc", "functionCall": {"args": {}}}
                ]},
                "finishReason": "MALFORMED_FUNCTION_CALL"
            }]
        });
        let out = convert_gemini_response(&gemini, &HashMap::new(), None, None);
        assert_eq!(out["type"], "error", "{out}");
        assert_eq!(out["error"]["type"], "api_error");
    }

    // ---- 对齐 CPA cf07c78a:UNEXPECTED_TOOL_CALL / MALFORMED_RESPONSE 零输出同走 api_error ----

    fn zero_output_terminal_chunk(reason: &str) -> Value {
        json!({
            "candidates": [{
                "content": {"role": "model", "parts": []},
                "finishReason": reason
            }],
            "usageMetadata": {"promptTokenCount": 10, "candidatesTokenCount": 5}
        })
    }

    #[test]
    fn test_zero_output_abnormal_finish_reasons() {
        for reason in [
            "MALFORMED_FUNCTION_CALL",
            "UNEXPECTED_TOOL_CALL",
            " unexpected_tool_call ",
            "MALFORMED_RESPONSE",
        ] {
            assert!(
                is_gemini_zero_output_abnormal_finish(reason, false),
                "{reason}"
            );
            assert!(
                !is_gemini_zero_output_abnormal_finish(reason, true),
                "{reason}"
            );
        }
        for reason in ["STOP", "MAX_TOKENS", "SAFETY", "TOO_MANY_TOOL_CALLS", ""] {
            assert!(
                !is_gemini_zero_output_abnormal_finish(reason, false),
                "{reason}"
            );
        }
    }

    #[test]
    fn test_convert_gemini_response_zero_output_reasons_return_api_error() {
        for reason in ["UNEXPECTED_TOOL_CALL", "MALFORMED_RESPONSE"] {
            let out = convert_gemini_response(
                &zero_output_terminal_chunk(reason),
                &HashMap::new(),
                None,
                None,
            );
            assert_eq!(out["type"], "error", "{out}");
            assert_eq!(out["error"]["type"], "api_error");
            assert_eq!(
                out["error"]["message"],
                format!("Gemini response finished with {reason}")
            );
        }
    }

    #[test]
    fn test_convert_gemini_response_unexpected_tool_call_thinking_only_returns_api_error() {
        let gemini = json!({
            "candidates": [{
                "content": {"role": "model", "parts": [{"text": "plan", "thought": true}]},
                "finishReason": "UNEXPECTED_TOOL_CALL"
            }]
        });
        let out = convert_gemini_response(&gemini, &HashMap::new(), None, None);
        assert_eq!(out["type"], "error", "{out}");
    }

    #[test]
    fn test_convert_gemini_response_unexpected_tool_call_with_text_keeps_message() {
        let gemini = json!({
            "candidates": [{
                "content": {"role": "model", "parts": [{"text": "partial answer"}]},
                "finishReason": "UNEXPECTED_TOOL_CALL"
            }]
        });
        let out = convert_gemini_response(&gemini, &HashMap::new(), None, None);
        assert_eq!(out["type"], "message", "{out}");
        assert_eq!(out["stop_reason"], "end_turn");
    }

    #[test]
    fn test_stream_zero_output_reasons_emit_api_error() {
        for reason in ["UNEXPECTED_TOOL_CALL", "MALFORMED_RESPONSE"] {
            let thinking = json!({"candidates": [{"content": {"role": "model", "parts": [
                {"text": "plan", "thought": true}
            ]}}]});
            let (events, state) = run_stream(&[thinking, zero_output_terminal_chunk(reason)]);
            assert!(state.api_error_emitted, "{reason}");
            let n = events.len();
            assert_eq!(events[n - 2]["type"], "content_block_stop", "{events:?}");
            assert_eq!(events[n - 1]["type"], "error", "{events:?}");
            assert_eq!(
                events[n - 1]["error"]["message"],
                format!("Gemini response finished with {reason}")
            );
            assert!(events.iter().all(|e| e["type"] != "message_delta"));
        }
    }

    #[test]
    fn test_stream_zero_output_reasons_after_visible_text_end_normally() {
        for (reason, stop) in [
            ("UNEXPECTED_TOOL_CALL", "end_turn"),
            ("MALFORMED_RESPONSE", "refusal"),
        ] {
            let text =
                json!({"candidates": [{"content": {"role": "model", "parts": [{"text": "hi"}]}}]});
            let (events, state) = run_stream(&[text, zero_output_terminal_chunk(reason)]);
            assert!(!state.api_error_emitted, "{reason}");
            assert!(!has_api_error(&events), "{events:?}");
            let delta = events
                .iter()
                .find(|e| e["type"] == "message_delta")
                .expect("message_delta");
            assert_eq!(delta["delta"]["stop_reason"], stop, "{reason}");
        }
    }

    #[test]
    fn test_force_finalize_unexpected_tool_call_without_content_emits_api_error() {
        // 终帧无 usage 时 finalize 也会立即报错;force([DONE]/EOF)不得再补空 text + end_turn
        let chunk = json!({"candidates": [{"finishReason": "UNEXPECTED_TOOL_CALL"}]});
        let mut state = GeminiStreamState::default();
        let mut events = finalize_gemini_stream(&chunk, &mut state);
        events.extend(force_finalize_gemini_stream(&mut state));
        assert!(state.api_error_emitted);
        assert_eq!(events.len(), 1, "{events:?}");
        assert_eq!(events[0]["type"], "error");
    }

    #[test]
    fn test_convert_gemini_response_empty_function_call_skipped_with_text() {
        let gemini = json!({
            "candidates": [{
                "content": {"role": "model", "parts": [
                    {"functionCall": {}},
                    {"text": "ok"}
                ]},
                "finishReason": "STOP"
            }]
        });
        let out = convert_gemini_response(&gemini, &HashMap::new(), None, None);
        assert_eq!(out["type"], "message", "{out}");
        assert_eq!(out["stop_reason"], "end_turn");
        let content = out["content"].as_array().expect("content");
        assert_eq!(content.len(), 1, "{out}");
        assert_eq!(content[0]["type"], "text");
    }

    const CONT_SIG: &str = "sig-continuation-0123456789";

    fn tool_first_chunk() -> Value {
        json!({"candidates": [{"content": {"parts": [
            {"functionCall": {"id": "native-r", "name": "read_file", "args": {"path": "a.go"}}}
        ]}}]})
    }

    fn tool_continuation_chunk(finish: bool) -> Value {
        let mut chunk = json!({"candidates": [{"content": {"parts": [
            {"thoughtSignature": CONT_SIG, "functionCall": {"args": {"line": 1}}}
        ]}}]});
        if finish {
            chunk["candidates"][0]["finishReason"] = json!("STOP");
            chunk["usageMetadata"] = json!({"promptTokenCount": 3, "candidatesTokenCount": 2});
        }
        chunk
    }

    fn joined_partial_json(events: &[Value]) -> String {
        events
            .iter()
            .filter(|e| e["delta"]["type"] == "input_json_delta")
            .filter_map(|e| e["delta"]["partial_json"].as_str())
            .collect()
    }

    /// 续传后:唯一 tool_use、参数合并为合法对象、tool 块关闭后紧跟承载续传签名的 carrier。
    fn assert_continuation_shape(events: &[Value]) {
        let tool_starts: Vec<usize> = events
            .iter()
            .enumerate()
            .filter(|(_, e)| {
                e["type"] == "content_block_start" && e["content_block"]["type"] == "tool_use"
            })
            .map(|(i, _)| i)
            .collect();
        assert_eq!(tool_starts.len(), 1, "{events:?}");
        let partial = joined_partial_json(events);
        let parsed: Value = serde_json::from_str(&partial).expect("valid partial_json");
        assert_eq!(parsed, json!({"path": "a.go", "line": 1}));
        let tool_index = events[tool_starts[0]]["index"].clone();
        let stop = events
            .iter()
            .position(|e| e["type"] == "content_block_stop" && e["index"] == tool_index)
            .expect("tool block closed");
        // tool 块内不得插入其它块(续传签名不能提前切断参数流)
        assert!(
            events[tool_starts[0] + 1..stop]
                .iter()
                .all(|e| e["type"] == "content_block_delta"),
            "{events:?}"
        );
        let carrier_index = tool_index.as_u64().unwrap() + 1;
        assert_eq!(events[stop + 1]["type"], "content_block_start");
        assert_eq!(events[stop + 1]["index"], carrier_index);
        assert_eq!(events[stop + 1]["content_block"]["type"], "thinking");
        assert_eq!(events[stop + 2]["delta"]["signature"], CONT_SIG);
        assert_eq!(events[stop + 3]["type"], "content_block_stop");
        assert_eq!(events[stop + 3]["index"], carrier_index);
    }

    /// 对齐 CPA 3a353935 #7908:AG 路径续传(无名 + args + 签名 + STOP)留在原 tool_use 块。
    #[test]
    fn test_stream_signed_continuation_keeps_single_valid_tool_use_antigravity() {
        let model = Some("gemini-3-pro-preview");
        let mut state = GeminiStreamState::default();
        let first = tool_first_chunk();
        let cont = tool_continuation_chunk(true);
        let mut events =
            convert_gemini_stream_chunk(&first, &mut state, &HashMap::new(), model, None);
        events.extend(convert_gemini_stream_chunk(
            &cont,
            &mut state,
            &HashMap::new(),
            model,
            None,
        ));
        events.extend(finalize_gemini_stream(&cont, &mut state));
        assert_continuation_shape(&events);
        assert_eq!(events.last().unwrap()["type"], "message_delta");
        assert_eq!(events.last().unwrap()["delta"]["stop_reason"], "tool_use");
    }

    /// 同上,Gemini 直连(signature_model = None)。
    #[test]
    fn test_stream_signed_continuation_keeps_single_valid_tool_use_gemini_direct() {
        let mut state = GeminiStreamState::default();
        let first = tool_first_chunk();
        let cont = tool_continuation_chunk(true);
        let mut events =
            convert_gemini_stream_chunk(&first, &mut state, &HashMap::new(), None, None);
        events.extend(convert_gemini_stream_chunk(
            &cont,
            &mut state,
            &HashMap::new(),
            None,
            None,
        ));
        events.extend(finalize_gemini_stream(&cont, &mut state));
        assert_continuation_shape(&events);
    }

    /// 上游缺 finishReason 断流([DONE]/EOF force 收尾):仍补 `}` 并发出延后的 carrier。
    #[test]
    fn test_stream_continuation_force_finalize_closes_brace_and_emits_carrier() {
        let model = Some("gemini-3-pro-preview");
        let mut state = GeminiStreamState::default();
        let first = tool_first_chunk();
        let cont = tool_continuation_chunk(false);
        let mut events =
            convert_gemini_stream_chunk(&first, &mut state, &HashMap::new(), model, None);
        events.extend(convert_gemini_stream_chunk(
            &cont,
            &mut state,
            &HashMap::new(),
            model,
            None,
        ));
        assert!(!joined_partial_json(&events).ends_with('}'));
        events.extend(force_finalize_gemini_stream(&mut state));
        assert_continuation_shape(&events);
    }

    /// 续传后紧跟新工具:前一块补 `}` + carrier,新 tool_use 索引顺延不冲突。
    #[test]
    fn test_stream_continuation_then_new_tool_keeps_indices_monotonic() {
        let mut state = GeminiStreamState::default();
        let first = tool_first_chunk();
        let cont = tool_continuation_chunk(false);
        let next = json!({"candidates": [{"content": {"parts": [
            {"functionCall": {"name": "list_dir", "args": {"dir": "."}}}
        ]}, "finishReason": "STOP"}], "usageMetadata": {"promptTokenCount": 1}});
        let mut events =
            convert_gemini_stream_chunk(&first, &mut state, &HashMap::new(), None, None);
        events.extend(convert_gemini_stream_chunk(
            &cont,
            &mut state,
            &HashMap::new(),
            None,
            None,
        ));
        let split = events.len();
        events.extend(convert_gemini_stream_chunk(
            &next,
            &mut state,
            &HashMap::new(),
            None,
            None,
        ));
        events.extend(finalize_gemini_stream(&next, &mut state));
        let first_tool: Vec<Value> = events[..split]
            .iter()
            .cloned()
            .chain(
                events[split..]
                    .iter()
                    .take_while(|e| e["content_block"]["type"] != "tool_use")
                    .cloned(),
            )
            .collect();
        assert_continuation_shape(&first_tool);
        let starts: Vec<u64> = events
            .iter()
            .filter(|e| e["type"] == "content_block_start")
            .map(|e| e["index"].as_u64().unwrap())
            .collect();
        assert_eq!(starts, vec![0, 1, 2], "{events:?}");
    }

    /// 续传块 args 为 null 或空白 name 时不破坏已打开 JSON,也不拆新 tool_use 块。
    #[test]
    fn test_stream_continuation_null_args_and_whitespace_name() {
        let mut state = GeminiStreamState::default();
        let first = json!({"candidates": [{"content": {"parts": [
            {"functionCall": {"name": "read_file", "args": {"path": "a.go"}}}
        ]}}]});
        let null_cont = json!({"candidates": [{"content": {"parts": [
            {"functionCall": {"name": "  ", "args": null}}
        ]}}]});
        let second_cont = json!({"candidates": [{"content": {"parts": [
            {"functionCall": {"name": " ", "args": {"line": 2}}}
        ]}, "finishReason": "STOP"}], "usageMetadata": {"promptTokenCount": 1}});
        let mut events =
            convert_gemini_stream_chunk(&first, &mut state, &HashMap::new(), None, None);
        events.extend(convert_gemini_stream_chunk(
            &null_cont,
            &mut state,
            &HashMap::new(),
            None,
            None,
        ));
        events.extend(convert_gemini_stream_chunk(
            &second_cont,
            &mut state,
            &HashMap::new(),
            None,
            None,
        ));
        events.extend(finalize_gemini_stream(&second_cont, &mut state));
        let tool_starts = events
            .iter()
            .filter(|e| {
                e["type"] == "content_block_start" && e["content_block"]["type"] == "tool_use"
            })
            .count();
        assert_eq!(tool_starts, 1, "{events:?}");
        let parsed: Value = serde_json::from_str(&joined_partial_json(&events))
            .expect("partial_json must stay valid JSON");
        assert_eq!(parsed, json!({"path": "a.go", "line": 2}));
    }
}
