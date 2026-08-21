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

/// 从 response.completed 的 `response.output` 提取可回放项并归一化
/// (对齐 cacheXAIReasoningReplayFromCompleted + normalizeXAIReasoningReplayItems):
/// - reasoning:须有非空 encrypted_content,归一化为最小形状
///   {type, summary:[], content:null, encrypted_content}
/// - message:仅 assistant 且 content 数组含 output_text/refusal,
///   归一化为最小形状;非 assistant(user message)丢弃
/// - function_call:call_id/name/arguments 均有效,归一化最小形状
/// - custom_tool_call:call_id/name/input 有效,归一化最小形状
///
/// 无 reasoning/function_call/custom_tool_call 锚点(如只剩 message)返回
/// None——调用方应删除旧缓存,防止上一轮的 encrypted state 注入后续轮次。
pub fn extract_replay_items(completed: &Value) -> Option<Vec<Value>> {
    let output = completed.pointer("/response/output")?.as_array()?;
    let mut items: Vec<Value> = Vec::with_capacity(output.len());
    let mut has_anchor = false;
    for item in output {
        let normalized = match item.get("type").and_then(|v| v.as_str()) {
            Some("reasoning") => {
                let Some(enc) = item.get("encrypted_content").and_then(|v| v.as_str()) else {
                    continue;
                };
                if enc.trim().is_empty() || enc.trim() != enc {
                    continue;
                }
                has_anchor = true;
                json!({
                    "type": "reasoning",
                    "summary": [],
                    "content": null,
                    "encrypted_content": enc,
                })
            }
            Some("message") => {
                if item
                    .get("role")
                    .and_then(|v| v.as_str())
                    .map(|r| r.trim().eq_ignore_ascii_case("assistant"))
                    != Some(true)
                {
                    continue;
                }
                let Some(parts) = item.get("content").and_then(|v| v.as_array()) else {
                    continue;
                };
                let mut content: Vec<Value> = Vec::with_capacity(parts.len());
                for part in parts {
                    match part.get("type").and_then(|v| v.as_str()) {
                        Some("output_text") => {
                            if let Some(text) = part.get("text").and_then(|v| v.as_str()) {
                                content.push(json!({"type": "output_text", "text": text}));
                            }
                        }
                        Some("refusal") => {
                            if let Some(r) = part.get("refusal").and_then(|v| v.as_str()) {
                                content.push(json!({"type": "refusal", "refusal": r}));
                            }
                        }
                        _ => continue,
                    }
                }
                if content.is_empty() {
                    continue;
                }
                json!({"type": "message", "role": "assistant", "content": content})
            }
            Some("function_call") => {
                let call_id = item
                    .get("call_id")
                    .and_then(|v| v.as_str())
                    .map(|c| c.trim());
                let name = item.get("name").and_then(|v| v.as_str()).map(|c| c.trim());
                let arguments = item.get("arguments").and_then(|v| v.as_str());
                let (Some(call_id), Some(name), Some(arguments)) = (call_id, name, arguments)
                else {
                    continue;
                };
                if call_id.is_empty() || name.is_empty() {
                    continue;
                }
                has_anchor = true;
                json!({
                    "type": "function_call",
                    "call_id": call_id,
                    "name": name,
                    "arguments": arguments,
                })
            }
            Some("custom_tool_call") => {
                let call_id = item
                    .get("call_id")
                    .and_then(|v| v.as_str())
                    .map(|c| c.trim());
                let name = item.get("name").and_then(|v| v.as_str()).map(|c| c.trim());
                let input = item.get("input");
                let (Some(call_id), Some(name), Some(input)) = (call_id, name, input) else {
                    continue;
                };
                if call_id.is_empty() || name.is_empty() {
                    continue;
                }
                has_anchor = true;
                let mut obj = json!({
                    "type": "custom_tool_call",
                    "status": "completed",
                    "call_id": call_id,
                    "name": name,
                    "input": input,
                });
                if let Some(status) = item
                    .get("status")
                    .and_then(|v| v.as_str())
                    .map(|s| s.trim())
                {
                    if !status.is_empty() {
                        obj["status"] = json!(status);
                    }
                }
                obj
            }
            Some("function_call_output") => {
                let call_id = item
                    .get("call_id")
                    .and_then(|v| v.as_str())
                    .map(|c| c.trim());
                let output = item.get("output");
                let (Some(call_id), Some(output)) = (call_id, output) else {
                    continue;
                };
                if call_id.is_empty() {
                    continue;
                }
                json!({
                    "type": "function_call_output",
                    "call_id": call_id,
                    "output": output,
                })
            }
            Some("custom_tool_call_output") => {
                let call_id = item
                    .get("call_id")
                    .and_then(|v| v.as_str())
                    .map(|c| c.trim());
                let output = item.get("output");
                let (Some(call_id), Some(output)) = (call_id, output) else {
                    continue;
                };
                if call_id.is_empty() {
                    continue;
                }
                json!({
                    "type": "custom_tool_call_output",
                    "call_id": call_id,
                    "output": output,
                })
            }
            _ => continue,
        };
        items.push(normalized);
    }
    if items.is_empty() || !has_anchor {
        None
    } else {
        Some(items)
    }
}

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
pub fn filter_replay_items(body: &Value, items: Vec<Value>) -> Vec<Value> {
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
            "function_call_output" | "custom_tool_call_output" => {
                // 保留 output（它们跟随对应的 function_call）
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

/// 把 replay 项注入 body.input(对齐 insertCodexReasoningReplayItems)。
/// 注入前按 input 已有 output 的 call_id 对齐 replay 项的 call_id
/// (codexAlignReasoningReplayToolCallIDs:CC 回传的 output call_id 可能
/// 是 sanitize 形态,回放 function_call 须用同一形态才能配对)。
/// input 非数组或 replay 为空返回 false。
pub fn insert_replay_items(body: &mut Value, replay_items: Vec<Value>) -> bool {
    if replay_items.is_empty() {
        return false;
    }
    let Some(input_items) = body.get("input").and_then(|v| v.as_array()).cloned() else {
        return false;
    };
    let insert_index = replay_insert_index(&input_items, &replay_items);
    let replay_items = align_replay_call_ids(&input_items, replay_items);

    let mut merged: Vec<Value> = Vec::with_capacity(input_items.len() + replay_items.len());
    for (i, item) in input_items.into_iter().enumerate() {
        if i == insert_index {
            merged.extend(replay_items.iter().cloned());
        }
        merged.push(item);
    }
    if insert_index >= merged.len() {
        merged.extend(replay_items);
    }
    if let Some(obj) = body.as_object_mut() {
        obj.insert("input".to_string(), Value::Array(merged));
        true
    } else {
        false
    }
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
// TODO(task2): build_replay_turn 消费后移除 allow
#[allow(dead_code)]
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
                        builder.push_str(part.get("refusal").and_then(|v| v.as_str()).unwrap_or(""));
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn completed(output: Value) -> Value {
        json!({"type": "response.completed", "response": {"id": "r1", "output": output}})
    }

    #[test]
    fn test_extract_replay_items_four_types() {
        let v = completed(json!([
            {"type": "reasoning", "encrypted_content": "gAAA"},
            {"type": "message", "role": "assistant", "content": [
                {"type": "output_text", "text": "hi"}
            ]},
            {"type": "function_call", "call_id": "c1", "name": "f", "arguments": "{}"},
            {"type": "custom_tool_call", "call_id": "c2", "name": "g", "input": "x"},
            {"type": "web_search_call"},
            {"type": "message", "role": "user", "content": "q"}
        ]));
        let items = extract_replay_items(&v).unwrap();
        // user message 丢弃(对齐 normalizeXAIReasoningReplayMessageItem 只认 assistant)
        assert_eq!(items.len(), 4);
        assert_eq!(items[0]["type"], "reasoning");
        assert_eq!(items[1]["role"], "assistant");
        assert_eq!(items[3]["type"], "custom_tool_call");
        // 归一化最小形状
        assert_eq!(items[0]["summary"], json!([]));
        assert_eq!(items[0]["content"], Value::Null);
    }

    #[test]
    fn test_extract_none_when_no_replayable() {
        let v = completed(json!([{"type": "web_search_call"}]));
        assert!(extract_replay_items(&v).is_none());
        // output 非数组
        assert!(extract_replay_items(&json!({"response": {"output": null}})).is_none());
    }

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
        assert!(insert_replay_items(&mut body, items));
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
        assert!(insert_replay_items(&mut body, items));
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
        assert!(insert_replay_items(&mut body, items));
        let input = body["input"].as_array().unwrap();
        // call.1 sanitize 后为 call_1,与 output 匹配,对齐为 output 的形态
        assert_eq!(input[0]["call_id"], "call_1");
        assert_eq!(input[1]["call_id"], "call_1");
    }

    #[test]
    fn test_insert_empty_or_bad_input() {
        let mut body = json!({"input": []});
        assert!(!insert_replay_items(&mut body, vec![]));
        let mut no_input = json!({});
        assert!(!insert_replay_items(
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
        assert_eq!(assistant_message_fingerprint(&json!({"role": "user", "content": "hello"})), "");
        assert_eq!(assistant_message_fingerprint(&json!({"type": "function_call", "call_id": "c"})), "");
        assert_eq!(assistant_message_fingerprint(&json!({"role": "assistant", "content": []})), "");
        // 数组含未知 part 类型 → 空串(歧义保守)
        let m5 = json!({"role": "assistant", "content": [{"type": "input_image", "image_url": "x"}]});
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
    fn test_extract_includes_function_call_output() {
        let completed = json!({
            "response": {
                "output": [
                    {"type": "function_call", "call_id": "call_1", "name": "tool_a", "arguments": "{}"},
                    {"type": "function_call_output", "call_id": "call_1", "output": [{"type": "input_text", "text": "result"}]},
                    {"type": "reasoning", "encrypted_content": "gAAAA-valid", "content": "think"},
                ]
            }
        });
        let items = extract_replay_items(&completed).unwrap();
        assert_eq!(items.len(), 3);
        assert_eq!(items[0]["type"], "function_call");
        assert_eq!(items[1]["type"], "function_call_output");
        assert_eq!(items[1]["call_id"], "call_1");
        assert_eq!(items[2]["type"], "reasoning");
    }

    #[test]
    fn test_extract_includes_custom_tool_call_output() {
        let completed = json!({
            "response": {
                "output": [
                    {"type": "custom_tool_call", "call_id": "call_1", "name": "tool", "input": {"query": "test"}},
                    {"type": "custom_tool_call_output", "call_id": "call_1", "output": {"result": "ok"}},
                ]
            }
        });
        let items = extract_replay_items(&completed).unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["type"], "custom_tool_call");
        assert_eq!(items[1]["type"], "custom_tool_call_output");
        assert_eq!(items[1]["call_id"], "call_1");
    }

    #[test]
    fn test_filter_preserves_output_with_call() {
        let body = json!({
            "input": [
                {"type": "function_call", "call_id": "call_1", "name": "tool_a", "arguments": "{}"},
                {"type": "function_call_output", "call_id": "call_1", "output": "ok"},
                {"type": "function_call_output", "call_id": "call_2", "output": "pending"}
            ]
        });
        let cached = vec![
            json!({"type": "function_call", "call_id": "call_2", "name": "tool_b", "arguments": "{}"}),
            json!({"type": "function_call_output", "call_id": "call_2", "output": "pending"}),
        ];
        let filtered = filter_replay_items(&body, cached);
        assert_eq!(filtered.len(), 2, "应保留 call + output");
        assert_eq!(filtered[0]["type"], "function_call");
        assert_eq!(filtered[0]["call_id"], "call_2");
        assert_eq!(filtered[1]["type"], "function_call_output");
        assert_eq!(filtered[1]["call_id"], "call_2");
    }
}
