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

use async_stream::stream;
use bytes::Bytes;
use futures::Stream;
use futures::StreamExt;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use super::emit;
use super::parser::SseEvent;
use super::parser::SseParser;
use super::SseStreamPin;
use ccextra_core::convert::{is_valid_gpt_reasoning_signature, is_valid_grok_encrypted_content};
use ccextra_core::doom_loop::{is_confident, parse_trigger};

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

/// 单个 function_call 流式块(对齐 codexFunctionCallStream)
///
/// custom_tool_call 复用同一状态机:`is_custom=true` 时 arguments 存字符串 input
/// (非 JSON),发 tool_use 时包成 `{"input": str}`(对齐响应转换 custom 分支)。
struct FunctionCallStream {
    call_id: String,
    name: String,
    block_index: i64,
    arguments: String,
    emitted_arguments_len: usize,
    has_received_arguments_delta: bool,
    emit_initial_empty_delta: bool,
    is_custom: bool,
    started: bool,
    done: bool,
    closed: bool,
}

impl FunctionCallStream {
    fn new() -> Self {
        Self {
            call_id: String::new(),
            name: String::new(),
            block_index: -1,
            arguments: String::new(),
            emitted_arguments_len: 0,
            has_received_arguments_delta: false,
            emit_initial_empty_delta: false,
            is_custom: false,
            started: false,
            done: false,
            closed: false,
        }
    }
}

/// OpenAI responses → Anthropic 状态机
struct ResponsesRelay {
    message_started: bool,
    finished: bool,
    model: String,
    id: String,
    next_block_index: i64,

    // text 块
    text_open: bool,
    text_index: i64,
    has_text_delta: bool,

    // thinking 块(每个 reasoning item 一个,output_item.done 才关)
    thinking_open: bool,
    thinking_index: i64,
    /// 待收尾的 encrypted_content(signature_delta 用)
    thinking_signature: String,
    thinking_summary_seen: bool,
    /// 当前 reasoning item 是否为 redacted_thinking(根据 encrypted_content 前缀判定)
    thinking_is_redacted: bool,

    /// function_call 流式状态(对齐 ConvertCodexResponseToClaudeParams)
    function_calls: HashMap<String, usize>,
    function_call_queue: Vec<FunctionCallStream>,
    active_function_call: Option<usize>,
    last_function_call: Option<usize>,
    /// 函数调用期间被 defer 的原始事件(空队时重放)
    deferred_stream_events: Vec<SseEvent>,
    has_emitted_tool_use: bool,

    /// web_search_call 去重(对齐 WebSearchToolUseIDs / WebSearchToolResultIDs)
    web_search_tool_use_ids: HashSet<String>,
    web_search_tool_result_ids: HashSet<String>,
    last_web_search_tool_use_id: String,

    /// 工具名还原表 short→original(请求转换侧产出)
    tool_names: Option<Arc<HashMap<String, String>>>,
    /// 入站 body 本地估算输入 token(http.rs 计算;上游流未回真实 usage 时占位)
    estimated_input: Option<usize>,
    /// doom loop 检测:已见触发器 raw label 去重(服务端重发累计集)
    doom_loop_seen: HashSet<String>,
}

impl ResponsesRelay {
    fn new(estimated_input: Option<usize>) -> Self {
        Self {
            message_started: false,
            finished: false,
            model: String::new(),
            id: String::new(),
            next_block_index: 0,
            text_open: false,
            text_index: -1,
            has_text_delta: false,
            thinking_open: false,
            thinking_index: -1,
            thinking_signature: String::new(),
            thinking_summary_seen: false,
            thinking_is_redacted: false,
            function_calls: HashMap::new(),
            function_call_queue: Vec::new(),
            active_function_call: None,
            last_function_call: None,
            deferred_stream_events: Vec::new(),
            has_emitted_tool_use: false,
            web_search_tool_use_ids: HashSet::new(),
            web_search_tool_result_ids: HashSet::new(),
            last_web_search_tool_use_id: String::new(),
            tool_names: None,
            estimated_input,
            doom_loop_seen: HashSet::new(),
        }
    }

    fn with_tool_names(mut self, tool_names: Option<Arc<HashMap<String, String>>>) -> Self {
        self.tool_names = tool_names;
        self
    }

    /// 工具名还原:short → original(对齐 resolveCodexClaudeToolUseName)
    fn resolve_tool_name(&self, name: &str) -> String {
        if let Some(rev) = &self.tool_names {
            if let Some(orig) = rev.get(name) {
                return orig.clone();
            }
        }
        name.to_string()
    }

    /// 处理一个 SSE 事件,产出 anthropic 字节事件
    fn process(&mut self, ev: &super::parser::SseEvent) -> Vec<Bytes> {
        // 已收尾:忽略后续事件,避免 error 后再输出正常收尾事件。
        if self.finished {
            return Vec::new();
        }
        let root: Value = match serde_json::from_str(&ev.data) {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        };
        let event_type = root.get("type").and_then(|v| v.as_str()).unwrap_or("");

        // 函数调用进行中,非关键事件 defer(对齐 shouldDeferCodexStreamEvent):
        // 避免流式 function_call 的 start/delta 与文本/思考块交错
        if self.active_function_call.is_some() && should_defer_stream_event(&root, event_type) {
            self.deferred_stream_events.push(ev.clone());
            return Vec::new();
        }

        match event_type {
            "error" => {
                self.finished = true;
                vec![stream_error_frame(&root)]
            }
            // Grok doom loop 检测事件(对齐 grok-build DoomLoopSignalCollector):
            // 吞掉不转发(非标准事件),置信信号立即中断流,CC 自动重试即重采样
            "response.doom_loop_check" => self.process_doom_loop_check(&root),
            // OpenAI Responses 终态错误事件:error 嵌在 response.error,提升为顶层复用映射
            "response.failed" => {
                self.finished = true;
                let mut failed_root = root.clone();
                if failed_root.get("error").is_none() {
                    if let Some(err) = failed_root.pointer("/response/error") {
                        failed_root["error"] = err.clone();
                    }
                }
                vec![stream_error_frame(&failed_root)]
            }
            "response.created" => {
                self.update_identity(root.get("response"));
                self.ensure_started()
            }
            "response.reasoning_summary_part.added" => {
                let mut out = self.stop_text();
                // Codex 一个 reasoning item 拆多个 summary part,块保持打开,
                // part 之间空行分隔,signature 只在 output_item.done 发一次
                if self.thinking_open {
                    out.extend(self.thinking_delta(SUMMARY_PART_SEPARATOR));
                } else {
                    // 根据 thinking_is_redacted 标志打开对应类型的块
                    if self.thinking_is_redacted {
                        out.extend(self.start_redacted_thinking());
                    } else {
                        out.extend(self.start_thinking());
                    }
                }
                self.thinking_summary_seen = true;
                out
            }
            "response.reasoning_summary_text.delta" => self.plaintext_reasoning_delta(&root),
            "response.reasoning_summary_text.done" => {
                // 对齐 CPA codex_openai_response.go:126-133:
                // reasoning_summary_text.done 发 "\n\n" 分隔 summary 与后续内容
                self.thinking_summary_seen = true;
                if self.thinking_open {
                    self.thinking_delta(SUMMARY_PART_SEPARATOR)
                } else {
                    Vec::new()
                }
            }
            // 不关 thinking 块:等 output_item.done 带最终 encrypted_content
            "response.reasoning_summary_part.done" => {
                self.thinking_summary_seen = true;
                Vec::new()
            }
            // 明文 reasoning 事件兜底(对齐 CPA xaiNormalizeReasoningSummaryData:
            // reasoning_text.delta 归一到 summary 语义,不得静默丢弃)。xAI/OpenAI
            // 明文推理流都可走这里,完整推理内容同 summary 进 thinking 块。
            "response.reasoning_text.delta" => self.plaintext_reasoning_delta(&root),
            "response.reasoning_text.done" => {
                // 对齐 CPA:reasoning_text.done 同样发 "\n\n" 分隔
                self.thinking_summary_seen = true;
                if self.thinking_open {
                    self.thinking_delta(SUMMARY_PART_SEPARATOR)
                } else {
                    Vec::new()
                }
            }
            "response.content_part.added" => {
                // 明文 reasoning part(part.type = reasoning,对齐 OpenAI Responses
                // 明文推理的 content_part 形状)与 summary part 同语义:
                // 块保持打开,分隔符续接
                let part_type = root.pointer("/part/type").and_then(|v| v.as_str());
                if part_type == Some("reasoning") {
                    let mut out = self.stop_text();
                    if self.thinking_open {
                        out.extend(self.thinking_delta(SUMMARY_PART_SEPARATOR));
                    } else {
                        // 根据 thinking_is_redacted 标志打开对应类型的块
                        if self.thinking_is_redacted {
                            out.extend(self.start_redacted_thinking());
                        } else {
                            out.extend(self.start_thinking());
                        }
                    }
                    self.thinking_summary_seen = true;
                    return out;
                }
                let mut out = self.finalize_thinking();
                if part_type == Some("output_text") {
                    out.extend(self.start_text());
                }
                out
            }
            "response.output_text.delta" => {
                let delta = root.get("delta").and_then(|v| v.as_str()).unwrap_or("");
                if delta.is_empty() {
                    return Vec::new();
                }
                self.has_text_delta = true;
                let mut out = self.finalize_thinking();
                out.extend(self.start_text());
                out.extend(self.text_delta(delta));
                out
            }
            "response.content_part.done" => {
                if root.pointer("/part/type").and_then(|v| v.as_str()) == Some("output_text") {
                    self.stop_text()
                } else {
                    Vec::new()
                }
            }
            "response.output_item.added" => {
                let item = root.get("item");
                let item_type = item
                    .and_then(|i| i.get("type"))
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                match item_type {
                    "reasoning" => {
                        let mut out = self.stop_text();
                        // 上一个没 done 的 reasoning item 不得泄漏未关块
                        out.extend(self.finalize_thinking());
                        self.thinking_summary_seen = false;
                        // 兜底快照:仅当 output_item.done 不带 encrypted_content 时用
                        self.thinking_signature = item
                            .and_then(|i| i.get("encrypted_content"))
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string();
                        // 检测是否 redacted_thinking(根据 encrypted_content 前缀)
                        self.thinking_is_redacted = self
                            .thinking_signature
                            .starts_with(CLAUDE_RESPONSES_REDACTED_THINKING_PREFIX);
                        out
                    }
                    "function_call" | "custom_tool_call" => {
                        // 对齐 output_item.added(function_call):先关 thinking/text,
                        // 登记调用,有名字的发初始空 delta,随后走队列
                        let is_custom = item_type == "custom_tool_call";
                        let mut out = self.finalize_thinking();
                        out.extend(self.stop_text());
                        let idx = self.record_function_call(item, Some(&root));
                        if let Some(i) = idx {
                            self.update_function_call_identity(i, item, Some(&root));
                            self.function_call_queue[i].is_custom = is_custom;
                            // custom 工具 input 可能是字符串,包成 {"input": str} 回放
                            if is_custom {
                                if let Some(input) =
                                    item.and_then(|i| i.get("input")).and_then(|v| v.as_str())
                                {
                                    if !input.is_empty() {
                                        self.function_call_queue[i].arguments = input.to_string();
                                        self.function_call_queue[i].has_received_arguments_delta =
                                            true;
                                    }
                                }
                            }
                            if !self.function_call_queue[i].name.is_empty() {
                                self.function_call_queue[i].emit_initial_empty_delta = true;
                            }
                        }
                        out.extend(self.append_function_call_queue());
                        if self.function_call_queue.is_empty() {
                            out.extend(self.append_deferred_events());
                        }
                        out
                    }
                    // web_search_call 的 server_tool_use 等 output_item.done 带 query/results
                    _ => Vec::new(),
                }
            }
            "response.output_item.done" => self.output_item_done(&root),
            "response.function_call_arguments.delta" => {
                let idx = self.record_function_call(None, Some(&root));
                if let Some(i) = idx {
                    let delta = root.get("delta").and_then(|v| v.as_str()).unwrap_or("");
                    self.function_call_queue[i].arguments.push_str(delta);
                    self.function_call_queue[i].has_received_arguments_delta = true;
                }
                self.append_buffered_arguments()
            }
            "response.function_call_arguments.done" => {
                let idx = self.record_function_call(None, Some(&root));
                if let Some(i) = idx {
                    let args = root.get("arguments").and_then(|v| v.as_str()).unwrap_or("");
                    let call = &mut self.function_call_queue[i];
                    if !call.has_received_arguments_delta || args.starts_with(&call.arguments) {
                        call.arguments = args.to_string();
                    }
                }
                self.append_buffered_arguments()
            }
            // custom 工具 input 流(custom_tool_call_input delta/done)
            "response.custom_tool_call_input.delta" => {
                let idx = self.record_function_call(None, Some(&root));
                if let Some(i) = idx {
                    let delta = root.get("delta").and_then(|v| v.as_str()).unwrap_or("");
                    let call = &mut self.function_call_queue[i];
                    call.is_custom = true;
                    call.arguments.push_str(delta);
                    call.has_received_arguments_delta = true;
                }
                self.append_buffered_arguments()
            }
            "response.custom_tool_call_input.done" => {
                let idx = self.record_function_call(None, Some(&root));
                if let Some(i) = idx {
                    let input = root.get("input").and_then(|v| v.as_str()).unwrap_or("");
                    let call = &mut self.function_call_queue[i];
                    call.is_custom = true;
                    if !call.has_received_arguments_delta || input.starts_with(&call.arguments) {
                        call.arguments = input.to_string();
                    }
                }
                self.append_buffered_arguments()
            }
            "response.completed" | "response.incomplete" => {
                let response = root.get("response");
                self.update_identity(response);
                // 终态响应对象上的 doom_loop_check 字段(对齐 grok-build 双路报告)
                if let Some(out) = self.check_terminal_doom_loop(response) {
                    return out;
                }
                let mut out = self.finalize_thinking();
                out.extend(self.stop_text());
                out.extend(self.append_function_calls_from_terminal(response));
                out.extend(self.append_deferred_events());
                out.extend(self.finalize_thinking());
                // finalize 内部负责 synthesize_empty_text_block + stop_text + message_delta
                out.extend(self.finalize(response));
                out
            }
            _ => Vec::new(),
        }
    }

    /// doom loop 事件处理:解析触发器,置信即中断流(best-effort 永不报错)
    ///
    /// 对齐 grok-build DoomLoopSignalCollector.absorb + is_confident:
    /// - 触发器按 raw label 去重(服务端重发累计集)
    /// - 置信(thinking channel tail_repetition ≤ 64)→ 发 anthropic error 中断,
    ///   CC 自动重试 = 免费获得重采样
    /// - 非置信只记 warn 日志,流照常转发
    fn process_doom_loop_check(&mut self, root: &Value) -> Vec<Bytes> {
        let triggers = root
            .pointer("/doom_loop_check/triggers")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|t| t.as_str().map(String::from))
                    .collect::<Vec<_>>()
            })
            .unwrap_or_default();
        self.absorb_doom_loop_triggers(&triggers)
    }

    /// 终态响应对象上的 doom_loop_check 字段(双路报告第二处)
    fn check_terminal_doom_loop(&mut self, response: Option<&Value>) -> Option<Vec<Bytes>> {
        let triggers = response?
            .pointer("/doom_loop_check/triggers")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|t| t.as_str().map(String::from))
                    .collect::<Vec<_>>()
            })?;
        let out = self.absorb_doom_loop_triggers(&triggers);
        if out.is_empty() {
            None
        } else {
            Some(out)
        }
    }

    /// 吸收触发器集:去重、解析、置信判定;置信返回中断帧
    fn absorb_doom_loop_triggers(&mut self, triggers: &[String]) -> Vec<Bytes> {
        for raw in triggers {
            if self.doom_loop_seen.contains(raw) {
                continue;
            }
            self.doom_loop_seen.insert(raw.clone());
            let signal = parse_trigger(raw);
            if is_confident(&signal) {
                tracing::warn!(trigger = raw, "doom loop 置信信号,中断流触发 CC 重采样");
                // 复用流中断 error 路径:CC 收到 error 事件自动重试
                return self.stream_error(&format!("doom loop detected: {raw}"));
            }
            tracing::warn!(trigger = raw, "doom loop 非置信信号,warn-only");
        }
        Vec::new()
    }

    /// output_item.done 分派(message 文本兜底 / reasoning 收尾)
    fn output_item_done(&mut self, root: &Value) -> Vec<Bytes> {
        let item = match root.get("item") {
            Some(i) => i,
            None => return Vec::new(),
        };
        let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match item_type {
            "message" => {
                if self.has_text_delta {
                    return Vec::new();
                }
                // 无 delta 流时从 item.content 补发文本(同兜底)
                let mut text = String::new();
                if let Some(parts) = item.get("content").and_then(|v| v.as_array()) {
                    for part in parts {
                        if part.get("type").and_then(|v| v.as_str()) == Some("output_text") {
                            if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                                text.push_str(t);
                            }
                        }
                    }
                }
                if text.is_empty() {
                    return Vec::new();
                }
                let mut out = self.finalize_thinking();
                out.extend(self.start_text());
                out.extend(self.text_delta(&text));
                out.extend(self.stop_text());
                self.has_text_delta = true;
                out
            }
            "reasoning" => {
                let mut out = self.stop_text();
                if let Some(sig) = item.get("encrypted_content").and_then(|v| v.as_str()) {
                    if sig.is_empty() {
                        // 空签名跳过
                    } else {
                        // 检测是否 redacted_thinking(根据 encrypted_content 前缀)
                        let is_redacted =
                            sig.starts_with(CLAUDE_RESPONSES_REDACTED_THINKING_PREFIX);

                        // redacted_thinking 直接通过,普通签名验证 GPT Fernet 或 Grok 格式
                        let valid = is_redacted
                            || is_valid_gpt_reasoning_signature(sig)
                            || is_valid_grok_encrypted_content(sig);

                        if valid {
                            self.thinking_signature = sig.to_string();

                            // 如果块已打开且类型不匹配，关闭旧块并用正确类型重开
                            // (防御性处理：summary delta 先于 output_item 到达的边缘情况)
                            if self.thinking_open && is_redacted != self.thinking_is_redacted {
                                out.push(emit::content_block_stop(self.thinking_index));
                                self.thinking_open = false;
                                self.thinking_index += 1;
                                if is_redacted {
                                    out.extend(self.start_redacted_thinking());
                                } else {
                                    out.extend(self.start_thinking());
                                }
                            }

                            self.thinking_is_redacted = is_redacted;
                        }
                    }
                }
                if self.thinking_summary_seen {
                    out.extend(self.finalize_thinking());
                } else {
                    out.extend(self.finalize_signature_only_thinking());
                }
                self.thinking_signature.clear();
                self.thinking_summary_seen = false;
                self.thinking_is_redacted = false;
                out
            }
            "function_call" | "custom_tool_call" => {
                // 对齐 output_item.done(function_call):关块、补身份与参数、标 done
                let is_custom = item_type == "custom_tool_call";
                let mut out = self.finalize_thinking();
                out.extend(self.stop_text());
                let idx = self.record_function_call(Some(item), Some(root));
                if let Some(i) = idx {
                    self.update_function_call_identity(i, Some(item), Some(root));
                    self.function_call_queue[i].is_custom = is_custom;
                    let args = if is_custom {
                        item.get("input")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string()
                    } else {
                        item.get("arguments")
                            .and_then(|v| v.as_str())
                            .unwrap_or("")
                            .to_string()
                    };
                    let call = &mut self.function_call_queue[i];
                    if !call.has_received_arguments_delta || args.starts_with(&call.arguments) {
                        call.arguments = args.to_string();
                    }
                    call.done = true;
                }
                out.extend(self.append_function_call_queue());
                if self.function_call_queue.is_empty() {
                    out.extend(self.append_deferred_events());
                }
                out
            }
            "web_search_call" => {
                // output_item.done 带全量 query/results 才发 server_tool_use + result
                self.append_web_search_tool_result(root, item)
            }
            _ => Vec::new(),
        }
    }

    fn update_identity(&mut self, response: Option<&Value>) {
        if let Some(r) = response {
            if let Some(id) = r.get("id").and_then(|v| v.as_str()) {
                if !id.is_empty() {
                    self.id = id.to_string();
                }
            }
            if let Some(model) = r.get("model").and_then(|v| v.as_str()) {
                if !model.is_empty() {
                    self.model = model.to_string();
                }
            }
        }
    }

    /// 确保 message_start 已发(model 空时兜底)
    fn ensure_started(&mut self) -> Vec<Bytes> {
        if self.message_started {
            return Vec::new();
        }
        self.message_started = true;
        let model = if self.model.is_empty() {
            FALLBACK_MODEL
        } else {
            self.model.as_str()
        };
        vec![emit::message_start(
            &self.id,
            model,
            self.estimated_input.unwrap_or(1) as i64,
            0,
            true,
        )]
    }

    fn start_text(&mut self) -> Vec<Bytes> {
        if self.text_open {
            return Vec::new();
        }
        self.text_index = self.next_block_index;
        self.next_block_index += 1;
        self.text_open = true;
        vec![emit::content_block_start_text(self.text_index)]
    }

    fn stop_text(&mut self) -> Vec<Bytes> {
        if !self.text_open {
            return Vec::new();
        }
        self.text_open = false;
        vec![emit::content_block_stop(self.text_index)]
    }

    fn text_delta(&self, text: &str) -> Vec<Bytes> {
        vec![emit::content_block_delta_text(self.text_index, text)]
    }

    fn start_thinking(&mut self) -> Vec<Bytes> {
        if self.thinking_open {
            return Vec::new();
        }
        self.thinking_index = self.next_block_index;
        self.next_block_index += 1;
        self.thinking_open = true;
        vec![emit::content_block_start_thinking(self.thinking_index)]
    }

    fn thinking_delta(&self, text: &str) -> Vec<Bytes> {
        if text.is_empty() || !self.thinking_open {
            return Vec::new();
        }
        vec![emit::content_block_delta_thinking(
            self.thinking_index,
            text,
        )]
    }

    /// reasoning delta 统一进块:先收 text,再开 thinking 块发内容
    /// (reasoning_summary_text.delta 与明文 reasoning_text.delta 共用)
    fn plaintext_reasoning_delta(&mut self, root: &Value) -> Vec<Bytes> {
        let delta = root.get("delta").and_then(|v| v.as_str()).unwrap_or("");
        let mut out = self.stop_text();
        // 根据 thinking_is_redacted 标志打开对应类型的块
        if self.thinking_is_redacted {
            out.extend(self.start_redacted_thinking());
        } else {
            out.extend(self.start_thinking());
        }
        out.extend(self.thinking_delta(delta));
        self.thinking_summary_seen = true;
        out
    }

    /// 关闭 thinking 块:有 signature 先发 signature_delta(加密内容回放闭环)
    /// 若 signature 为 redacted_thinking 载荷(带前缀),发 redacted_thinking 块而非 thinking
    fn finalize_thinking(&mut self) -> Vec<Bytes> {
        if !self.thinking_open {
            return Vec::new();
        }
        let mut out = Vec::new();
        // 检测 redacted_thinking 载荷(对齐 responsesRedactedThinkingData)
        if let Some(data) = self
            .thinking_signature
            .strip_prefix(CLAUDE_RESPONSES_REDACTED_THINKING_PREFIX)
        {
            if !data.is_empty() {
                // 发送 redacted_thinking 的 data 字段
                out.push(emit::content_block_delta_redacted_thinking_data(
                    self.thinking_index,
                    data,
                ));
            }
        } else if !self.thinking_signature.is_empty() {
            // 普通 thinking 签名
            out.push(emit::content_block_delta_signature(
                self.thinking_index,
                &self.thinking_signature,
            ));
        }
        out.push(emit::content_block_stop(self.thinking_index));
        self.thinking_open = false;
        out
    }

    /// 无 summary 只有 encrypted_content 的 reasoning item:开块即收尾
    /// 检测 redacted_thinking 载荷,开 redacted_thinking 块而非 thinking 块
    fn finalize_signature_only_thinking(&mut self) -> Vec<Bytes> {
        if self.thinking_signature.is_empty() {
            return Vec::new();
        }
        // 检测是否 redacted_thinking
        let is_redacted = self
            .thinking_signature
            .starts_with(CLAUDE_RESPONSES_REDACTED_THINKING_PREFIX);
        let mut out = if is_redacted {
            self.start_redacted_thinking()
        } else {
            self.start_thinking()
        };
        out.extend(self.finalize_thinking());
        out
    }

    /// 开 redacted_thinking 块(对齐 content_block_start redacted_thinking)
    fn start_redacted_thinking(&mut self) -> Vec<Bytes> {
        if self.thinking_open {
            return Vec::new();
        }
        self.thinking_index = self.next_block_index;
        self.next_block_index += 1;
        self.thinking_open = true;
        vec![emit::content_block_start_redacted_thinking(
            self.thinking_index,
        )]
    }

    // ---- function_call 流式状态机 ----

    /// 事件 → 候选 key 列表(output_index / call_id / item_id;对齐 codexFunctionCallKeys)
    fn call_keys(&self, root: Option<&Value>, item: Option<&Value>) -> Vec<String> {
        let mut keys: Vec<String> = Vec::new();
        let push = |k: String, keys: &mut Vec<String>| {
            if !k.is_empty() && !keys.contains(&k) {
                keys.push(k);
            }
        };
        if let Some(r) = root {
            if let Some(oi) = r.get("output_index") {
                push(format!("output:{}", oi), &mut keys);
            }
            if let Some(ci) = r.get("call_id").and_then(|v| v.as_str()) {
                push(format!("call:{ci}"), &mut keys);
            }
            if let Some(ii) = r.get("item_id").and_then(|v| v.as_str()) {
                push(format!("item:{ii}"), &mut keys);
            }
        }
        if let Some(i) = item {
            if let Some(ci) = i.get("call_id").and_then(|v| v.as_str()) {
                push(format!("call:{ci}"), &mut keys);
            }
            if let Some(ii) = i.get("id").and_then(|v| v.as_str()) {
                push(format!("item:{ii}"), &mut keys);
            }
        }
        keys
    }

    /// 按 keys 找已有调用(对齐 codexFunctionCallForKeys)
    fn function_call_for_keys(&self, keys: &[String]) -> Option<usize> {
        for k in keys {
            if let Some(&i) = self.function_calls.get(k) {
                return Some(i);
            }
        }
        None
    }

    /// 登记调用(无则新建入队),登记别名(对齐 recordCodexFunctionCall)
    fn record_function_call(
        &mut self,
        item: Option<&Value>,
        root: Option<&Value>,
    ) -> Option<usize> {
        let keys = self.call_keys(root, item);
        let idx = if !keys.is_empty() {
            // keys 非空:只按 keys 匹配,miss 则新建(对齐 codexFunctionCallForKeys)
            match self.function_call_for_keys(&keys) {
                Some(i) => i,
                None => {
                    self.function_call_queue.push(FunctionCallStream::new());
                    self.function_call_queue.len() - 1
                }
            }
        } else {
            // keys 为空(delta 事件通常只带 delta)回退到 last(对齐 codexFunctionCallForEvent fallback)
            match self.last_function_call {
                Some(i) => i,
                None => {
                    self.function_call_queue.push(FunctionCallStream::new());
                    self.function_call_queue.len() - 1
                }
            }
        };
        for k in &keys {
            self.function_calls.insert(k.clone(), idx);
        }
        self.last_function_call = Some(idx);
        Some(idx)
    }

    /// 从事件补齐 call_id / name(对齐 updateCodexFunctionCallIdentity)
    fn update_function_call_identity(
        &mut self,
        idx: usize,
        item: Option<&Value>,
        root: Option<&Value>,
    ) {
        let call = &mut self.function_call_queue[idx];
        if let Some(i) = item {
            if let Some(cid) = i.get("call_id").and_then(|v| v.as_str()) {
                if !cid.is_empty() {
                    call.call_id = cid.to_string();
                }
            }
            if let Some(n) = i.get("name").and_then(|v| v.as_str()) {
                if !n.is_empty() {
                    call.name = n.to_string();
                }
            }
        }
        let keys = self.call_keys(root, item);
        for k in &keys {
            self.function_calls.insert(k.clone(), idx);
        }
    }

    /// 追发未发完的参数片段(对齐 appendCodexFunctionCallBufferedArguments)
    fn append_buffered_arguments(&mut self) -> Vec<Bytes> {
        let Some(active) = self.active_function_call else {
            return Vec::new();
        };
        let call = &mut self.function_call_queue[active];
        if !call.started || call.closed {
            return Vec::new();
        }
        // custom 工具:input 是字符串,包成 {"input": str} 一次性发(不逐段流)
        if call.is_custom {
            if !call.done || call.emitted_arguments_len >= call.arguments.len() {
                return Vec::new();
            }
            let wrapped = custom_input_json(&call.arguments);
            call.emitted_arguments_len = call.arguments.len();
            return vec![function_call_argument_delta(&wrapped, call.block_index)];
        }
        if call.emitted_arguments_len >= call.arguments.len() {
            return Vec::new();
        }
        let delta = call.arguments[call.emitted_arguments_len..].to_string();
        call.emitted_arguments_len = call.arguments.len();
        vec![function_call_argument_delta(&delta, call.block_index)]
    }

    /// 刷新队列:收尾 done 的 active,逐个启动新调用,发参数片段
    /// (对齐 appendCodexFunctionCallQueue)
    fn append_function_call_queue(&mut self) -> Vec<Bytes> {
        let mut out = Vec::new();

        loop {
            // 当前 active:先 flush 参数,再按 done 收尾
            if let Some(active) = self.active_function_call {
                out.extend(self.append_buffered_arguments());
                if !self.function_call_queue[active].done {
                    return out;
                }
                let block_index = self.function_call_queue[active].block_index;
                out.push(content_block_stop(block_index));
                if self.next_block_index <= block_index {
                    self.next_block_index = block_index + 1;
                }
                self.function_call_queue[active].closed = true;
                self.active_function_call = None;
            }

            // 跳过已关闭项(不 remove,保持 map 索引稳定;completed 后统一 clear)
            let mut pos = 0;
            while pos < self.function_call_queue.len() && self.function_call_queue[pos].closed {
                pos += 1;
            }
            if pos >= self.function_call_queue.len() {
                return out;
            }
            let idx = pos;
            if self.function_call_queue[idx].name.is_empty() {
                return out;
            }

            let block_index = self.next_block_index;
            self.next_block_index += 1;
            let call_id = self.function_call_queue[idx].call_id.clone();
            let name = self.resolve_tool_name(&self.function_call_queue[idx].name);
            let emit_empty = self.function_call_queue[idx].emit_initial_empty_delta;
            out.push(function_call_start(&call_id, &name, block_index));
            if emit_empty {
                out.push(function_call_argument_delta("", block_index));
            }
            self.function_call_queue[idx].block_index = block_index;
            self.function_call_queue[idx].started = true;
            self.active_function_call = Some(idx);
            self.has_emitted_tool_use = true;
            out.extend(self.append_buffered_arguments());
        }
    }

    /// completed/incomplete 时从 response.output 补齐函数调用
    /// (对齐 appendCodexFunctionCallsFromTerminal:避免工具调用在非流式/迟到场景丢失)
    fn append_function_calls_from_terminal(&mut self, response: Option<&Value>) -> Vec<Bytes> {
        if let Some(output) = response
            .and_then(|r| r.get("output"))
            .and_then(|v| v.as_array())
        {
            for (i, item) in output.iter().enumerate() {
                let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
                if item_type != "function_call" && item_type != "custom_tool_call" {
                    continue;
                }
                let is_custom = item_type == "custom_tool_call";
                let mut keys = self.call_keys(None, Some(item));
                if let Some(oi) = item.get("output_index") {
                    push_key(&mut keys, format!("output:{}", oi));
                }
                push_key(&mut keys, format!("output:{i}"));
                let idx = match self.function_call_for_keys(&keys) {
                    Some(i) => i,
                    None => {
                        self.function_call_queue.push(FunctionCallStream::new());
                        self.function_call_queue.len() - 1
                    }
                };
                for k in &keys {
                    self.function_calls.insert(k.clone(), idx);
                }
                let args = if is_custom {
                    item.get("input")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string()
                } else {
                    item.get("arguments")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string()
                };
                self.update_function_call_identity(idx, Some(item), None);
                let call = &mut self.function_call_queue[idx];
                call.is_custom = is_custom;
                if !call.has_received_arguments_delta || args.starts_with(&call.arguments) {
                    call.arguments = args.to_string();
                }
                call.done = true;
            }
        }

        // 收口:未关闭且无名的调用直接关闭;其余标 done(不重建队列,索引保持稳定)
        for call in self.function_call_queue.iter_mut() {
            if call.closed {
                continue;
            }
            if call.name.is_empty() {
                call.closed = true;
                continue;
            }
            call.done = true;
        }
        let out = self.append_function_call_queue();
        self.clear_function_calls();
        out
    }

    /// 清空调用状态(对齐 clearCodexFunctionCalls,completed 后调用)
    fn clear_function_calls(&mut self) {
        self.function_calls.clear();
        self.function_call_queue.clear();
        self.active_function_call = None;
        self.last_function_call = None;
    }

    /// 重放 defer 的事件(对齐 appendDeferredCodexStreamEvents)
    fn append_deferred_events(&mut self) -> Vec<Bytes> {
        if self.deferred_stream_events.is_empty() {
            return Vec::new();
        }
        let events = std::mem::take(&mut self.deferred_stream_events);
        let mut out = Vec::new();
        for ev in &events {
            out.extend(self.process(ev));
        }
        out
    }

    // ---- web_search_call 转换 ----

    /// web_search_call 事件 → server_tool_use + web_search_tool_result
    fn append_web_search_tool_result(&mut self, root: &Value, item: &Value) -> Vec<Bytes> {
        let tool_use_id = self.web_search_tool_use_id(root, item);
        if tool_use_id.is_empty() {
            return Vec::new();
        }
        let mut out = Vec::new();
        out.extend(self.append_web_search_server_tool_use(root, item));

        if self.web_search_tool_result_ids.contains(&tool_use_id) {
            return vec![];
        }
        let query = web_search_query(root, item);
        let result_content = web_search_result_content(root, item);
        let has_action = item.get("action").is_some();
        if query.is_empty() && result_content.is_empty() && !has_action {
            return out;
        }

        let result_content = if result_content.is_empty() {
            json!([])
        } else {
            Value::Array(result_content)
        };
        out.push(emit::content_block_start_web_search_result(
            self.next_block_index,
            &tool_use_id,
            &result_content,
        ));
        out.push(content_block_stop(self.next_block_index));
        self.web_search_tool_result_ids.insert(tool_use_id.clone());
        self.next_block_index += 1;
        if tool_use_id == self.last_web_search_tool_use_id {
            self.last_web_search_tool_use_id.clear();
        }
        out
    }

    /// server_tool_use 块(去重;query 走 input_json_delta)
    fn append_web_search_server_tool_use(&mut self, root: &Value, item: &Value) -> Vec<Bytes> {
        let tool_use_id = self.web_search_tool_use_id(root, item);
        if tool_use_id.is_empty() {
            return Vec::new();
        }
        let query = web_search_query(root, item);
        let already_started = self.web_search_tool_use_ids.contains(&tool_use_id);
        if already_started && query.is_empty() {
            return Vec::new();
        }

        let mut out = Vec::new();
        if !already_started {
            out.extend(self.stop_text());
            out.extend(self.finalize_thinking());
            out.push(emit::content_block_start_server_tool_use(
                self.next_block_index,
                &tool_use_id,
                "web_search",
            ));
        }
        if !query.is_empty() {
            let partial_json = serde_json::to_string(&json!({"query": query})).unwrap_or_default();
            out.push(function_call_argument_delta(
                &partial_json,
                self.next_block_index,
            ));
        }
        if !already_started {
            out.push(content_block_stop(self.next_block_index));
            self.web_search_tool_use_ids.insert(tool_use_id);
            self.next_block_index += 1;
        }
        out
    }

    /// web_search_call 的 id 提取(对齐 codexWebSearchToolUseID:item/root 多路径 + last 兜底)
    fn web_search_tool_use_id(&mut self, root: &Value, item: &Value) -> String {
        for path in ["id", "output_item_id", "call_id", "item_id"] {
            if let Some(v) = item.get(path).and_then(|v| v.as_str()) {
                if !v.trim().is_empty() {
                    return v.trim().to_string();
                }
            }
            if let Some(v) = root.get(path).and_then(|v| v.as_str()) {
                if !v.trim().is_empty() {
                    return v.trim().to_string();
                }
            }
        }
        if !self.last_web_search_tool_use_id.is_empty() {
            return self.last_web_search_tool_use_id.clone();
        }
        format!("web_search_{}", self.next_block_index)
    }

    /// 空轮次/纯思考轮次合成空 text 块
    /// (对齐 synthesizeCodexEmptyTextBlock:Claude 客户端遇零块消息报
    /// "Content block not found")
    fn synthesize_empty_text_block(&mut self) -> Vec<Bytes> {
        if self.text_open || self.has_text_delta || self.has_emitted_tool_use || self.thinking_open
        {
            return Vec::new();
        }
        let mut out = self.start_text();
        out.extend(self.stop_text());
        out
    }

    /// message_delta + message_stop(usage 扣 cached,stop_reason 走统一映射)
    fn finalize(&mut self, response: Option<&Value>) -> Vec<Bytes> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        let mut out = self.ensure_started();
        // synthesize 内部已含 start+stop;此处前一个 stop_text 关已开 text 块
        out.extend(self.finalize_thinking());
        out.extend(self.stop_text());
        out.extend(self.synthesize_empty_text_block());

        // usage(对齐 extractResponsesUsage:cached 从 input 扣除;cache_write
        // 映射为 cache_creation_input_tokens,对齐 CPA 893abbab)
        let (input_tokens, output_tokens, cached, cache_write) = response
            .and_then(|r| r.get("usage"))
            .map(super::extract_usage_responses)
            .unwrap_or((0, 0, 0, 0));

        let stop_seq = response.and_then(stop_sequence);
        let raw_reason = response.map(codex_stop_reason).unwrap_or_default();
        let stop_reason = map_stop_reason(&raw_reason, self.has_emitted_tool_use);

        out.push(emit::message_delta(
            &stop_reason,
            stop_seq.as_deref(),
            input_tokens,
            output_tokens,
            cached,
            cache_write,
        ));
        out.push(emit::message_stop());
        out
    }

    /// EOF 兜底:未收到 Responses 终态时显式报 error,不把半个回答包装成正常完成。
    fn finish(&mut self) -> Vec<Bytes> {
        if self.finished {
            return Vec::new();
        }
        // completed/incomplete 分支必已 finalize 置 finished,到此即残缺
        self.stream_error("upstream stream ended before response completion")
    }

    /// 上游流中断:发 Anthropic error 事件,不伪造 message_start。
    fn stream_error(&mut self, message: &str) -> Vec<Bytes> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        vec![emit::error_event(message)]
    }
}

/// 流内 error 事件 → anthropic error(对齐 codexStreamErrorToClaudeError)
fn stream_error_frame(root: &Value) -> Bytes {
    let error = root.get("error");
    let mut err_type = error
        .and_then(|e| e.get("type"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if err_type.is_empty() {
        err_type = root
            .get("error_type")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
    }
    if err_type.is_empty() {
        err_type = "api_error".to_string();
    }
    let code = error
        .and_then(|e| e.get("code"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let mut message = error
        .and_then(|e| e.get("message"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if message.is_empty() {
        message = root
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
    }
    if message.is_empty() {
        message = code.clone();
    }
    if message.is_empty() {
        message = err_type.clone();
    }
    if code == "cyber_policy" || err_type == "invalid_request" {
        err_type = "invalid_request_error".to_string();
    }
    emit::error_event_typed(&err_type, &message)
}

/// stop_reason 提取(对齐 codexStopReason):
/// stop_reason > incomplete_details.reason > stop_sequence 推断
pub(super) fn codex_stop_reason(r: &Value) -> String {
    if let Some(sr) = r.get("stop_reason").and_then(|v| v.as_str()) {
        if !sr.is_empty() {
            if sr == "stop" && stop_sequence(r).is_some() {
                return "stop_sequence".to_string();
            }
            return sr.to_string();
        }
    }
    if let Some(reason) = r
        .pointer("/incomplete_details/reason")
        .and_then(|v| v.as_str())
    {
        if !reason.is_empty() {
            return reason.to_string();
        }
    }
    if stop_sequence(r).is_some() {
        return "stop_sequence".to_string();
    }
    String::new()
}

pub(super) fn stop_sequence(r: &Value) -> Option<String> {
    r.get("stop_sequence")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// stop_reason → anthropic(对齐 mapCodexStopReasonToClaude)
pub(super) fn map_stop_reason(stop_reason: &str, has_tool_use: bool) -> String {
    if has_tool_use {
        return "tool_use".to_string();
    }
    match stop_reason {
        "" | "stop" | "completed" => "end_turn".to_string(),
        "max_tokens" | "max_output_tokens" | "max_prompt_tokens" | "max_time_limit" => {
            "max_tokens".to_string()
        }
        // 无工具调用时参考实现把 tool 类原因映射为 end_turn
        "tool_use" | "tool_calls" | "function_call" => "end_turn".to_string(),
        "content_filter" => "refusal".to_string(),
        "end_turn"
        | "stop_sequence"
        | "pause_turn"
        | "refusal"
        | "model_context_window_exceeded" => stop_reason.to_string(),
        _ => "end_turn".to_string(),
    }
}

/// tool id 清洗:非法字符 → _,超 64 截断
/// (对齐 SanitizeClaudeToolID + shortenCodexCallIDIfNeeded)
pub(super) fn sanitize_tool_id(id: &str) -> String {
    let mut out: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.len() > 64 {
        out.truncate(64);
    }
    out
}

/// 函数调用进行中是否 defer 该事件(对齐 shouldDeferCodexStreamEvent)
fn should_defer_stream_event(root: &Value, event_type: &str) -> bool {
    match event_type {
        // 永不 defer:错误/收尾/参数增量(需在函数调用间隙立即处理)
        "error"
        | "response.failed"
        | "response.completed"
        | "response.incomplete"
        | "response.function_call_arguments.delta"
        | "response.function_call_arguments.done"
        | "response.custom_tool_call_input.delta"
        | "response.custom_tool_call_input.done" => false,
        // function_call / custom_tool_call 自身的事件不 defer
        "response.output_item.added" | "response.output_item.done" => {
            let it = root
                .get("item")
                .and_then(|i| i.get("type"))
                .and_then(|v| v.as_str());
            it != Some("function_call") && it != Some("custom_tool_call")
        }
        _ => true,
    }
}

/// tool_use 块 start(对齐 appendCodexFunctionCallStart)
fn function_call_start(call_id: &str, name: &str, index: i64) -> Bytes {
    emit::content_block_start_tool_use(index, &sanitize_tool_id(call_id), name)
}

/// custom 工具 input(字符串)→ tool_use.input 的 JSON 文本(包成 {"input": str})
fn custom_input_json(input: &str) -> String {
    let escaped = serde_json::to_string(input).unwrap_or_else(|_| "\"\"".to_string());
    format!(r#"{{"input":{}}}"#, escaped)
}

/// input_json_delta(对齐 appendCodexFunctionCallArgumentDelta)
fn function_call_argument_delta(partial_json: &str, index: i64) -> Bytes {
    emit::content_block_delta_input_json(index, partial_json)
}

/// 通用块 stop(对齐 appendCodexFunctionCallStop)
fn content_block_stop(index: i64) -> Bytes {
    emit::content_block_stop(index)
}

/// 追加去重 key(对齐 appendUniqueCodexFunctionCallKey)
fn push_key(keys: &mut Vec<String>, key: String) {
    if !key.is_empty() && !keys.contains(&key) {
        keys.push(key);
    }
}

/// web_search_call 的 query(对齐 codexWebSearchQuery:item/root 多路径)
fn web_search_query(root: &Value, item: &Value) -> String {
    for path in ["/action/query", "/query", "/input/query"] {
        if let Some(v) = item.pointer(path).and_then(|v| v.as_str()) {
            if !v.trim().is_empty() {
                return v.trim().to_string();
            }
        }
        if let Some(v) = root.pointer(path).and_then(|v| v.as_str()) {
            if !v.trim().is_empty() {
                return v.trim().to_string();
            }
        }
    }
    String::new()
}

/// web_search_call 的 results → web_search_result 块数组(对齐 codexWebSearchResultContent)
pub(super) fn web_search_result_content(root: &Value, item: &Value) -> Vec<Value> {
    let results = item
        .get("results")
        .and_then(|v| v.as_array())
        .or_else(|| root.get("results").and_then(|v| v.as_array()));
    let Some(results) = results else {
        return Vec::new();
    };
    let mut out = Vec::new();
    for result in results {
        let url = result
            .get("url")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if url.is_empty() {
            continue;
        }
        let mut title = result
            .get("title")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if title.is_empty() {
            title = url.clone();
        }
        out.push(json!({
            "type": "web_search_result",
            "title": title,
            "url": url,
            "page_age": null
        }));
    }
    out
}

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
            let Some(chunk) = stream.next().await else { break };
            let chunk = match chunk {
                Ok(c) => c,
                Err(e) => {
                    // 上游中断:发结构化 error 事件,不裸断流
                    for out in relay.stream_error(&e.to_string()) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sse::parser::SseEvent;

    fn s(bufs: &[Bytes]) -> String {
        String::from_utf8_lossy(&bufs.concat()).into_owned()
    }
    fn ev(data: &str) -> SseEvent {
        SseEvent {
            event: None,
            data: data.into(),
        }
    }

    fn created() -> SseEvent {
        ev(r#"{"type":"response.created","response":{"id":"r1","model":"gpt-5"}}"#)
    }

    /// 从 SSE 帧提取 data JSON
    fn frame_data(frame: &Bytes) -> Value {
        let s = String::from_utf8_lossy(frame);
        let (_, rest) = s.split_once('\n').unwrap();
        serde_json::from_str(rest.strip_prefix("data: ").unwrap().trim()).unwrap()
    }

    #[test]
    fn test_message_start_uses_estimated_when_upstream_silent() {
        // 上游未回真实 usage 时,message_start 用入站估算占位(context 不跳 1)
        let mut r = ResponsesRelay::new(Some(1234));
        let out = r.process(&created());
        let start = out
            .iter()
            .find(|b| b.starts_with(b"event: message_start"))
            .expect("message_start 应存在");
        let v = frame_data(start);
        assert_eq!(v["message"]["usage"]["input_tokens"], 1234);
        assert_eq!(v["message"]["usage"]["cache_read_input_tokens"], 0);
    }

    #[test]
    fn test_responses_text_stream() {
        let mut r = ResponsesRelay::new(None);
        let out1 = r.process(&created());
        assert!(out1.iter().any(|b| b.starts_with(b"event: message_start")));

        let out2 = r.process(&ev(
            r#"{"type":"response.output_text.delta","delta":"hello"}"#,
        ));
        assert!(out2
            .iter()
            .any(|b| b.starts_with(b"event: content_block_start")));
        assert!(out2
            .iter()
            .any(|b| b.starts_with(b"event: content_block_delta")));
    }

    #[test]
    fn test_responses_completed_with_tool_call() {
        let mut r = ResponsesRelay::new(None);
        r.process(&created());

        let out = r.process(&ev(
            r#"{"type":"response.completed","response":{"id":"r1","output":[
                {"type":"function_call","id":"fc_1","call_id":"call_9","name":"get_weather","arguments":"{\"city\":\"beijing\"}","status":"completed"}
            ],"usage":{"input_tokens":10,"output_tokens":5}}}"#,
        ));
        assert!(out.iter().any(|b| b.starts_with(b"event: message_delta")));
        assert!(out.iter().any(|b| b.starts_with(b"event: message_stop")));
        // call_id 优先于 id(对齐 codexFunctionCallID)
        let start = out
            .iter()
            .find(|b| b.starts_with(b"event: content_block_start"))
            .unwrap();
        let s = String::from_utf8_lossy(start);
        assert!(s.contains("get_weather"));
        assert!(s.contains("call_9"));
        assert!(!s.contains("fc_1"));
        // 有工具调用 → stop_reason tool_use
        let delta = out
            .iter()
            .find(|b| b.starts_with(b"event: message_delta"))
            .unwrap();
        assert!(String::from_utf8_lossy(delta).contains("tool_use"));
    }

    #[test]
    fn test_reasoning_replay_streaming_with_signature() {
        // 完整闭环:summary 流式可见 + encrypted_content 走 signature_delta
        let mut r = ResponsesRelay::new(None);
        r.process(&created());

        let out1 = r.process(&ev(
            r#"{"type":"response.reasoning_summary_text.delta","delta":"让我想想"}"#,
        ));
        let bufs1 = out1.concat();
        let s1 = String::from_utf8_lossy(&bufs1);
        assert!(s1.contains("content_block_start"));
        assert!(s1.contains("thinking_delta"));
        assert!(s1.contains("让我想想"));

        let out2 = r.process(&ev(
            r#"{"type":"response.output_item.done","item":{"type":"reasoning","encrypted_content":"gAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"}}"#,
        ));
        let bufs2 = out2.concat();
        let s2 = String::from_utf8_lossy(&bufs2);
        assert!(s2.contains("signature_delta"));
        assert!(s2.contains("gAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"));
        assert!(s2.contains("content_block_stop"));
    }

    #[test]
    fn test_reasoning_signature_only() {
        // 无 summary 的 reasoning item:开块即收尾,仍带 signature
        let mut r = ResponsesRelay::new(None);
        r.process(&created());
        r.process(&ev(
            r#"{"type":"response.output_item.added","item":{"type":"reasoning"}}"#,
        ));
        let out = r.process(&ev(
            r#"{"type":"response.output_item.done","item":{"type":"reasoning","encrypted_content":"gAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"}}"#,
        ));
        let bufs = out.concat();
        let s = String::from_utf8_lossy(&bufs);
        assert!(s.contains("\"type\":\"thinking\""));
        assert!(s.contains("signature_delta"));
        assert!(s.contains("gAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"));
    }

    #[test]
    fn test_reasoning_grok_encrypted_content_signature() {
        // Grok 无信封密文: 应合法保留并发送 signature_delta
        const GROK_CIPHER: &str =
            "5+C3No7B0G0P2dR5lSbMvwrctRb+B3vLAJ5/VYemHZNuaDbKv8IfNb+4Gyd125rfZtRtG4+iqYjT5uWOZbE44A";
        let mut r = ResponsesRelay::new(None);
        r.process(&created());
        r.process(&ev(
            r#"{"type":"response.output_item.added","item":{"type":"reasoning"}}"#,
        ));
        let out = r.process(&ev(&format!(
            r#"{{"type":"response.output_item.done","item":{{"type":"reasoning","encrypted_content":"{GROK_CIPHER}"}}}}"#
        )));
        let bufs = out.concat();
        let s = String::from_utf8_lossy(&bufs);
        assert!(s.contains("\"type\":\"thinking\""));
        assert!(s.contains("signature_delta"));
        assert!(s.contains(GROK_CIPHER));
    }

    #[test]
    fn test_reasoning_plaintext_delta() {
        // 非订阅网关发明文 reasoning_text.delta(无 encrypted_content):内容进 thinking 块
        let mut r = ResponsesRelay::new(None);
        r.process(&created());
        let out = r.process(&ev(
            r#"{"type":"response.reasoning_text.delta","delta":"明文推理"}"#,
        ));
        let so = s(&out);
        assert!(so.contains("content_block_start"));
        assert!(so.contains("thinking_delta"));
        assert!(so.contains("明文推理"));
        // done 发分隔符(对齐 CPA "\n\n");text 到来时正常收尾
        let done = r.process(&ev(
            r#"{"type":"response.reasoning_text.done","item":{"type":"reasoning"}}"#,
        ));
        let sd = s(&done);
        assert!(sd.contains("thinking_delta"), "done 应发分隔 delta");
        let text = r.process(&ev(
            r#"{"type":"response.output_text.delta","delta":"答案"}"#,
        ));
        let st = s(&text);
        assert!(st.contains("content_block_stop"));
        assert!(st.contains("text_delta"));
    }

    #[test]
    fn test_content_part_added_reasoning_plaintext() {
        // content_part.added(part.type=reasoning):保持块打开,分隔符续接,不误关块
        let mut r = ResponsesRelay::new(None);
        r.process(&created());
        r.process(&ev(
            r#"{"type":"response.output_item.added","item":{"type":"reasoning"}}"#,
        ));
        let out = r.process(&ev(
            r#"{"type":"response.content_part.added","part":{"type":"reasoning"}}"#,
        ));
        let s1 = s(&out);
        assert!(
            s1.contains("content_block_start"),
            "明文 reasoning part 应开块,实际: {s1}"
        );
        let out1b = r.process(&ev(
            r#"{"type":"response.reasoning_text.delta","delta":"推理中"}"#,
        ));
        let s1b = s(&out1b);
        assert!(
            s1b.contains("thinking_delta"),
            "明文 delta 应进块,实际: {s1b}"
        );
        assert!(
            !s1b.contains("content_block_start"),
            "块应保持打开,实际: {s1b}"
        );
        let out2 = r.process(&ev(
            r#"{"type":"response.content_part.added","part":{"type":"output_text"}}"#,
        ));
        let s2 = s(&out2);
        assert!(
            s2.contains("content_block_stop"),
            "output_text part 到来应收尾块,实际: {s2}"
        );
        assert!(s2.contains("content_block_start"));
        assert!(s2.contains("text"));
    }

    #[test]
    fn test_reasoning_summary_parts_separated() {
        let mut r = ResponsesRelay::new(None);
        r.process(&created());
        r.process(&ev(r#"{"type":"response.reasoning_summary_part.added"}"#));
        r.process(&ev(
            r#"{"type":"response.reasoning_summary_text.delta","delta":"A"}"#,
        ));
        let out = r.process(&ev(r#"{"type":"response.reasoning_summary_part.added"}"#));
        // 第二个 part:块保持打开,发空行分隔而非新块
        let bufs = out.concat();
        let s = String::from_utf8_lossy(&bufs);
        assert!(s.contains("thinking_delta"));
        assert!(!s.contains("content_block_start"));
    }

    #[test]
    fn test_usage_subtracts_cached_tokens() {
        let mut r = ResponsesRelay::new(None);
        r.process(&created());
        let out = r.process(&ev(
            r#"{"type":"response.completed","response":{"id":"r1","output":[],
                "usage":{"input_tokens":100,"output_tokens":5,
                         "input_tokens_details":{"cached_tokens":80}}}}"#,
        ));
        let delta = out
            .iter()
            .find(|b| b.starts_with(b"event: message_delta"))
            .unwrap();
        let v = frame_data(delta);
        assert_eq!(v["usage"]["input_tokens"], 20);
        assert_eq!(v["usage"]["cache_read_input_tokens"], 80);
    }

    #[test]
    fn test_usage_maps_cache_write_tokens() {
        // 对齐 CPA 893abbab:cache_write_tokens → cache_creation_input_tokens,
        // 0 时不下发该键;Anthropic 互斥语义:input 扣除 cached 与 cache_write
        let mut r = ResponsesRelay::new(None);
        r.process(&created());
        let out = r.process(&ev(
            r#"{"type":"response.completed","response":{"id":"r1","output":[],
                "usage":{"input_tokens":100,"output_tokens":5,
                         "input_tokens_details":{"cached_tokens":80,"cache_write_tokens":15}}}}"#,
        ));
        let delta = out
            .iter()
            .find(|b| b.starts_with(b"event: message_delta"))
            .unwrap();
        let v = frame_data(delta);
        assert_eq!(v["usage"]["input_tokens"], 5); // 100 - 80 - 15
        assert_eq!(v["usage"]["cache_read_input_tokens"], 80);
        assert_eq!(v["usage"]["cache_creation_input_tokens"], 15);

        let mut r = ResponsesRelay::new(None);
        r.process(&created());
        let out = r.process(&ev(
            r#"{"type":"response.completed","response":{"id":"r1","output":[],
                "usage":{"input_tokens":100,"output_tokens":5,
                         "input_tokens_details":{"cached_tokens":80,"cache_write_tokens":0}}}}"#,
        ));
        let delta = out
            .iter()
            .find(|b| b.starts_with(b"event: message_delta"))
            .unwrap();
        let v = frame_data(delta);
        assert_eq!(v["usage"]["input_tokens"], 20); // 100 - 80 - 0
        assert!(v["usage"].get("cache_creation_input_tokens").is_none());
    }

    #[test]
    fn test_empty_output_synthesizes_text_block() {
        // 空轮次合成空 text 块(对齐 synthesizeCodexEmptyTextBlock)
        let mut r = ResponsesRelay::new(None);
        let out = r.process(&ev(
            r#"{"type":"response.completed","response":{"id":"r1","output":[]}}"#,
        ));
        let bufs = out.concat();
        let s = String::from_utf8_lossy(&bufs);
        assert!(s.contains("message_stop"));
        assert!(s.contains("\"type\":\"text\""));
    }

    #[test]
    fn test_thinking_only_turn_synthesizes_text_block() {
        let mut r = ResponsesRelay::new(None);
        r.process(&created());
        r.process(&ev(
            r#"{"type":"response.output_item.done","item":{"type":"reasoning","encrypted_content":"E"}}"#,
        ));
        let out = r.process(&ev(
            r#"{"type":"response.completed","response":{"id":"r1","output":[]}}"#,
        ));
        let bufs = out.concat();
        let s = String::from_utf8_lossy(&bufs);
        assert!(s.contains("\"type\":\"text\""));
    }

    #[test]
    fn test_stream_error_event() {
        let mut r = ResponsesRelay::new(None);
        let out = r.process(&ev(
            r#"{"type":"error","error":{"type":"invalid_request","code":"cyber_policy","message":"blocked"}}"#,
        ));
        let bufs = out.concat();
        let s = String::from_utf8_lossy(&bufs);
        assert!(s.starts_with("event: error"));
        assert!(s.contains("invalid_request_error"));
        assert!(s.contains("blocked"));
    }

    #[test]
    fn test_response_failed_event() {
        let mut r = ResponsesRelay::new(None);
        let out = r.process(&ev(
            r#"{"type":"response.failed","sequence_number":0,"response":{"status":"failed","error":{"type":"server_error","code":"internal_server_error","message":"boom"}}}"#,
        ));
        let bufs = out.concat();
        let s = String::from_utf8_lossy(&bufs);
        assert!(s.starts_with("event: error"));
        assert!(s.contains("server_error"));
        assert!(s.contains("boom"));
    }

    #[test]
    fn test_response_failed_during_function_call_is_immediate() {
        let mut r = ResponsesRelay::new(None);
        r.process(&created());
        r.process(&ev(
            r#"{"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_1","name":"get_weather","output_index":0}}"#,
        ));
        let out = r.process(&ev(
            r#"{"type":"response.failed","response":{"status":"failed","error":{"type":"server_error","message":"boom"}}}"#,
        ));
        let s = s(&out);
        assert!(s.starts_with("event: error"), "失败事件不得延迟,实际: {s}");
        assert!(s.contains("server_error"), "应保留上游错误类型,实际: {s}");
        assert!(s.contains("boom"), "应保留上游错误消息,实际: {s}");
        assert!(r.finish().is_empty(), "失败后 finish 应为空");
    }

    #[test]
    fn test_error_event_stops_followup_events() {
        // 上游 error 后仍发正常事件(部分网关 overloaded 后混续):只保留 error,
        // 不得再产出 text/delta/stop 混合流,否则 SDK 判 "malformed"。
        let mut r = ResponsesRelay::new(None);
        r.process(&created());
        let out = r.process(&ev(
            r#"{"type":"error","error":{"type":"overloaded_error","message":"servers overloaded"}}"#,
        ));
        assert!(out.iter().any(|b| b.starts_with(b"event: error")));
        // 后续 completed 等事件一律忽略
        let after = r.process(&ev(
            r#"{"type":"response.completed","response":{"id":"r1","output":[]}}"#,
        ));
        assert!(after.is_empty(), "error 后不得继续处理,实际: {after:?}");
        // finish 兜底也不得再产 message_delta/stop
        let fin = r.finish();
        assert!(fin.is_empty(), "error 后 finish 应为空,实际: {fin:?}");
    }

    #[test]
    fn test_doom_loop_confident_signal_aborts_stream() {
        // 置信信号(thinking channel tail_repetition ≤ 64):吞掉事件 + 发 error 中断
        let mut r = ResponsesRelay::new(None);
        r.process(&created());
        let out = r.process(&ev(
            r#"{"type":"response.doom_loop_check","doom_loop_check":{"triggers":["tail_repetition:32@thinking"]}}"#,
        ));
        let s = s(&out);
        assert!(
            s.starts_with("event: error"),
            "置信信号应发 error 中断,实际: {s}"
        );
        assert!(
            s.contains("doom loop detected"),
            "error 应含 doom loop 信息,实际: {s}"
        );
        // 中断后后续事件一律忽略
        let after = r.process(&ev(
            r#"{"type":"response.completed","response":{"id":"r1","output":[]}}"#,
        ));
        assert!(after.is_empty(), "中断后不得继续处理,实际: {after:?}");
        assert!(
            r.finish().is_empty(),
            "中断后 finish 应为空,实际: {:?}",
            r.finish()
        );
    }

    #[test]
    fn test_doom_loop_non_confident_signal_passes_through() {
        // 非置信信号(response channel):吞掉事件,流照常
        let mut r = ResponsesRelay::new(None);
        r.process(&created());
        let out = r.process(&ev(
            r#"{"type":"response.doom_loop_check","doom_loop_check":{"triggers":["tail_repetition:32@response"]}}"#,
        ));
        assert!(out.is_empty(), "非置信信号不应产出任何帧,实际: {out:?}");
        // 流继续正常处理
        let text = r.process(&ev(r#"{"type":"response.output_text.delta","delta":"ok"}"#));
        assert!(s(&text).contains("text_delta"));
    }

    #[test]
    fn test_doom_loop_terminal_field_confident() {
        // 终态响应对象上的 doom_loop_check 字段:置信时中断而非正常收尾
        let mut r = ResponsesRelay::new(None);
        r.process(&created());
        let out = r.process(&ev(
            r#"{"type":"response.completed","response":{"id":"r1","output":[],"doom_loop_check":{"triggers":["tail_repetition:16@thinking"]}}}"#,
        ));
        let s = s(&out);
        assert!(
            s.starts_with("event: error"),
            "终态置信信号应发 error,实际: {s}"
        );
        assert!(!s.contains("message_stop"), "不得正常收尾,实际: {s}");
    }

    #[test]
    fn test_doom_loop_dedup_repeated_triggers() {
        // 服务端重发累计集:同 label 去重,不重复判定
        let mut r = ResponsesRelay::new(None);
        r.process(&created());
        // 第一次:非置信
        r.process(&ev(
            r#"{"type":"response.doom_loop_check","doom_loop_check":{"triggers":["tail_repetition:32@response"]}}"#,
        ));
        // 第二次重发同 label:仍非置信,流照常
        let out = r.process(&ev(
            r#"{"type":"response.doom_loop_check","doom_loop_check":{"triggers":["tail_repetition:32@response"]}}"#,
        ));
        assert!(out.is_empty(), "重复 label 应去重,实际: {out:?}");
    }

    #[test]
    fn test_doom_loop_malformed_payload_never_fails() {
        // malformed payload:best-effort,不弄挂流
        let mut r = ResponsesRelay::new(None);
        r.process(&created());
        let out = r.process(&ev(
            r#"{"type":"response.doom_loop_check","doom_loop_check":{"triggers":[123,null]}}"#,
        ));
        assert!(
            out.is_empty(),
            "malformed 不应产出帧也不应报错,实际: {out:?}"
        );
        // 流继续正常
        let text = r.process(&ev(r#"{"type":"response.output_text.delta","delta":"ok"}"#));
        assert!(s(&text).contains("text_delta"));
    }

    #[test]
    fn test_stop_reason_mappings() {
        assert_eq!(map_stop_reason("content_filter", false), "refusal");
        assert_eq!(map_stop_reason("max_output_tokens", false), "max_tokens");
        assert_eq!(map_stop_reason("max_prompt_tokens", false), "max_tokens");
        assert_eq!(map_stop_reason("max_time_limit", false), "max_tokens");
        assert_eq!(map_stop_reason("", false), "end_turn");
        assert_eq!(map_stop_reason("stop", true), "tool_use");
        assert_eq!(map_stop_reason("pause_turn", false), "pause_turn");
        let r = json!({"status":"incomplete","incomplete_details":{"reason":"max_output_tokens"}});
        assert_eq!(codex_stop_reason(&r), "max_output_tokens");
        let r = json!({"status":"incomplete","incomplete_details":{"reason":"max_prompt_tokens"}});
        assert_eq!(codex_stop_reason(&r), "max_prompt_tokens");
        let r = json!({"status":"incomplete","incomplete_details":{"reason":"max_time_limit"}});
        assert_eq!(codex_stop_reason(&r), "max_time_limit");
        let r = json!({"stop_reason":"stop","stop_sequence":"END"});
        assert_eq!(codex_stop_reason(&r), "stop_sequence");
    }

    #[test]
    fn test_stop_sequence_in_message_delta() {
        let mut r = ResponsesRelay::new(None);
        let out = r.process(&ev(
            r#"{"type":"response.completed","response":{"id":"r1","output":[],
                "stop_reason":"stop","stop_sequence":"END"}}"#,
        ));
        let delta = out
            .iter()
            .find(|b| b.starts_with(b"event: message_delta"))
            .unwrap();
        let s = String::from_utf8_lossy(delta);
        assert!(s.contains("stop_sequence"));
        assert!(s.contains("END"));
    }

    #[test]
    fn test_model_fallback() {
        let mut r = ResponsesRelay::new(None);
        let out = r.process(&ev(r#"{"type":"response.created","response":{"id":"r1"}}"#));
        let bufs = out.concat();
        let s = String::from_utf8_lossy(&bufs);
        assert!(s.contains(FALLBACK_MODEL));
    }

    #[test]
    fn test_message_item_text_fallback() {
        // 无 delta 流时从 output_item.done(message) 的 content 补发
        let mut r = ResponsesRelay::new(None);
        r.process(&created());
        let out = r.process(&ev(
            r#"{"type":"response.output_item.done","item":{"type":"message",
                "content":[{"type":"output_text","text":"补发文本"}]}}"#,
        ));
        let bufs = out.concat();
        let s = String::from_utf8_lossy(&bufs);
        assert!(s.contains("补发文本"));
        assert!(s.contains("content_block_stop"));
    }

    #[test]
    fn test_finish_eof_after_completed_keeps_normal() {
        // 正常 completed 后 finish 幂等,不再产出(已完成流)
        let mut r = ResponsesRelay::new(None);
        r.process(&ev(
            r#"{"type":"response.completed","response":{"id":"r1","output":[]}}"#,
        ));
        assert!(r.finish().is_empty());
    }

    #[test]
    fn test_finish_eof_without_completed_errors() {
        // 残缺流:已发 content delta 但未 completed 就 EOF,显式报 error,不包装成正常收尾
        let mut r = ResponsesRelay::new(None);
        r.process(&created());
        r.process(&ev(
            r#"{"type":"response.output_text.delta","delta":"partial"}"#,
        ));
        let out = r.finish();
        let bufs = out.concat();
        let s = String::from_utf8_lossy(&bufs);
        assert!(s.contains("event: error"), "残缺流应发 error 帧,实际: {s}");
        assert!(
            !s.contains("message_delta"),
            "残缺流不得发正常 message_delta,实际: {s}"
        );
        assert!(
            !s.contains("message_stop"),
            "残缺流不得发正常 message_stop,实际: {s}"
        );
        // 二次 finish 幂等
        assert!(r.finish().is_empty());
    }

    #[test]
    fn test_finish_eof_empty_stream_errors() {
        // 空流:一个事件都没有,EOF 直接报 error
        let mut r = ResponsesRelay::new(None);
        let out = r.finish();
        let bufs = out.concat();
        let s = String::from_utf8_lossy(&bufs);
        assert!(s.contains("event: error"), "空流应发 error 帧,实际: {s}");
        assert!(
            !s.contains("message_start"),
            "空流不得伪造 message_start,实际: {s}"
        );
        assert!(!s.contains("message_stop"));
    }

    #[test]
    fn test_sanitize_tool_id() {
        assert_eq!(sanitize_tool_id("call_abc-123"), "call_abc-123");
        assert_eq!(sanitize_tool_id("a.b/c"), "a_b_c");
        let long = "x".repeat(100);
        assert_eq!(sanitize_tool_id(&long).len(), 64);
    }

    #[test]
    fn test_function_call_streaming_full_lifecycle() {
        // 完整生命周期:added → args delta → args done → item done → start/stop
        let mut r = ResponsesRelay::new(None);
        r.process(&created());

        let out1 = r.process(&ev(
            r#"{"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_1","name":"get_weather","output_index":0}}"#,
        ));
        let s1 = s(&out1);
        assert!(s1.contains("content_block_start"));
        assert!(s1.contains("\"name\":\"get_weather\""));
        assert!(s1.contains("\"id\":\"call_1\""));

        let out2 = r.process(&ev(
            r#"{"type":"response.function_call_arguments.delta","delta":"{\"cit"}"#,
        ));
        let s2 = s(&out2);
        assert!(!s2.contains("content_block_start"), "参数增量不应重复开块");
        assert!(s2.contains("input_json_delta"));
        assert!(s2.contains("{\\\"cit"));

        let out3 = r.process(&ev(
            r#"{"type":"response.function_call_arguments.done","call_id":"call_1","arguments":"{\"city\":\"beijing\"}"}"#,
        ));
        let s3 = s(&out3);
        assert!(s3.contains("input_json_delta"));
        assert!(s3.contains("beijing"));

        let out4 = r.process(&ev(
            r#"{"type":"response.output_item.done","item":{"type":"function_call","call_id":"call_1","name":"get_weather","arguments":"{\"city\":\"beijing\"}"}}"#,
        ));
        let s4 = s(&out4);
        assert!(s4.contains("content_block_stop"));
    }

    #[test]
    fn test_function_call_tool_name_restored() {
        // 请求侧缩短的名,响应侧还原(对齐 buildReverseMap)
        let mut names = HashMap::new();
        names.insert(
            "mcp__short".to_string(),
            "mcp__very_long_original_name".to_string(),
        );
        let mut r = ResponsesRelay::new(None);
        r.tool_names = Some(Arc::new(names));
        r.process(&created());

        let out = r.process(&ev(
            r#"{"type":"response.output_item.added","item":{"type":"function_call","call_id":"call_9","name":"mcp__short"}}"#,
        ));
        let s = s(&out);
        assert!(s.contains("mcp__very_long_original_name"));
        assert!(!s.contains("\"name\":\"mcp__short\""));
    }

    #[test]
    fn test_function_call_multiple_queued_serially() {
        // 并发两个调用:串行输出,每个独立 start/delta/stop
        let mut r = ResponsesRelay::new(None);
        r.process(&created());

        r.process(&ev(
            r#"{"type":"response.output_item.added","item":{"type":"function_call","call_id":"a","name":"tool_a","output_index":0}}"#,
        ));
        r.process(&ev(
            r#"{"type":"response.output_item.added","item":{"type":"function_call","call_id":"b","name":"tool_b","output_index":1}}"#,
        ));
        let out = r.process(&ev(
            r#"{"type":"response.output_item.done","item":{"type":"function_call","call_id":"a","name":"tool_a","arguments":"{}"}}"#,
        ));
        let s = s(&out);
        assert!(s.contains("tool_b"), "a 完成后应接着输出 b");
    }

    #[test]
    fn test_response_completed_flushes_terminal_calls_with_deferred() {
        // 无流式事件只有 completed:从 response.output 补发工具调用
        let mut r = ResponsesRelay::new(None);
        r.process(&created());
        let out = r.process(&ev(
            r#"{"type":"response.completed","response":{"id":"r1","output":[
                {"type":"function_call","call_id":"call_9","name":"get_weather","arguments":"{\"city\":\"beijing\"}"}
            ],"usage":{"input_tokens":10,"output_tokens":5}}}"#,
        ));
        let s = s(&out);
        assert!(s.contains("content_block_start"));
        assert!(s.contains("get_weather"));
        assert!(s.contains("call_9"));
        assert!(s.contains("message_delta"));
        assert!(s.contains("tool_use"));
    }

    #[test]
    fn test_custom_tool_call_streaming_full_lifecycle() {
        // custom 工具:added → input delta → input done → item done,
        // input 是字符串,包成 {"input": str} 发 tool_use(对齐响应转换 custom 分支)
        let mut r = ResponsesRelay::new(None);
        r.process(&created());

        let out1 = r.process(&ev(
            r#"{"type":"response.output_item.added","item":{"type":"custom_tool_call","call_id":"call_c","name":"apply_patch","input":"","output_index":0}}"#,
        ));
        let s1 = s(&out1);
        assert!(s1.contains("content_block_start"));
        assert!(s1.contains("\"name\":\"apply_patch\""));
        assert!(s1.contains("\"id\":\"call_c\""));

        let out2 = r.process(&ev(
            r#"{"type":"response.custom_tool_call_input.delta","call_id":"call_c","delta":"*** Begin Patch\n"}"#,
        ));
        // delta 阶段不立即发流(一次性在 done 发)
        assert!(!s(&out2).contains("input_json_delta"));

        let out3 = r.process(&ev(
            r#"{"type":"response.custom_tool_call_input.done","call_id":"call_c","input":"*** Begin Patch\n+hello"}"#,
        ));
        assert!(!s(&out3).contains("input_json_delta"));

        let out4 = r.process(&ev(
            r#"{"type":"response.output_item.done","item":{"type":"custom_tool_call","call_id":"call_c","name":"apply_patch","input":"*** Begin Patch\n+hello"}}"#,
        ));
        let s4 = s(&out4);
        assert!(s4.contains("input_json_delta"));
        assert!(s4.contains("{\\\"input\\\":\\\"*** Begin Patch"));
        assert!(s4.contains("content_block_stop"));
    }

    #[test]
    fn test_custom_tool_call_from_terminal_output() {
        // 无流式事件只有 completed:从 response.output 补发 custom 工具调用
        let mut r = ResponsesRelay::new(None);
        r.process(&created());
        let out = r.process(&ev(
            r#"{"type":"response.completed","response":{"id":"r1","output":[
                {"type":"custom_tool_call","call_id":"call_c","name":"apply_patch","input":"patch-content"}
            ],"usage":{"input_tokens":10,"output_tokens":5}}}"#,
        ));
        let bufs = out.concat();
        let s = String::from_utf8_lossy(&bufs);
        assert!(s.contains("content_block_start"));
        assert!(s.contains("apply_patch"));
        assert!(s.contains("call_c"));
        assert!(
            s.contains("{\"input\":\"patch-content\"}")
                || s.contains("{\\\"input\\\":\\\"patch-content\\\"}")
        );
        assert!(s.contains("message_delta"));
        assert!(s.contains("tool_use"));
    }

    #[test]
    fn test_custom_tool_call_done_with_full_input_direct() {
        // output_item.added 直接带完整 input(无 delta 流),done 时一次性发
        let mut r = ResponsesRelay::new(None);
        r.process(&created());
        r.process(&ev(
            r#"{"type":"response.output_item.added","item":{"type":"custom_tool_call","call_id":"call_x","name":"freeform","input":"pwd","output_index":0}}"#,
        ));
        let out = r.process(&ev(
            r#"{"type":"response.output_item.done","item":{"type":"custom_tool_call","call_id":"call_x","name":"freeform","input":"pwd"}}"#,
        ));
        let bufs = out.concat();
        let s = String::from_utf8_lossy(&bufs);
        // 字符串 input 包成 {"input": "pwd"} 对象
        assert!(s.contains("{\\\"input\\\":\\\"pwd\\\"}") || s.contains("{\"input\":\"pwd\"}"));
        assert!(s.contains("content_block_stop"));
    }

    #[test]
    fn test_web_search_call_streaming() {
        // web_search_call → server_tool_use + web_search_tool_result
        let mut r = ResponsesRelay::new(None);
        r.process(&created());
        let out = r.process(&ev(
            r#"{"type":"response.output_item.done","item":{"type":"web_search_call","id":"ws_1","action":{"query":"rust async"},"results":[{"url":"https://example.com","title":"Example"}]}}"#,
        ));
        let s = s(&out);
        assert!(s.contains("server_tool_use"));
        assert!(s.contains("web_search"));
        assert!(s.contains("web_search_tool_result"));
        assert!(s.contains("rust async"));
        assert!(s.contains("https://example.com"));
    }

    #[test]
    fn test_redacted_thinking_restores_from_encrypted_content() {
        // encrypted_content 带前缀 "claude-redacted-thinking:" 应还原为 redacted_thinking 块
        let mut r = ResponsesRelay::new(None);
        r.process(&created());
        r.process(&ev(
            r#"{"type":"response.output_item.added","item":{"type":"reasoning"}}"#,
        ));
        let out = r.process(&ev(
            r#"{"type":"response.output_item.done","item":{"type":"reasoning","encrypted_content":"claude-redacted-thinking:opaque_data_xyz"}}"#,
        ));
        let bufs = out.concat();
        let s = String::from_utf8_lossy(&bufs);
        // 应发 redacted_thinking 块，不是 thinking 块
        assert!(
            s.contains("\"type\":\"redacted_thinking\""),
            "应发 redacted_thinking 块,实际: {s}"
        );
        assert!(
            s.contains("redacted_thinking_data"),
            "应发 redacted_thinking_data delta,实际: {s}"
        );
        assert!(s.contains("opaque_data_xyz"), "应包含 data 载荷,实际: {s}");
        assert!(
            !s.contains("signature_delta"),
            "redacted_thinking 不应发 signature_delta,实际: {s}"
        );
        assert!(s.contains("content_block_stop"));
    }

    #[test]
    fn test_redacted_thinking_with_summary_seen() {
        // 带 summary 的 redacted_thinking: summary 可见 + 最后发 data
        let mut r = ResponsesRelay::new(None);
        r.process(&created());
        r.process(&ev(
            r#"{"type":"response.reasoning_summary_text.delta","delta":"thinking process"}"#,
        ));
        let out = r.process(&ev(
            r#"{"type":"response.output_item.done","item":{"type":"reasoning","encrypted_content":"claude-redacted-thinking:data123"}}"#,
        ));
        let bufs = out.concat();
        let s = String::from_utf8_lossy(&bufs);
        assert!(
            s.contains("\"type\":\"redacted_thinking\""),
            "应为 redacted_thinking 块"
        );
        assert!(s.contains("redacted_thinking_data"));
        assert!(s.contains("data123"));
    }

    #[test]
    fn test_normal_thinking_signature_not_affected() {
        // 无前缀的普通 signature 应继续正常处理（不误判为 redacted_thinking）
        let mut r = ResponsesRelay::new(None);
        r.process(&created());
        let out1 = r.process(&ev(
            r#"{"type":"response.reasoning_summary_text.delta","delta":"思考中"}"#,
        ));
        let buf1 = out1.concat();
        let s1 = String::from_utf8_lossy(&buf1);

        let out2 = r.process(&ev(
            r#"{"type":"response.output_item.done","item":{"type":"reasoning","encrypted_content":"gAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"}}"#,
        ));
        let buf2 = out2.concat();
        let s2 = String::from_utf8_lossy(&buf2);

        let combined = format!("{}{}", s1, s2);
        assert!(
            combined.contains("\"type\":\"thinking\""),
            "普通签名应发 thinking 块,实际: {}",
            combined
        );
        assert!(
            combined.contains("signature_delta"),
            "应发 signature_delta,实际: {}",
            combined
        );
        assert!(combined.contains("gAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAAA"));
        assert!(
            !combined.contains("redacted_thinking"),
            "不应误判为 redacted_thinking"
        );
    }

    #[test]
    fn test_redacted_thinking_signature_only() {
        // 无 summary 只有 encrypted_content 的 redacted_thinking
        let mut r = ResponsesRelay::new(None);
        r.process(&created());
        r.process(&ev(
            r#"{"type":"response.output_item.added","item":{"type":"reasoning","encrypted_content":"claude-redacted-thinking:sig_only"}}"#,
        ));
        let out = r.process(&ev(
            r#"{"type":"response.output_item.done","item":{"type":"reasoning","encrypted_content":"claude-redacted-thinking:sig_only"}}"#,
        ));
        let bufs = out.concat();
        let s = String::from_utf8_lossy(&bufs);
        assert!(s.contains("\"type\":\"redacted_thinking\""));
        assert!(s.contains("redacted_thinking_data"));
        assert!(s.contains("sig_only"));
        assert!(s.contains("content_block_stop"));
    }
}
