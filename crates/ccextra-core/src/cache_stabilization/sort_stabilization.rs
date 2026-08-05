//! PR-E8：对 `<system-reminder>` 块列表进行确定性排序。
//!
//! Claude Code 会在系统提示中注入 `<system-reminder>` 块，其正文是无序列表
//! ——即技能列表和延迟工具列表。列表顺序在轮次之间并不稳定，因此即使逻辑内容
//! 相同，cache prefix 字节也会发生 drift。
//!
//! 本模块对这些块内的列表项进行确定性排序（按字母序），与 cccf 的
//! `sort-stabilization` 扩展保持一致。仅处理 **system** 提示——消息不处理。
//! 工具定义排序已由 [`sort_tools_deterministically`] 处理，此处不重复。
//!
//! # 范围
//!
//! - Anthropic：`system`（字符串或块数组）
//! - OpenAI Chat：`messages[].role in (system, developer)` 的 content
//! - OpenAI Responses：`instructions` + `input[].role in (system,
//!   developer)` 的 content
//!
//! 仅重写文本中含技能或延迟工具标记的块；其他系统文本保持不变。幂等：对
//! 已排序的输入再执行一次是空操作。

use crate::cache_stabilization::drift_detector::ApiKind;
use serde_json::Value;
use std::sync::OnceLock;

/// 编译好的内联（非锚定）技能块匹配器。捕获技能列表 `<system-reminder>`
/// 块的头部 / 条目 / 结束标签，该块可出现在更大的系统提示中的任意位置。
///
/// 之前的版本将模式锚定到 `^...$`，只有当 *整个* 系统字符串恰好是一个技能块
/// 时才能匹配。实际的 Claude Code 系统提示把该块嵌入在字符串中间（前面有
/// 身份文本，后面有其他提醒），因此锚定模式永远无法匹配，排序成了静默空操作。
static SKILLS_BLOCK: OnceLock<regex::Regex> = OnceLock::new();

/// 编译好的内联延迟工具块匹配器：捕获系统提示中任意位置的延迟工具
/// `<system-reminder>` 块的头部 / 工具列表 / 结束标签。
static DEFERRED_BLOCK: OnceLock<regex::Regex> = OnceLock::new();

fn skills_regex() -> &'static regex::Regex {
    SKILLS_BLOCK.get_or_init(|| {
        regex::Regex::new(
            r"(<system-reminder>\nThe following skills are available[^\n]*\n+)(- [\s\S]+?)(\n+</system-reminder>)",
        )
        .expect("valid")
    })
}

fn deferred_regex() -> &'static regex::Regex {
    DEFERRED_BLOCK.get_or_init(|| {
        regex::Regex::new(
            r"(<system-reminder>\nThe following deferred tools are now available[^\n]*\n)([\s\S]+?)(\n+</system-reminder>)",
        )
        .expect("valid")
    })
}

/// 对系统提示内的技能和延迟工具列表块进行确定性排序。返回被修改的块数量。
pub fn stabilize_block_sort(body: &mut Value, kind: ApiKind) -> usize {
    let skills_re = skills_regex();
    let deferred_re = deferred_regex();
    let mut count = 0;

    match kind {
        ApiKind::Anthropic => {
            count += sort_anthropic_system(body, skills_re, deferred_re);
        }
        ApiKind::OpenAiChat => {
            count += sort_openai_system_messages(body, "messages", false, skills_re, deferred_re);
        }
        ApiKind::OpenAiResponses => {
            if let Some(Value::String(s)) = body.get_mut("instructions") {
                count += sort_system_text(s, skills_re, deferred_re);
            }
            count += sort_openai_system_messages(body, "input", true, skills_re, deferred_re);
        }
    }

    count
}

/// Anthropic `system`：字符串或 `{type:"text",text}` 块数组。
fn sort_anthropic_system(
    body: &mut Value,
    skills_re: &regex::Regex,
    deferred_re: &regex::Regex,
) -> usize {
    let Some(system) = body.get_mut("system") else {
        return 0;
    };
    match system {
        Value::String(s) => sort_system_text(s, skills_re, deferred_re),
        Value::Array(blocks) => {
            let mut n = 0;
            for block in blocks.iter_mut() {
                if block.get("type").and_then(|t| t.as_str()) == Some("text") {
                    if let Some(Value::String(text)) = block.get_mut("text") {
                        n += sort_system_text(text, skills_re, deferred_re);
                    }
                }
            }
            n
        }
        _ => 0,
    }
}

/// OpenAI Chat/Responses 系统消息：`body[messages_key][]`，其中
/// `role in (system, developer)`。content 可以是字符串或 parts 数组。
fn sort_openai_system_messages(
    body: &mut Value,
    messages_key: &str,
    responses_parts: bool,
    skills_re: &regex::Regex,
    deferred_re: &regex::Regex,
) -> usize {
    let Some(Value::Array(messages)) = body.get_mut(messages_key) else {
        return 0;
    };
    let mut n = 0;
    for msg in messages.iter_mut() {
        let is_system = matches!(
            msg.get("role").and_then(|r| r.as_str()),
            Some("system") | Some("developer")
        );
        if !is_system {
            continue;
        }
        match msg.get_mut("content") {
            Some(Value::String(s)) => {
                n += sort_system_text(s, skills_re, deferred_re);
            }
            Some(Value::Array(parts)) => {
                for part in parts.iter_mut() {
                    let is_text = match part.get("type").and_then(|t| t.as_str()) {
                        Some("text") => true,
                        Some("input_text") | Some("output_text") => responses_parts,
                        _ => false,
                    };
                    if is_text {
                        if let Some(Value::String(text)) = part.get_mut("text") {
                            n += sort_system_text(text, skills_re, deferred_re);
                        }
                    }
                }
            }
            _ => {}
        }
    }
    n
}

/// 对单个系统文本字符串内的任意技能/延迟工具列表块进行排序。返回被修改的
/// 块数量（0、1 或更多）。
fn sort_system_text(s: &mut String, skills_re: &regex::Regex, deferred_re: &regex::Regex) -> usize {
    let mut n = 0;
    if s.contains("The following skills are available") {
        n += sort_skills_block(s, skills_re);
    }
    if s.contains("deferred tools are now available") {
        n += sort_deferred_block(s, deferred_re);
    }
    n
}

/// 对 `s` 中每个技能列表块的条目进行排序。正则表达式内联匹配该块
/// （在字符串中的任意位置），因此所有出现位置都通过带闭包的 `replace_all`
/// 重写。
fn sort_skills_block(s: &mut String, re: &regex::Regex) -> usize {
    let mut n = 0;
    let replaced = re.replace_all(s, |caps: &regex::Captures| {
        let header = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        let entries_text = caps.get(2).map(|m| m.as_str()).unwrap_or("");
        let footer = caps.get(3).map(|m| m.as_str()).unwrap_or("");

        let entries = split_on_newline_dash(entries_text);
        let mut sorted: Vec<&str> = entries.clone();
        sorted.sort_unstable();
        if entries == sorted {
            // 已排序：原样返回原始文本，使替换成为逐字节的空操作。
            return caps.get(0).map(|m| m.as_str().to_string()).unwrap_or_default();
        }
        n += 1;
        format!("{}{}{}", header, sorted.join("\n"), footer)
    });
    if n > 0 {
        *s = replaced.into_owned();
    }
    n
}

/// 对 `s` 中每个延迟工具块的条目进行排序（内联匹配，所有出现位置）。
fn sort_deferred_block(s: &mut String, re: &regex::Regex) -> usize {
    let mut n = 0;
    let replaced = re.replace_all(s, |caps: &regex::Captures| {
        let header = caps.get(1).map(|m| m.as_str()).unwrap_or("");
        let tools_list = caps.get(2).map(|m| m.as_str()).unwrap_or("");
        let footer = caps.get(3).map(|m| m.as_str()).unwrap_or("");

        let mut tools: Vec<&str> = tools_list
            .split('\n')
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .collect();
        let original = tools.clone();
        tools.sort_unstable();
        if tools == original {
            return caps.get(0).map(|m| m.as_str().to_string()).unwrap_or_default();
        }
        n += 1;
        format!("{}{}{}", header, tools.join("\n"), footer)
    });
    if n > 0 {
        *s = replaced.into_owned();
    }
    n
}

/// 在紧跟 `- ` 的 `\n`（列表项的开头）处切分 `text`，并在每一段上保留 `- `
/// 前缀。复刻 cccf 的 `split(/\n(?=- )/)`，但不使用正则前瞻（Rust regex 不支持
/// lookaround）。
fn split_on_newline_dash(text: &str) -> Vec<&str> {
    let mut out = Vec::new();
    let bytes = text.as_bytes();
    let mut start = 0;
    let mut i = 0;
    while i + 1 < bytes.len() {
        if bytes[i] == b'\n' && bytes.get(i + 1) == Some(&b'-') && bytes.get(i + 2) == Some(&b' ') {
            out.push(&text[start..i]);
            start = i + 1;
            i += 1;
        }
        i += 1;
    }
    out.push(&text[start..]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const SKILLS_HEADER: &str =
        "<system-reminder>\nThe following skills are available for use with the Skill tool:\n";
    const SKILLS_FOOTER: &str = "\n</system-reminder>";

    fn skills_block(entries: &[&str]) -> String {
        SKILLS_HEADER.to_string() + &entries.join("\n") + SKILLS_FOOTER
    }

    /// 按 Claude Code 实际嵌入的样式包裹技能块：前面是身份文本，后面是其他
    /// 内容。这正是旧版锚定正则始终静默匹配不到的形状。
    fn embedded(entries: &[&str]) -> String {
        format!(
            "You are Claude Code. Today's date is 2026-07-21.\n\n{}\n\nOther stable trailing text.",
            skills_block(entries)
        )
    }

    // ── Anthropic ───────────────────────────────────────────────────────

    #[test]
    fn anthropic_sorts_skills_block_in_system_array() {
        let mut body = json!({
            "system": [
                {"type": "text", "text": skills_block(&[
                    "- update-config: Configure the harness.",
                    "- agent-browser: Browser automation.",
                    "- keybindings-help: Customize keys.",
                ])}
            ],
            "messages": []
        });
        let n = stabilize_block_sort(&mut body, ApiKind::Anthropic);
        assert_eq!(n, 1);
        assert_eq!(
            body["system"][0]["text"].as_str().unwrap(),
            skills_block(&[
                "- agent-browser: Browser automation.",
                "- keybindings-help: Customize keys.",
                "- update-config: Configure the harness.",
            ])
        );
    }

    #[test]
    fn anthropic_sorts_deferred_block_in_system_array() {
        let header = "<system-reminder>\nThe following deferred tools are now available:\n";
        let footer = "\n</system-reminder>";
        let block = format!("{header}read_file\nwrite_file\nbash{footer}");
        let mut body = json!({
            "system": [{"type": "text", "text": block}],
            "messages": []
        });
        let n = stabilize_block_sort(&mut body, ApiKind::Anthropic);
        assert_eq!(n, 1);
        assert_eq!(
            body["system"][0]["text"].as_str().unwrap(),
            format!("{header}bash\nread_file\nwrite_file{footer}")
        );
    }

    #[test]
    fn anthropic_skills_block_already_sorted_untouched() {
        let sorted = skills_block(&[
            "- a-skill: First.",
            "- b-skill: Second.",
            "- c-skill: Third.",
        ]);
        let mut body = json!({
            "system": [{"type": "text", "text": sorted}],
            "messages": []
        });
        let before = serde_json::to_vec(&body).unwrap();
        let n = stabilize_block_sort(&mut body, ApiKind::Anthropic);
        assert_eq!(n, 0);
        let after = serde_json::to_vec(&body).unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn anthropic_non_listing_block_untouched() {
        let mut body = json!({
            "system": [
                {"type": "text", "text": "<system-reminder>\nSome other reminder.\n</system-reminder>\n"}
            ],
            "messages": []
        });
        let before = serde_json::to_vec(&body).unwrap();
        let n = stabilize_block_sort(&mut body, ApiKind::Anthropic);
        assert_eq!(n, 0);
        let after = serde_json::to_vec(&body).unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn anthropic_system_string_sorted() {
        let mut body = json!({
            "system": skills_block(&["- z-skill: Z.", "- a-skill: A."]),
            "messages": []
        });
        let n = stabilize_block_sort(&mut body, ApiKind::Anthropic);
        assert_eq!(n, 1);
        assert_eq!(
            body["system"].as_str().unwrap(),
            skills_block(&["- a-skill: A.", "- z-skill: Z."])
        );
    }

    #[test]
    fn anthropic_multiple_blocks_each_sorted() {
        let mut body = json!({
            "system": [
                {"type": "text", "text": skills_block(&["- z: Z.", "- a: A."])},
                {"type": "text", "text": "plain system text"},
                {"type": "text", "text": skills_block(&["- m: M.", "- b: B."])}
            ],
            "messages": []
        });
        let n = stabilize_block_sort(&mut body, ApiKind::Anthropic);
        assert_eq!(n, 2);
        assert_eq!(
            body["system"][0]["text"].as_str().unwrap(),
            skills_block(&["- a: A.", "- z: Z."])
        );
        assert_eq!(
            body["system"][2]["text"].as_str().unwrap(),
            skills_block(&["- b: B.", "- m: M."])
        );
    }

    #[test]
    fn anthropic_idempotent() {
        let mut body = json!({
            "system": [{"type": "text", "text": skills_block(&["- z: Z.", "- a: A."])}],
            "messages": []
        });
        stabilize_block_sort(&mut body, ApiKind::Anthropic);
        let after_first = serde_json::to_vec(&body).unwrap();
        let n = stabilize_block_sort(&mut body, ApiKind::Anthropic);
        assert_eq!(n, 0);
        let after_second = serde_json::to_vec(&body).unwrap();
        assert_eq!(after_first, after_second);
    }

    // ── 回归：块嵌入在字符串中间（生产环境 bug） ─────────────────

    #[test]
    fn anthropic_skills_block_embedded_in_larger_system_is_sorted() {
        let mut body = json!({
            "system": embedded(&["- z: Z.", "- a: A.", "- m: M."]),
            "messages": []
        });
        let n = stabilize_block_sort(&mut body, ApiKind::Anthropic);
        assert_eq!(n, 1, "embedded skills block must be sorted");
        let out = body["system"].as_str().unwrap();
        assert!(out.contains("- a: A.\n- m: M.\n- z: Z."));
        assert!(out.starts_with("You are Claude Code."));
        assert!(out.ends_with("Other stable trailing text."));
    }

    #[test]
    fn anthropic_skills_block_embedded_idempotent() {
        let mut body = json!({
            "system": embedded(&["- z: Z.", "- a: A."]),
            "messages": []
        });
        stabilize_block_sort(&mut body, ApiKind::Anthropic);
        let after_first = serde_json::to_vec(&body).unwrap();
        let n = stabilize_block_sort(&mut body, ApiKind::Anthropic);
        assert_eq!(n, 0);
        assert_eq!(serde_json::to_vec(&body).unwrap(), after_first);
    }

    #[test]
    fn openai_chat_skills_block_embedded_is_sorted() {
        let mut body = json!({
            "messages": [
                {"role": "system", "content": embedded(&["- z: Z.", "- a: A."])},
                {"role": "user", "content": "hi"}
            ]
        });
        let n = stabilize_block_sort(&mut body, ApiKind::OpenAiChat);
        assert_eq!(n, 1);
        let out = body["messages"][0]["content"].as_str().unwrap();
        assert!(out.contains("- a: A.\n- z: Z."));
    }

    #[test]
    fn anthropic_two_skills_blocks_in_one_string_both_sorted() {
        let text = format!(
            "intro\n\n{}\n\nmiddle\n\n{}\n\noutro",
            skills_block(&["- z: Z.", "- a: A."]),
            skills_block(&["- y: Y.", "- b: B."])
        );
        let mut body = json!({
            "system": text,
            "messages": []
        });
        let n = stabilize_block_sort(&mut body, ApiKind::Anthropic);
        assert_eq!(n, 2);
        let out = body["system"].as_str().unwrap();
        assert!(out.contains("- a: A.\n- z: Z."));
        assert!(out.contains("- b: B.\n- y: Y."));
    }

    #[test]
    fn deferred_block_embedded_is_sorted() {
        let text =
            "identity\n\n<system-reminder>\nThe following deferred tools are now available:\nread_file\nwrite_file\nbash\n</system-reminder>\n\ntrailing";
        let mut body = json!({
            "system": text,
            "messages": []
        });
        let n = stabilize_block_sort(&mut body, ApiKind::Anthropic);
        assert_eq!(n, 1);
        let out = body["system"].as_str().unwrap();
        assert!(out.contains("bash\nread_file\nwrite_file"));
        assert!(out.ends_with("trailing"));
    }

    // ── OpenAI Chat ─────────────────────────────────────────────────────

    #[test]
    fn openai_chat_sorts_skills_in_system_message_string() {
        let mut body = json!({
            "messages": [
                {"role": "system", "content": skills_block(&["- z: Z.", "- a: A."])},
                {"role": "user", "content": "hi"}
            ]
        });
        let n = stabilize_block_sort(&mut body, ApiKind::OpenAiChat);
        assert_eq!(n, 1);
        assert_eq!(
            body["messages"][0]["content"].as_str().unwrap(),
            skills_block(&["- a: A.", "- z: Z."])
        );
    }

    #[test]
    fn openai_chat_sorts_skills_in_developer_message_array() {
        let mut body = json!({
            "messages": [
                {"role": "developer", "content": [
                    {"type": "text", "text": skills_block(&["- z: Z.", "- a: A."])}
                ]}
            ]
        });
        let n = stabilize_block_sort(&mut body, ApiKind::OpenAiChat);
        assert_eq!(n, 1);
        assert_eq!(
            body["messages"][0]["content"][0]["text"].as_str().unwrap(),
            skills_block(&["- a: A.", "- z: Z."])
        );
    }

    #[test]
    fn openai_chat_input_text_system_part_untouched() {
        let block = skills_block(&["- z: Z.", "- a: A."]);
        let mut body = json!({
            "messages": [{
                "role": "system",
                "content": [{"type": "input_text", "text": block}]
            }]
        });
        let before = serde_json::to_vec(&body).unwrap();

        let n = stabilize_block_sort(&mut body, ApiKind::OpenAiChat);

        assert_eq!(n, 0);
        assert_eq!(serde_json::to_vec(&body).unwrap(), before);
    }

    #[test]
    fn openai_chat_non_system_message_untouched() {
        let block = skills_block(&["- z: Z.", "- a: A."]);
        let mut body = json!({
            "messages": [
                {"role": "user", "content": block}
            ]
        });
        let before = serde_json::to_vec(&body).unwrap();
        let n = stabilize_block_sort(&mut body, ApiKind::OpenAiChat);
        assert_eq!(n, 0);
        let after = serde_json::to_vec(&body).unwrap();
        assert_eq!(before, after);
    }

    // ── OpenAI Responses ───────────────────────────────────────────────

    #[test]
    fn openai_responses_sorts_instructions() {
        let mut body = json!({
            "instructions": skills_block(&["- z: Z.", "- a: A."]),
            "input": []
        });
        let n = stabilize_block_sort(&mut body, ApiKind::OpenAiResponses);
        assert_eq!(n, 1);
        assert_eq!(
            body["instructions"].as_str().unwrap(),
            skills_block(&["- a: A.", "- z: Z."])
        );
    }

    #[test]
    fn openai_responses_sorts_input_system_content_string() {
        let mut body = json!({
            "instructions": null,
            "input": [
                {"role": "system", "content": skills_block(&["- z: Z.", "- a: A."])},
                {"role": "user", "content": "hi"}
            ]
        });
        let n = stabilize_block_sort(&mut body, ApiKind::OpenAiResponses);
        assert_eq!(n, 1);
        assert_eq!(
            body["input"][0]["content"].as_str().unwrap(),
            skills_block(&["- a: A.", "- z: Z."])
        );
    }

    #[test]
    fn openai_responses_sorts_input_text_system_part() {
        let mut body = json!({
            "instructions": "",
            "input": [{
                "type": "message",
                "role": "developer",
                "content": [{
                    "type": "input_text",
                    "text": skills_block(&["- z: Z.", "- a: A."])
                }]
            }]
        });

        let n = stabilize_block_sort(&mut body, ApiKind::OpenAiResponses);

        assert_eq!(n, 1);
        assert_eq!(
            body["input"][0]["content"][0]["text"].as_str().unwrap(),
            skills_block(&["- a: A.", "- z: Z."])
        );
    }

    #[test]
    fn openai_responses_sorts_output_text_system_part() {
        let mut body = json!({
            "instructions": "",
            "input": [{
                "type": "message",
                "role": "developer",
                "content": [{
                    "type": "output_text",
                    "text": skills_block(&["- z: Z.", "- a: A."])
                }]
            }]
        });

        let n = stabilize_block_sort(&mut body, ApiKind::OpenAiResponses);

        assert_eq!(n, 1);
    }

    #[test]
    fn openai_responses_non_system_input_untouched() {
        let block = skills_block(&["- z: Z.", "- a: A."]);
        let mut body = json!({
            "instructions": null,
            "input": [{"role": "user", "content": block}]
        });
        let before = serde_json::to_vec(&body).unwrap();
        let n = stabilize_block_sort(&mut body, ApiKind::OpenAiResponses);
        assert_eq!(n, 0);
        let after = serde_json::to_vec(&body).unwrap();
        assert_eq!(before, after);
    }

    // ── 边界情况 ───────────────────────────────────────────────────────

    #[test]
    fn skills_single_entry_untouched() {
        let single = skills_block(&["- only-skill: The only one."]);
        let mut body = json!({
            "system": [{"type": "text", "text": single}],
            "messages": []
        });
        let before = serde_json::to_vec(&body).unwrap();
        let n = stabilize_block_sort(&mut body, ApiKind::Anthropic);
        assert_eq!(n, 0);
        let after = serde_json::to_vec(&body).unwrap();
        assert_eq!(before, after);
    }

    #[test]
    fn skills_block_missing_closing_tag_untouched() {
        let broken = format!("{SKILLS_HEADER}- z: Z.\n- a: A.");
        let mut body = json!({
            "system": [{"type": "text", "text": broken}],
            "messages": []
        });
        let before = serde_json::to_vec(&body).unwrap();
        let n = stabilize_block_sort(&mut body, ApiKind::Anthropic);
        assert_eq!(n, 0);
        let after = serde_json::to_vec(&body).unwrap();
        assert_eq!(before, after);
    }
}
