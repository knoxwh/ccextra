//! PR-E9:从 `tool_result.content` 中剥离开被"揉合"的 `<system-reminder>` 块。
//!
//! Claude Code 有时会把 `<system-reminder>` 文本直接追加到 `tool_result` 块的
//! 字符串内容中,而不是作为独立的文本块发出。`content_strip` 针对的是独立的
//! 文本块,无法触及这些被揉合的 reminder。本模块先将其剥离,把:
//!   tool_result.content: "...output\n\n<system-reminder>…</system-reminder>"
//! 转换为:
//!   tool_result.content: "...output"
//!   {type:"text", text:"<system-reminder>…</system-reminder>"}
//! 并追加到同一条消息的 content 数组中。
//!
//! # 性质
//! - 幂等:第二次遍历找不到尾部 reminder,返回 0。
//! - 无损:所有文本字节都被保留;只有结构发生变化。
//! - 仅针对 Anthropic walker;OpenAI 分支返回 0。

use crate::cache_stabilization::drift_detector::ApiKind;
use serde_json::{json, Value};
use std::sync::OnceLock;

static TRAILING_SMOOSH: OnceLock<regex::Regex> = OnceLock::new();

fn smoosh_regex() -> &'static regex::Regex {
    TRAILING_SMOOSH.get_or_init(|| {
        // 注意:Rust 的 `regex` crate 不支持 look-around,因此简报中的
        // `(?!</system-reminder>)` 负向前瞻改用惰性量词(`*?`)来表达,
        // 它会在第一个 `</system-reminder>` 处停止,从而每次匹配恰好产生一个块。
        // 简报中尾部的 `\s*$` 锚点则通过代码强制执行(见
        // `split_anthropic_messages` 中的尾部检查),因此只有被揉合到内容
        // *末尾* 的 reminder 才会被剥离。行为与简报在所有测试用例上的
        // 预期语义一致。
        regex::Regex::new(r"\n\n(<system-reminder>\n[\s\S]*?\n</system-reminder>)")
            .expect("valid static regex")
    })
}

/// 从 `tool_result.content` 字符串中剥离被揉合的 `<system-reminder>` 块。
/// 返回被剥离的块数。
pub fn split_smooshed_reminders(body: &mut Value, kind: ApiKind) -> usize {
    match kind {
        ApiKind::Anthropic => split_anthropic_messages(body),
        // CC 特有模式;Anthropic 形式由 /v1/messages 和 /v1/pretransform/messages 负责
        ApiKind::OpenAiChat | ApiKind::OpenAiResponses => 0,
    }
}

fn split_anthropic_messages(body: &mut Value) -> usize {
    let re = smoosh_regex();
    let Some(Value::Array(messages)) = body.get_mut("messages") else {
        return 0;
    };
    let mut total = 0;

    for msg in messages.iter_mut() {
        if msg.get("role").and_then(Value::as_str) != Some("user") {
            continue;
        }
        let Some(Value::Array(content)) = msg.get_mut("content") else {
            continue;
        };

        // 收集 (block_index, trimmed_content, peeled_reminders)
        let mut patches: Vec<(usize, String, Vec<String>)> = vec![];

        for (i, block) in content.iter().enumerate() {
            if block.get("type").and_then(Value::as_str) != Some("tool_result") {
                continue;
            }
            let Some(Value::String(s)) = block.get("content") else {
                continue;
            };
            let mut current = s.clone();
            let mut reminders: Vec<String> = vec![];
            // 取最右侧的 `\n\n<system-reminder>…</system-reminder>` 块。
            // `captures_iter` 从左到右产生不重叠的匹配;`.last()` 给出最接近
            // 尾部的那一个。
            while let Some(caps) = re.captures_iter(&current).last() {
                let whole = caps.get(0).expect("group 0 always present");
                // 强制执行简报中的 `\s*$` 尾部锚点:只剥离被揉合到内容
                // *末尾* 的 reminder(其后只有空白)。处于内容中间的 reminder
                // 保持原样。
                if !current[whole.end()..].trim().is_empty() {
                    break;
                }
                let captured = caps
                    .get(1)
                    .map(|g| g.as_str().to_string())
                    .expect("capture group 1 always present on a match");
                reminders.insert(0, captured);
                current = current[..whole.start()].to_string();
            }
            if !reminders.is_empty() {
                patches.push((i, current, reminders));
            }
        }

        if patches.is_empty() {
            continue;
        }

        // 按正向索引顺序应用补丁。补丁只改写已有块的 `content` 字段
        // (不改变数组长度),被剥离的 reminder 被追加到尾部,因此较早的补丁
        // 不会改变后续块的索引。正向顺序与 JS 参考实现(smoosh-split.mjs)
        // 一致,从而多块剥离时字节稳定。
        for (i, trimmed, reminders) in patches.iter() {
            let block = &mut content[*i];
            if let Some(c) = block.get_mut("content") {
                *c = Value::String(trimmed.clone());
            }
            for r in reminders.iter() {
                total += 1;
                content.push(json!({"type": "text", "text": r}));
            }
        }
    }
    total
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn peels_single_smooshed_reminder() {
        let mut body = json!({
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "x",
                    "content": "result text\n\n<system-reminder>\nToken usage: 100/200; 100 remaining\n</system-reminder>"
                }]
            }]
        });
        let count = split_smooshed_reminders(&mut body, ApiKind::Anthropic);
        assert_eq!(count, 1);
        let msg = &body["messages"][0];
        assert_eq!(msg["content"][0]["content"], "result text");
        assert_eq!(msg["content"][1]["type"], "text");
        assert_eq!(
            msg["content"][1]["text"],
            "<system-reminder>\nToken usage: 100/200; 100 remaining\n</system-reminder>"
        );
    }

    #[test]
    fn peels_multiple_stacked_smooshed_reminders() {
        let mut body = json!({
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "x",
                    "content": "output\n\n<system-reminder>\nA\n</system-reminder>\n\n<system-reminder>\nB\n</system-reminder>"
                }]
            }]
        });
        let count = split_smooshed_reminders(&mut body, ApiKind::Anthropic);
        assert_eq!(count, 2);
        let content = &body["messages"][0]["content"];
        assert_eq!(content[0]["content"], "output");
        assert_eq!(content[1]["text"], "<system-reminder>\nA\n</system-reminder>");
        assert_eq!(content[2]["text"], "<system-reminder>\nB\n</system-reminder>");
    }

    #[test]
    fn idempotent_second_pass_returns_zero() {
        let mut body = json!({
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "x",
                    "content": "result text\n\n<system-reminder>\nToken usage: 1/2; 1 remaining\n</system-reminder>"
                }]
            }]
        });
        let c1 = split_smooshed_reminders(&mut body, ApiKind::Anthropic);
        assert_eq!(c1, 1);
        let snapshot = body.clone();
        let c2 = split_smooshed_reminders(&mut body, ApiKind::Anthropic);
        assert_eq!(c2, 0);
        assert_eq!(body, snapshot);
    }

    #[test]
    fn does_not_touch_mid_content_reminder() {
        // reminder 不在末尾 —— 不得剥离
        let original = json!({
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "x",
                    "content": "before\n\n<system-reminder>\nA\n</system-reminder>\n\nafter"
                }]
            }]
        });
        let mut body = original.clone();
        let count = split_smooshed_reminders(&mut body, ApiKind::Anthropic);
        assert_eq!(count, 0);
        assert_eq!(body, original);
    }

    #[test]
    fn openai_branches_return_zero() {
        let mut body = json!({"messages": [{"role": "user", "content": [{"type": "tool_result", "content": "x\n\n<system-reminder>\nA\n</system-reminder>"}]}]});
        assert_eq!(split_smooshed_reminders(&mut body, ApiKind::OpenAiChat), 0);
        assert_eq!(split_smooshed_reminders(&mut body, ApiKind::OpenAiResponses), 0);
    }

    #[test]
    fn non_tool_result_blocks_untouched() {
        let original = json!({
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "hello\n\n<system-reminder>\nA\n</system-reminder>"},
                    {"type": "tool_result", "tool_use_id": "x", "content": "clean output"}
                ]
            }]
        });
        let mut body = original.clone();
        let count = split_smooshed_reminders(&mut body, ApiKind::Anthropic);
        assert_eq!(count, 0);
        assert_eq!(body, original);
    }

    #[test]
    fn peels_multiple_tool_result_blocks_in_forward_order() {
        // 两个 tool_result 块各带一个尾部被揉合的 reminder。
        // 被剥离的 reminder 必须按正向块顺序(A 在前,B 在后)就位,
        // 与 JS 参考实现(smoosh-split.mjs)一致。
        let mut body = json!({
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "tool_result", "tool_use_id": "x", "content": "out0\n\n<system-reminder>\nA\n</system-reminder>"},
                    {"type": "text", "text": "mid"},
                    {"type": "tool_result", "tool_use_id": "y", "content": "out2\n\n<system-reminder>\nB\n</system-reminder>"}
                ]
            }]
        });
        let count = split_smooshed_reminders(&mut body, ApiKind::Anthropic);
        assert_eq!(count, 2);
        let content = &body["messages"][0]["content"];
        assert_eq!(content[0]["content"], "out0");
        assert_eq!(content[1]["text"], "mid");
        assert_eq!(content[2]["content"], "out2");
        // 正向顺序:A 在 B 之前。
        assert_eq!(content[3]["text"], "<system-reminder>\nA\n</system-reminder>");
        assert_eq!(content[4]["text"], "<system-reminder>\nB\n</system-reminder>");
    }
}
