use serde_json::{json, Value};

use crate::convert::signature::{
    compatible_signature_for_provider_block, is_valid_gpt_reasoning_signature,
    is_valid_grok_encrypted_content, SignatureBlockKind, SignatureProvider,
};
use super::instructions::is_grok_upstream;

/// thinking signature → 可回放给目标上游的 reasoning.encrypted_content
/// (对齐 CPA codex_claude_request.appendReasoningContent):
/// GPT 目标走兼容性解析(剥 provider 前缀 + Fernet 形状校验);
/// grok 目标无信封,按来源确认后做形状校验;其余一律丢弃。
pub(crate) fn gpt_compatible_signature(signature: Option<&str>, upstream_model: &str) -> Option<String> {
    let raw = signature.unwrap_or("").trim();
    if let Some(normalized) = compatible_signature_for_provider_block(
        SignatureProvider::Gpt,
        raw,
        SignatureBlockKind::Unknown,
    ) {
        return Some(normalized);
    }
    // 空签名:CPA 仅在 preserveEmptyThinkingBlocks 时保留空 encrypted_content,
    // 该开关未移植,ccextra 一律丢弃无签名 thinking
    if raw.is_empty() {
        return None;
    }
    if is_grok_upstream(upstream_model) && is_valid_grok_encrypted_content(raw) {
        return Some(raw.to_string());
    }
    None
}

/// 移除 reasoning 项的孤儿 id(encrypted_content 已无且 store 非 true 时)
pub(crate) fn remove_reasoning_id_if_orphan(item: &mut Value, store_true: bool) -> bool {
    if !store_true && item.get("encrypted_content").is_none() && item.get("id").is_some() {
        if let Some(obj) = item.as_object_mut() {
            obj.remove("id");
            return true;
        }
    }
    false
}

/// reasoning.summary 是否为空(缺失/null/空数组;字符串形式视为非空)
/// 对齐 CPA openaiResponsesReasoningSummaryIsEmpty
pub(crate) fn reasoning_summary_is_empty(summary: Option<&Value>) -> bool {
    match summary {
        None | Some(Value::Null) => true,
        Some(Value::Array(a)) => a.is_empty(),
        Some(_) => false,
    }
}

/// GPT/Codex 请求发送前仅剥离格式无效的 encrypted_content。
/// `store` 非 true 时顺带丢掉无法回放的 reasoning id；空 reasoning 项整项丢弃。
pub fn sanitize_gpt_reasoning_items(body: &mut Value) -> bool {
    let store_true = body.get("store").and_then(|v| v.as_bool()) == Some(true);
    let Some(input) = body.get_mut("input").and_then(|v| v.as_array_mut()) else {
        return false;
    };
    let mut changed = false;
    let mut kept = Vec::with_capacity(input.len());
    for mut item in input.drain(..) {
        if item.get("type").and_then(|v| v.as_str()) != Some("reasoning") {
            kept.push(item);
            continue;
        }
        let invalid = match item.get("encrypted_content") {
            Some(Value::String(content)) => !is_valid_gpt_reasoning_signature(content),
            Some(_) => true,
            None => false,
        };
        if invalid {
            if let Some(obj) = item.as_object_mut() {
                obj.remove("encrypted_content");
            }
            changed = true;
        }
        // 对齐 CPA sanitizeOpenAIResponsesReasoningEncryptedContent:官方 Codex
        // schema 对 reasoning.content 设 maxItems:0,第三方通道回放的明文 thinking
        // 数组会被 400。summary 为空时先把 reasoning_text 提升为 summary_text,
        // 然后一律强制 content: []。
        let had_content_parts =
            matches!(item.get("content"), Some(Value::Array(parts)) if !parts.is_empty());
        if had_content_parts {
            let promoted: Vec<Value> = if reasoning_summary_is_empty(item.get("summary")) {
                item.get("content")
                    .and_then(|v| v.as_array())
                    .map(|parts| {
                        parts
                            .iter()
                            .filter(|p| {
                                p.get("type").and_then(|v| v.as_str()) == Some("reasoning_text")
                            })
                            .filter_map(|p| p.get("text").and_then(|v| v.as_str()))
                            .filter(|t| !t.is_empty())
                            .map(|t| json!({"type": "summary_text", "text": t}))
                            .collect()
                    })
                    .unwrap_or_default()
            } else {
                Vec::new()
            };
            if let Some(obj) = item.as_object_mut() {
                if !promoted.is_empty() {
                    obj.insert("summary".into(), Value::Array(promoted));
                }
                obj.insert("content".into(), json!([]));
            }
            changed = true;
        }
        if remove_reasoning_id_if_orphan(&mut item, store_true) {
            changed = true;
        }
        // 清空前的 content 数组形式不算空项(对齐 CPA changed 分支 keep)
        if reasoning_item_empty(&item) && !had_content_parts {
            changed = true;
            continue;
        }
        kept.push(item);
    }
    *input = kept;
    changed
}

/// 上游 400 `invalid_encrypted_content` / thinking signature invalid 时,
/// 剥离 Responses `input[]` reasoning 项的 `encrypted_content` 再重试。
/// 对齐 CPA sanitizeOpenAIResponsesReasoningEncryptedContent 的剥离动作
/// (重试路径全剥,不做形状校验)。剥后无 content/summary 的空项整项丢掉。
/// `store` 非 true 时顺带丢掉孤儿 `id`(防 store=false 的 lookup 400)。
pub fn trim_encrypted_reasoning_items(body: &mut Value) -> bool {
    let store_true = body.get("store").and_then(|v| v.as_bool()) == Some(true);
    let Some(input) = body.get_mut("input").and_then(|v| v.as_array_mut()) else {
        return false;
    };
    let mut changed = false;
    let mut kept: Vec<Value> = Vec::with_capacity(input.len());
    for mut item in input.drain(..) {
        if item.get("type").and_then(|v| v.as_str()) != Some("reasoning") {
            kept.push(item);
            continue;
        }
        if item.get("encrypted_content").is_some() {
            if let Some(obj) = item.as_object_mut() {
                obj.remove("encrypted_content");
            }
            changed = true;
        }
        if remove_reasoning_id_if_orphan(&mut item, store_true) {
            changed = true;
        }
        if reasoning_item_empty(&item) {
            changed = true;
            continue;
        }
        kept.push(item);
    }
    *input = kept;
    changed
}

/// 上游错误 body 是否 thinking 签名无效。
/// Codex: `invalid_encrypted_content` / `invalid signature in thinking block`
/// (CPA thinking_signature_invalid)。xAI/grok: `Could not decrypt`。
pub fn is_thinking_signature_invalid(body: &[u8]) -> bool {
    let lower = String::from_utf8_lossy(body).to_ascii_lowercase();
    lower.contains("invalid_encrypted_content")
        || lower.contains("invalid signature in thinking block")
        || lower.contains("could not decrypt")
}

pub(crate) fn reasoning_item_empty(item: &Value) -> bool {
    let content_empty = match item.get("content") {
        None | Some(Value::Null) => true,
        Some(Value::String(s)) => s.is_empty(),
        Some(Value::Array(a)) => a.is_empty(),
        Some(_) => false,
    };
    let summary_empty = match item.get("summary") {
        None | Some(Value::Null) => true,
        Some(Value::String(s)) => s.is_empty(),
        Some(Value::Array(a)) => a.is_empty(),
        Some(_) => false,
    };
    content_empty && summary_empty && item.get("encrypted_content").is_none()
}

