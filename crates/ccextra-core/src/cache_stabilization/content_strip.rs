//! PR-E10:剥离 CC 每轮记账 reminder 以及 continue 尾缀。
//!
//! Claude Code 会把每轮的状态文本(token 用量、USD 预算、剩余轮次等)
//! 作为独立的 `<system-reminder>` 文本块注入到用户消息中,外加一个
//! "Continue from where you left off." 尾缀。它们的内容每轮都会变化,
//! 在流式过程中破坏 cache prefix。这些都是仅展示给 *当前* 轮的临时状态
//! 提示;新一轮会重新注入全新副本,因此历史副本换成稳定占位不改模型行为——
//! 只会(有利地)影响缓存与计费。
//!
//! # 安全性
//! - 白名单是精确的:7 种记账模式 + 1 种 continue 尾缀。任何其他 reminder
//!   (技能列表、hook 输出)都会原样保留。
//! - 记账块整块移除,不改剩余文本。历史剥空才换成稳定占位
//!   (不能删条:打断 user/assistant 交替;不能留空数组:Anthropic 拒;
//!   不能用空白:chat convert 会剥掉整条)。
//!   最后一条是活尾,剥空则保持原样——Continue / 当轮用量给模型看;
//!   下轮它变成历史后再占位(边界一次 miss,之后稳)。
//! - 字符串 content 仅当整段是记账 / continue 才换成同一占位;不中段抠。
//! - 仅针对 Anthropic walker;OpenAI 分支返回 0。
//!
//! # 匹配形态
//! - 包装正则捕获 `<system-reminder>\n` 与 `\n</system-reminder>` 分隔符
//!   之间的 *单行* 内部文本。CC 将记账 reminder 恰好输出为一行;若内部文本
//!   周围有填充的空行,或一个文本块内堆叠了两个 reminder 块,则会得到
//!   多行内部内容,无法匹配任何白名单模式而被保留。`smoosh_split` 会先运行,
//!   把堆叠的揉合 reminder 拆分为独立块,因此实际上 content_strip 看到的
//!   是"每块一个 reminder"。
//! - `is_continue_trailer` 要求字符串完全相等,因此带尾部空白的尾缀会被
//!   保留。CC 输出的是精确字符串。

use crate::cache_stabilization::drift_detector::ApiKind;
use serde_json::{json, Value};
use std::sync::OnceLock;

const CONTINUE_TRAILER: &str = "Continue from where you left off.";
/// 历史纯记账消息的稳定占位。非空白(逃过 convert trim),不匹配白名单(幂等)。
const BOOKKEEPING_PLACEHOLDER: &str = "<system-reminder>\n[bookkeeping]\n</system-reminder>";

/// `^<system-reminder>\n(INNER)\n</system-reminder>\s*$` — 捕获内部文本,
/// 以便与记账白名单进行匹配。
static REMINDER_WRAP: OnceLock<regex::Regex> = OnceLock::new();
/// 7 种记账内部文本模式。
static BOOKKEEPING: OnceLock<Vec<regex::Regex>> = OnceLock::new();

fn wrap_regex() -> &'static regex::Regex {
    REMINDER_WRAP.get_or_init(|| {
        regex::Regex::new(r"(?s)^<system-reminder>\n(.*?)\n</system-reminder>\s*$")
            .expect("valid static regex")
    })
}

fn bookkeeping_patterns() -> &'static Vec<regex::Regex> {
    BOOKKEEPING.get_or_init(|| {
        [
            r"^Token usage: \d+/\d+; \d+ remaining\s*$",
            r"^Output tokens — turn: [^\n]+ · session: [^\n]+\s*$",
            r"^USD budget: \$[\d.]+/\$[\d.]+; \$[\d.]+ remaining\s*$",
            r"^The task tools haven't been used recently\.",
            r"^The TodoWrite tool hasn't been used recently\.",
            r"^Remaining conversation turns: ",
            r"^Messages? until auto-compact: ",
        ]
        .iter()
        .map(|p| regex::Regex::new(p).expect("valid static regex"))
        .collect()
    })
}

fn is_bookkeeping_reminder(text: &str) -> bool {
    let Some(caps) = wrap_regex().captures(text) else {
        return false;
    };
    let inner = caps.get(1).map(|m| m.as_str()).unwrap_or("");
    bookkeeping_patterns().iter().any(|rx| rx.is_match(inner))
}

fn is_continue_trailer(block: &Value) -> bool {
    block.get("type").and_then(Value::as_str) == Some("text")
        && block.get("text").and_then(Value::as_str) == Some(CONTINUE_TRAILER)
}

fn is_pure_bookkeeping_string(s: &str) -> bool {
    s == CONTINUE_TRAILER || is_bookkeeping_reminder(s)
}

fn placeholder_block() -> Value {
    json!({"type": "text", "text": BOOKKEEPING_PLACEHOLDER})
}

/// 从历史用户消息中剥离记账 reminder 和 continue 尾缀。
/// 返回被移除的块数(字符串整段替换计 1)。
pub fn strip_bookkeeping_content(body: &mut Value, kind: ApiKind) -> usize {
    match kind {
        ApiKind::Anthropic => strip_anthropic_messages(body),
        // CC 特有模式;Anthropic 形式由入站全量归一化负责
        ApiKind::OpenAiChat | ApiKind::OpenAiResponses => 0,
    }
}

fn strip_anthropic_messages(body: &mut Value) -> usize {
    let Some(Value::Array(messages)) = body.get_mut("messages") else {
        return 0;
    };
    let last_idx = messages.len().saturating_sub(1);
    let mut total = 0;

    for (idx, msg) in messages.iter_mut().enumerate() {
        if msg.get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }
        // 最后一条是活尾:Continue / 当轮用量给模型看。下轮变历史再占位。
        let is_live_tail = idx == last_idx;

        match msg.get_mut("content") {
            Some(Value::String(s)) => {
                if !is_pure_bookkeeping_string(s) || is_live_tail {
                    continue;
                }
                *s = BOOKKEEPING_PLACEHOLDER.to_string();
                total += 1;
            }
            Some(Value::Array(content)) => {
                let original_len = content.len();
                let kept: Vec<Value> = content
                    .iter()
                    .filter(|block| {
                        if is_continue_trailer(block) {
                            return false;
                        }
                        if block.get("type").and_then(Value::as_str) == Some("text") {
                            if let Some(text) = block.get("text").and_then(Value::as_str) {
                                if is_bookkeeping_reminder(text) {
                                    return false;
                                }
                            }
                        }
                        true
                    })
                    .cloned()
                    .collect();

                if kept.len() == original_len {
                    continue;
                }
                if kept.is_empty() {
                    if is_live_tail {
                        continue;
                    }
                    total += original_len;
                    *content = vec![placeholder_block()];
                    continue;
                }
                total += original_len - kept.len();
                *content = kept;
            }
            _ => {}
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn reminder(inner: &str) -> Value {
        json!({"type": "text", "text": format!("<system-reminder>\n{}\n</system-reminder>", inner)})
    }

    #[test]
    fn strips_all_seven_bookkeeping_kinds() {
        let inners = [
            "Token usage: 100/200; 100 remaining",
            "Output tokens — turn: 1.2k · session: 5k",
            "USD budget: $1.50/$5.00; $3.50 remaining",
            "The task tools haven't been used recently. Consider using them.",
            "The TodoWrite tool hasn't been used recently.",
            "Remaining conversation turns: 42",
            "Messages until auto-compact: 7",
        ];
        for inner in inners {
            let mut body = json!({
                "messages": [{
                    "role": "user",
                    "content": [{"type": "text", "text": "real work"}, reminder(inner)]
                }]
            });
            let count = strip_bookkeeping_content(&mut body, ApiKind::Anthropic);
            assert_eq!(count, 1, "inner: {inner}");
            let content = &body["messages"][0]["content"];
            assert_eq!(content.as_array().unwrap().len(), 1);
            assert_eq!(content[0]["text"], "real work");
        }
    }

    #[test]
    fn strips_continue_trailer() {
        let mut body = json!({
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "work"},
                    {"type": "text", "text": "Continue from where you left off."}
                ]
            }]
        });
        let count = strip_bookkeeping_content(&mut body, ApiKind::Anthropic);
        assert_eq!(count, 1);
        assert_eq!(body["messages"][0]["content"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn preserves_unknown_reminders() {
        let original = json!({
            "messages": [{
                "role": "user",
                "content": [reminder("The following skills are available:\n- foo: bar")]
            }]
        });
        let mut body = original.clone();
        let count = strip_bookkeeping_content(&mut body, ApiKind::Anthropic);
        assert_eq!(count, 0);
        assert_eq!(body, original);
    }

    #[test]
    fn live_tail_pure_bookkeeping_left_intact() {
        // 最后一条是活尾:Continue / 当轮用量给模型看,剥空则保持原样。
        let original = json!({
            "messages": [{
                "role": "user",
                "content": [reminder("Token usage: 1/2; 1 remaining")]
            }]
        });
        let mut body = original.clone();
        let count = strip_bookkeeping_content(&mut body, ApiKind::Anthropic);
        assert_eq!(count, 0);
        assert_eq!(body, original);
    }

    #[test]
    fn historical_pure_bookkeeping_becomes_placeholder() {
        // 历史纯记账换成稳定占位,不删条(角色交替),不留空数组。
        let mut body = json!({
            "messages": [
                {
                    "role": "user",
                    "content": [
                        reminder("Token usage: 1/2; 1 remaining"),
                        {"type": "text", "text": "Continue from where you left off."}
                    ]
                },
                {"role": "assistant", "content": [{"type": "text", "text": "ok"}]}
            ]
        });
        let count = strip_bookkeeping_content(&mut body, ApiKind::Anthropic);
        assert_eq!(count, 2);
        let content = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["text"], BOOKKEEPING_PLACEHOLDER);
        assert_eq!(body["messages"][1]["role"], "assistant");
    }

    #[test]
    fn historical_pure_bookkeeping_cross_turn_stable() {
        // 历史纯记账数字不同 → 占位后字节相同。
        let mk = |n: u32| {
            json!({
                "messages": [
                    {
                        "role": "user",
                        "content": [reminder(&format!(
                            "Token usage: {n}/200; {} remaining",
                            200 - n
                        ))]
                    },
                    {"role": "assistant", "content": [{"type": "text", "text": "ok"}]}
                ]
            })
        };
        let mut a = mk(50);
        let mut b = mk(150);
        strip_bookkeeping_content(&mut a, ApiKind::Anthropic);
        strip_bookkeeping_content(&mut b, ApiKind::Anthropic);
        assert_eq!(a, b);
        assert_eq!(
            a["messages"][0]["content"][0]["text"],
            BOOKKEEPING_PLACEHOLDER
        );
    }

    #[test]
    fn historical_bookkeeping_string_becomes_placeholder() {
        let mut body = json!({
            "messages": [
                {
                    "role": "user",
                    "content": "<system-reminder>\nToken usage: 1/2; 1 remaining\n</system-reminder>"
                },
                {"role": "assistant", "content": "ok"}
            ]
        });
        let count = strip_bookkeeping_content(&mut body, ApiKind::Anthropic);
        assert_eq!(count, 1);
        assert_eq!(body["messages"][0]["content"], BOOKKEEPING_PLACEHOLDER);
    }

    #[test]
    fn live_tail_bookkeeping_string_left_intact() {
        let original = json!({
            "messages": [{
                "role": "user",
                "content": "<system-reminder>\nToken usage: 1/2; 1 remaining\n</system-reminder>"
            }]
        });
        let mut body = original.clone();
        assert_eq!(strip_bookkeeping_content(&mut body, ApiKind::Anthropic), 0);
        assert_eq!(body, original);
    }

    #[test]
    fn placeholder_is_idempotent() {
        let mut body = json!({
            "messages": [
                {
                    "role": "user",
                    "content": [{"type": "text", "text": BOOKKEEPING_PLACEHOLDER}]
                },
                {"role": "assistant", "content": [{"type": "text", "text": "ok"}]}
            ]
        });
        let snapshot = body.clone();
        assert_eq!(strip_bookkeeping_content(&mut body, ApiKind::Anthropic), 0);
        assert_eq!(body, snapshot);
    }

    #[test]
    fn idempotent() {
        let mut body = json!({
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "work"},
                    reminder("Token usage: 1/2; 1 remaining")
                ]
            }]
        });
        let c1 = strip_bookkeeping_content(&mut body, ApiKind::Anthropic);
        assert_eq!(c1, 1);
        let snapshot = body.clone();
        let c2 = strip_bookkeeping_content(&mut body, ApiKind::Anthropic);
        assert_eq!(c2, 0);
        assert_eq!(body, snapshot);
    }

    #[test]
    fn cross_turn_stable_after_strip() {
        // 仅记账数字不同的两个 body,剥离后必须字节完全相同
        // (核心缓存收益断言)。
        let mk = |n: u32| {
            json!({
                "messages": [{
                    "role": "user",
                    "content": [
                        {"type": "text", "text": "stable work"},
                        reminder(&format!("Token usage: {n}/200; {} remaining", 200 - n))
                    ]
                }]
            })
        };
        let mut a = mk(50);
        let mut b = mk(150);
        strip_bookkeeping_content(&mut a, ApiKind::Anthropic);
        strip_bookkeeping_content(&mut b, ApiKind::Anthropic);
        assert_eq!(a, b);
    }

    #[test]
    fn openai_branches_return_zero() {
        let mut body = json!({"messages": [{"role": "user", "content": [reminder("Token usage: 1/2; 1 remaining")]}]});
        assert_eq!(strip_bookkeeping_content(&mut body, ApiKind::OpenAiChat), 0);
        assert_eq!(
            strip_bookkeeping_content(&mut body, ApiKind::OpenAiResponses),
            0
        );
    }

    #[test]
    fn preserves_stacked_double_reminder_block() {
        // 一个文本块内堆叠两个 reminder,会产生无法匹配任何白名单模式的
        // 多行内部内容,因此该块被保留。实际上 smoosh_split 会先拆分这些。
        let stacked = "<system-reminder>\nA\n</system-reminder>\n<system-reminder>\nToken usage: 1/2; 1 remaining\n</system-reminder>";
        let mut body = json!({
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "real work"},
                    {"type": "text", "text": stacked}
                ]
            }]
        });
        let original = body.clone();
        let count = strip_bookkeeping_content(&mut body, ApiKind::Anthropic);
        assert_eq!(
            count, 0,
            "stacked block must be preserved (no single-line match)"
        );
        assert_eq!(body, original);
    }
}
