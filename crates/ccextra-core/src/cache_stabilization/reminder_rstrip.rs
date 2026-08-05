//! PR-E7：对 `<system-reminder>` 块进行尾部空白规范化。
//!
//! Claude Code 在相邻轮次之间以字节级不稳定方式重新序列化历史用户消息内容
//! ——具体来说，`</system-reminder>` 之后的尾部 `\n` 有时存在、有时不存在
//! （anthropics/claude-code#48734，drift form 1）。任何历史消息上单个 2 字节的
//! 差异都会使该轮次的整个 prompt-cache prefix 失效。
//!
//! 本模块采用与 cccf 的 `pinBlockContent`（`identity-normalization.mjs`）相同的
//! 修复：折叠每个包含 `</system-reminder>` 的 `text` 块尾部的空白，使块在轮次之间
//! 保持字节稳定。与 cccf 在进程内映射中固定首次见到的字节不同，这是一个
//! **确定性的纯函数**——相同输入、相同输出、无状态——因此在进程重启和多个
//! 代理实例之间都保持稳定。
//!
//! # 修改策略
//!
//! 仅当文本承载内容块（`type: "text"`、`input_text`、`output_text`）的 `text`
//! 字段以及字符串形式的 `system` / OpenAI `content` **包含 `</system-reminder>`**
//! 时才修改它们。其他块以及不含标记的文本块保持不变。工具输出——Anthropic
//! `tool_result.content` 和 OpenAI `role:"tool"` 的 content——不修改
//! （该 drift 形式 smoosh-split 被推迟处理）。
//!
//! # 范围
//!
//! - Anthropic：`system` + `messages[].content`（字符串或块数组）
//! - OpenAI Chat：`messages[].content`（字符串或 parts 数组），除 `tool`
//!   外的所有 role
//! - OpenAI Responses：`instructions` + `input[].content`（当 `input` 不存在时
//!   回退到 `messages`），除 `tool` 外的所有 role
//!
//! 仅规范化 **尾部** 的 `</system-reminder>`（正则锚定在 `$`），与 cccf 一致。
//! 内容中间的标记保持不变。

use crate::cache_stabilization::drift_detector::ApiKind;
use serde_json::Value;
use std::sync::OnceLock;

/// cccf 的 `/\s+(<\/system-reminder>)\s*$/` 的编译形式。
///
/// 组 1 捕获字面量 `</system-reminder>`；开头的 `\s+` 被替换为单个 `\n`，
/// 尾部的 `\s*$` 被丢弃。
static REMINDER_TRAILING: OnceLock<regex::Regex> = OnceLock::new();

fn reminder_regex() -> &'static regex::Regex {
    REMINDER_TRAILING.get_or_init(|| {
        regex::Regex::new(r"\s+(</system-reminder>)\s*$").expect("valid static regex")
    })
}

/// 在 cache prefix 和历史消息中，对每个包含 `</system-reminder>` 的文本承载
/// 内容块，规范化其尾部 `</system-reminder>` 周围的空白。返回被修改的块数量。
///
/// 确定性且幂等：第二次执行是空操作（正则不再匹配已规范化的文本）。
pub fn normalize_reminder_trailing_whitespace(body: &mut Value, kind: ApiKind) -> usize {
    let re = reminder_regex();
    let mut count = 0;

    match kind {
        ApiKind::Anthropic => {
            count += normalize_anthropic_system(body, re);
            count += normalize_messages(body, "messages", re);
        }
        ApiKind::OpenAiChat => {
            count += normalize_messages(body, "messages", re);
        }
        ApiKind::OpenAiResponses => {
            if let Some(Value::String(s)) = body.get_mut("instructions") {
                count += normalize_text_string(s, re);
            }
            // 规范键是 `input`；有些客户端改用 `messages` 发送
            // （drift_detector 回退）。当两者都存在时，同时规范化两者，
            // 使 cache-key 回退路径也保持字节稳定。
            // 当键不存在时 normalize_messages 会静默返回 0，
            // 因此这里无条件调用是安全的。
            count += normalize_messages(body, "input", re);
            count += normalize_messages(body, "messages", re);
        }
    }

    count
}

/// Anthropic `system`：可以是字符串或 `{type:"text",text}` 块数组。
/// 规范化每个文本承载槽位中的尾部标记。
fn normalize_anthropic_system(body: &mut Value, re: &regex::Regex) -> usize {
    let Some(system) = body.get_mut("system") else {
        return 0;
    };
    match system {
        Value::String(s) => normalize_text_string(s, re),
        Value::Array(blocks) => normalize_text_blocks(blocks, re),
        _ => 0,
    }
}

/// 遍历 `body[messages_key]`（Anthropic/Chat：`messages`；Responses：`input`）
/// 并规范化每条消息的 content。
///
/// 跳过 `role:"tool"`——那是工具输出（OpenAI）。Anthropic 没有 `role:"tool"`
/// （role 只有 user/assistant；工具输出是 `tool_result` 块），因此那里的跳过
/// 是无效的，而 `normalize_text_blocks` 已通过只修改文本类型条目来保持
/// `tool_result` 不变。
///
/// 字符串 content 被直接规范化（drift_detector 对整个消息做哈希，因此字符串
/// content 是真正的 drift 目标）；数组 content 仅规范化文本块。
fn normalize_messages(body: &mut Value, messages_key: &str, re: &regex::Regex) -> usize {
    let Some(Value::Array(messages)) = body.get_mut(messages_key) else {
        return 0;
    };
    let mut n = 0;
    for msg in messages.iter_mut() {
        if msg.get("role").and_then(Value::as_str) == Some("tool") {
            continue;
        }
        match msg.get_mut("content") {
            Some(Value::String(s)) => {
                n += normalize_text_string(s, re);
            }
            Some(Value::Array(blocks)) => {
                n += normalize_text_blocks(blocks, re);
            }
            _ => {}
        }
    }
    n
}

/// 遍历内容块/parts 数组，并规范化每个文本承载条目的 `text` 字段。
/// 被 Anthropic 系统数组、Anthropic 消息 content 和 OpenAI content-parts
/// 遍历器共用。
///
/// 同时接受 `type:"text"`（Anthropic 块、OpenAI Chat parts）和
/// `type:"input_text"` / `type:"output_text"`（OpenAI Responses input
/// parts）。Responses API 使用 `input_text`/`output_text` 而非 `text`；
/// 若不支持这些，Responses 路径会静默跳过每个 content part。
fn normalize_text_blocks(blocks: &mut [Value], re: &regex::Regex) -> usize {
    let mut n = 0;
    for block in blocks.iter_mut() {
        if !matches!(
            block.get("type").and_then(Value::as_str),
            Some("text" | "input_text" | "output_text")
        ) {
            continue;
        }
        if let Some(Value::String(text)) = block.get_mut("text") {
            n += normalize_text_string(text, re);
        }
    }
    n
}

/// 对单个字符串应用尾部提醒正则。若字符串被修改则返回 1，否则返回 0。
/// 对标记子串做廉价预检查，避免对不可能匹配的文本运行正则。
fn normalize_text_string(s: &mut String, re: &regex::Regex) -> usize {
    if !s.contains("</system-reminder>") {
        return 0;
    }
    let replaced = re.replace(s, "\n$1");
    if replaced == s.as_str() {
        return 0;
    }
    *s = replaced.into_owned();
    1
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ── Anthropic ───────────────────────────────────────────────────────

    #[test]
    fn anthropic_strips_trailing_newline_after_reminder_in_message() {
        let mut body = json!({
            "system": "You are helpful.",
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "Allowed\n</system-reminder>\n"}
                ]
            }]
        });
        let n = normalize_reminder_trailing_whitespace(&mut body, ApiKind::Anthropic);
        assert_eq!(n, 1);
        let text = body["messages"][0]["content"][0]["text"].as_str().unwrap();
        assert_eq!(text, "Allowed\n</system-reminder>");
    }

    #[test]
    fn anthropic_collapses_multiple_blank_lines_before_marker() {
        let mut body = json!({
            "system": [],
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "Allowed\n\n\n</system-reminder>\n\n"}
                ]
            }]
        });
        let n = normalize_reminder_trailing_whitespace(&mut body, ApiKind::Anthropic);
        assert_eq!(n, 1);
        let text = body["messages"][0]["content"][0]["text"].as_str().unwrap();
        assert_eq!(text, "Allowed\n</system-reminder>");
    }

    #[test]
    fn anthropic_no_marker_untouched() {
        let mut body = json!({
            "system": "You are helpful.\n",
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "just user text\n\n"}
                ]
            }]
        });
        let before = serde_json::to_vec(&body).unwrap();
        let n = normalize_reminder_trailing_whitespace(&mut body, ApiKind::Anthropic);
        assert_eq!(n, 0);
        let after = serde_json::to_vec(&body).unwrap();
        assert_eq!(before, after, "no-marker body must be byte-equal");
    }

    #[test]
    fn anthropic_marker_without_trailing_untouched() {
        let mut body = json!({
            "system": [],
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "Allowed\n</system-reminder>"}
                ]
            }]
        });
        let before = serde_json::to_vec(&body).unwrap();
        let n = normalize_reminder_trailing_whitespace(&mut body, ApiKind::Anthropic);
        assert_eq!(n, 0);
        let after = serde_json::to_vec(&body).unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn anthropic_normalizes_system_string() {
        let mut body = json!({
            "system": "preamble\n</system-reminder>\n",
            "messages": []
        });
        let n = normalize_reminder_trailing_whitespace(&mut body, ApiKind::Anthropic);
        assert_eq!(n, 1);
        assert_eq!(
            body["system"].as_str().unwrap(),
            "preamble\n</system-reminder>"
        );
    }

    #[test]
    fn anthropic_normalizes_system_array_text_block() {
        let mut body = json!({
            "system": [
                {"type": "text", "text": "preamble\n</system-reminder>\n"}
            ],
            "messages": []
        });
        let n = normalize_reminder_trailing_whitespace(&mut body, ApiKind::Anthropic);
        assert_eq!(n, 1);
        assert_eq!(
            body["system"][0]["text"].as_str().unwrap(),
            "preamble\n</system-reminder>"
        );
    }

    #[test]
    fn anthropic_only_trailing_marker_normalized_mid_marker_untouched() {
        // 内容中间的标记（不在尾部）不会被 $ 锚定匹配到。
        let mut body = json!({
            "system": [],
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "</system-reminder>\nmore text"}
                ]
            }]
        });
        let before = serde_json::to_vec(&body).unwrap();
        let n = normalize_reminder_trailing_whitespace(&mut body, ApiKind::Anthropic);
        assert_eq!(n, 0);
        let after = serde_json::to_vec(&body).unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn anthropic_tool_result_content_not_touched() {
        // 携带 smooshed reminder 的 tool_result.content 字符串在此处不会被规范化
        // （smoosh-split 被推迟处理）。
        let mut body = json!({
            "system": [],
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "tool_result", "content": "output\n</system-reminder>\n"}
                ]
            }]
        });
        let before = serde_json::to_vec(&body).unwrap();
        let n = normalize_reminder_trailing_whitespace(&mut body, ApiKind::Anthropic);
        assert_eq!(n, 0);
        let after = serde_json::to_vec(&body).unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn anthropic_idempotent() {
        let mut body = json!({
            "system": "p\n</system-reminder>\n",
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "a\n</system-reminder>\n"}
                ]
            }]
        });
        normalize_reminder_trailing_whitespace(&mut body, ApiKind::Anthropic);
        let after_first = serde_json::to_vec(&body).unwrap();
        let n = normalize_reminder_trailing_whitespace(&mut body, ApiKind::Anthropic);
        assert_eq!(n, 0);
        let after_second = serde_json::to_vec(&body).unwrap();
        assert_eq!(
            after_first, after_second,
            "second pass must be byte-equal no-op"
        );
    }

    #[test]
    fn anthropic_byte_stable_across_trailing_delta() {
        // 两个仅因尾部 \n（drift form 1）而不同的 body。
        let mut healthy = json!({
            "system": [],
            "messages": [{
                "role": "user",
                "content": [{"type": "text", "text": "Allowed\n</system-reminder>"}]
            }]
        });
        let mut miss = json!({
            "system": [],
            "messages": [{
                "role": "user",
                "content": [{"type": "text", "text": "Allowed\n</system-reminder>\n"}]
            }]
        });
        normalize_reminder_trailing_whitespace(&mut healthy, ApiKind::Anthropic);
        normalize_reminder_trailing_whitespace(&mut miss, ApiKind::Anthropic);
        let h = serde_json::to_vec(&healthy).unwrap();
        let m = serde_json::to_vec(&miss).unwrap();
        assert_eq!(h, m, "post-normalize bodies must be byte-equal");
    }

    // ── OpenAI Chat ─────────────────────────────────────────────────────

    #[test]
    fn openai_chat_strips_trailing_in_user_content_array() {
        let mut body = json!({
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "Allowed\n</system-reminder>\n"}
                ]
            }]
        });
        let n = normalize_reminder_trailing_whitespace(&mut body, ApiKind::OpenAiChat);
        assert_eq!(n, 1);
        assert_eq!(
            body["messages"][0]["content"][0]["text"].as_str().unwrap(),
            "Allowed\n</system-reminder>"
        );
    }

    #[test]
    fn openai_chat_strips_trailing_in_user_content_string() {
        let mut body = json!({
            "messages": [{
                "role": "user",
                "content": "Allowed\n</system-reminder>\n"
            }]
        });
        let n = normalize_reminder_trailing_whitespace(&mut body, ApiKind::OpenAiChat);
        assert_eq!(n, 1);
        assert_eq!(
            body["messages"][0]["content"].as_str().unwrap(),
            "Allowed\n</system-reminder>"
        );
    }

    #[test]
    fn openai_chat_strips_trailing_in_system_message_string() {
        let mut body = json!({
            "messages": [
                {"role": "system", "content": "preamble\n</system-reminder>\n"},
                {"role": "user", "content": "hi"}
            ]
        });
        let n = normalize_reminder_trailing_whitespace(&mut body, ApiKind::OpenAiChat);
        assert_eq!(n, 1);
        assert_eq!(
            body["messages"][0]["content"].as_str().unwrap(),
            "preamble\n</system-reminder>"
        );
    }

    #[test]
    fn openai_chat_strips_trailing_in_developer_message_array() {
        let mut body = json!({
            "messages": [
                {"role": "developer", "content": [
                    {"type": "text", "text": "p\n</system-reminder>\n"}
                ]}
            ]
        });
        let n = normalize_reminder_trailing_whitespace(&mut body, ApiKind::OpenAiChat);
        assert_eq!(n, 1);
        assert_eq!(
            body["messages"][0]["content"][0]["text"].as_str().unwrap(),
            "p\n</system-reminder>"
        );
    }

    #[test]
    fn openai_chat_no_marker_untouched() {
        let mut body = json!({
            "messages": [
                {"role": "system", "content": "no marker here\n"},
                {"role": "user", "content": "plain\n\n"}
            ]
        });
        let before = serde_json::to_vec(&body).unwrap();
        let n = normalize_reminder_trailing_whitespace(&mut body, ApiKind::OpenAiChat);
        assert_eq!(n, 0);
        let after = serde_json::to_vec(&body).unwrap();
        assert_eq!(before, after);
    }

    // ── OpenAI Responses ───────────────────────────────────────────────

    #[test]
    fn openai_responses_strips_trailing_in_instructions() {
        let mut body = json!({
            "instructions": "preamble\n</system-reminder>\n",
            "input": []
        });
        let n = normalize_reminder_trailing_whitespace(&mut body, ApiKind::OpenAiResponses);
        assert_eq!(n, 1);
        assert_eq!(
            body["instructions"].as_str().unwrap(),
            "preamble\n</system-reminder>"
        );
    }

    #[test]
    fn openai_responses_strips_trailing_in_input_user_content_array() {
        let mut body = json!({
            "instructions": "",
            "input": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "Allowed\n</system-reminder>\n"}
                ]
            }]
        });
        let n = normalize_reminder_trailing_whitespace(&mut body, ApiKind::OpenAiResponses);
        assert_eq!(n, 1);
        assert_eq!(
            body["input"][0]["content"][0]["text"].as_str().unwrap(),
            "Allowed\n</system-reminder>"
        );
    }

    #[test]
    fn openai_responses_strips_trailing_in_input_system_content_string() {
        let mut body = json!({
            "instructions": null,
            "input": [
                {"role": "system", "content": "p\n</system-reminder>\n"},
                {"role": "user", "content": "hi"}
            ]
        });
        let n = normalize_reminder_trailing_whitespace(&mut body, ApiKind::OpenAiResponses);
        assert_eq!(n, 1);
        assert_eq!(
            body["input"][0]["content"].as_str().unwrap(),
            "p\n</system-reminder>"
        );
    }

    #[test]
    fn openai_responses_no_marker_untouched() {
        let mut body = json!({
            "instructions": "no marker\n",
            "input": [{"role": "user", "content": "plain\n"}]
        });
        let before = serde_json::to_vec(&body).unwrap();
        let n = normalize_reminder_trailing_whitespace(&mut body, ApiKind::OpenAiResponses);
        assert_eq!(n, 0);
        let after = serde_json::to_vec(&body).unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn openai_responses_falls_back_to_messages_when_no_input() {
        // 有些客户端以 `messages` 而非规范键 `input` 发送 Responses 形态的 body。
        // drift_detector::messages_array 处理这一回退；rstrip 也必须这样做，
        // 否则会静默跳过所有消息。
        let mut body = json!({
            "instructions": null,
            "messages": [{
                "role": "user",
                "content": "Allowed\n</system-reminder>\n"
            }]
        });
        let n = normalize_reminder_trailing_whitespace(&mut body, ApiKind::OpenAiResponses);
        assert_eq!(n, 1);
        assert_eq!(
            body["messages"][0]["content"].as_str().unwrap(),
            "Allowed\n</system-reminder>"
        );
    }

    #[test]
    fn openai_responses_normalizes_both_input_and_messages_when_both_present() {
        // 当 `input` 和 `messages` 都存在时，两者都会被规范化：
        // `input` 是规范键，但当 `input` 没有 system 项时，
        // openai_cache_key::extract_system 会为 cache key 回退到 `messages`，
        // 因此 `messages` 也必须保持字节稳定。
        let mut body = json!({
            "instructions": null,
            "input": [{"role": "user", "content": "from_input\n</system-reminder>\n"}],
            "messages": [{"role": "user", "content": "from_messages\n</system-reminder>\n"}]
        });
        let n = normalize_reminder_trailing_whitespace(&mut body, ApiKind::OpenAiResponses);
        assert_eq!(n, 2);
        assert_eq!(
            body["input"][0]["content"].as_str().unwrap(),
            "from_input\n</system-reminder>"
        );
        assert_eq!(
            body["messages"][0]["content"].as_str().unwrap(),
            "from_messages\n</system-reminder>"
        );
    }

    // ── 回归：来自最大努力审查的发现 ───────────────────────────────

    #[test]
    fn anthropic_string_content_normalized() {
        // Anthropic 用户消息采用字符串形式的 content（非数组）。
        // drift_detector 对整个消息做哈希，因此字符串 content 是真正的
        // drift 目标——不能被跳过。
        let mut body = json!({
            "system": [],
            "messages": [{
                "role": "user",
                "content": "Allowed\n</system-reminder>\n"
            }]
        });
        let n = normalize_reminder_trailing_whitespace(&mut body, ApiKind::Anthropic);
        assert_eq!(n, 1);
        assert_eq!(
            body["messages"][0]["content"].as_str().unwrap(),
            "Allowed\n</system-reminder>"
        );
    }

    #[test]
    fn openai_responses_input_text_part_normalized() {
        // Responses API 的 input parts 使用 "input_text" 类型，而非 "text"。
        // 若不接受 input_text，Responses 路径会跳过每个 part。
        let mut body = json!({
            "instructions": null,
            "input": [{
                "role": "user",
                "content": [
                    {"type": "input_text", "text": "Allowed\n</system-reminder>\n"}
                ]
            }]
        });
        let n = normalize_reminder_trailing_whitespace(&mut body, ApiKind::OpenAiResponses);
        assert_eq!(n, 1);
        assert_eq!(
            body["input"][0]["content"][0]["text"].as_str().unwrap(),
            "Allowed\n</system-reminder>"
        );
    }

    #[test]
    fn openai_responses_output_text_part_normalized() {
        // 助手输出 parts 使用 "output_text" 类型。
        let mut body = json!({
            "instructions": null,
            "input": [{
                "role": "assistant",
                "content": [
                    {"type": "output_text", "text": "p\n</system-reminder>\n"}
                ]
            }]
        });
        let n = normalize_reminder_trailing_whitespace(&mut body, ApiKind::OpenAiResponses);
        assert_eq!(n, 1);
        assert_eq!(
            body["input"][0]["content"][0]["text"].as_str().unwrap(),
            "p\n</system-reminder>"
        );
    }

    #[test]
    fn openai_chat_role_tool_content_not_touched() {
        // role:tool 的 content 是工具输出（对应 Anthropic 的
        // tool_result.content），必须保持不动，与 Anthropic 路径的
        // tool_result-skip 策略一致。
        let mut body = json!({
            "messages": [
                {"role": "assistant", "content": "ok"},
                {"role": "tool", "tool_call_id": "x", "content": "output\n</system-reminder>\n"},
                {"role": "user", "content": "next\n</system-reminder>\n"}
            ]
        });
        let before_tool = body["messages"][1]["content"].as_str().unwrap().to_string();
        let n = normalize_reminder_trailing_whitespace(&mut body, ApiKind::OpenAiChat);
        assert_eq!(n, 1); // only the user message
        assert_eq!(
            body["messages"][1]["content"].as_str().unwrap(),
            before_tool,
            "role:tool content must not be touched"
        );
        assert_eq!(
            body["messages"][2]["content"].as_str().unwrap(),
            "next\n</system-reminder>"
        );
    }

    // ── 交叉主题 ───────────────────────────────────────────────────────

    #[test]
    fn multiple_reminders_only_trailing_normalized() {
        // 两个提醒；只有尾部那个周围的空白会被处理（正则锚定在 $）。
        let mut body = json!({
            "system": [],
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "</system-reminder>\nmid\n</system-reminder>\n\n"}
                ]
            }]
        });
        let n = normalize_reminder_trailing_whitespace(&mut body, ApiKind::Anthropic);
        assert_eq!(n, 1);
        let text = body["messages"][0]["content"][0]["text"].as_str().unwrap();
        // 中间的标记不动；尾部的被折叠。
        assert_eq!(text, "</system-reminder>\nmid\n</system-reminder>");
    }

    #[test]
    fn non_text_block_types_untouched() {
        let mut body = json!({
            "system": [],
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "image", "source": {"data": "abc\n</system-reminder>\n"}},
                    {"type": "tool_use", "name": "x", "input": {"k": "v\n</system-reminder>\n"}}
                ]
            }]
        });
        let before = serde_json::to_vec(&body).unwrap();
        let n = normalize_reminder_trailing_whitespace(&mut body, ApiKind::Anthropic);
        assert_eq!(n, 0);
        let after = serde_json::to_vec(&body).unwrap();
        assert_eq!(before, after);
    }
}
