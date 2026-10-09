//! RunSSE 双通道读侧 JSON 线格式解码(2026-10-09 探针 E/G 实证)。
//!
//! 双通道模式下读侧 `AgentService/RunSSE` 返回 connect+json 帧:Connect
//! 信封不变,载荷是 JSON 而非 proto。字段名为 proto camelCase JSON 名,
//! int64 数值按 protobuf JSON 惯例编码为字符串(如 turnEnded.inputTokens)。
//! 写侧 BidiAppend 仍是 proto hex,回复编码复用 reply.rs。未知字段静默
//! 忽略(顶层 ttftBreakdown、interactionUpdate.messageStartedAtMs 等),
//! 与 proto 路径的宽容策略一致。

use super::super::generated;
use super::{
    ExecKind, ExecRequest, InteractionQuery, InteractionQueryKind, RawCheckpoint, ServerMessage,
    TurnUsage,
};
use crate::convert::cursor::proto::wire::WireError;
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use prost::Message;
use serde_json::Value;

/// int64/int32 数值字段:protobuf JSON 把 int64 编码为字符串,int32 为数字
fn int_field(value: &Value) -> Option<i64> {
    match value {
        Value::Number(number) => number.as_i64(),
        Value::String(text) => text.parse().ok(),
        _ => None,
    }
}

/// bytes 字段:JSON 里是 base64
fn bytes_field(value: &Value) -> Option<Vec<u8>> {
    value.as_str().and_then(|text| STANDARD.decode(text).ok())
}

/// JSON 值转 protobuf Value(mcpArgs 的 map 值是 google.protobuf.Value,
/// JSON 形态就是裸 JSON 值;编码回 proto 字节以复用 decode_mcp_args)
fn to_proto_value(value: &Value) -> prost_types::Value {
    let kind = match value {
        Value::Null => Some(prost_types::value::Kind::NullValue(0)),
        Value::Bool(flag) => Some(prost_types::value::Kind::BoolValue(*flag)),
        Value::Number(number) => Some(prost_types::value::Kind::NumberValue(
            number.as_f64().unwrap_or_default(),
        )),
        Value::String(text) => Some(prost_types::value::Kind::StringValue(text.clone())),
        Value::Array(items) => Some(prost_types::value::Kind::ListValue(
            prost_types::ListValue {
                values: items.iter().map(to_proto_value).collect(),
            },
        )),
        Value::Object(fields) => Some(prost_types::value::Kind::StructValue(prost_types::Struct {
            fields: fields
                .iter()
                .map(|(key, item)| (key.clone(), to_proto_value(item)))
                .collect(),
        })),
    };
    prost_types::Value { kind }
}

/// 解析 RunSSE JSON 帧(载荷是 AgentServerMessage 的 JSON 形态)
pub fn decode_agent_server_message_json(data: &[u8]) -> Result<Vec<ServerMessage>, WireError> {
    let root: Value = serde_json::from_slice(data)
        .map_err(|err| WireError::Json(format!("AgentServerMessage JSON 解析失败: {err}")))?;
    let object = root
        .as_object()
        .ok_or_else(|| WireError::Json("AgentServerMessage JSON 不是对象".into()))?;
    let mut messages = Vec::new();
    for (key, value) in object {
        match key.as_str() {
            "interactionUpdate" => messages.extend(decode_interaction_json(value)?),
            "execServerMessage" => messages.push(decode_exec_json(value)?),
            "conversationCheckpointUpdate" => {
                let raw = conversation_state_from_json(value)?;
                messages.push(ServerMessage::Checkpoint(RawCheckpoint(raw)));
            }
            "kvServerMessage" => {
                if let Some(message) = decode_kv_json(value)? {
                    messages.push(message);
                }
            }
            "interactionQuery" => {
                messages.push(ServerMessage::InteractionQuery(
                    decode_interaction_query_json(value),
                ));
            }
            // 服务端主动中止:立即失败,不等 idle 超时(对齐 proto 路径)
            "execServerControlMessage" => return Err(WireError::ServerAbort),
            // ttftBreakdown 等未知顶层字段:静默忽略
            _ => {}
        }
    }
    Ok(messages)
}

/// InteractionUpdate:单帧可同时携带多个更新,全部收集(对齐 proto 路径)
fn decode_interaction_json(value: &Value) -> Result<Vec<ServerMessage>, WireError> {
    let mut messages = Vec::new();
    let Some(fields) = value.as_object() else {
        return Ok(messages);
    };
    for (key, item) in fields {
        match key.as_str() {
            "textDelta" => {
                if let Some(text) = item.get("text").and_then(Value::as_str) {
                    messages.push(ServerMessage::TextDelta(text.to_string()));
                }
            }
            "thinkingDelta" => {
                if let Some(text) = item.get("text").and_then(Value::as_str) {
                    messages.push(ServerMessage::ThinkingDelta(text.to_string()));
                }
            }
            "thinkingCompleted" => messages.push(ServerMessage::ThinkingCompleted),
            "tokenDelta" => {
                if let Some(tokens) = item.get("tokens").and_then(int_field) {
                    messages.push(ServerMessage::TokenDelta(tokens));
                }
            }
            "heartbeat" => messages.push(ServerMessage::Heartbeat),
            "turnEnded" => messages.push(ServerMessage::TurnEnded(decode_turn_ended_json(item))),
            // messageStartedAtMs/stepCompleted/partialToolCall 等:静默忽略
            _ => {}
        }
    }
    Ok(messages)
}

/// turnEnded 用量:int64 字段按 protobuf JSON 惯例是字符串
fn decode_turn_ended_json(value: &Value) -> TurnUsage {
    TurnUsage {
        input_tokens: value.get("inputTokens").and_then(int_field),
        output_tokens: value.get("outputTokens").and_then(int_field),
        cache_read_tokens: value.get("cacheReadTokens").and_then(int_field),
        cache_write_tokens: value.get("cacheWriteTokens").and_then(int_field),
    }
}

/// ExecServerMessage:字段名对应 proto oneof 的 camelCase JSON 名。
/// 未知字段告警忽略(对齐 proto 路径的排查日志)
fn decode_exec_json(value: &Value) -> Result<ServerMessage, WireError> {
    let mut id = 0u32;
    let mut exec_id = String::new();
    let mut kind = ExecKind::Builtin { field_number: 0 };
    let Some(fields) = value.as_object() else {
        return Ok(ServerMessage::Exec(ExecRequest {
            exec_msg_id: id,
            exec_id,
            kind,
        }));
    };
    for (key, item) in fields {
        match key.as_str() {
            "id" => id = int_field(item).unwrap_or_default() as u32,
            "execId" => exec_id = item.as_str().unwrap_or_default().to_string(),
            "requestContextArgs" => kind = ExecKind::RequestContext,
            "mcpArgs" => kind = decode_mcp_json(item),
            "mcpStateExecArgs" => {
                kind = ExecKind::McpState {
                    server_identifiers: item
                        .get("serverIdentifiers")
                        .and_then(Value::as_array)
                        .map(|items| {
                            items
                                .iter()
                                .filter_map(Value::as_str)
                                .map(str::to_string)
                                .collect()
                        })
                        .unwrap_or_default(),
                };
            }
            "subagentArgs" => {
                kind = ExecKind::Subagent {
                    tool_call_id: item
                        .get("toolCallId")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                };
            }
            // spanContext/acceptHookAdditionalContexts 等元数据:静默忽略
            "spanContext" | "acceptHookAdditionalContexts" => {}
            other => {
                if let Some(number) = builtin_field_number(other) {
                    kind = ExecKind::Builtin {
                        field_number: number,
                    };
                } else {
                    tracing::warn!("Cursor exec JSON 消息未识别字段 {other}");
                }
            }
        }
    }
    Ok(ServerMessage::Exec(ExecRequest {
        exec_msg_id: id,
        exec_id,
        kind,
    }))
}

/// McpArgs:name(field 1)/args(field 2, map<string, Value>)/toolCallId(field 3)。
/// name 优先取 toolName(field 5)对齐 proto 路径的回退
fn decode_mcp_json(value: &Value) -> ExecKind {
    let mut name = value
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let tool_call_id = value
        .get("toolCallId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if name.is_empty() {
        if let Some(tool_name) = value.get("toolName").and_then(Value::as_str) {
            name = tool_name.to_string();
        }
    }
    let mut args = std::collections::BTreeMap::new();
    if let Some(entries) = value.get("args").and_then(Value::as_object) {
        for (key, item) in entries {
            args.insert(key.clone(), to_proto_value(item).encode_to_vec());
        }
    }
    ExecKind::Mcp {
        name,
        tool_call_id,
        args,
    }
}

/// 内置 exec 工具的 JSON 名到 proto 字段号(对齐 proto 路径 is_builtin 白名单)
fn builtin_field_number(name: &str) -> Option<u64> {
    match name {
        "shellArgs" => Some(2),
        "writeArgs" => Some(3),
        "deleteArgs" => Some(4),
        "grepArgs" => Some(5),
        "readArgs" => Some(7),
        "lsArgs" => Some(8),
        "diagnosticsArgs" => Some(9),
        "shellStreamArgs" => Some(14),
        "backgroundShellSpawnArgs" => Some(16),
        "listMcpResourcesExecArgs" => Some(17),
        "readMcpResourceExecArgs" => Some(18),
        "fetchArgs" => Some(20),
        "recordScreenArgs" => Some(21),
        "computerUseArgs" => Some(22),
        "writeShellStdinArgs" => Some(23),
        _ => None,
    }
}

/// KvServerMessage:getBlobArgs/setBlobArgs,blobId/blobData 是 base64
fn decode_kv_json(value: &Value) -> Result<Option<ServerMessage>, WireError> {
    let mut id = 0u32;
    let mut message = None;
    let Some(fields) = value.as_object() else {
        return Ok(None);
    };
    for (key, item) in fields {
        match key.as_str() {
            "id" => id = int_field(item).unwrap_or_default() as u32,
            "getBlobArgs" => {
                message = Some(ServerMessage::KvGet {
                    id,
                    blob_id: item
                        .get("blobId")
                        .and_then(bytes_field)
                        .ok_or_else(|| WireError::Json("getBlobArgs.blobId 缺失".into()))?,
                });
            }
            "setBlobArgs" => {
                message = Some(ServerMessage::KvSet {
                    id,
                    blob_id: item
                        .get("blobId")
                        .and_then(bytes_field)
                        .ok_or_else(|| WireError::Json("setBlobArgs.blobId 缺失".into()))?,
                    data: item
                        .get("blobData")
                        .and_then(bytes_field)
                        .unwrap_or_default(),
                });
            }
            // spanContext:静默忽略
            _ => {}
        }
    }
    Ok(message)
}

/// InteractionQuery:查询体字段名即种类(对齐 proto 路径的 oneof 字段号),
/// 查询体缺失时按 WebSearch 兜底
fn decode_interaction_query_json(value: &Value) -> InteractionQuery {
    let id = value.get("id").and_then(int_field).unwrap_or_default() as u32;
    let kind = value
        .as_object()
        .and_then(|fields| {
            fields.keys().find_map(|key| match key.as_str() {
                "webSearch" => Some(InteractionQueryKind::WebSearch),
                "askQuestion" => Some(InteractionQueryKind::AskQuestion),
                "switchMode" => Some(InteractionQueryKind::SwitchMode),
                "exaSearch" => Some(InteractionQueryKind::ExaSearch),
                "exaFetch" => Some(InteractionQueryKind::ExaFetch),
                "createPlan" => Some(InteractionQueryKind::CreatePlan),
                "setupVm" => Some(InteractionQueryKind::SetupVm),
                _ => None,
            })
        })
        .unwrap_or(InteractionQueryKind::WebSearch);
    InteractionQuery { id, kind }
}

/// conversationCheckpointUpdate 的 JSON 形态转 ConversationStateStructure
/// proto 字节(下一轮 runRequest 是 proto,必须重编码)。生产 JSON 携带
/// 生成表之外的字段(agentType/conversationStartedTimestampMs 等),属
/// 服务端元数据,丢弃并记 debug;模型上下文(root prompt + turns)完整保留
pub fn conversation_state_from_json(value: &Value) -> Result<Vec<u8>, WireError> {
    let mut state = generated::ConversationStateStructure::default();
    let Some(fields) = value.as_object() else {
        return Err(WireError::Json(
            "conversationCheckpointUpdate 不是对象".into(),
        ));
    };
    for (key, item) in fields {
        match key.as_str() {
            "rootPromptMessagesJson" => {
                state.root_prompt_messages_json = bytes_list(item, key)?;
            }
            "turnsOld" => state.turns_old = bytes_list(item, key)?,
            "turns" => state.turns = bytes_list(item, key)?,
            "todos" => state.todos = bytes_list(item, key)?,
            "pendingToolCalls" => {
                state.pending_tool_calls = string_list(item);
            }
            "tokenDetails" => {
                state.token_details = Some(generated::ConversationTokenDetails {
                    used_tokens: item
                        .get("usedTokens")
                        .and_then(int_field)
                        .unwrap_or_default() as u32,
                    max_tokens: item
                        .get("maxTokens")
                        .and_then(int_field)
                        .unwrap_or_default() as u32,
                });
            }
            "summary" => state.summary = bytes_field(item),
            "plan" => state.plan = bytes_field(item),
            "previousWorkspaceUris" => {
                state.previous_workspace_uris = string_list(item);
            }
            "mode" => state.mode = Some(agent_mode_value(item)),
            "summaryArchive" => state.summary_archive = bytes_field(item),
            "fileStates" => {
                state.file_states = bytes_map(item);
            }
            "summaryArchives" => state.summary_archives = bytes_list(item, key)?,
            "turnTimings" => {
                state.turn_timings = item
                    .as_array()
                    .map(|items| {
                        items
                            .iter()
                            .map(|entry| generated::StepTiming {
                                duration_ms: entry
                                    .get("durationMs")
                                    .and_then(int_field)
                                    .unwrap_or_default()
                                    as u64,
                                timestamp_ms: entry
                                    .get("timestampMs")
                                    .and_then(int_field)
                                    .unwrap_or_default()
                                    as u64,
                            })
                            .collect()
                    })
                    .unwrap_or_default();
            }
            "fileStatesV2" => {
                state.file_states_v2 = item
                    .as_object()
                    .map(|entries| {
                        entries
                            .iter()
                            .map(|(path, entry)| {
                                (
                                    path.clone(),
                                    generated::FileStateStructure {
                                        content: entry.get("content").and_then(bytes_field),
                                        initial_content: entry
                                            .get("initialContent")
                                            .and_then(bytes_field),
                                    },
                                )
                            })
                            .collect()
                    })
                    .unwrap_or_default();
            }
            "selfSummaryCount" => {
                state.self_summary_count = int_field(item).unwrap_or_default() as u32;
            }
            "readPaths" => state.read_paths = string_list(item),
            // ccextra 回 subagent error,会话不会出现 subagent 状态;出现即异常
            "subagentStates" => {
                tracing::warn!("Cursor checkpoint 携带 subagentStates,转换时丢弃");
            }
            other => {
                tracing::debug!("Cursor checkpoint JSON 未识别字段 {other},丢弃");
            }
        }
    }
    Ok(state.encode_to_vec())
}

/// repeated bytes:JSON 里是 base64 数组
fn bytes_list(value: &Value, key: &str) -> Result<Vec<Vec<u8>>, WireError> {
    value
        .as_array()
        .map(|items| items.iter().filter_map(bytes_field).collect())
        .ok_or_else(|| WireError::Json(format!("{key} 不是数组")))
}

fn string_list(value: &Value) -> Vec<String> {
    value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

fn bytes_map(value: &Value) -> std::collections::HashMap<String, Vec<u8>> {
    value
        .as_object()
        .map(|entries| {
            entries
                .iter()
                .filter_map(|(key, item)| bytes_field(item).map(|bytes| (key.clone(), bytes)))
                .collect()
        })
        .unwrap_or_default()
}

/// AgentMode:JSON 里是枚举名字符串或数字
fn agent_mode_value(value: &Value) -> i32 {
    match value {
        Value::Number(number) => number.as_i64().unwrap_or_default() as i32,
        Value::String(name) => match name.as_str() {
            "AGENT_MODE_UNSPECIFIED" => 0,
            "AGENT_MODE_AGENT" => 1,
            "AGENT_MODE_ASK" => 2,
            "AGENT_MODE_PLAN" => 3,
            "AGENT_MODE_DEBUG" => 4,
            "AGENT_MODE_TRIAGE" => 5,
            "AGENT_MODE_PROJECT" => 6,
            "AGENT_MODE_MULTITASK" => 7,
            _ => 0,
        },
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interaction_update_json_decodes_all_updates() {
        // 探针 E 活体帧形状:int64 用量是字符串,未知字段(messageStartedAtMs)忽略
        let data = br#"{"interactionUpdate":{"textDelta":{"text":"Got"},"tokenDelta":{"tokens":5},"messageStartedAtMs":"1791537541234"}}"#;
        let messages = decode_agent_server_message_json(data).unwrap();
        assert_eq!(
            messages,
            vec![
                ServerMessage::TextDelta("Got".into()),
                ServerMessage::TokenDelta(5),
            ]
        );
    }

    #[test]
    fn turn_ended_json_parses_string_int64_usage() {
        let data = br#"{"interactionUpdate":{"turnEnded":{"inputTokens":"11292","outputTokens":"58","cacheReadTokens":"11232","cacheWriteTokens":"0","reasoningTokens":"0"}}}"#;
        let messages = decode_agent_server_message_json(data).unwrap();
        assert_eq!(
            messages,
            vec![ServerMessage::TurnEnded(TurnUsage {
                input_tokens: Some(11292),
                output_tokens: Some(58),
                cache_read_tokens: Some(11232),
                cache_write_tokens: Some(0),
            })]
        );
    }

    #[test]
    fn kv_json_decodes_base64_blobs() {
        let blob = b"hello";
        let data = format!(
            r#"{{"kvServerMessage":{{"id":3,"setBlobArgs":{{"blobId":"{}","blobData":"{}"}}}}}}"#,
            STANDARD.encode(blob),
            STANDARD.encode(b"world")
        );
        let messages = decode_agent_server_message_json(data.as_bytes()).unwrap();
        assert_eq!(
            messages,
            vec![ServerMessage::KvSet {
                id: 3,
                blob_id: blob.to_vec(),
                data: b"world".to_vec(),
            }]
        );
    }

    #[test]
    fn exec_json_decodes_request_context_and_ignores_metadata() {
        let data = br#"{"execServerMessage":{"requestContextArgs":{"notesSessionId":"abc"},"spanContext":{"traceId":"t","spanId":"s","traceFlags":0},"acceptHookAdditionalContexts":false,"id":7,"execId":"exec-7"}}"#;
        let messages = decode_agent_server_message_json(data).unwrap();
        assert_eq!(
            messages,
            vec![ServerMessage::Exec(ExecRequest {
                exec_msg_id: 7,
                exec_id: "exec-7".into(),
                kind: ExecKind::RequestContext,
            })]
        );
    }

    #[test]
    fn mcp_args_json_roundtrips_through_proto_value() {
        let data = br#"{"execServerMessage":{"mcpArgs":{"name":"proxy-lookup","toolCallId":"call-1","args":{"path":"/tmp/notes.txt","limit":7,"flag":true}},"id":9,"execId":"exec-9"}}"#;
        let messages = decode_agent_server_message_json(data).unwrap();
        let ServerMessage::Exec(exec) = &messages[0] else {
            panic!("expected exec");
        };
        let ExecKind::Mcp {
            name,
            tool_call_id,
            args,
        } = &exec.kind
        else {
            panic!("expected mcp");
        };
        assert_eq!(name, "proxy-lookup");
        assert_eq!(tool_call_id, "call-1");
        // args 值是 ProtoValue 编码,decode_mcp_args 应还原 JSON 语义
        let decoded = crate::convert::cursor::decode_mcp_args(args).unwrap();
        assert_eq!(decoded["path"], "/tmp/notes.txt");
        assert_eq!(decoded["limit"], 7);
        assert_eq!(decoded["flag"], true);
    }

    #[test]
    fn checkpoint_json_converts_to_proto_state() {
        // 探针 E 活体 checkpoint 形状;agentType 等生成表外字段丢弃
        let root_id = STANDARD.encode([1u8; 32]);
        let turn_id = STANDARD.encode([2u8; 32]);
        let data = format!(
            r#"{{"conversationCheckpointUpdate":{{"rootPromptMessagesJson":["{root_id}"],"turns":["{turn_id}"],"pendingToolCalls":["{{\"id\":\"1\"}}"],"tokenDetails":{{"usedTokens":11350,"maxTokens":200000,"breakdown":{{}}}},"mode":"AGENT_MODE_AGENT","agentType":"cli","conversationStartedTimestampMs":"1791537538495","conversationStartedTimeZone":"UTC","recentUserMessageIdsOlderTurnCount":0}}}}"#
        );
        let messages = decode_agent_server_message_json(data.as_bytes()).unwrap();
        let ServerMessage::Checkpoint(raw) = &messages[0] else {
            panic!("expected checkpoint");
        };
        let state = generated::ConversationStateStructure::decode(raw.0.as_slice()).unwrap();
        assert_eq!(state.root_prompt_messages_json, vec![vec![1u8; 32]]);
        assert_eq!(state.turns, vec![vec![2u8; 32]]);
        assert_eq!(state.pending_tool_calls, vec!["{\"id\":\"1\"}".to_string()]);
        assert_eq!(
            state.token_details,
            Some(generated::ConversationTokenDetails {
                used_tokens: 11350,
                max_tokens: 200000,
            })
        );
        assert_eq!(state.mode, Some(1));
    }

    #[test]
    fn interaction_query_and_control_json_decode() {
        let messages =
            decode_agent_server_message_json(br#"{"interactionQuery":{"id":4,"webSearch":{}}}"#)
                .unwrap();
        assert_eq!(
            messages,
            vec![ServerMessage::InteractionQuery(InteractionQuery {
                id: 4,
                kind: InteractionQueryKind::WebSearch,
            })]
        );
        assert_eq!(
            decode_agent_server_message_json(br#"{"execServerControlMessage":{}}"#).unwrap_err(),
            WireError::ServerAbort
        );
    }

    #[test]
    fn unknown_top_level_fields_are_ignored() {
        // 探针 E 帧携带 ttftBreakdown 顶层字段
        let data = br#"{"interactionUpdate":{"heartbeat":{}},"ttftBreakdown":{"ttftMs":"12"}}"#;
        let messages = decode_agent_server_message_json(data).unwrap();
        assert_eq!(messages, vec![ServerMessage::Heartbeat]);
    }
}
