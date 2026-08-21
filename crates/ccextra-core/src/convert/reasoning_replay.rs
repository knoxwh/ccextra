// reasoning replay:responses 上游的服务器端 reasoning 状态回放
// (对齐 CLIProxyAPI internal/runtime/executor/xai_reasoning_replay.go +
// codex_executor_reasoning.go 的注入/过滤辅助)
//
// 背景:responses 协议 reasoning 是服务器端状态,store=false 时上游不保留。
// Claude Code 回传的 thinking 块经 anthropic→responses 转换虽会回放
// encrypted_content,但流式往返中签名/形状可能丢失(实测 grok 会话
// 33 个 function_call 仅 2 条 reasoning),模型每轮丢失决策记忆,重复
// 发相同工具调用陷入死循环。CPA 的解法:每轮从 response.completed 缓存
// 原始 reasoning/message/function_call 项,下轮注入 input[](按 call_id
// 对齐位置)。缓存状态由 server 层持有,本模块只做纯函数。

use serde_json::{json, Value};

/// call_id 匹配候选(对齐 codexReplayComparableCallIDs):
/// 原始 id + sanitize/缩短后的 claude 可见 id。CC 回传 tool_result 时
/// call_id 可能经 sanitize 与上游原始 id 不同,双候选兜底匹配。
fn comparable_call_ids(call_id: &str) -> Vec<String> {
    let call_id = call_id.trim();
    if call_id.is_empty() {
        return Vec::new();
    }
    let claude_visible = shorten_call_id(&super::tool_id::sanitize_claude_tool_id(call_id));
    if claude_visible.is_empty() || claude_visible == call_id {
        return vec![call_id.to_string()];
    }
    vec![call_id.to_string(), claude_visible]
}

/// call_id 超 64 字符确定性截短(对齐 shortenCodexReplayCallIDIfNeeded,
// 与 to_openai_responses::shorten_call_id 同算法)
fn shorten_call_id(id: &str) -> String {
    const LIMIT: usize = 64;
    if id.len() <= LIMIT {
        return id.to_string();
    }
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(id.as_bytes());
    let suffix = format!("_{}", hex::encode(&digest[..8]));
    let mut prefix_len = LIMIT.saturating_sub(suffix.len());
    while prefix_len > 0 && !id.is_char_boundary(prefix_len) {
        prefix_len -= 1;
    }
    format!("{}{}", &id[..prefix_len], suffix)
}

/// 工具调用项的判重键(对齐 codexReplayToolCallKeys):`{type}:{call_id}`
/// 每个匹配候选一个键。
fn tool_call_keys(item: &Value) -> Vec<String> {
    let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
    if !matches!(item_type, "function_call" | "custom_tool_call") {
        return Vec::new();
    }
    let call_id = item.get("call_id").and_then(|v| v.as_str()).unwrap_or("");
    comparable_call_ids(call_id)
        .into_iter()
        .map(|c| format!("{}:{}", item_type, c))
        .collect()
}

/// input 尾部最后一条 assistant message(对齐 xaiInputLastAssistantMessage)
fn input_last_assistant_message(input: &[Value]) -> Option<&Value> {
    input.iter().rev().find(|item| {
        let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
        (item_type.is_empty() || item_type == "message")
            && item
                .get("role")
                .and_then(|v| v.as_str())
                .is_some_and(|r| r.trim().eq_ignore_ascii_case("assistant"))
    })
}

/// 缓存项中的 assistant message(对齐 xaiReplayAssistantMessage)
fn replay_assistant_message(items: &[Value]) -> Option<&Value> {
    items.iter().find(|item| {
        item.get("type").and_then(|v| v.as_str()) == Some("message")
            && item
                .get("role")
                .and_then(|v| v.as_str())
                .is_some_and(|r| r.trim().eq_ignore_ascii_case("assistant"))
    })
}

/// assistant message content 归一化比较(对齐 xaiAssistantMessageParts):
/// 字符串形态视作单个 output_text;数组只认 output_text/refusal,
/// 其他 part 类型判不等(歧义保守处理)。
fn assistant_content_equal(left: &Value, right: &Value) -> bool {
    match message_parts(left) {
        None => false,
        Some(lp) => match message_parts(right) {
            None => false,
            Some(rp) => lp == rp,
        },
    }
}

/// content → (type, value) 列表;不可比形态返回 None
fn message_parts(content: &Value) -> Option<Vec<(String, String)>> {
    if let Some(s) = content.as_str() {
        return Some(vec![("output_text".to_string(), s.to_string())]);
    }
    let arr = content.as_array()?;
    let mut parts = Vec::with_capacity(arr.len());
    for part in arr {
        let ptype = part.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let value = match ptype {
            "output_text" => part.get("text").and_then(|v| v.as_str())?.to_string(),
            "refusal" => part.get("refusal").and_then(|v| v.as_str())?.to_string(),
            _ => return None,
        };
        parts.push((ptype.to_string(), value));
    }
    if parts.is_empty() {
        None
    } else {
        Some(parts)
    }
}

/// 过滤缓存项中 input 已覆盖的部分(对齐 filterXAIReasoningReplayItemsForInput):
/// - input 尾 assistant 与缓存 assistant 内容不匹配 → 歧义,整体不注入
/// - reasoning:input 已有相同 encrypted_content → 跳过
/// - message:input 已含相同 assistant 内容 → 跳过
/// - function_call/custom_tool_call:键为空或已存在 → 跳过;
///   无匹配 output(说明 CC 未回传该轮结果)→ 跳过
fn filter_replay_items(body: &Value, items: Vec<Value>) -> Vec<Value> {
    let Some(input_items) = body.get("input").and_then(|v| v.as_array()) else {
        return Vec::new();
    };

    let last_assistant = input_last_assistant_message(input_items);
    let cached_assistant = replay_assistant_message(&items);
    let assistant_matches = match (last_assistant, cached_assistant) {
        (Some(l), Some(c)) => assistant_content_equal(
            l.get("content").unwrap_or(&Value::Null),
            c.get("content").unwrap_or(&Value::Null),
        ),
        _ => false,
    };
    // 歧义历史:input 尾 assistant 与缓存 assistant 都在但内容不同,
    // 说明缓存已过期(可能来自别的分支),注入会重复轮次,放弃
    if last_assistant.is_some() && cached_assistant.is_some() && !assistant_matches {
        return Vec::new();
    }

    let mut existing_calls: std::collections::HashSet<String> = std::collections::HashSet::new();
    let mut existing_outputs: std::collections::HashSet<String> = std::collections::HashSet::new();
    for item in input_items {
        let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if matches!(
            item_type,
            "function_call_output" | "custom_tool_call_output"
        ) {
            if let Some(call_id) = item
                .get("call_id")
                .and_then(|v| v.as_str())
                .filter(|c| !c.trim().is_empty())
            {
                for candidate in comparable_call_ids(call_id) {
                    existing_outputs.insert(candidate);
                }
            }
        }
        for key in tool_call_keys(item) {
            existing_calls.insert(key);
        }
    }

    let mut filtered = Vec::with_capacity(items.len());
    for item in items {
        let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match item_type {
            "reasoning" => {
                let enc = item.get("encrypted_content").and_then(|v| v.as_str());
                if let Some(enc) = enc.filter(|e| !e.is_empty()) {
                    if input_has_encrypted_content(input_items, enc) {
                        continue;
                    }
                }
            }
            "message" => {
                if assistant_matches {
                    continue;
                }
            }
            "function_call" | "custom_tool_call" => {
                let keys = tool_call_keys(&item);
                if keys.is_empty() || keys.iter().any(|k| existing_calls.contains(k)) {
                    continue;
                }
                let has_matching_output = item
                    .get("call_id")
                    .and_then(|v| v.as_str())
                    .map(|c| {
                        comparable_call_ids(c)
                            .iter()
                            .any(|candidate| existing_outputs.contains(candidate))
                    })
                    .unwrap_or(false);
                if !has_matching_output {
                    continue;
                }
                for key in keys {
                    existing_calls.insert(key);
                }
            }
            _ => continue,
        }
        filtered.push(item);
    }
    filtered
}

/// input 中是否已有相同 encrypted_content 的 reasoning 项
/// (对齐 xaiInputHasReasoningEncryptedContent)
fn input_has_encrypted_content(input_items: &[Value], encrypted: &str) -> bool {
    input_items.iter().any(|item| {
        item.get("type").and_then(|v| v.as_str()) == Some("reasoning")
            && item.get("encrypted_content").and_then(|v| v.as_str()) == Some(encrypted)
    })
}

/// 注入位置(对齐 codexReasoningReplayInsertIndex):
/// - replay 含 call_id:首个未匹配 replay 的 function_call_output 前
///   (call_id 取全部 comparable 候选,对齐 CPA 的 replayCallIDs 收集)
/// - 否则:最后一条 assistant message 处(插其前)
/// - 都没有:input 末尾
fn replay_insert_index(input_items: &[Value], replay_items: &[Value]) -> usize {
    let mut replay_call_ids: std::collections::HashSet<String> = std::collections::HashSet::new();
    for item in replay_items {
        let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if !matches!(item_type, "function_call" | "custom_tool_call") {
            continue;
        }
        if let Some(call_id) = item
            .get("call_id")
            .and_then(|v| v.as_str())
            .map(|c| c.trim())
            .filter(|c| !c.is_empty())
        {
            replay_call_ids.extend(comparable_call_ids(call_id));
        }
    }
    if !replay_call_ids.is_empty() {
        for (index, item) in input_items.iter().enumerate() {
            let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
            if !matches!(
                item_type,
                "function_call_output" | "custom_tool_call_output"
            ) {
                continue;
            }
            let call_id = item
                .get("call_id")
                .and_then(|v| v.as_str())
                .map(|c| c.trim())
                .unwrap_or("");
            if call_id.is_empty()
                || comparable_call_ids(call_id)
                    .iter()
                    .any(|c| replay_call_ids.contains(c))
            {
                return index;
            }
        }
    }
    for index in (0..input_items.len()).rev() {
        let item = &input_items[index];
        let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
        let role = item
            .get("role")
            .and_then(|v| v.as_str())
            .map(|r| r.trim().to_ascii_lowercase())
            .unwrap_or_default();
        if role == "assistant" && (item_type.is_empty() || item_type == "message") {
            return index;
        }
    }
    input_items.len()
}

/// output call_id 映射(对齐 codexReplayOutputCallIDs):
/// 每个 output 的所有匹配候选 → 原始 call_id
fn output_call_ids(input_items: &[Value]) -> std::collections::HashMap<String, String> {
    let mut map = std::collections::HashMap::new();
    for item in input_items {
        let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if !matches!(
            item_type,
            "function_call_output" | "custom_tool_call_output"
        ) {
            continue;
        }
        let Some(call_id) = item
            .get("call_id")
            .and_then(|v| v.as_str())
            .map(|c| c.trim())
            .filter(|c| !c.is_empty())
        else {
            continue;
        };
        for candidate in comparable_call_ids(call_id) {
            map.insert(candidate, call_id.to_string());
        }
    }
    map
}

/// replay 工具调用项 call_id 对齐到 input 已有 output 的形态
/// (对齐 codexAlignReasoningReplayToolCallIDs)
fn align_replay_call_ids(input_items: &[Value], replay_items: Vec<Value>) -> Vec<Value> {
    let outputs = output_call_ids(input_items);
    if outputs.is_empty() {
        return replay_items;
    }
    replay_items
        .into_iter()
        .map(|mut item| {
            let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
            if !matches!(item_type, "function_call" | "custom_tool_call") {
                return item;
            }
            let call_id = item
                .get("call_id")
                .and_then(|v| v.as_str())
                .map(|c| c.trim().to_string())
                .unwrap_or_default();
            let mut output_call_id = None;
            for candidate in comparable_call_ids(&call_id) {
                if let Some(v) = outputs.get(&candidate) {
                    output_call_id = Some(v.clone());
                    break;
                }
            }
            match output_call_id {
                Some(v) if v != call_id => {
                    if let Some(obj) = item.as_object_mut() {
                        obj.insert("call_id".to_string(), Value::String(v));
                    }
                    item
                }
                _ => item,
            }
        })
        .collect()
}

/// assistant message 内容指纹(对齐 codexReplayAssistantMessageFingerprint):
/// 字符串 content 直接取;数组只认 input_text/output_text/refusal,
/// 其他 part 类型返回空串(歧义保守)。sha256 hex,空内容空串。
fn assistant_message_fingerprint(item: &Value) -> String {
    use sha2::{Digest, Sha256};
    let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
    if !item_type.is_empty() && item_type != "message" {
        return String::new();
    }
    let is_assistant = item
        .get("role")
        .and_then(|v| v.as_str())
        .is_some_and(|r| r.trim().eq_ignore_ascii_case("assistant"));
    if !is_assistant {
        return String::new();
    }
    let mut builder = String::new();
    match item.get("content") {
        Some(Value::String(s)) => builder.push_str(s),
        Some(Value::Array(parts)) => {
            for part in parts {
                match part.get("type").and_then(|v| v.as_str()).unwrap_or("") {
                    "input_text" | "output_text" => {
                        builder.push_str(part.get("text").and_then(|v| v.as_str()).unwrap_or(""));
                    }
                    "refusal" => {
                        builder.push_str("\u{0}refusal\u{0}");
                        builder
                            .push_str(part.get("refusal").and_then(|v| v.as_str()).unwrap_or(""));
                    }
                    _ => return String::new(),
                }
            }
        }
        _ => return String::new(),
    }
    if builder.is_empty() {
        return String::new();
    }
    hex::encode(Sha256::digest(builder.as_bytes()))
}

/// input 前 end 项的指纹(对齐 codexReplayInputPrefixFingerprint):
/// 逐项吸收 `\0item\0` + 原始 JSON 字节,sha256 hex。
/// end 越界返回空串。
pub fn input_prefix_fingerprint(input_items: &[Value], end: usize) -> String {
    use sha2::{Digest, Sha256};
    if end > input_items.len() {
        return String::new();
    }
    let mut hasher = Sha256::new();
    for item in &input_items[..end] {
        hasher.update(b"\0item\0");
        hasher.update(serde_json::to_vec(item).unwrap_or_default());
    }
    hex::encode(hasher.finalize())
}

/// turn 边界 marker 类型(对齐 CPA CodexReasoningReplayTurnType,改名避免
/// 与 CPA 字面量混淆;注入前 filter 会剔除,永不上游)
pub const REPLAY_TURN_TYPE: &str = "ccextra_replay_turn";

/// 累积缓存的每轮 turn 上限(对齐 CodexReasoningReplayCacheMaxTurnsPerEntry)
pub const MAX_REPLAY_TURNS: usize = 256;
/// 累积缓存单条目字节上限(对齐 CodexReasoningReplayCacheMaxBytesPerEntry)
pub const MAX_REPLAY_BYTES: usize = 16 << 20;

/// 从 response.completed 构造一个 turn:[marker, ...items]
/// (对齐 cacheCodexReasoningReplayFromCompleted)。
///
/// marker id = sha256(request_fingerprint、assistant_fingerprint、call_ids
/// 与 items 原始字节拼接),同轮重放 id 相同,append 时按 id 去重。
/// items 不归一化(CPA codex 路径存 raw,只认 reasoning/function_call/
/// custom_tool_call/message,不缓存 output——output 由客户端 input 提供)。
/// 无 reasoning/function_call/custom_tool_call 项返回 None。
pub fn build_replay_turn(completed: &Value, request_fingerprint: &str) -> Option<Vec<Value>> {
    use sha2::{Digest, Sha256};
    let output = completed.pointer("/response/output")?.as_array()?;
    let mut items: Vec<Value> = Vec::with_capacity(output.len());
    let mut call_ids: Vec<String> = Vec::new();
    let mut assistant_fingerprint = String::new();
    for item in output {
        match item.get("type").and_then(|v| v.as_str()).unwrap_or("") {
            "reasoning" | "function_call" | "custom_tool_call" => {
                items.push(item.clone());
                if let Some(call_id) = item
                    .get("call_id")
                    .and_then(|v| v.as_str())
                    .map(|c| c.trim())
                    .filter(|c| !c.is_empty())
                {
                    call_ids.push(call_id.to_string());
                }
            }
            "message" => {
                let fp = assistant_message_fingerprint(item);
                if !fp.is_empty() {
                    assistant_fingerprint = fp;
                }
            }
            _ => continue,
        }
    }
    if items.is_empty() {
        return None;
    }
    let mut hasher = Sha256::new();
    hasher.update(request_fingerprint.as_bytes());
    hasher.update(format!("\u{0}assistant\u{0}{}", assistant_fingerprint).as_bytes());
    for call_id in &call_ids {
        hasher.update(format!("\u{0}call\u{0}{}", call_id).as_bytes());
    }
    for item in &items {
        hasher.update(b"\0item\0");
        hasher.update(serde_json::to_vec(item).unwrap_or_default());
    }
    let mut marker = json!({
        "type": REPLAY_TURN_TYPE,
        "id": hex::encode(hasher.finalize()),
    });
    if !assistant_fingerprint.is_empty() {
        marker["assistant_fingerprint"] = json!(assistant_fingerprint);
    }
    if !request_fingerprint.is_empty() {
        marker["request_fingerprint"] = json!(request_fingerprint);
    }
    if !call_ids.is_empty() {
        marker["call_ids"] = json!(call_ids);
    }
    let mut turn = Vec::with_capacity(items.len() + 1);
    turn.push(marker);
    turn.extend(items);
    Some(turn)
}

/// 累积 append 一个 turn(对齐 CPA appendCodexReasoningReplayTurn):
/// - existing 首项非 marker(旧单块格式)→ 丢弃重建,只保留新 turn
/// - turn id 已存在 → 幂等返回(不重复追加)
/// - 否则拼接后 trim(超限丢最老)
pub fn append_replay_turn(existing: &[Value], turn: &[Value]) -> Vec<Value> {
    let is_marker = |v: &Value| v.get("type").and_then(|t| t.as_str()) == Some(REPLAY_TURN_TYPE);
    let mut items: Vec<Value> = if !existing.is_empty() && !is_marker(&existing[0]) {
        Vec::new()
    } else {
        existing.to_vec()
    };
    let turn_id = turn
        .first()
        .filter(|m| is_marker(m))
        .and_then(|m| m.get("id"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if !turn_id.is_empty() {
        let dup = items.iter().any(|v| {
            is_marker(v) && v.get("id").and_then(|i| i.as_str()) == Some(turn_id.as_str())
        });
        if dup {
            return trim_replay_items(items);
        }
    }
    items.extend(turn.iter().cloned());
    trim_replay_items(items)
}

/// trim 到上限内(对齐 CPA trimCodexReasoningReplayItems):
/// 超 MAX_REPLAY_TURNS 个 turn 或 MAX_REPLAY_BYTES 总字节 → 循环丢最老 turn;
/// 单 turn 超限(turn_starts 只剩 1 仍超)→ 返回空。
fn trim_replay_items(mut items: Vec<Value>) -> Vec<Value> {
    let is_marker = |v: &Value| v.get("type").and_then(|t| t.as_str()) == Some(REPLAY_TURN_TYPE);
    loop {
        let mut turn_starts = vec![0usize];
        let mut total_bytes: usize = 0;
        for (index, item) in items.iter().enumerate() {
            total_bytes += serde_json::to_vec(item).map(|v| v.len()).unwrap_or(0);
            if index > 0 && is_marker(item) {
                turn_starts.push(index);
            }
        }
        if turn_starts.len() <= MAX_REPLAY_TURNS && total_bytes <= MAX_REPLAY_BYTES {
            return items;
        }
        if turn_starts.len() <= 1 {
            return Vec::new();
        }
        items.drain(..turn_starts[1]);
    }
}

/// 一个 replay turn:marker 元数据 + 该轮 items
struct ReplayTurn {
    marked: bool,
    assistant_fingerprint: String,
    request_fingerprint: String,
    call_ids: Vec<String>,
    items: Vec<Value>,
}

/// 按 marker 分段(对齐 splitCodexReasoningReplayTurns):
/// marker 开新段;无 marker 前导的头部 items 归入一个 unmarked 段。
fn split_replay_turns(items: &[Value]) -> Vec<ReplayTurn> {
    let mut turns: Vec<ReplayTurn> = Vec::new();
    let mut current = ReplayTurn {
        marked: false,
        assistant_fingerprint: String::new(),
        request_fingerprint: String::new(),
        call_ids: Vec::new(),
        items: Vec::new(),
    };
    for item in items {
        if item.get("type").and_then(|v| v.as_str()) == Some(REPLAY_TURN_TYPE) {
            if !current.items.is_empty() {
                turns.push(current);
            }
            current = ReplayTurn {
                marked: true,
                assistant_fingerprint: item
                    .get("assistant_fingerprint")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string(),
                request_fingerprint: item
                    .get("request_fingerprint")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string(),
                call_ids: item
                    .get("call_ids")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str())
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect()
                    })
                    .unwrap_or_default(),
                items: Vec::new(),
            };
            continue;
        }
        current.items.push(item.clone());
    }
    if !current.items.is_empty() {
        turns.push(current);
    }
    turns
}

/// 增量前缀指纹(对齐 codexReplayPrefixFingerprints):
/// 逐项推进 hasher,clone 中间状态取各前缀指纹,应答任意 end 查询。
struct PrefixFingerprints {
    hasher: sha2::Sha256, // 持续推进
    sums: Vec<String>,    // sums[end] = items[0:end] 指纹
    len: usize,
}

impl PrefixFingerprints {
    fn new(len: usize) -> Self {
        use sha2::{Digest, Sha256};
        Self {
            hasher: Sha256::new(),
            sums: vec![hex::encode(Sha256::new().finalize())],
            len,
        }
    }

    fn push(&mut self, item: &Value) {
        use sha2::Digest;
        self.hasher.update(b"\0item\0");
        self.hasher
            .update(serde_json::to_vec(item).unwrap_or_default());
        // clone 当前状态再 finalize,得到该前缀指纹
        let fp = hex::encode(self.hasher.clone().finalize());
        self.sums.push(fp);
    }

    fn at(&self, end: usize) -> String {
        if end > self.len {
            return String::new();
        }
        self.sums[end].clone()
    }
}

/// turn 锚点定位(对齐 codexReasoningReplayTurnAnchorIndex):
/// 1. call_ids 非空:从 searchEnd 向前找首个 call_id 匹配的 call/output 项
/// 2. 否则 assistant_fingerprint 非空:向前找指纹匹配的 assistant message
/// 3. 两者都空:退回现有 replay_insert_index
///
/// request_fingerprint 非空时要求锚点处前缀指纹匹配(防错位)。
/// 已用锚点(used)跳过。找不到返回 None。
fn turn_anchor_index(
    input_items: &[Value],
    turn: &ReplayTurn,
    fallback_end: usize,
    used: &mut std::collections::HashSet<usize>,
    fingerprints: &PrefixFingerprints,
) -> Option<usize> {
    let mut search_end = fallback_end.min(input_items.len().saturating_sub(1));
    if !turn.request_fingerprint.is_empty() {
        search_end = input_items.len().saturating_sub(1);
    }
    let matches_prefix = |index: usize| {
        turn.request_fingerprint.is_empty() || fingerprints.at(index) == turn.request_fingerprint
    };
    if !turn.call_ids.is_empty() {
        let mut wanted: std::collections::HashSet<String> = Default::default();
        for call_id in &turn.call_ids {
            wanted.extend(comparable_call_ids(call_id));
        }
        for index in (0..=search_end).rev() {
            if used.contains(&index) || !matches_prefix(index) {
                continue;
            }
            let item_type = input_items[index]
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if !matches!(
                item_type,
                "function_call"
                    | "custom_tool_call"
                    | "function_call_output"
                    | "custom_tool_call_output"
            ) {
                continue;
            }
            let call_id = input_items[index]
                .get("call_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if comparable_call_ids(call_id)
                .iter()
                .any(|c| wanted.contains(c))
            {
                return Some(index);
            }
        }
    }
    if !turn.assistant_fingerprint.is_empty() {
        for index in (0..=search_end).rev() {
            if used.contains(&index) || !matches_prefix(index) {
                continue;
            }
            if assistant_message_fingerprint(&input_items[index]) == turn.assistant_fingerprint {
                return Some(index);
            }
        }
    }
    if turn.call_ids.is_empty() && turn.assistant_fingerprint.is_empty() {
        return Some(replay_insert_index(input_items, &turn.items));
    }
    None
}

/// turn 内 items 过滤(对齐 filterCodexReasoningReplayTurnItems):
/// reasoning 按 encrypted_content 去重;call 按键去重且须有匹配 output;
/// message/output 直接丢(codex 路径不回放 output——output 由客户端提供,
/// call 的配对门保证只注入 input 已有 output 的 call)。
fn filter_turn_items(input_items: &[Value], items: Vec<Value>) -> Vec<Value> {
    let mut existing_reasoning: std::collections::HashSet<String> = Default::default();
    let mut existing_calls: std::collections::HashSet<String> = Default::default();
    let mut existing_outputs: std::collections::HashSet<String> = Default::default();
    for item in input_items {
        match item.get("type").and_then(|v| v.as_str()).unwrap_or("") {
            "reasoning" => {
                if let Some(enc) = item
                    .get("encrypted_content")
                    .and_then(|v| v.as_str())
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty())
                {
                    existing_reasoning.insert(enc.to_string());
                }
            }
            "function_call_output" | "custom_tool_call_output" => {
                if let Some(call_id) = item.get("call_id").and_then(|v| v.as_str()) {
                    existing_outputs.extend(comparable_call_ids(call_id));
                }
            }
            _ => {}
        }
        for key in tool_call_keys(item) {
            existing_calls.insert(key);
        }
    }
    let mut filtered = Vec::with_capacity(items.len());
    for item in items {
        match item.get("type").and_then(|v| v.as_str()).unwrap_or("") {
            "reasoning" => {
                let enc = item
                    .get("encrypted_content")
                    .and_then(|v| v.as_str())
                    .map(|s| s.trim())
                    .unwrap_or("");
                if existing_reasoning.contains(enc) {
                    continue;
                }
            }
            "function_call" | "custom_tool_call" => {
                let keys = tool_call_keys(&item);
                if keys.is_empty() || keys.iter().any(|k| existing_calls.contains(k)) {
                    continue;
                }
                let has_output = item
                    .get("call_id")
                    .and_then(|v| v.as_str())
                    .map(|c| {
                        comparable_call_ids(c)
                            .iter()
                            .any(|id| existing_outputs.contains(id))
                    })
                    .unwrap_or(false);
                if !has_output {
                    continue;
                }
                for key in keys {
                    existing_calls.insert(key);
                }
            }
            _ => continue,
        }
        filtered.push(item);
    }
    filtered
}

/// 累积 replay 注入(对齐 insertCodexReasoningReplayTurns):
/// 按 turn 从新到旧处理,各 turn 锚定后过滤、call_id 对齐、插入。
/// unmarked turn(旧格式)走现有单块路径。任一 turn 注入成功即 true。
pub fn insert_replay_turns(body: &mut Value, replay_items: Vec<Value>) -> bool {
    let Some(input_items) = body.get("input").and_then(|v| v.as_array().cloned()) else {
        return false;
    };
    if replay_items.is_empty() {
        return false;
    }
    let input_len = input_items.len();
    let turns = split_replay_turns(&replay_items);
    let mut insertions: std::collections::HashMap<usize, Vec<Value>> = Default::default();
    let mut used: std::collections::HashSet<usize> = Default::default();
    let mut fingerprints = PrefixFingerprints::new(input_len);
    for item in &input_items {
        fingerprints.push(item);
    }
    let mut fallback_end = input_len.saturating_sub(1);
    let mut inserted = false;
    for turn in turns.iter().rev() {
        if turn.items.is_empty() {
            continue;
        }
        if !turn.marked {
            let filtered = filter_replay_items(body, turn.items.clone());
            if filtered.is_empty() {
                continue;
            }
            let index = replay_insert_index(&input_items, &filtered);
            let aligned = align_replay_call_ids(&input_items, filtered);
            insertions.entry(index).or_default().extend(aligned);
            inserted = true;
            continue;
        }
        let Some(anchor) =
            turn_anchor_index(&input_items, turn, fallback_end, &mut used, &fingerprints)
        else {
            continue;
        };
        used.insert(anchor);
        if turn.request_fingerprint.is_empty() {
            fallback_end = anchor.saturating_sub(1);
        }
        let filtered = filter_turn_items(&input_items, turn.items.clone());
        if filtered.is_empty() {
            continue;
        }
        let aligned = align_replay_call_ids(&input_items, filtered);
        insertions.entry(anchor).or_default().extend(aligned);
        inserted = true;
    }
    if !inserted {
        return false;
    }
    let mut merged: Vec<Value> = Vec::with_capacity(input_len + replay_items.len());
    for (index, item) in input_items.into_iter().enumerate() {
        if let Some(extra) = insertions.remove(&index) {
            merged.extend(extra);
        }
        merged.push(item);
    }
    // 末尾插入键 = input_len(对齐 CPA insertions[len(inputItems)])
    if let Some(tail) = insertions.remove(&input_len) {
        merged.extend(tail);
    }
    if let Some(obj) = body.as_object_mut() {
        obj.insert("input".to_string(), Value::Array(merged));
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_filter_drops_existing_call_and_requires_output() {
        let body = json!({"input": [
            {"type": "message", "role": "user", "content": "q"},
            {"type": "function_call", "call_id": "call_1", "name": "f", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "call_1", "output": "ok"},
            {"type": "function_call_output", "call_id": "call_2", "output": "pending"}
        ]});
        let items = vec![
            json!({"type": "reasoning", "encrypted_content": "gAAA"}),
            // call_1 已在 input,跳过
            json!({"type": "function_call", "call_id": "call_1", "name": "f", "arguments": "{}"}),
            // call_3 无匹配 output,跳过
            json!({"type": "function_call", "call_id": "call_3", "name": "f", "arguments": "{}"}),
        ];
        let filtered = filter_replay_items(&body, items);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0]["type"], "reasoning");
    }

    #[test]
    fn test_filter_reasoning_dedup_by_encrypted_content() {
        let body = json!({"input": [
            {"type": "reasoning", "encrypted_content": "gAAA"}
        ]});
        let items = vec![
            json!({"type": "reasoning", "encrypted_content": "gAAA"}),
            json!({"type": "reasoning", "encrypted_content": "gBBB"}),
        ];
        let filtered = filter_replay_items(&body, items);
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0]["encrypted_content"], "gBBB");
    }

    #[test]
    fn test_filter_ambiguous_assistant_history_blocks_all() {
        let body = json!({"input": [
            {"type": "message", "role": "user", "content": "q"},
            {"type": "message", "role": "assistant", "content": "different"}
        ]});
        let items = vec![
            json!({"type": "reasoning", "encrypted_content": "gAAA"}),
            json!({"type": "message", "role": "assistant", "content": [
                {"type": "output_text", "text": "cached"}
            ]}),
        ];
        assert!(filter_replay_items(&body, items).is_empty());
    }

    #[test]
    fn test_filter_matching_assistant_message_dropped() {
        let body = json!({"input": [
            {"type": "message", "role": "user", "content": "q"},
            {"type": "message", "role": "assistant", "content": [
                {"type": "output_text", "text": "same"}
            ]}
        ]});
        let items = vec![
            json!({"type": "reasoning", "encrypted_content": "gAAA"}),
            json!({"type": "message", "role": "assistant", "content": "same"}),
        ];
        let filtered = filter_replay_items(&body, items);
        // reasoning 保留,assistant message 已在 input 跳过
        assert_eq!(filtered.len(), 1);
        assert_eq!(filtered[0]["type"], "reasoning");
    }

    #[test]
    fn test_insert_before_first_unmatched_output() {
        let mut body = json!({"input": [
            {"type": "message", "role": "user", "content": "q"},
            {"type": "function_call_output", "call_id": "call_1", "output": "ok"},
            {"type": "message", "role": "user", "content": "next"}
        ]});
        let items = vec![
            json!({"type": "reasoning", "encrypted_content": "gAAA"}),
            json!({"type": "function_call", "call_id": "call_1", "name": "f", "arguments": "{}"}),
        ];
        assert!(insert_replay_turns(&mut body, items));
        let input = body["input"].as_array().unwrap();
        // 插在首个 output 前(index 1),reasoning 与 function_call 都在其前
        assert_eq!(input.len(), 5);
        assert_eq!(input[1]["type"], "reasoning");
        assert_eq!(input[2]["type"], "function_call");
        assert_eq!(input[3]["type"], "function_call_output");
    }

    #[test]
    fn test_insert_after_last_assistant_when_no_calls() {
        let mut body = json!({"input": [
            {"type": "message", "role": "user", "content": "q"},
            {"type": "message", "role": "assistant", "content": "a"},
            {"type": "message", "role": "user", "content": "next"}
        ]});
        let items = vec![json!({"type": "reasoning", "encrypted_content": "gAAA"})];
        assert!(insert_replay_turns(&mut body, items));
        let input = body["input"].as_array().unwrap();
        // 插在最后一条 assistant 处(其前)
        assert_eq!(input.len(), 4);
        assert_eq!(input[1]["type"], "reasoning");
        assert_eq!(input[2]["role"], "assistant");
    }

    #[test]
    fn test_insert_aligns_call_id_to_output_form() {
        // input 的 output call_id 是 sanitize 形态(如含非法字符被替换)
        let mut body = json!({"input": [
            {"type": "function_call_output", "call_id": "call_1", "output": "ok"}
        ]});
        let items = vec![json!({
            "type": "function_call",
            "call_id": "call.1",
            "name": "f",
            "arguments": "{}"
        })];
        assert!(insert_replay_turns(&mut body, items));
        let input = body["input"].as_array().unwrap();
        // call.1 sanitize 后为 call_1,与 output 匹配,对齐为 output 的形态
        assert_eq!(input[0]["call_id"], "call_1");
        assert_eq!(input[1]["call_id"], "call_1");
    }

    #[test]
    fn test_insert_empty_or_bad_input() {
        let mut body = json!({"input": []});
        assert!(!insert_replay_turns(&mut body, vec![]));
        let mut no_input = json!({});
        assert!(!insert_replay_turns(
            &mut no_input,
            vec![json!({"type": "reasoning", "encrypted_content": "g"})]
        ));
    }

    #[test]
    fn test_assistant_message_fingerprint() {
        // assistant message,字符串 content
        let m1 = json!({"type": "message", "role": "assistant", "content": "hello"});
        let f1 = assistant_message_fingerprint(&m1);
        assert!(!f1.is_empty());
        // 相同内容相同指纹
        let m2 = json!({"role": "assistant", "content": "hello"});
        assert_eq!(f1, assistant_message_fingerprint(&m2));
        // 不同内容不同指纹
        let m3 = json!({"role": "assistant", "content": "world"});
        assert_ne!(f1, assistant_message_fingerprint(&m3));
        // 数组 content:input_text/output_text 取 text,refusal 特殊标记
        let m4 = json!({"role": "assistant", "content": [
            {"type": "output_text", "text": "a"},
            {"type": "refusal", "refusal": "no"}
        ]});
        assert!(!assistant_message_fingerprint(&m4).is_empty());
        // user message / 非 message / 空 content → 空串
        assert_eq!(
            assistant_message_fingerprint(&json!({"role": "user", "content": "hello"})),
            ""
        );
        assert_eq!(
            assistant_message_fingerprint(&json!({"type": "function_call", "call_id": "c"})),
            ""
        );
        assert_eq!(
            assistant_message_fingerprint(&json!({"role": "assistant", "content": []})),
            ""
        );
        // 数组含未知 part 类型 → 空串(歧义保守)
        let m5 =
            json!({"role": "assistant", "content": [{"type": "input_image", "image_url": "x"}]});
        assert_eq!(assistant_message_fingerprint(&m5), "");
    }

    #[test]
    fn test_input_prefix_fingerprint() {
        let items = vec![
            json!({"type": "message", "role": "user", "content": "q"}),
            json!({"type": "function_call", "call_id": "c1", "name": "f", "arguments": "{}"}),
        ];
        let f0 = input_prefix_fingerprint(&items, 0);
        let f1 = input_prefix_fingerprint(&items, 1);
        let f2 = input_prefix_fingerprint(&items, 2);
        assert!(!f0.is_empty() && !f1.is_empty() && !f2.is_empty());
        assert_ne!(f0, f1);
        assert_ne!(f1, f2);
        // 前缀性质:不同长度不同指纹;越界返回空串
        assert_eq!(input_prefix_fingerprint(&items, 3), "");
        assert_eq!(input_prefix_fingerprint(&items, usize::MAX), "");
        // 空数组 end=0 有值(空哈希)
        assert!(!input_prefix_fingerprint(&[], 0).is_empty());
    }

    #[test]
    fn test_build_replay_turn_basic() {
        let completed = json!({"response": {"output": [
            {"type": "reasoning", "encrypted_content": "gAAA"},
            {"type": "function_call", "call_id": "c1", "name": "f", "arguments": "{}"},
            {"type": "message", "role": "assistant", "content": [
                {"type": "output_text", "text": "hi"}
            ]},
            {"type": "web_search_call"}
        ]}});
        let turn = build_replay_turn(&completed, "reqfp").unwrap();
        // marker + 2 个可回放项(message 只提指纹不入 items,web_search_call 丢弃)
        assert_eq!(turn.len(), 3);
        assert_eq!(turn[0]["type"], REPLAY_TURN_TYPE);
        assert!(!turn[0]["id"].as_str().unwrap().is_empty());
        // call_ids 收集
        assert_eq!(turn[0]["call_ids"], json!(["c1"]));
        // assistant_fingerprint 非空
        assert!(!turn[0]["assistant_fingerprint"]
            .as_str()
            .unwrap()
            .is_empty());
        // request_fingerprint 透传
        assert_eq!(turn[0]["request_fingerprint"], "reqfp");
        // items 原样保留(不归一化,对齐 CPA cacheCodexReasoningReplayFromCompleted)
        assert_eq!(turn[1]["encrypted_content"], "gAAA");
        assert_eq!(turn[2]["call_id"], "c1");
    }

    #[test]
    fn test_build_replay_turn_dedup_and_empty() {
        // 同一 completed + 同 request_fingerprint → marker id 相同(幂等去重键)
        let completed = json!({"response": {"output": [
            {"type": "reasoning", "encrypted_content": "gAAA"}
        ]}});
        let t1 = build_replay_turn(&completed, "fp").unwrap();
        let t2 = build_replay_turn(&completed, "fp").unwrap();
        assert_eq!(t1[0]["id"], t2[0]["id"]);
        // 不同 request_fingerprint → 不同 id
        let t3 = build_replay_turn(&completed, "fp2").unwrap();
        assert_ne!(t1[0]["id"], t3[0]["id"]);
        // 无可回放项 → None
        let empty = json!({"response": {"output": [{"type": "web_search_call"}]}});
        assert!(build_replay_turn(&empty, "fp").is_none());
        // output 缺失 → None
        assert!(build_replay_turn(&json!({"response": {}}), "fp").is_none());
    }

    #[test]
    fn test_append_replay_turn_accumulates() {
        let turn1 = vec![
            json!({"type": REPLAY_TURN_TYPE, "id": "t1"}),
            json!({"type": "reasoning", "encrypted_content": "g1"}),
        ];
        let turn2 = vec![
            json!({"type": REPLAY_TURN_TYPE, "id": "t2"}),
            json!({"type": "reasoning", "encrypted_content": "g2"}),
        ];
        let merged = append_replay_turn(&turn1, &turn2);
        assert_eq!(merged.len(), 4);
        assert_eq!(merged[0]["id"], "t1");
        assert_eq!(merged[2]["id"], "t2");
        // 幂等:同 id turn 重复 append 不增长
        let again = append_replay_turn(&merged, &turn2);
        assert_eq!(again.len(), 4);
        // 旧格式 existing(首项非 marker)→ 视为过期,整体替换
        let legacy = vec![json!({"type": "reasoning", "encrypted_content": "old"})];
        let replaced = append_replay_turn(&legacy, &turn1);
        assert_eq!(replaced.len(), 2);
        assert_eq!(replaced[0]["id"], "t1");
    }

    #[test]
    fn test_append_trims_oldest_beyond_limit() {
        // 构造 MAX_REPLAY_TURNS 个 turn,append 新 turn 后超限,丢最老
        let mut existing: Vec<Value> = Vec::new();
        for i in 0..MAX_REPLAY_TURNS {
            existing.push(json!({"type": REPLAY_TURN_TYPE, "id": format!("t{}", i)}));
            existing.push(json!({"type": "reasoning", "encrypted_content": format!("g{}", i)}));
        }
        let new_turn = vec![
            json!({"type": REPLAY_TURN_TYPE, "id": "t_new"}),
            json!({"type": "reasoning", "encrypted_content": "g_new"}),
        ];
        let merged = append_replay_turn(&existing, &new_turn);
        // 256 turn 上限:丢最老一个(t0),剩 256 个 marker
        let markers = merged
            .iter()
            .filter(|i| i["type"] == REPLAY_TURN_TYPE)
            .count();
        assert_eq!(markers, MAX_REPLAY_TURNS);
        // t0 已被丢,t_new 在
        assert!(!merged.iter().any(|i| i["id"] == "t0"));
        assert!(merged.iter().any(|i| i["id"] == "t_new"));
    }

    #[test]
    fn test_split_replay_turns() {
        let items = vec![
            json!({"type": REPLAY_TURN_TYPE, "id": "t1", "call_ids": ["c1"]}),
            json!({"type": "reasoning", "encrypted_content": "g1"}),
            json!({"type": REPLAY_TURN_TYPE, "id": "t2"}),
            json!({"type": "reasoning", "encrypted_content": "g2"}),
            json!({"type": "reasoning", "encrypted_content": "g3"}), // 尾部归入 t2 段
        ];
        let turns = split_replay_turns(&items);
        // CPA 语义:尾部无 marker 的 items 归入最后一个 marker 段,非新段
        assert_eq!(turns.len(), 2);
        assert!(turns[0].marked);
        assert_eq!(turns[0].call_ids, vec!["c1"]);
        assert_eq!(turns[0].items.len(), 1);
        assert!(turns[1].marked);
        assert_eq!(turns[1].items.len(), 2);
        // 头部无 marker 前导 = unmarked 段(旧格式兜底)
        let legacy = vec![
            json!({"type": "reasoning", "encrypted_content": "g0"}),
            json!({"type": REPLAY_TURN_TYPE, "id": "t1"}),
            json!({"type": "reasoning", "encrypted_content": "g1"}),
        ];
        let turns = split_replay_turns(&legacy);
        assert_eq!(turns.len(), 2);
        assert!(!turns[0].marked);
        assert!(turns[1].marked);
    }

    #[test]
    fn test_insert_replay_turns_two_rounds_anchored() {
        // 两轮缓存,input 已含第二轮的 call/output;第一轮的 call/output
        // 已被 CC compact 掉(不在 input)。各轮 reasoning 锚定到对应位置。
        let mut body = json!({"input": [
            {"type": "message", "role": "user", "content": "q1"},
            {"type": "message", "role": "user", "content": "q2"},
            {"type": "function_call", "call_id": "c2", "name": "f", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "c2", "output": "ok2"}
        ]});
        let replay = vec![
            json!({"type": REPLAY_TURN_TYPE, "id": "t1", "call_ids": ["c1"]}),
            json!({"type": "reasoning", "encrypted_content": "g1"}),
            json!({"type": "function_call", "call_id": "c1", "name": "f", "arguments": "{}"}),
            json!({"type": REPLAY_TURN_TYPE, "id": "t2", "call_ids": ["c2"]}),
            json!({"type": "reasoning", "encrypted_content": "g2"}),
            json!({"type": "function_call", "call_id": "c2", "name": "f", "arguments": "{}"}),
        ];
        assert!(insert_replay_turns(&mut body, replay));
        let input = body["input"].as_array().unwrap();
        // t2 锚定 c2(在 input),t1 的 c1 不在 input → 锚定失败被丢
        // (call 无匹配 output,filter 剔除;reasoning 无锚也不注入)
        // CPA 语义:从尾往前扫先命中 output 项,插入在锚点项前
        // = g2 插在 c2 的 call 与 output 之间
        let idx_g2 = input.iter().position(|i| i["encrypted_content"] == "g2");
        let idx_c2 = input
            .iter()
            .position(|i| i["call_id"] == "c2" && i["type"] == "function_call");
        let idx_o2 = input
            .iter()
            .position(|i| i["call_id"] == "c2" && i["type"] == "function_call_output");
        assert!(idx_g2.is_some());
        assert!(idx_c2.unwrap() < idx_g2.unwrap());
        assert!(idx_g2.unwrap() < idx_o2.unwrap());
        assert!(!input.iter().any(|i| i["encrypted_content"] == "g1"));
        // c2 的 call 已在 input,不重复注入
        let c2_calls = input
            .iter()
            .filter(|i| i["type"] == "function_call" && i["call_id"] == "c2")
            .count();
        assert_eq!(c2_calls, 1);
    }

    #[test]
    fn test_insert_replay_turns_unmarked_fallback() {
        // 旧格式(无 marker):整块走现有 filter+insert 语义
        let mut body = json!({"input": [
            {"type": "message", "role": "user", "content": "q"},
            {"type": "function_call_output", "call_id": "c1", "output": "ok"}
        ]});
        let replay = vec![
            json!({"type": "reasoning", "encrypted_content": "g1"}),
            json!({"type": "function_call", "call_id": "c1", "name": "f", "arguments": "{}"}),
        ];
        assert!(insert_replay_turns(&mut body, replay));
        let input = body["input"].as_array().unwrap();
        // 插在首个 output 前
        assert_eq!(input[1]["type"], "reasoning");
        assert_eq!(input[2]["type"], "function_call");
        assert_eq!(input[3]["type"], "function_call_output");
    }

    #[test]
    fn test_insert_replay_turns_empty() {
        let mut body = json!({"input": []});
        assert!(!insert_replay_turns(&mut body, vec![]));
    }
}
