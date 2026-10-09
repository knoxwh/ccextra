//! checkpoint 丢失时的历史伪造(2026-10-09 探针 F/G 实证配方)。
//!
//! 模型上下文来自 `ConversationStateStructure.root_prompt_messages_json`
//! (JSON role/content blob,sha256 键),`turns` 只是结构化记录。Anthropic
//! 历史映射为 root prompt blob 序列 + turns 结构,替代 flatten 全量
//! UserText 重放(64KiB RST 风险)。assistant thinking 省略:真实流量是
//! 服务端加密的 redacted-reasoning,客户端无法伪造。

use super::input;
use super::proto::generated::{
    conversation_turn_structure, AgentConversationTurnStructure, AssistantMessage,
    ConversationTurnStructure, UserMessage,
};
use super::CursorConvertError;
use prost::Message;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

/// 伪造历史产物。root_prompt_ids 不含 system blob(由 request.rs 前置)。
pub(super) struct FabricatedHistory {
    pub root_prompt_ids: Vec<Vec<u8>>,
    pub turn_ids: Vec<Vec<u8>>,
    pub blobs: HashMap<String, Vec<u8>>,
}

/// 存 blob 并返回其 sha256 ID(键为 hex,对齐 drive.rs KvGet 查询)
fn insert_blob(blobs: &mut HashMap<String, Vec<u8>>, value: Vec<u8>) -> Vec<u8> {
    let digest = Sha256::digest(&value);
    blobs.insert(hex::encode(digest), value);
    digest.to_vec()
}

fn content_parts(content: &Value) -> &[Value] {
    content.as_array().map(Vec::as_slice).unwrap_or(&[])
}

/// tool_result 内容提取:字符串原样,数组拼 text 块
fn tool_result_text(content: &Value) -> String {
    match content {
        Value::String(text) => text.clone(),
        Value::Array(parts) => parts
            .iter()
            .filter(|part| part.get("type").and_then(Value::as_str) == Some("text"))
            .filter_map(|part| part.get("text").and_then(Value::as_str))
            .collect::<Vec<_>>()
            .join("\n"),
        _ => String::new(),
    }
}

/// 落盘当前 turn:user_message + steps 各自成 blob,turn 结构引用其 ID
fn flush_turn(
    blobs: &mut HashMap<String, Vec<u8>>,
    turn_ids: &mut Vec<Vec<u8>>,
    pending_user: &mut Option<Vec<u8>>,
    pending_steps: &mut Vec<Vec<u8>>,
) {
    let Some(user_message) = pending_user.take() else {
        // 无起点的孤儿 steps(首条 user 前出现 assistant)直接丢弃
        pending_steps.clear();
        return;
    };
    let user_id = insert_blob(blobs, user_message);
    let steps: Vec<Vec<u8>> = pending_steps
        .drain(..)
        .map(|step| insert_blob(blobs, step))
        .collect();
    let turn = ConversationTurnStructure {
        turn: Some(conversation_turn_structure::Turn::AgentConversationTurn(
            AgentConversationTurnStructure {
                user_message: user_id,
                steps,
                ..Default::default()
            },
        )),
    }
    .encode_to_vec();
    turn_ids.push(insert_blob(blobs, turn));
}

/// 伪造历史:扫描 messages(末条非 system 消息的文本归 UserText,不进历史;
/// 其 tool_result 块进历史——伪造路径无 parked 流可走)。
/// 末条之前:user 文本起 turn 并产 user blob,assistant 产 text/tool-call
/// blob 与 turn step,tool_result 产 role:"tool" blob。
pub(super) fn fabricate(body: &Value) -> Result<FabricatedHistory, CursorConvertError> {
    let messages = body
        .get("messages")
        .and_then(Value::as_array)
        .ok_or_else(|| CursorConvertError::Invalid("缺少 messages 数组".into()))?;
    let last_conversation = messages
        .iter()
        .rposition(|message| message.get("role").and_then(Value::as_str) != Some("system"));
    // tool_use_id → 工具名(tool-result 条目需要 toolName,来自前面的 tool_use)
    let mut tool_names: HashMap<String, String> = HashMap::new();
    for message in messages {
        for part in content_parts(message.get("content").unwrap_or(&Value::Null)) {
            if part.get("type").and_then(Value::as_str) == Some("tool_use") {
                if let (Some(id), Some(name)) = (
                    part.get("id").and_then(Value::as_str),
                    part.get("name").and_then(Value::as_str),
                ) {
                    tool_names.insert(id.to_string(), name.to_string());
                }
            }
        }
    }
    let mut blobs = HashMap::new();
    let mut root_prompt_ids = Vec::new();
    let mut turn_ids = Vec::new();
    let mut pending_user: Option<Vec<u8>> = None;
    let mut pending_steps: Vec<Vec<u8>> = Vec::new();

    for (index, message) in messages.iter().enumerate() {
        let role = message.get("role").and_then(Value::as_str).unwrap_or("");
        if role == "system" {
            continue;
        }
        let content = message.get("content").unwrap_or(&Value::Null);
        let is_last = Some(index) == last_conversation;
        match role {
            "user" => {
                let mut text = input::text(content)?;
                // 纯图片等无文本的非末条回合:占位标记保证 turn 与 root prompt 完整
                if !is_last
                    && text.is_empty()
                    && content_parts(content)
                        .iter()
                        .any(|part| part.get("type").and_then(Value::as_str) == Some("image"))
                {
                    text = "[image]".into();
                }
                for part in content_parts(content) {
                    if part.get("type").and_then(Value::as_str) != Some("tool_result") {
                        continue;
                    }
                    let id = part
                        .get("tool_use_id")
                        .and_then(Value::as_str)
                        .unwrap_or_default();
                    let entry = json!({
                        "role": "tool",
                        "id": id,
                        "content": [{
                            "type": "tool-result",
                            "toolName": tool_names.get(id).cloned().unwrap_or_default(),
                            "toolCallId": id,
                            "result": tool_result_text(
                                part.get("content").unwrap_or(&Value::Null)
                            ),
                        }],
                    });
                    root_prompt_ids.push(insert_blob(&mut blobs, serde_json::to_vec(&entry)?));
                }
                // 非末条且有文本:新 turn 起点;末条文本归 UserText,不起 turn
                if !is_last && !text.is_empty() {
                    flush_turn(
                        &mut blobs,
                        &mut turn_ids,
                        &mut pending_user,
                        &mut pending_steps,
                    );
                    pending_user = Some(
                        UserMessage {
                            text: text.clone(),
                            ..Default::default()
                        }
                        .encode_to_vec(),
                    );
                    let entry = json!({
                        "role": "user",
                        "content": [{
                            "type": "text",
                            "text": format!("<user_query>\n{text}\n</user_query>"),
                        }],
                    });
                    root_prompt_ids.push(insert_blob(&mut blobs, serde_json::to_vec(&entry)?));
                }
            }
            "assistant" => {
                let (entries, step_texts) = assistant_entries(content)?;
                if !entries.is_empty() {
                    let entry = json!({ "role": "assistant", "content": entries });
                    root_prompt_ids.push(insert_blob(&mut blobs, serde_json::to_vec(&entry)?));
                }
                for text in step_texts {
                    pending_steps.push(AssistantMessage { text }.encode_to_vec());
                }
            }
            other => {
                return Err(CursorConvertError::Invalid(format!("未知角色 {other}")));
            }
        }
    }
    flush_turn(
        &mut blobs,
        &mut turn_ids,
        &mut pending_user,
        &mut pending_steps,
    );
    Ok(FabricatedHistory {
        root_prompt_ids,
        turn_ids,
        blobs,
    })
}

/// assistant content → (root prompt JSON 条目, turn step 文本)。
/// thinking/redacted_thinking/image 省略,tool_use 转 tool-call 条目。
fn assistant_entries(content: &Value) -> Result<(Vec<Value>, Vec<String>), CursorConvertError> {
    let mut entries = Vec::new();
    let mut step_texts = Vec::new();
    match content {
        Value::Null => {}
        Value::String(text) => {
            if !text.is_empty() {
                entries.push(json!({ "type": "text", "text": text }));
                step_texts.push(text.clone());
            }
        }
        Value::Array(parts) => {
            for part in parts {
                match part.get("type").and_then(Value::as_str) {
                    Some("text") => {
                        let text = part.get("text").and_then(Value::as_str).unwrap_or("");
                        if !text.is_empty() {
                            entries.push(json!({ "type": "text", "text": text }));
                            step_texts.push(text.to_string());
                        }
                    }
                    Some("tool_use") => {
                        entries.push(json!({
                            "type": "tool-call",
                            "toolCallId": part.get("id").and_then(Value::as_str).unwrap_or_default(),
                            "toolName": part.get("name").and_then(Value::as_str).unwrap_or_default(),
                            "args": part.get("input").cloned().unwrap_or_else(|| json!({})),
                        }));
                    }
                    Some("thinking" | "redacted_thinking" | "image") => {}
                    Some(other) => {
                        return Err(CursorConvertError::Unsupported(format!("输入块 {other}")))
                    }
                    None => {
                        return Err(CursorConvertError::Invalid("content 数组缺少 type".into()))
                    }
                }
            }
        }
        _ => {
            return Err(CursorConvertError::Invalid(
                "content 必须是文本或块数组".into(),
            ))
        }
    }
    Ok((entries, step_texts))
}
