use bytes::Bytes;
use serde_json::Value;

use crate::sse::emit;
use super::compensations::{extract_output_text, sanitize_tool_id};
use super::state_machine::ResponsesRelay;

/// 单个 function_call 流式块(对齐 codexFunctionCallStream)
///
/// custom_tool_call 复用同一状态机:`is_custom=true` 时 arguments 存字符串 input
/// (非 JSON),发 tool_use 时包成 `{"input": str}`(对齐响应转换 custom 分支)。
pub(crate) struct FunctionCallStream {
    pub(crate) call_id: String,
    pub(crate) name: String,
    pub(crate) block_index: i64,
    pub(crate) arguments: String,
    pub(crate) emitted_arguments_len: usize,
    pub(crate) has_received_arguments_delta: bool,
    pub(crate) emit_initial_empty_delta: bool,
    pub(crate) is_custom: bool,
    pub(crate) started: bool,
    pub(crate) done: bool,
    pub(crate) closed: bool,
}

impl FunctionCallStream {
    pub(crate) fn new() -> Self {
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

pub(crate) fn should_defer_stream_event(root: &Value, event_type: &str) -> bool {
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
pub(crate) fn function_call_start(call_id: &str, name: &str, index: i64) -> Bytes {
    emit::content_block_start_tool_use(index, &sanitize_tool_id(call_id), name)
}

/// custom 工具 input(字符串)→ tool_use.input 的 JSON 文本(包成 {"input": str})
pub(crate) fn custom_input_json(input: &str) -> String {
    let escaped = serde_json::to_string(input).unwrap_or_else(|_| "\"\"".to_string());
    format!(r#"{{"input":{}}}"#, escaped)
}

/// input_json_delta(对齐 appendCodexFunctionCallArgumentDelta)
pub(crate) fn function_call_argument_delta(partial_json: &str, index: i64) -> Bytes {
    emit::content_block_delta_input_json(index, partial_json)
}

/// 通用块 stop(对齐 appendCodexFunctionCallStop)
pub(crate) fn content_block_stop(index: i64) -> Bytes {
    emit::content_block_stop(index)
}

/// 追加去重 key(对齐 appendUniqueCodexFunctionCallKey)
pub(crate) fn push_key(keys: &mut Vec<String>, key: String) {
    if !key.is_empty() && !keys.contains(&key) {
        keys.push(key);
    }
}
impl ResponsesRelay {
    // ---- function_call 流式状态机 ----

    /// 事件 → 候选 key 列表(output_index / call_id / item_id;对齐 codexFunctionCallKeys)
    pub(crate) fn call_keys(&self, root: Option<&Value>, item: Option<&Value>) -> Vec<String> {
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
    pub(crate) fn function_call_for_keys(&self, keys: &[String]) -> Option<usize> {
        for k in keys {
            if let Some(&i) = self.function_calls.get(k) {
                return Some(i);
            }
        }
        None
    }

    /// 登记调用(无则新建入队),登记别名(对齐 recordCodexFunctionCall)
    pub(crate) fn record_function_call(
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
    pub(crate) fn update_function_call_identity(
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
    pub(crate) fn append_buffered_arguments(&mut self) -> Vec<Bytes> {
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
    pub(crate) fn append_function_call_queue(&mut self) -> Vec<Bytes> {
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
    pub(crate) fn append_function_calls_from_terminal(&mut self, response: Option<&Value>) -> Vec<Bytes> {
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

    /// 终态文本兜底(对齐 sub2api resToAnthRecoverTerminalText):部分上游不发任何
    /// output_text.delta,正文只出现在 response.completed 的 output[].message 里。
    /// 仅在本次流从未发过文本时补发,避免重复。
    pub(crate) fn recover_terminal_text(&mut self, response: Option<&Value>) -> Vec<Bytes> {
        if self.has_text_delta {
            return Vec::new();
        }
        let Some(output) = response
            .and_then(|r| r.get("output"))
            .and_then(|v| v.as_array())
        else {
            return Vec::new();
        };
        let mut text = String::new();
        for item in output {
            if item.get("type").and_then(|v| v.as_str()) != Some("message") {
                continue;
            }
            text.push_str(&extract_output_text(item.get("content")));
        }
        if text.is_empty() {
            return Vec::new();
        }
        let mut out = self.ensure_started();
        out.extend(self.emit_recovered_text(&text));
        out
    }

    /// 补发文本块(与 output_item.done 的 message 兜底共用):
    /// 关思考块 → 开 text → delta → 关 text
    pub(crate) fn emit_recovered_text(&mut self, text: &str) -> Vec<Bytes> {
        if self.has_text_delta {
            return Vec::new();
        }
        let mut out = self.finalize_thinking();
        out.extend(self.start_text());
        out.extend(self.text_delta(text));
        out.extend(self.stop_text());
        self.has_text_delta = true;
        out
    }

    /// 清空调用状态(对齐 clearCodexFunctionCalls,completed 后调用)
    pub(crate) fn clear_function_calls(&mut self) {
        self.function_calls.clear();
        self.function_call_queue.clear();
        self.active_function_call = None;
        self.last_function_call = None;
    }

}
