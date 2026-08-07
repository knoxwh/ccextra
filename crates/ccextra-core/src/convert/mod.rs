// 协议转换:三条独立 body-to-body 转换
//
// - passthrough: claude → claude 只改 model
// - to_openai_chat: anthropic → openai chat
// - to_openai_responses: anthropic → openai responses

use thiserror::Error;

pub mod fix_json;
pub mod passthrough;
pub mod shorten;
pub mod to_openai_chat;
pub mod to_openai_responses;

pub use fix_json::fix_json_quotes;
pub use passthrough::convert_passthrough;
pub use shorten::build_reverse_map;
pub use shorten::build_short_name_map;
pub use to_openai_chat::convert_to_openai_chat;
pub use to_openai_responses::convert_to_openai_responses;

/// Claude Code 每请求注入 system 的计费+prompt 指纹块前缀(内容逐请求变化)。
/// 转换到 openai 侧必须剥离,否则上游缓存前缀每次请求全 miss。
const CLAUDE_CODE_ATTRIBUTION_PREFIX: &str = "x-anthropic-billing-header:";

/// 是否为 Claude Code 计费归属文本(前导空白后以前缀开头)
pub fn is_attribution_text(text: &str) -> bool {
    text.trim_start()
        .starts_with(CLAUDE_CODE_ATTRIBUTION_PREFIX)
}

/// type:object 节点递归补 properties:{}(部分 OpenAI 兼容上游要求 object schema 必须带 properties)。
pub fn normalize_object_schema_properties(schema: serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    match schema {
        Value::Object(mut map) => {
            let is_object_type = map
                .get("type")
                .and_then(|t| t.as_str())
                .is_some_and(|t| t == "object");
            if is_object_type && !map.contains_key("properties") {
                map.insert("properties".into(), serde_json::json!({}));
            }
            for (_, v) in map.iter_mut() {
                let taken = std::mem::take(v);
                *v = normalize_object_schema_properties(taken);
            }
            Value::Object(map)
        }
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .map(normalize_object_schema_properties)
                .collect(),
        ),
        other => other,
    }
}

/// 合并连续同角色消息(对齐上游 ClaudeMessageAccumulator)。
///
/// Claude Code 可能把一轮 assistant 输出拆成多条消息(如 thinking 单独一条、
/// text+tool_use 另一条),转换到 openai 后会产生连续同角色消息,部分兼容
/// 上游会拒收或语义错乱。合并规则:
/// - 仅处理 user/assistant;system 原位保留(转换器单独处理 reminder)
/// - 空消息(空串/null content)跳过,且不打断当前轮次
/// - 同 role 连续 → 拼接 content 数组
/// - assistant 轮 tool_use parts 移到末尾(对齐上游,保持 text 在前工具收尾)
///
/// 幂等:对已合并结果二次执行零改动。
pub fn merge_consecutive_messages(messages: &[serde_json::Value]) -> Vec<serde_json::Value> {
    use serde_json::Value;
    let mut out: Vec<Value> = Vec::with_capacity(messages.len());
    for msg in messages {
        let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("");
        if role != "user" && role != "assistant" {
            out.push(msg.clone());
            continue;
        }
        let content = msg.get("content").cloned().unwrap_or(Value::Null);
        // 空消息(空串/空数组/null)跳过但不打断当前轮,对齐上游 parts 为空丢弃;
        // 非字符串/数组的 content(对象/数字)同样丢弃(对齐 claudeMessageContentParts)
        match &content {
            Value::Null => continue,
            Value::String(s) if s.trim().is_empty() => continue,
            Value::Array(a) if a.is_empty() => continue,
            Value::String(_) | Value::Array(_) => {}
            _ => continue,
        }
        if let Some(last) = out.last_mut() {
            if last.get("role").and_then(|v| v.as_str()) == Some(role) {
                append_message_content(last, content, role);
                continue;
            }
        }
        out.push(msg.clone());
    }
    out
}

/// 把 content 追加到已存在的消息里(字符串 → text 数组,数组 → 拼接)。
/// assistant 轮的 tool_use parts 移到末尾,保持相对顺序(对齐上游)。
fn append_message_content(
    msg: &mut serde_json::Value,
    content: serde_json::Value,
    role: &str,
) {
    use serde_json::Value;
    let new_parts = match &content {
        Value::String(s) => vec![serde_json::json!({"type": "text", "text": s})],
        Value::Array(parts) => parts.clone(),
        other => vec![other.clone()],
    };
    let (tool_parts, non_tool): (Vec<Value>, Vec<Value>) = if role == "assistant" {
        new_parts.into_iter().partition(|p| {
            p.get("type").and_then(|t| t.as_str()) == Some("tool_use")
        })
    } else {
        (Vec::new(), new_parts)
    };

    // 先把已存在的 tool_use 从内容里抽出,合并后统一追加到末尾
    let mut existing_tools: Vec<Value> = Vec::new();
    match msg.get_mut("content") {
        Some(Value::Array(arr)) => {
            let mut kept: Vec<Value> = Vec::with_capacity(arr.len());
            for p in arr.drain(..) {
                if role == "assistant"
                    && p.get("type").and_then(|t| t.as_str()) == Some("tool_use")
                {
                    existing_tools.push(p);
                } else {
                    kept.push(p);
                }
            }
            kept.extend(non_tool);
            kept.extend(existing_tools);
            kept.extend(tool_parts);
            *arr = kept;
        }
        Some(Value::String(existing)) => {
            let mut parts = vec![serde_json::json!({"type": "text", "text": existing})];
            parts.extend(non_tool);
            parts.extend(existing_tools);
            parts.extend(tool_parts);
            msg["content"] = Value::Array(parts);
        }
        _ => {
            let combined: Vec<Value> = non_tool
                .into_iter()
                .chain(existing_tools)
                .chain(tool_parts)
                .collect();
            msg["content"] = Value::Array(combined);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_merge_consecutive_merges_assistant_turns() {
        let messages = vec![
            json!({"role": "assistant", "content": [{"type": "thinking", "thinking": "t1"}]}),
            json!({"role": "assistant", "content": [
                {"type": "text", "text": "answer"},
                {"type": "tool_use", "id": "c1", "name": "Read", "input": {"p": "a"}}
            ]}),
            json!({"role": "user", "content": "go"}),
        ];
        let merged = merge_consecutive_messages(&messages);
        assert_eq!(merged.len(), 2);
        assert_eq!(merged[0]["content"].as_array().unwrap().len(), 3);
        assert_eq!(merged[0]["content"][0]["type"], "thinking");
        assert_eq!(merged[0]["content"][2]["type"], "tool_use");
        assert_eq!(merged[1]["role"], "user");
    }

    #[test]
    fn test_merge_consecutive_skips_empty_without_breaking_turn() {
        let messages = vec![
            json!({"role": "assistant", "content": [{"type": "thinking", "thinking": "t1"}]}),
            json!({"role": "assistant", "content": ""}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "answer"}]}),
        ];
        let merged = merge_consecutive_messages(&messages);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0]["content"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn test_merge_consecutive_keeps_system_in_place() {
        let messages = vec![
            json!({"role": "assistant", "content": [{"type": "text", "text": "a"}]}),
            json!({"role": "system", "content": [{"type": "text", "text": "reminder"}]}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "b"}]}),
        ];
        let merged = merge_consecutive_messages(&messages);
        assert_eq!(merged.len(), 3);
        // system 保留原位,打断合并
        assert_eq!(merged[1]["role"], "system");
    }

    #[test]
    fn test_merge_consecutive_idempotent() {
        let messages = vec![
            json!({"role": "assistant", "content": [{"type": "text", "text": "a"}]}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "b"}]}),
        ];
        let once = merge_consecutive_messages(&messages);
        let twice = merge_consecutive_messages(&once);
        assert_eq!(once, twice);
        assert_eq!(once.len(), 1);
    }

    #[test]
    fn test_merge_consecutive_moves_tool_use_to_end() {
        // 对齐上游 TestClaudeMessageAccumulatorGroupsAndOrdersAssistantParts:
        // tool_use 全部移到轮次末尾,非工具 parts 保持原始顺序
        let messages = vec![
            json!({"role": "assistant", "content": [{"type": "tool_use", "id": "call_1", "name": "first", "input": {}}]}),
            json!({"role": "assistant", "content": [
                {"type": "thinking", "thinking": "reason"},
                {"type": "text", "text": "answer"}
            ]}),
            json!({"role": "assistant", "content": [{"type": "tool_use", "id": "call_2", "name": "second", "input": {}}]}),
        ];
        let merged = merge_consecutive_messages(&messages);
        assert_eq!(merged.len(), 1);
        let content = merged[0]["content"].as_array().unwrap();
        let types: Vec<&str> = content
            .iter()
            .map(|p| p["type"].as_str().unwrap_or(""))
            .collect();
        assert_eq!(types, vec!["thinking", "text", "tool_use", "tool_use"]);
        assert_eq!(content[2]["id"], "call_1");
        assert_eq!(content[3]["id"], "call_2");
    }

    #[test]
    fn test_merge_consecutive_user_string_and_array() {
        let messages = vec![
            json!({"role": "user", "content": "first"}),
            json!({"role": "user", "content": [{"type": "text", "text": "second"}]}),
        ];
        let merged = merge_consecutive_messages(&messages);
        assert_eq!(merged.len(), 1);
        let content = merged[0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["text"], "first");
        assert_eq!(content[1]["text"], "second");
    }

    #[test]
    fn test_merge_consecutive_empty_array_skipped() {
        // 空数组成员:对齐上游 parts 为空丢弃,不打断当前轮
        let messages = vec![
            json!({"role": "assistant", "content": [{"type": "text", "text": "a"}]}),
            json!({"role": "assistant", "content": []}),
            json!({"role": "assistant", "content": [{"type": "text", "text": "b"}]}),
        ];
        let merged = merge_consecutive_messages(&messages);
        assert_eq!(merged.len(), 1);
        assert_eq!(merged[0]["content"].as_array().unwrap().len(), 2);
    }
}

#[derive(Debug, Error)]
pub enum ConvertError {
    #[error("JSON 解析错误: {0}")]
    JsonError(#[from] serde_json::Error),

    #[error("缺少必需字段: {0}")]
    MissingField(String),

    #[error("字段类型错误: {0}")]
    InvalidType(String),
}

pub type Result<T> = std::result::Result<T, ConvertError>;
