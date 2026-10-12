//! Antigravity / Gemini thoughtSignature reasoning replay（对齐 CPA
//! `antigravity_reasoning_replay.go` 的签名回填 MVP 子集）。
//!
//! - 缓存条目形状：`thought_signature` / `function_call_part`
//! - 注入：在已有 `request.contents` part 上补 `thoughtSignature`，不插入缺失的 functionCall
//! - 累积：从响应 `candidates[0].content.parts` 收集可回放项；仅 terminal（有 finishReason）才可提交

use serde_json::{json, Value};
use sha2::{Digest, Sha256};

use super::signature::GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR;

/// 对齐 CPA `minAntigravityThoughtSignatureReplayLen`
pub const MIN_THOUGHT_SIGNATURE_LEN: usize = 16;

/// 单 entry 条目上限（对齐 CPA MaxItemsPerEntry）
pub const MAX_ITEMS_PER_ENTRY: usize = 4096;
/// 单 entry 字节上限（对齐 CPA MaxBytesPerEntry）
pub const MAX_BYTES_PER_ENTRY: usize = 16 << 20;

/// 是否为可入库的 native thoughtSignature（非空且非 bypass 哨兵）。
pub fn has_native_thought_signature(signature: &str) -> bool {
    let signature = signature.trim();
    !signature.is_empty() && signature != GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR
}

fn part_thought_signature(part: &Value) -> Option<&str> {
    part.get("thoughtSignature")
        .or_else(|| part.get("thought_signature"))
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
}

/// 规范化单条 replay item；非法返回 None。
pub fn normalize_ag_replay_item(item: &Value) -> Option<Value> {
    let ty = item
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    match ty {
        "thought_signature" => normalize_thought_signature_item(item),
        "function_call_part" => normalize_function_call_part_item(item),
        _ => None,
    }
}

/// 批量规范化；空或全非法返回 None。
pub fn normalize_ag_replay_items(items: &[Value]) -> Option<Vec<Value>> {
    if items.len() > MAX_ITEMS_PER_ENTRY {
        return None;
    }
    let mut out = Vec::with_capacity(items.len());
    let mut total = 0usize;
    for item in items {
        let Some(normalized) = normalize_ag_replay_item(item) else {
            continue;
        };
        total = total.saturating_add(normalized.to_string().len());
        if total > MAX_BYTES_PER_ENTRY {
            return None;
        }
        out.push(normalized);
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

fn normalize_thought_signature_item(item: &Value) -> Option<Value> {
    let mut sig = item
        .get("thoughtSignature")
        .or_else(|| item.get("thought_signature"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if sig.is_empty()
        || sig == GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR
        || sig.len() < MIN_THOUGHT_SIGNATURE_LEN
    {
        return None;
    }
    let mut out = json!({"type": "thought_signature", "thoughtSignature": sig});
    copy_optional_number(item, &mut out, "contentIndex");
    copy_optional_number(item, &mut out, "partIndex");
    if let Some(kind) = item.get("targetKind").and_then(|v| v.as_str()) {
        if kind == "text" || kind == "thought" {
            out["targetKind"] = json!(kind);
        }
    }
    if let Some(hash) = item
        .get("targetHash")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        out["targetHash"] = json!(hash);
    }
    copy_optional_number(item, &mut out, "targetOccurrence");
    if let Some(ch) = item
        .get("contextHash")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        out["contextHash"] = json!(ch);
    }
    let _ = &mut sig;
    Some(out)
}

fn normalize_function_call_part_item(item: &Value) -> Option<Value> {
    let mut call_id = item
        .get("call_id")
        .or_else(|| item.get("id"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let mut name = item
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let mut args = item.get("args").cloned();
    if name.is_empty() || args.is_none() {
        if let Some(fc) = item.get("functionCall") {
            if call_id.is_empty() {
                call_id = fc
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string();
            }
            if name.is_empty() {
                name = fc
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string();
            }
            if args.is_none() {
                args = fc.get("args").cloned();
            }
        }
    }
    let args = args?;
    if name.is_empty() {
        return None;
    }
    let mut out = json!({"type": "function_call_part", "name": name, "args": args});
    if !call_id.is_empty() {
        out["call_id"] = json!(call_id);
    }
    if let Some(sig) = item
        .get("thoughtSignature")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR)
    {
        out["thoughtSignature"] = json!(sig);
    }
    copy_optional_number(item, &mut out, "contentIndex");
    copy_optional_number(item, &mut out, "partIndex");
    copy_optional_number(item, &mut out, "targetOccurrence");
    if let Some(ch) = item
        .get("contextHash")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        out["contextHash"] = json!(ch);
    }
    Some(out)
}

fn copy_optional_number(src: &Value, dst: &mut Value, key: &str) {
    if let Some(n) = src.get(key).and_then(|v| v.as_i64()) {
        dst[key] = json!(n);
    }
}

fn part_fingerprint(part: &Value) -> Option<(String, String)> {
    if part.get("functionCall").is_some() || part.get("functionResponse").is_some() {
        return None;
    }
    let text = part.get("text")?.as_str()?;
    let kind = if part.get("thought").and_then(|v| v.as_bool()) == Some(true) {
        "thought"
    } else {
        "text"
    };
    let mut hasher = Sha256::new();
    hasher.update(kind.as_bytes());
    hasher.update([0u8]);
    hasher.update(text.as_bytes());
    Some((kind.to_string(), hex::encode(hasher.finalize())))
}

fn canonical_json(value: &Value) -> String {
    // 稳定比较：对象键排序后序列化
    fn sort_value(v: &Value) -> Value {
        match v {
            Value::Object(map) => {
                let mut keys: Vec<_> = map.keys().cloned().collect();
                keys.sort();
                let mut out = serde_json::Map::new();
                for k in keys {
                    if let Some(child) = map.get(&k) {
                        out.insert(k, sort_value(child));
                    }
                }
                Value::Object(out)
            }
            Value::Array(arr) => Value::Array(arr.iter().map(sort_value).collect()),
            other => other.clone(),
        }
    }
    serde_json::to_string(&sort_value(value)).unwrap_or_default()
}

fn args_equal(a: &Value, b: &Value) -> bool {
    canonical_json(a) == canonical_json(b)
}

fn contents_mut(request: &mut Value) -> Option<&mut Vec<Value>> {
    request.get_mut("contents").and_then(|v| v.as_array_mut())
}

fn contents_ref(request: &Value) -> Option<&Vec<Value>> {
    request.get("contents").and_then(|v| v.as_array())
}

fn is_model_role(content: &Value) -> bool {
    content
        .get("role")
        .and_then(|v| v.as_str())
        .map(|r| r.eq_ignore_ascii_case("model"))
        .unwrap_or(false)
}

fn locate_thought_signature_part(contents: &[Value], item: &Value) -> Option<(usize, usize)> {
    let content_index = item.get("contentIndex").and_then(|v| v.as_u64())? as usize;
    if content_index >= contents.len() {
        return None;
    }
    let content = &contents[content_index];
    if !is_model_role(content) {
        return None;
    }
    let parts = content.get("parts").and_then(|v| v.as_array())?;
    let target_kind = item
        .get("targetKind")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    let target_hash = item
        .get("targetHash")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();

    let mut part_index: Option<usize> = None;
    if !target_hash.is_empty() {
        if let Some(wanted) = item.get("targetOccurrence").and_then(|v| v.as_i64()) {
            let mut occurrence = 0i64;
            for (idx, part) in parts.iter().enumerate() {
                let Some((kind, fingerprint)) = part_fingerprint(part) else {
                    continue;
                };
                if fingerprint != target_hash {
                    continue;
                }
                if !target_kind.is_empty() && kind != target_kind {
                    continue;
                }
                if occurrence == wanted {
                    part_index = Some(idx);
                    break;
                }
                occurrence += 1;
            }
        } else {
            if let Some(candidate) = item.get("partIndex").and_then(|v| v.as_u64()) {
                let candidate = candidate as usize;
                if candidate < parts.len() {
                    if let Some((kind, fingerprint)) = part_fingerprint(&parts[candidate]) {
                        if fingerprint == target_hash
                            && (target_kind.is_empty() || kind == target_kind)
                        {
                            part_index = Some(candidate);
                        }
                    }
                }
            }
            if part_index.is_none() {
                for (idx, part) in parts.iter().enumerate() {
                    let Some((kind, fingerprint)) = part_fingerprint(part) else {
                        continue;
                    };
                    if fingerprint == target_hash && (target_kind.is_empty() || kind == target_kind)
                    {
                        part_index = Some(idx);
                        break;
                    }
                }
            }
        }
    } else {
        // MVP：无 targetHash 时仅按 partIndex 猜测（CPA 还要求 contextHash；首期放宽）
        if let Some(candidate) = item.get("partIndex").and_then(|v| v.as_u64()) {
            let candidate = candidate as usize;
            if candidate < parts.len() && part_fingerprint(&parts[candidate]).is_some() {
                part_index = Some(candidate);
            }
        }
        if part_index.is_none() {
            for idx in (0..parts.len()).rev() {
                if part_fingerprint(&parts[idx]).is_some() {
                    part_index = Some(idx);
                    break;
                }
            }
        }
    }
    part_index.map(|pi| (content_index, pi))
}

fn locate_function_call_part(contents: &[Value], item: &Value) -> Option<(usize, usize)> {
    let name = item.get("name").and_then(|v| v.as_str())?.trim();
    if name.is_empty() {
        return None;
    }
    let args = item.get("args")?;
    let call_id = item
        .get("call_id")
        .or_else(|| item.get("id"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();

    // 优先 contentIndex/partIndex 命中
    if let (Some(ci), Some(pi)) = (
        item.get("contentIndex").and_then(|v| v.as_u64()),
        item.get("partIndex").and_then(|v| v.as_u64()),
    ) {
        let ci = ci as usize;
        let pi = pi as usize;
        if ci < contents.len() {
            if let Some(parts) = contents[ci].get("parts").and_then(|v| v.as_array()) {
                if pi < parts.len() {
                    if let Some(fc) = parts[pi].get("functionCall") {
                        if fc.get("name").and_then(|v| v.as_str()) == Some(name) {
                            if let Some(current_args) = fc.get("args") {
                                if args_equal(current_args, args) {
                                    let id_ok = call_id.is_empty()
                                        || fc.get("id").and_then(|v| v.as_str()) == Some(call_id);
                                    if id_ok {
                                        return Some((ci, pi));
                                    }
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    for (ci, content) in contents.iter().enumerate() {
        let Some(parts) = content.get("parts").and_then(|v| v.as_array()) else {
            continue;
        };
        for (pi, part) in parts.iter().enumerate() {
            let Some(fc) = part.get("functionCall") else {
                continue;
            };
            if fc.get("name").and_then(|v| v.as_str()) != Some(name) {
                continue;
            }
            let Some(current_args) = fc.get("args") else {
                continue;
            };
            if !args_equal(current_args, args) {
                continue;
            }
            if !call_id.is_empty() && fc.get("id").and_then(|v| v.as_str()) != Some(call_id) {
                continue;
            }
            return Some((ci, pi));
        }
    }
    None
}

fn remove_thought_signature_from_other_parts(
    contents: &mut [Value],
    content_index: usize,
    signature: &str,
    keep_part: usize,
) {
    let Some(parts) = contents
        .get_mut(content_index)
        .and_then(|c| c.get_mut("parts"))
        .and_then(|v| v.as_array_mut())
    else {
        return;
    };
    for (idx, part) in parts.iter_mut().enumerate() {
        if idx == keep_part {
            continue;
        }
        if part_thought_signature(part) == Some(signature) {
            if let Some(obj) = part.as_object_mut() {
                obj.remove("thoughtSignature");
                obj.remove("thought_signature");
            }
        }
    }
}

fn set_part_thought_signature(part: &mut Value, signature: &str) -> bool {
    if let Some(existing) = part_thought_signature(part) {
        if has_native_thought_signature(existing) {
            return false;
        }
    }
    if let Some(obj) = part.as_object_mut() {
        obj.insert("thoughtSignature".into(), json!(signature));
        obj.remove("thought_signature");
        true
    } else {
        false
    }
}

/// 将缓存 items 注入到 Antigravity `request` 对象（含 `contents`）。
/// MVP：只补已有 part 的 thoughtSignature，不插入缺失 functionCall。
/// 返回是否发生变更。
pub fn apply_ag_replay_items(request: &mut Value, items: &[Value]) -> bool {
    let Some(normalized) = normalize_ag_replay_items(items) else {
        return false;
    };
    let Some(contents) = contents_mut(request) else {
        return false;
    };
    let mut changed = false;
    // 先 clone 一份只读快照用于定位，避免边改边找
    let snapshot = contents.clone();
    for item in &normalized {
        let ty = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match ty {
            "thought_signature" => {
                let Some(sig) = item
                    .get("thoughtSignature")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                else {
                    continue;
                };
                let Some((ci, pi)) = locate_thought_signature_part(&snapshot, item) else {
                    continue;
                };
                // 已有 native 签名则跳过
                if let Some(part) = snapshot
                    .get(ci)
                    .and_then(|c| c.get("parts"))
                    .and_then(|p| p.as_array())
                    .and_then(|arr| arr.get(pi))
                {
                    if part_thought_signature(part)
                        .map(has_native_thought_signature)
                        .unwrap_or(false)
                    {
                        continue;
                    }
                }
                remove_thought_signature_from_other_parts(contents, ci, sig, pi);
                if let Some(part) = contents
                    .get_mut(ci)
                    .and_then(|c| c.get_mut("parts"))
                    .and_then(|p| p.as_array_mut())
                    .and_then(|arr| arr.get_mut(pi))
                {
                    if set_part_thought_signature(part, sig) {
                        changed = true;
                    }
                }
            }
            "function_call_part" => {
                let Some(sig) = item
                    .get("thoughtSignature")
                    .and_then(|v| v.as_str())
                    .map(str::trim)
                    .filter(|s| !s.is_empty())
                else {
                    continue; // MVP：无签名的 function_call_part 不插入、不改身份
                };
                let Some((ci, pi)) = locate_function_call_part(&snapshot, item) else {
                    continue;
                };
                if let Some(part) = snapshot
                    .get(ci)
                    .and_then(|c| c.get("parts"))
                    .and_then(|p| p.as_array())
                    .and_then(|arr| arr.get(pi))
                {
                    if part_thought_signature(part)
                        .map(has_native_thought_signature)
                        .unwrap_or(false)
                    {
                        continue;
                    }
                }
                remove_thought_signature_from_other_parts(contents, ci, sig, pi);
                if let Some(part) = contents
                    .get_mut(ci)
                    .and_then(|c| c.get_mut("parts"))
                    .and_then(|p| p.as_array_mut())
                    .and_then(|arr| arr.get_mut(pi))
                {
                    if set_part_thought_signature(part, sig) {
                        changed = true;
                    }
                }
            }
            _ => {}
        }
    }
    changed
}

/// 对流式/非流式响应的可回放项累积器（纯逻辑，无 IO）。
#[derive(Debug, Default)]
pub struct AgReplayAccumulator {
    items: Vec<Value>,
    seen_signatures: std::collections::HashSet<String>,
    seen_fc: std::collections::HashSet<String>,
    content_index: i64,
    next_part_index: i64,
    pending_signatures: Vec<(String, String)>, // (sig, target_kind)
    visible_text: String,
    thought_text: String,
    visible_part_index: i64,
    thought_part_index: i64,
    last_response_kind: String,
    segment_occurrences: std::collections::HashMap<String, i64>,
    pub terminal: bool,
    pub overflow: bool,
    item_bytes: usize,
}

impl AgReplayAccumulator {
    /// 从请求 contents 种子已有签名，并确定下一 model content 槽位。
    pub fn new_from_request(request: &Value) -> Self {
        let mut acc = Self {
            content_index: -1,
            next_part_index: 0,
            visible_part_index: -1,
            thought_part_index: -1,
            ..Self::default()
        };
        let Some(contents) = contents_ref(request) else {
            return acc;
        };
        // pending model content = 末尾之后
        acc.content_index = contents.len() as i64;
        for (ci, content) in contents.iter().enumerate() {
            if !is_model_role(content) {
                continue;
            }
            let Some(parts) = content.get("parts").and_then(|v| v.as_array()) else {
                continue;
            };
            for (pi, part) in parts.iter().enumerate() {
                if let Some(sig) = part_thought_signature(part) {
                    if has_native_thought_signature(sig) {
                        acc.seen_signatures.insert(sig.to_string());
                        if let Some(fc) = part.get("functionCall") {
                            if let Some(item) =
                                build_function_call_part_item(ci as i64, pi as i64, 0, fc, sig)
                            {
                                acc.append_item(item);
                            }
                        } else if let Some((kind, hash)) = part_fingerprint(part) {
                            let mut item = json!({
                                "type": "thought_signature",
                                "thoughtSignature": sig,
                                "contentIndex": ci,
                                "partIndex": pi,
                                "targetKind": kind,
                                "targetHash": hash,
                            });
                            let occ_key = format!("{kind}\0{hash}");
                            let occ = *acc.segment_occurrences.get(&occ_key).unwrap_or(&0);
                            item["targetOccurrence"] = json!(occ);
                            acc.segment_occurrences.insert(occ_key, occ + 1);
                            acc.append_item(item);
                        }
                    }
                }
            }
        }
        acc
    }

    fn append_item(&mut self, item: Value) {
        let bytes = item.to_string().len();
        if self.items.len() + 1 > MAX_ITEMS_PER_ENTRY
            || self.item_bytes.saturating_add(bytes) > MAX_BYTES_PER_ENTRY
        {
            self.overflow = true;
            return;
        }
        self.item_bytes += bytes;
        self.items.push(item);
    }

    /// 观察一帧上游响应 JSON（流式 chunk 或非流完整 body）。
    /// 支持 `response.candidates` 与顶层 `candidates`。
    pub fn observe_response(&mut self, payload: &Value) {
        let response = payload.get("response").unwrap_or(payload);
        let candidates = response.get("candidates").and_then(|v| v.as_array());
        let Some(candidates) = candidates else {
            return;
        };
        let Some(first) = candidates.first() else {
            return;
        };
        if first
            .get("finishReason")
            .and_then(|v| v.as_str())
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false)
        {
            self.terminal = true;
        }
        let Some(parts) = first.pointer("/content/parts").and_then(|v| v.as_array()) else {
            return;
        };
        for part in parts {
            self.observe_part(part);
        }
    }

    fn observe_part(&mut self, part: &Value) {
        let pi = self.next_part_index;
        self.next_part_index += 1;
        let mut signature = part_thought_signature(part)
            .filter(|s| has_native_thought_signature(s))
            .unwrap_or("")
            .to_string();

        if let Some(fc) = part.get("functionCall") {
            if self.last_response_kind == "text" || self.last_response_kind == "thought" {
                self.flush_pending_for_kind(&self.last_response_kind.clone());
            }
            if signature.is_empty() {
                // 取最后一个 unbound pending
                if let Some(idx) = self
                    .pending_signatures
                    .iter()
                    .rposition(|(_, kind)| kind.is_empty())
                {
                    signature = self.pending_signatures.remove(idx).0;
                }
            } else {
                self.pending_signatures.retain(|(_, kind)| !kind.is_empty());
            }
            let name = fc.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let args_raw = fc.get("args").map(canonical_json).unwrap_or_default();
            let call_id = fc.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let key = format!("{name}\0{args_raw}\0{call_id}");
            let dedupe = if signature.is_empty() {
                format!("{key}\0part:{pi}")
            } else {
                format!("{key}\0{signature}")
            };
            if self.seen_fc.contains(&dedupe) {
                return;
            }
            self.seen_fc.insert(dedupe);
            if let Some(item) =
                build_function_call_part_item(self.content_index, pi, 0, fc, &signature)
            {
                if !signature.is_empty() {
                    self.seen_signatures.insert(signature.clone());
                }
                self.append_item(item);
            }
            self.last_response_kind = "function_call".into();
            return;
        }

        let thought = part.get("thought").and_then(|v| v.as_bool()) == Some(true);
        let text = part.get("text").and_then(|v| v.as_str()).unwrap_or("");
        let has_text = !text.is_empty();
        let signature_only = !signature.is_empty() && !has_text;

        if signature_only && self.last_response_kind == "function_call" {
            if !self.seen_signatures.contains(&signature) {
                self.attach_detached_signature_to_last_function_call(&signature);
                self.seen_signatures.insert(signature);
            }
            return;
        }

        let mut target_kind = String::new();
        if has_text {
            target_kind = if thought {
                "thought".into()
            } else {
                "text".into()
            };
            if self.last_response_kind != target_kind
                && (self.last_response_kind == "text" || self.last_response_kind == "thought")
            {
                self.flush_pending_for_kind(&self.last_response_kind.clone());
            }
            if target_kind == "thought" {
                if self.thought_text.is_empty() {
                    self.thought_part_index = pi;
                }
                self.thought_text.push_str(text);
            } else {
                if self.visible_text.is_empty() {
                    self.visible_part_index = pi;
                }
                self.visible_text.push_str(text);
            }
            self.last_response_kind = target_kind.clone();
        }

        if !signature.is_empty() && !self.seen_signatures.contains(&signature) {
            if target_kind.is_empty() {
                target_kind = self.last_response_kind.clone();
            }
            self.pending_signatures
                .push((signature.clone(), target_kind.clone()));
            self.seen_signatures.insert(signature);
            if signature_only || has_text {
                if target_kind == "text" && !self.visible_text.is_empty() {
                    self.flush_pending_for_kind("text");
                } else if target_kind == "thought" && !self.thought_text.is_empty() {
                    self.flush_pending_for_kind("thought");
                }
            }
        }
    }

    fn attach_detached_signature_to_last_function_call(&mut self, signature: &str) {
        for item in self.items.iter_mut().rev() {
            if item.get("type").and_then(|v| v.as_str()) != Some("function_call_part") {
                continue;
            }
            if item
                .get("thoughtSignature")
                .and_then(|v| v.as_str())
                .map(has_native_thought_signature)
                .unwrap_or(false)
            {
                continue;
            }
            item["thoughtSignature"] = json!(signature);
            return;
        }
    }

    fn flush_pending_for_kind(&mut self, target_kind: &str) {
        if target_kind != "text" && target_kind != "thought" {
            return;
        }
        let (text, part_index) = if target_kind == "thought" {
            (self.thought_text.clone(), self.thought_part_index)
        } else {
            (self.visible_text.clone(), self.visible_part_index)
        };
        if text.is_empty() {
            return;
        }
        let mut hasher = Sha256::new();
        hasher.update(target_kind.as_bytes());
        hasher.update([0u8]);
        hasher.update(text.as_bytes());
        let target_hash = hex::encode(hasher.finalize());
        let occ_key = format!("{target_kind}\0{target_hash}");
        let occurrence = *self.segment_occurrences.get(&occ_key).unwrap_or(&0);
        self.segment_occurrences.insert(occ_key, occurrence + 1);

        let mut remaining = Vec::new();
        for (sig, kind) in std::mem::take(&mut self.pending_signatures) {
            if kind != target_kind {
                remaining.push((sig, kind));
                continue;
            }
            let item = json!({
                "type": "thought_signature",
                "thoughtSignature": sig,
                "contentIndex": self.content_index,
                "partIndex": part_index,
                "targetKind": target_kind,
                "targetHash": target_hash,
                "targetOccurrence": occurrence,
            });
            self.append_item(item);
        }
        self.pending_signatures = remaining;
        if target_kind == "thought" {
            self.thought_text.clear();
            self.thought_part_index = -1;
        } else {
            self.visible_text.clear();
            self.visible_part_index = -1;
        }
    }

    fn flush_all_pending(&mut self) {
        for (_sig, kind) in self.pending_signatures.iter_mut() {
            if !kind.is_empty() {
                continue;
            }
            if self.last_response_kind == "text" && !self.visible_text.is_empty() {
                *kind = "text".into();
            } else if self.last_response_kind == "thought" && !self.thought_text.is_empty() {
                *kind = "thought".into();
            } else if !self.visible_text.is_empty() {
                *kind = "text".into();
            } else if !self.thought_text.is_empty() {
                *kind = "thought".into();
            }
        }
        self.flush_pending_for_kind("thought");
        self.flush_pending_for_kind("text");
        self.pending_signatures.clear();
    }

    /// 若 terminal 且未 overflow，返回可提交的规范化 items；
    /// terminal 但空/overflow → Some(empty) 表示应清空缓存；
    /// 非 terminal → None（不提交）。
    pub fn take_commit_items(&mut self) -> Option<Vec<Value>> {
        if !self.terminal {
            return None;
        }
        if self.overflow {
            return Some(Vec::new());
        }
        self.flush_all_pending();
        if self.overflow {
            return Some(Vec::new());
        }
        if self.items.is_empty() {
            return Some(Vec::new());
        }
        Some(normalize_ag_replay_items(&self.items).unwrap_or_default())
    }

    pub fn items(&self) -> &[Value] {
        &self.items
    }
}

fn build_function_call_part_item(
    content_index: i64,
    part_index: i64,
    target_occurrence: i64,
    fc: &Value,
    signature: &str,
) -> Option<Value> {
    let name = fc.get("name").and_then(|v| v.as_str())?.trim();
    if name.is_empty() {
        return None;
    }
    let args = fc.get("args")?.clone();
    let mut item = json!({
        "type": "function_call_part",
        "contentIndex": content_index,
        "partIndex": part_index,
        "targetOccurrence": target_occurrence,
        "name": name,
        "args": args,
    });
    if let Some(id) = fc
        .get("id")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        item["call_id"] = json!(id);
    }
    if !signature.is_empty() {
        item["thoughtSignature"] = json!(signature);
    }
    Some(item)
}

/// 从完整非流响应构造可提交 items（内部用 accumulator）。
pub fn collect_ag_replay_from_response(
    request: &Value,
    response_body: &Value,
) -> Option<Vec<Value>> {
    let mut acc = AgReplayAccumulator::new_from_request(request);
    acc.observe_response(response_body);
    acc.take_commit_items()
}

/// 上游 400 是否像 thoughtSignature 校验失败（对齐 CPA
/// clearAntigravityReasoningReplayOnInvalidSignature 的文案探测）。
pub fn is_antigravity_thought_signature_invalid(body: &[u8]) -> bool {
    let lower = String::from_utf8_lossy(body).to_ascii_lowercase();
    lower.contains("thoughtsignature")
        || lower.contains("thought_signature")
        || (lower.contains("signature")
            && (lower.contains("invalid") || lower.contains("failed") || lower.contains("error")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn normalize_rejects_short_and_bypass_signature() {
        assert!(normalize_ag_replay_item(&json!({
            "type": "thought_signature",
            "thoughtSignature": "short"
        }))
        .is_none());
        assert!(normalize_ag_replay_item(&json!({
            "type": "thought_signature",
            "thoughtSignature": GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR
        }))
        .is_none());
        let ok = normalize_ag_replay_item(&json!({
            "type": "thought_signature",
            "thoughtSignature": "native-signature-ok-16",
            "contentIndex": 1,
            "partIndex": 0,
            "targetKind": "text",
        }))
        .unwrap();
        assert_eq!(ok["thoughtSignature"], "native-signature-ok-16");
        assert_eq!(ok["contentIndex"], 1);
    }

    #[test]
    fn normalize_function_call_part() {
        let ok = normalize_ag_replay_item(&json!({
            "type": "function_call_part",
            "name": "Read",
            "call_id": "id1",
            "args": {"file_path": "/a"},
            "thoughtSignature": "sig-first-long-enough"
        }))
        .unwrap();
        assert_eq!(ok["name"], "Read");
        assert_eq!(ok["call_id"], "id1");
        assert_eq!(ok["thoughtSignature"], "sig-first-long-enough");
    }

    #[test]
    fn apply_merges_signature_into_existing_function_call() {
        let items = vec![json!({
            "type": "function_call_part",
            "contentIndex": 1,
            "partIndex": 0,
            "name": "Read",
            "call_id": "id1",
            "args": {"file_path": "/a"},
            "thoughtSignature": "sig-first-long-enough"
        })];
        let mut request = json!({
            "contents": [
                {"role": "user", "parts": [{"text": "hi"}]},
                {"role": "model", "parts": [{"functionCall": {"id": "id1", "name": "Read", "args": {"file_path": "/a"}}}]},
                {"role": "user", "parts": [{"functionResponse": {"id": "id1", "name": "Read", "response": {"result": "ok"}}}]}
            ]
        });
        assert!(apply_ag_replay_items(&mut request, &items));
        assert_eq!(
            request["contents"][1]["parts"][0]["thoughtSignature"],
            "sig-first-long-enough"
        );
    }

    #[test]
    fn apply_attaches_text_signature_by_target_hash() {
        let text = "answer-one";
        let mut hasher = Sha256::new();
        hasher.update(b"text");
        hasher.update([0u8]);
        hasher.update(text.as_bytes());
        let hash = hex::encode(hasher.finalize());
        let items = vec![json!({
            "type": "thought_signature",
            "thoughtSignature": "native-signature-turn-one-123456",
            "contentIndex": 1,
            "partIndex": 0,
            "targetKind": "text",
            "targetHash": hash,
            "targetOccurrence": 0,
        })];
        let mut request = json!({
            "contents": [
                {"role": "user", "parts": [{"text": "turn one"}]},
                {"role": "model", "parts": [{"text": "answer-one"}]},
                {"role": "user", "parts": [{"text": "turn two"}]}
            ]
        });
        assert!(apply_ag_replay_items(&mut request, &items));
        assert_eq!(
            request["contents"][1]["parts"][0]["thoughtSignature"],
            "native-signature-turn-one-123456"
        );
    }

    #[test]
    fn apply_skips_stale_positional_signature_without_matching_part() {
        // contentIndex 越界 → 不改
        let items = vec![json!({
            "type": "thought_signature",
            "thoughtSignature": "stale-thought-sig-ok12",
            "contentIndex": 8,
            "partIndex": 3,
        })];
        let mut request = json!({
            "contents": [
                {"role": "user", "parts": [{"text": "hi"}]},
                {"role": "model", "parts": [{"text": "visible answer"}]},
                {"role": "user", "parts": [{"text": "next"}]}
            ]
        });
        assert!(!apply_ag_replay_items(&mut request, &items));
        assert!(request["contents"][1]["parts"][0]
            .get("thoughtSignature")
            .is_none());
    }

    #[test]
    fn apply_does_not_insert_missing_function_call() {
        // MVP：只回填签名，不插入缺失 call
        let items = vec![json!({
            "type": "function_call_part",
            "contentIndex": 1,
            "partIndex": 0,
            "name": "Read",
            "call_id": "id1",
            "args": {"file_path": "/a"},
            "thoughtSignature": "sig-first-long-enough"
        })];
        let mut request = json!({
            "contents": [
                {"role": "user", "parts": [{"text": "hi"}]},
                {"role": "user", "parts": [{"functionResponse": {"id": "id1", "name": "Read", "response": {"result": "ok"}}}]}
            ]
        });
        assert!(!apply_ag_replay_items(&mut request, &items));
        assert_eq!(request["contents"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn accumulator_collects_function_calls_with_signature() {
        let request = json!({"contents": [{"role": "user", "parts": [{"text": "hi"}]}]});
        let mut acc = AgReplayAccumulator::new_from_request(&request);
        assert_eq!(acc.content_index, 1);
        acc.observe_response(&json!({
            "response": {"candidates": [{"content": {"parts": [
                {"thoughtSignature": "sig-first-long-enough", "functionCall": {"name": "Read", "args": {"file_path": "/a"}, "id": "id1"}}
            ]}}]}
        }));
        acc.observe_response(&json!({
            "response": {"candidates": [{"content": {"parts": [
                {"functionCall": {"name": "Read", "args": {"file_path": "/b"}, "id": "id2"}}
            ]}, "finishReason": "STOP"}]}
        }));
        let items = acc.take_commit_items().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["partIndex"], 0);
        assert_eq!(items[0]["thoughtSignature"], "sig-first-long-enough");
        assert_eq!(items[1]["partIndex"], 1);
    }

    #[test]
    fn accumulator_text_signature_chain_roundtrip() {
        let sig1 = "native-signature-turn-one-123456";
        let request1 = json!({"contents": [{"role": "user", "parts": [{"text": "turn one"}]}]});
        let mut acc1 = AgReplayAccumulator::new_from_request(&request1);
        acc1.observe_response(&json!({
            "response": {"candidates": [{"content": {"parts": [{"text": "answer-"}]}}]}
        }));
        acc1.observe_response(&json!({
            "response": {"candidates": [{"content": {"parts": [{"text": "one"}]}}]}
        }));
        acc1.observe_response(&json!({
            "response": {"candidates": [{"content": {"parts": [
                {"text": "", "thoughtSignature": sig1}
            ]}, "finishReason": "STOP"}]}
        }));
        let items = acc1.take_commit_items().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["type"], "thought_signature");

        let mut request2 = json!({
            "contents": [
                {"role": "user", "parts": [{"text": "turn one"}]},
                {"role": "model", "parts": [{"text": "answer-one"}]},
                {"role": "user", "parts": [{"text": "turn two"}]}
            ]
        });
        assert!(apply_ag_replay_items(&mut request2, &items));
        assert_eq!(
            request2["contents"][1]["parts"][0]["thoughtSignature"],
            sig1
        );
    }

    #[test]
    fn accumulator_does_not_commit_partial() {
        let request = json!({"contents": [{"role": "user", "parts": [{"text": "turn"}]}]});
        let mut acc = AgReplayAccumulator::new_from_request(&request);
        acc.observe_response(&json!({
            "response": {"candidates": [{"content": {"parts": [
                {"text": "partial"},
                {"text": "", "thoughtSignature": "partial-signature-123456789"}
            ]}}]}
        }));
        assert!(acc.take_commit_items().is_none());
    }

    #[test]
    fn accumulator_terminal_empty_yields_clear() {
        let request = json!({"contents": [{"role": "user", "parts": [{"text": "new"}]}]});
        let mut acc = AgReplayAccumulator::new_from_request(&request);
        acc.observe_response(&json!({
            "response": {"candidates": [{"content": {"parts": [
                {"text": "answer without signature"}
            ]}, "finishReason": "STOP"}]}
        }));
        let items = acc.take_commit_items().unwrap();
        assert!(items.is_empty());
    }
    #[test]
    fn detects_invalid_thought_signature_errors() {
        assert!(is_antigravity_thought_signature_invalid(
            br#"{"error":{"message":"Invalid thoughtSignature"}}"#
        ));
        assert!(is_antigravity_thought_signature_invalid(
            br#"{"error":{"message":"thought_signature validation failed"}}"#
        ));
        assert!(!is_antigravity_thought_signature_invalid(
            br#"{"error":{"message":"quota exceeded"}}"#
        ));
    }

    #[test]
    fn normalize_rejects_when_over_max_bytes() {
        // 单条序列化后超过 MAX_BYTES_PER_ENTRY → 整批拒绝
        let sig = "b".repeat(MAX_BYTES_PER_ENTRY);
        let items = vec![json!({
            "type": "thought_signature",
            "thoughtSignature": sig,
            "contentIndex": 1,
            "partIndex": 0,
        })];
        assert!(normalize_ag_replay_items(&items).is_none());
    }

    #[test]
    fn normalize_accepts_under_max_bytes() {
        let sig = "native-signature-ok-16chars";
        let items = vec![json!({
            "type": "thought_signature",
            "thoughtSignature": sig,
            "contentIndex": 1,
            "partIndex": 0,
            "targetKind": "text",
        })];
        let out = normalize_ag_replay_items(&items).expect("under limit");
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn accumulator_byte_overflow_yields_clear_on_terminal() {
        let request = json!({"contents": [{"role": "user", "parts": [{"text": "hi"}]}]});
        let mut acc = AgReplayAccumulator::new_from_request(&request);
        // 超大签名使 append_item 触发 overflow；terminal 后应清空（Some(empty)）
        let huge = "s".repeat(MAX_BYTES_PER_ENTRY);
        acc.observe_response(&json!({
            "response": {"candidates": [{"content": {"parts": [
                {"thoughtSignature": huge, "functionCall": {"name": "Read", "args": {"p": "/a"}, "id": "id1"}}
            ]}, "finishReason": "STOP"}]}
        }));
        assert!(acc.overflow);
        let items = acc.take_commit_items().expect("terminal");
        assert!(
            items.is_empty(),
            "overflow must clear ledger, not store partial"
        );
    }
}
