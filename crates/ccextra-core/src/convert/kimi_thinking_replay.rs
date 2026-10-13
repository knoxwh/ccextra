//! Kimi thinking 多轮回放（Claude Messages / protocol:claude 直通）。
//!
//! 对齐 CPA `internal/runtime/executor/kimi_thinking_replay.go`：
//! - 缓存带非空 signature 的 thinking + tool_use 的完整 assistant content 数组
//! - Claude Code 剥掉非法 Anthropic 签名的 thinking 后，按下轮非 thinking 块指纹还原
//! - 仅签名非空即可入库（不强制固定长度校验；长度门留给 signature.rs）
//!
//! 纯函数，无 IO；server 层持有缓存。

use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

/// 单条 content 数组字节上限（对齐 CPA KimiThinkingReplayCacheMaxBytesPerEntry）
pub const MAX_BYTES_PER_ENTRY: usize = 8 << 20;
/// 单条 content 块数上限（对齐 CPA MaxBlocksPerEntry）
pub const MAX_BLOCKS_PER_ENTRY: usize = 512;

/// 模型是否启用 Kimi thinking replay（对齐 signature_provider_from_model_name 的 Kimi 族）。
/// protocol:claude + 第三方 base_url 时按 **upstream_model 名** 判定，不按 URL。
pub fn is_kimi_thinking_replay_model(model: &str) -> bool {
    matches!(
        super::signature::signature_provider_from_model_name(model),
        super::signature::SignatureProvider::Kimi
    )
}

/// 剥 thinking 后缀 `(…)` / `[1m]`，再归一家族键（对齐 CPA kimiThinkingReplayModelFamily）。
pub fn kimi_thinking_replay_model_family(model: &str) -> String {
    let base = strip_thinking_and_context_suffix(model);
    let normalized = normalize_kimi_upstream_model(&base);
    match normalized.as_str() {
        "k3" | "k3-256k" => "k3".to_string(),
        other => other.to_string(),
    }
}

fn strip_thinking_and_context_suffix(model: &str) -> String {
    let mut s = model.trim().to_string();
    // Claude Code effort / thinking 后缀：`model(high)` / `model(1024)`
    if let Some(open) = s.rfind('(') {
        if s.ends_with(')') && open > 0 {
            s = s[..open].to_string();
        }
    }
    let lower = s.to_ascii_lowercase();
    if let Some(stripped) = lower.strip_suffix("[1m]") {
        // 保留原大小写前缀长度
        let keep = stripped.len();
        s = s[..keep].to_string();
    }
    s.trim().to_string()
}

/// 对齐 CPA `normalizeKimiUpstreamModel`（不含 thinking 后缀重挂，家族键用裸名）。
fn normalize_kimi_upstream_model(model: &str) -> String {
    let base = model.trim().to_ascii_lowercase();
    match base.as_str() {
        "kimi-k2.8" | "k2.8" | "kimi-k2.8-code" | "k2.8-code" | "kimi-k2.8-preview"
        | "k2.8-preview" | "kimi-k2.7-code" | "k2.7-code" | "kimi-for-coding" | "for-coding" => {
            "kimi-for-coding".to_string()
        }
        "kimi-k2.7-code-highspeed"
        | "k2.7-code-highspeed"
        | "kimi-for-coding-highspeed"
        | "for-coding-highspeed" => "kimi-for-coding-highspeed".to_string(),
        _ => strip_kimi_prefix(&base),
    }
}

fn strip_kimi_prefix(model: &str) -> String {
    model.strip_prefix("kimi-").unwrap_or(model).to_string()
}

/// content 是否可入库：至少一个带非空 signature 的 thinking，且至少一个带 id 的 tool_use。
pub fn content_is_replayable(content: &Value) -> bool {
    let Some(arr) = content.as_array() else {
        return false;
    };
    if arr.is_empty() || arr.len() > MAX_BLOCKS_PER_ENTRY {
        return false;
    }
    let mut has_signed_thinking = false;
    let mut has_tool_use = false;
    for part in arr {
        match part
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
        {
            "thinking" => {
                if part
                    .get("signature")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .is_some_and(|s| !s.is_empty())
                {
                    has_signed_thinking = true;
                }
            }
            "tool_use" => {
                if part
                    .get("id")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .is_some_and(|s| !s.is_empty())
                {
                    has_tool_use = true;
                }
            }
            _ => {}
        }
    }
    has_signed_thinking && has_tool_use
}

/// 校验并克隆可缓存 content；超限 / 非法 → None。
pub fn normalize_cached_content(content: &Value) -> Option<Value> {
    if !content_is_replayable(content) {
        return None;
    }
    let raw = serde_json::to_vec(content).ok()?;
    if raw.is_empty() || raw.len() > MAX_BYTES_PER_ENTRY {
        return None;
    }
    Some(content.clone())
}

fn content_has_thinking(content: &Value) -> bool {
    let Some(arr) = content.as_array() else {
        return false;
    };
    arr.iter().any(|part| {
        matches!(
            part.get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim(),
            "thinking" | "redacted_thinking"
        )
    })
}

/// 去掉 thinking / redacted_thinking 后的非 thinking 块列表；须至少含一个带 id 的 tool_use。
fn non_thinking_parts(content: &Value) -> Option<Vec<Value>> {
    let arr = content.as_array()?;
    let mut parts = Vec::with_capacity(arr.len());
    let mut has_tool_use = false;
    for part in arr {
        match part
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
        {
            "thinking" | "redacted_thinking" => continue,
            "tool_use" => {
                if part
                    .get("id")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .is_none_or(|s| s.is_empty())
                {
                    return None;
                }
                has_tool_use = true;
                parts.push(canonicalize_value(part));
            }
            _ => parts.push(canonicalize_value(part)),
        }
    }
    if has_tool_use {
        Some(parts)
    } else {
        None
    }
}

fn canonicalize_value(v: &Value) -> Value {
    match v {
        Value::Object(map) => {
            let mut keys: Vec<_> = map.keys().cloned().collect();
            keys.sort();
            let mut out = Map::new();
            for k in keys {
                if let Some(child) = map.get(&k) {
                    out.insert(k, canonicalize_value(child));
                }
            }
            Value::Object(out)
        }
        Value::Array(arr) => Value::Array(arr.iter().map(canonicalize_value).collect()),
        other => other.clone(),
    }
}

fn values_equal(a: &[Value], b: &[Value]) -> bool {
    a.len() == b.len() && a.iter().zip(b.iter()).all(|(l, r)| l == r)
}

fn content_equal(a: &Value, b: &Value) -> bool {
    canonicalize_value(a) == canonicalize_value(b)
}

/// 将缓存的完整 assistant content 写回 body.messages 中匹配的最后一条 assistant。
/// 已有 thinking / 已与缓存完全一致 → 不改。返回是否改写。
pub fn restore_kimi_thinking_replay(body: &mut Value, cached_content: &Value) -> bool {
    let Some(cached_parts) = non_thinking_parts(cached_content) else {
        return false;
    };
    let Some(messages) = body.get_mut("messages").and_then(|v| v.as_array_mut()) else {
        return false;
    };
    for index in (0..messages.len()).rev() {
        let role = messages[index]
            .get("role")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        if !role.eq_ignore_ascii_case("assistant") {
            continue;
        }
        let Some(current) = messages[index].get("content") else {
            continue;
        };
        if content_equal(current, cached_content) {
            return false;
        }
        if content_has_thinking(current) {
            continue;
        }
        let Some(current_parts) = non_thinking_parts(current) else {
            continue;
        };
        if !values_equal(&current_parts, &cached_parts) {
            continue;
        }
        messages[index]["content"] = cached_content.clone();
        return true;
    }
    false
}

/// 从非流 Anthropic message JSON 取 content 数组（可缓存形态）。
pub fn content_from_message_response(response: &Value) -> Option<Value> {
    let content = response.get("content")?;
    normalize_cached_content(content)
}

// ---- 流式 Anthropic SSE 累积（对齐 CPA kimiThinkingReplayStreamAccumulator）----

enum FieldKind {
    Text,
    Thinking,
    Signature,
}

struct StreamBlock {
    raw: Value,
    text: String,
    thinking: String,
    signature: String,
    input: String,
    text_initialized: bool,
    thinking_initialized: bool,
    signature_initialized: bool,
    has_input_delta: bool,
    finished: bool,
}

/// 旁路解析 Anthropic Messages SSE，组装完整 content 数组。
#[derive(Default)]
pub struct KimiThinkingReplayStreamAccumulator {
    blocks: BTreeMap<i64, StreamBlock>,
    observed: bool,
    complete: bool,
    upstream_error: bool,
    abandoned: bool,
    bytes_used: usize,
}

impl KimiThinkingReplayStreamAccumulator {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn upstream_error(&self) -> bool {
        self.upstream_error
    }

    /// 观察一帧 SSE data JSON（或整段含多行 data: 的 chunk）。
    pub fn observe_payload(&mut self, payload: &Value) {
        if self.abandoned {
            return;
        }
        match payload.get("type").and_then(|v| v.as_str()).unwrap_or("") {
            "message_start" => self.observed = true,
            "content_block_start" => self.observe_block_start(payload),
            "content_block_delta" => self.observe_block_delta(payload),
            "content_block_stop" => {
                let index = payload.get("index").and_then(|v| v.as_i64()).unwrap_or(0);
                self.finish_block(index);
            }
            "message_stop" => self.complete = true,
            "error" => {
                self.upstream_error = true;
                self.abandon();
            }
            _ => {}
        }
    }

    /// 观察原始 SSE 文本（可含多行 `data:`）。
    pub fn observe_sse_chunk(&mut self, chunk: &[u8]) {
        for line in chunk.split(|&b| b == b'\n') {
            let line = trim_ascii(line);
            let Some(rest) = line.strip_prefix(b"data:") else {
                continue;
            };
            let payload = trim_ascii(rest);
            if payload.is_empty() || payload == b"[DONE]" {
                continue;
            }
            let Ok(value) = serde_json::from_slice::<Value>(payload) else {
                self.abandon();
                continue;
            };
            self.observe_payload(&value);
        }
    }

    fn observe_block_start(&mut self, root: &Value) {
        let index = root.get("index").and_then(|v| v.as_i64()).unwrap_or(0);
        let Some(block) = root.get("content_block").filter(|v| v.is_object()) else {
            self.abandon();
            return;
        };
        if self.blocks.len() >= MAX_BLOCKS_PER_ENTRY {
            self.abandon();
            return;
        }
        if self.blocks.contains_key(&index) {
            self.abandon();
            return;
        }
        let raw_len = serde_json::to_vec(block).map(|v| v.len()).unwrap_or(0);
        if !self.reserve_bytes(raw_len) {
            return;
        }
        self.blocks.insert(
            index,
            StreamBlock {
                raw: block.clone(),
                text: String::new(),
                thinking: String::new(),
                signature: String::new(),
                input: String::new(),
                text_initialized: false,
                thinking_initialized: false,
                signature_initialized: false,
                has_input_delta: false,
                finished: false,
            },
        );
    }

    fn observe_block_delta(&mut self, root: &Value) {
        let index = root.get("index").and_then(|v| v.as_i64()).unwrap_or(0);
        if !self.blocks.contains_key(&index) {
            self.abandon();
            return;
        }
        let delta = root.get("delta").cloned().unwrap_or(Value::Null);
        let dtype = delta
            .get("type")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        match dtype.as_str() {
            "text_delta" => {
                let suffix = delta
                    .get("text")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                self.append_field(index, "text", &suffix, FieldKind::Text);
            }
            "thinking_delta" => {
                let suffix = delta
                    .get("thinking")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                self.append_field(index, "thinking", &suffix, FieldKind::Thinking);
            }
            "signature_delta" => {
                let suffix = delta
                    .get("signature")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                self.append_field(index, "signature", &suffix, FieldKind::Signature);
            }
            "input_json_delta" => {
                let suffix = delta
                    .get("partial_json")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                if self.reserve_bytes(suffix.len()) {
                    if let Some(block) = self.blocks.get_mut(&index) {
                        block.input.push_str(suffix);
                        block.has_input_delta = true;
                    }
                }
            }
            _ => self.abandon(),
        }
    }

    fn append_field(&mut self, index: i64, path: &str, suffix: &str, kind: FieldKind) {
        // Initialize from raw once
        let need_init = self
            .blocks
            .get(&index)
            .map(|b| match kind {
                FieldKind::Text => !b.text_initialized,
                FieldKind::Thinking => !b.thinking_initialized,
                FieldKind::Signature => !b.signature_initialized,
            })
            .unwrap_or(true);
        if need_init {
            let initial = self
                .blocks
                .get(&index)
                .and_then(|b| b.raw.get(path))
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .to_string();
            if !self.reserve_bytes(initial.len()) {
                return;
            }
            if let Some(block) = self.blocks.get_mut(&index) {
                match kind {
                    FieldKind::Text => {
                        block.text.push_str(&initial);
                        block.text_initialized = true;
                    }
                    FieldKind::Thinking => {
                        block.thinking.push_str(&initial);
                        block.thinking_initialized = true;
                    }
                    FieldKind::Signature => {
                        block.signature.push_str(&initial);
                        block.signature_initialized = true;
                    }
                }
            }
        }
        if !self.reserve_bytes(suffix.len()) {
            return;
        }
        if let Some(block) = self.blocks.get_mut(&index) {
            match kind {
                FieldKind::Text => block.text.push_str(suffix),
                FieldKind::Thinking => block.thinking.push_str(suffix),
                FieldKind::Signature => block.signature.push_str(suffix),
            }
        }
    }

    fn reserve_bytes(&mut self, count: usize) -> bool {
        if self.abandoned {
            return false;
        }
        if count > MAX_BYTES_PER_ENTRY
            || self.bytes_used > MAX_BYTES_PER_ENTRY.saturating_sub(count)
        {
            self.abandon();
            return false;
        }
        self.bytes_used += count;
        true
    }

    fn finish_block(&mut self, index: i64) {
        let Some(block) = self.blocks.get_mut(&index) else {
            self.abandon();
            return;
        };
        if block.has_input_delta && serde_json::from_str::<Value>(&block.input).is_err() {
            self.abandon();
            return;
        }
        block.finished = true;
    }

    fn abandon(&mut self) {
        self.abandoned = true;
        self.blocks.clear();
        self.bytes_used = 0;
    }

    /// 流完整且可组装时返回 content 数组；否则 None。
    pub fn take_content(&mut self) -> Option<Value> {
        if !self.observed || !self.complete || self.upstream_error || self.abandoned {
            return None;
        }
        let indexes: Vec<i64> = self.blocks.keys().copied().collect();
        let mut parts = Vec::with_capacity(indexes.len());
        for index in indexes {
            let block = self.blocks.get_mut(&index)?;
            if !block.finished {
                self.abandon();
                return None;
            }
            let mut raw = block.raw.clone();
            if block.text_initialized {
                raw["text"] = json!(block.text);
            }
            if block.thinking_initialized {
                raw["thinking"] = json!(block.thinking);
            }
            if block.signature_initialized {
                raw["signature"] = json!(block.signature);
            }
            if block.has_input_delta {
                match serde_json::from_str::<Value>(&block.input) {
                    Ok(input) => raw["input"] = input,
                    Err(_) => {
                        self.abandon();
                        return None;
                    }
                }
            }
            parts.push(raw);
        }
        let content = Value::Array(parts);
        let raw_len = serde_json::to_vec(&content).map(|v| v.len()).unwrap_or(0);
        if raw_len > MAX_BYTES_PER_ENTRY {
            self.abandon();
            return None;
        }
        Some(content)
    }
}

fn trim_ascii(s: &[u8]) -> &[u8] {
    let mut start = 0;
    let mut end = s.len();
    while start < end && s[start].is_ascii_whitespace() {
        start += 1;
    }
    while end > start && s[end - 1].is_ascii_whitespace() {
        end -= 1;
    }
    &s[start..end]
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn model_family_merges_k3_variants() {
        assert_eq!(kimi_thinking_replay_model_family("k3"), "k3");
        assert_eq!(kimi_thinking_replay_model_family("kimi-k3"), "k3");
        assert_eq!(kimi_thinking_replay_model_family("k3-256k"), "k3");
        assert_eq!(
            kimi_thinking_replay_model_family("kimi-k3-256k(high)"),
            "k3"
        );
        assert_eq!(
            kimi_thinking_replay_model_family("kimi-k2.7-code"),
            "kimi-for-coding"
        );
        assert_eq!(
            kimi_thinking_replay_model_family("kimi-k2.7-code-highspeed"),
            "kimi-for-coding-highspeed"
        );
        assert_eq!(
            kimi_thinking_replay_model_family("kimi-k2.8"),
            "kimi-for-coding"
        );
        assert_eq!(
            kimi_thinking_replay_model_family("kimi-k2.8-code[1m](high)"),
            "kimi-for-coding"
        );
        assert!(is_kimi_thinking_replay_model("kimi-k3"));
        assert!(is_kimi_thinking_replay_model("moonshot-v1"));
        assert!(!is_kimi_thinking_replay_model("claude-opus-4"));
    }

    #[test]
    fn restore_preserves_complete_assistant_content() {
        let cached = json!([
            {"type":"thinking","thinking":"full reasoning","signature":"kimi-signature"},
            {"type":"text","text":"I will inspect the file."},
            {"type":"tool_use","id":"toolu_1","name":"Read","input":{"path":"README.md"}}
        ]);
        let mut body = json!({"messages":[
            {"role":"user","content":"inspect"},
            {"role":"assistant","content":[
                {"type":"text","text":"I will inspect the file."},
                {"type":"tool_use","id":"toolu_1","name":"Read","input":{"path":"README.md"}}
            ]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"ok"}]}
        ]});
        assert!(restore_kimi_thinking_replay(&mut body, &cached));
        assert_eq!(body["messages"][1]["content"], cached);
    }

    #[test]
    fn restore_does_not_replace_existing_thinking() {
        let cached = json!([
            {"type":"thinking","thinking":"cached","signature":"cached-signature"},
            {"type":"tool_use","id":"toolu_1","name":"Read","input":{"path":"README.md"}}
        ]);
        let mut body = json!({"messages":[{
            "role":"assistant",
            "content":[
                {"type":"thinking","thinking":"current","signature":"current-signature"},
                {"type":"tool_use","id":"toolu_1","name":"Read","input":{"path":"README.md"}}
            ]
        }]});
        let before = body.clone();
        assert!(!restore_kimi_thinking_replay(&mut body, &cached));
        assert_eq!(body, before);
    }

    #[test]
    fn restore_skips_when_already_complete() {
        let cached = json!([
            {"type":"thinking","thinking":"t","signature":"sig"},
            {"type":"tool_use","id":"toolu_1","name":"Read","input":{}}
        ]);
        let mut body = json!({"messages":[{
            "role":"assistant",
            "content": cached.clone()
        }]});
        assert!(!restore_kimi_thinking_replay(&mut body, &cached));
    }

    #[test]
    fn restore_requires_tool_use_fingerprint_match() {
        let cached = json!([
            {"type":"thinking","thinking":"t","signature":"sig"},
            {"type":"tool_use","id":"toolu_1","name":"Read","input":{"path":"A"}}
        ]);
        let mut body = json!({"messages":[{
            "role":"assistant",
            "content":[
                {"type":"tool_use","id":"toolu_1","name":"Read","input":{"path":"B"}}
            ]
        }]});
        assert!(!restore_kimi_thinking_replay(&mut body, &cached));
    }

    #[test]
    fn content_is_replayable_requires_signed_thinking_and_tool() {
        assert!(!content_is_replayable(&json!([
            {"type":"thinking","thinking":"t","signature":""},
            {"type":"tool_use","id":"toolu_1","name":"Read","input":{}}
        ])));
        assert!(!content_is_replayable(&json!([
            {"type":"thinking","thinking":"t","signature":"sig"},
            {"type":"text","text":"hi"}
        ])));
        assert!(content_is_replayable(&json!([
            {"type":"thinking","thinking":"t","signature":"sig"},
            {"type":"tool_use","id":"toolu_1","name":"Read","input":{}}
        ])));
    }

    #[test]
    fn stream_accumulator_assembles_signed_tool_turn() {
        let mut acc = KimiThinkingReplayStreamAccumulator::new();
        acc.observe_payload(&json!({"type":"message_start","message":{"id":"m1"}}));
        acc.observe_payload(&json!({
            "type":"content_block_start","index":0,
            "content_block":{"type":"thinking","thinking":"","signature":""}
        }));
        acc.observe_payload(&json!({
            "type":"content_block_delta","index":0,
            "delta":{"type":"thinking_delta","thinking":"plan"}
        }));
        acc.observe_payload(&json!({
            "type":"content_block_delta","index":0,
            "delta":{"type":"signature_delta","signature":"kimi-sig"}
        }));
        acc.observe_payload(&json!({"type":"content_block_stop","index":0}));
        acc.observe_payload(&json!({
            "type":"content_block_start","index":1,
            "content_block":{"type":"tool_use","id":"toolu_1","name":"Read","input":{}}
        }));
        acc.observe_payload(&json!({
            "type":"content_block_delta","index":1,
            "delta":{"type":"input_json_delta","partial_json":"{\"path\":\"README.md\"}"}
        }));
        acc.observe_payload(&json!({"type":"content_block_stop","index":1}));
        acc.observe_payload(&json!({"type":"message_stop"}));
        let content = acc.take_content().expect("complete");
        assert!(content_is_replayable(&content));
        assert_eq!(content[0]["thinking"], "plan");
        assert_eq!(content[0]["signature"], "kimi-sig");
        assert_eq!(content[1]["input"]["path"], "README.md");
    }

    #[test]
    fn stream_accumulator_partial_does_not_commit() {
        let mut acc = KimiThinkingReplayStreamAccumulator::new();
        acc.observe_payload(&json!({"type":"message_start"}));
        acc.observe_payload(&json!({
            "type":"content_block_start","index":0,
            "content_block":{"type":"thinking","thinking":"","signature":""}
        }));
        assert!(acc.take_content().is_none());
    }

    #[test]
    fn stream_accumulator_error_marks_upstream() {
        let mut acc = KimiThinkingReplayStreamAccumulator::new();
        acc.observe_payload(&json!({"type":"message_start"}));
        acc.observe_payload(&json!({"type":"error","error":{"message":"bad"}}));
        assert!(acc.upstream_error());
        assert!(acc.take_content().is_none());
    }
}
