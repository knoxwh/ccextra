use bytes::Bytes;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use crate::sse::emit;
use crate::sse::extract_usage_responses;
use crate::sse::parser::SseEvent;
use ccextra_core::convert::{is_valid_gpt_reasoning_signature, is_valid_grok_encrypted_content};
use ccextra_core::doom_loop::{is_confident, parse_trigger};

use super::compensations::*;
use super::function_call::*;
use super::web_search::*;
use super::{
    CLAUDE_RESPONSES_REDACTED_THINKING_PREFIX, FALLBACK_MODEL, SUMMARY_PART_SEPARATOR,
};

pub(crate) struct ResponsesRelay {
    pub(crate) message_started: bool,
    pub(crate) finished: bool,
    pub(crate) model: String,
    pub(crate) id: String,
    pub(crate) next_block_index: i64,

    // text 块
    pub(crate) text_open: bool,
    pub(crate) text_index: i64,
    pub(crate) has_text_delta: bool,

    // thinking 块(每个 reasoning item 一个,output_item.done 才关)
    pub(crate) thinking_open: bool,
    pub(crate) thinking_index: i64,
    /// 待收尾的 encrypted_content(signature_delta 用)
    pub(crate) thinking_signature: String,
    pub(crate) thinking_summary_seen: bool,
    /// 当前 reasoning item 是否为 redacted_thinking(根据 encrypted_content 前缀判定)
    pub(crate) thinking_is_redacted: bool,

    /// function_call 流式状态(对齐 ConvertCodexResponseToClaudeParams)
    pub(crate) function_calls: HashMap<String, usize>,
    pub(crate) function_call_queue: Vec<FunctionCallStream>,
    pub(crate) active_function_call: Option<usize>,
    pub(crate) last_function_call: Option<usize>,
    /// 函数调用期间被 defer 的原始事件(空队时重放)
    pub(crate) deferred_stream_events: Vec<SseEvent>,
    pub(crate) has_emitted_tool_use: bool,

    /// web_search_call 去重(对齐 WebSearchToolUseIDs / WebSearchToolResultIDs)
    pub(crate) web_search_tool_use_ids: HashSet<String>,
    pub(crate) web_search_tool_result_ids: HashSet<String>,
    pub(crate) last_web_search_tool_use_id: String,

    /// 工具名还原表 short→original(请求转换侧产出)
    pub(crate) tool_names: Option<Arc<HashMap<String, String>>>,
    /// 入站 body 本地估算输入 token(http.rs 计算;上游流未回真实 usage 时占位)
    pub(crate) estimated_input: Option<usize>,
    /// doom loop 检测:已见触发器 raw label 去重(服务端重发累计集)
    pub(crate) doom_loop_seen: HashSet<String>,

    /// 空 incomplete 终态检测(对齐 CPA IsCodexTerminalEmptyIncomplete):
    /// 已见有效输出 delta 与已完成 output item 计数
    pub(crate) saw_output_delta: bool,
    pub(crate) output_items_seen: usize,
}

impl ResponsesRelay {
    pub(crate) fn new(estimated_input: Option<usize>) -> Self {
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
            saw_output_delta: false,
            output_items_seen: 0,
        }
    }

    pub(crate) fn with_tool_names(mut self, tool_names: Option<Arc<HashMap<String, String>>>) -> Self {
        self.tool_names = tool_names;
        self
    }

    /// 工具名还原:short → original(对齐 resolveCodexClaudeToolUseName)
    pub(crate) fn resolve_tool_name(&self, name: &str) -> String {
        if let Some(rev) = &self.tool_names {
            if let Some(orig) = rev.get(name) {
                return orig.clone();
            }
        }
        name.to_string()
    }

    /// 处理一个 SSE 事件,产出 anthropic 字节事件
    pub(crate) fn process(&mut self, ev: &SseEvent) -> Vec<Bytes> {
        // 已收尾:忽略后续事件,避免 error 后再输出正常收尾事件。
        if self.finished {
            return Vec::new();
        }
        let root: Value = match serde_json::from_str(&ev.data) {
            Ok(v) => v,
            Err(_) => return Vec::new(),
        };
        let event_type = root.get("type").and_then(|v| v.as_str()).unwrap_or("");

        // 有效输出 delta 记账(对齐 CPA HasMeaningfulCodexOutputDelta:非空
        // 文本/推理/工具参数 delta 均算产出;在 defer 之前,计账不受影响)
        if is_meaningful_output_delta(&root, event_type) {
            self.saw_output_delta = true;
        }

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
                // 上游静默中止(0 token 空 incomplete):报错触发 CC 自动重试,
                // 而非伪造成功空消息(对齐 CPA IsCodexTerminalEmptyIncomplete → 502)
                if is_terminal_empty_incomplete(
                    &root,
                    self.saw_output_delta,
                    self.output_items_seen,
                ) {
                    return self.stream_error(
                        "upstream terminated with incomplete empty response (0 tokens)",
                    );
                }
                let response = root.get("response");
                self.update_identity(response);
                // 终态响应对象上的 doom_loop_check 字段(对齐 grok-build 双路报告)
                if let Some(out) = self.check_terminal_doom_loop(response) {
                    return out;
                }
                let mut out = self.finalize_thinking();
                out.extend(self.stop_text());
                // 终态兜底:正文只在 completed 里时补发(对齐 sub2api,须在合成
                // 空 text 块之前;发过 delta 或已合成则 no-op)
                out.extend(self.recover_terminal_text(response));
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
    pub(crate) fn check_terminal_doom_loop(&mut self, response: Option<&Value>) -> Option<Vec<Bytes>> {
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
            Some(i) => {
                self.output_items_seen += 1;
                i
            }
            None => return Vec::new(),
        };
        let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match item_type {
            "message" => {
                if self.has_text_delta {
                    return Vec::new();
                }
                // 无 delta 流时从 item.content 补发文本(与 recover_terminal_text 共用)
                let text = extract_output_text(item.get("content"));
                if text.is_empty() {
                    return Vec::new();
                }
                self.emit_recovered_text(&text)
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
    pub(crate) fn ensure_started(&mut self) -> Vec<Bytes> {
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

    pub(crate) fn start_text(&mut self) -> Vec<Bytes> {
        if self.text_open {
            return Vec::new();
        }
        self.text_index = self.next_block_index;
        self.next_block_index += 1;
        self.text_open = true;
        vec![emit::content_block_start_text(self.text_index)]
    }

    pub(crate) fn stop_text(&mut self) -> Vec<Bytes> {
        if !self.text_open {
            return Vec::new();
        }
        self.text_open = false;
        vec![emit::content_block_stop(self.text_index)]
    }

    pub(crate) fn text_delta(&self, text: &str) -> Vec<Bytes> {
        vec![emit::content_block_delta_text(self.text_index, text)]
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
    pub(crate) fn synthesize_empty_text_block(&mut self) -> Vec<Bytes> {
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
        let (input_tokens, output_tokens, cached, cache_write, thinking_tokens) = response
            .and_then(|r| r.get("usage"))
            .map(extract_usage_responses)
            .unwrap_or((0, 0, 0, 0, -1));

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
            thinking_tokens,
        ));
        out.push(emit::message_stop());
        out
    }

    /// EOF 兜底:未收到 Responses 终态时显式报 error,不把半个回答包装成正常完成。
    pub(crate) fn finish(&mut self) -> Vec<Bytes> {
        if self.finished {
            return Vec::new();
        }
        // completed/incomplete 分支必已 finalize 置 finished,到此即残缺
        self.stream_error("upstream stream ended before response completion")
    }

    /// 上游流中断:发 Anthropic error 事件,不伪造 message_start。
    pub(crate) fn stream_error(&mut self, message: &str) -> Vec<Bytes> {
        if self.finished {
            return Vec::new();
        }
        self.finished = true;
        vec![emit::error_event(message)]
    }
}

