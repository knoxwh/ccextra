mod parts;

use super::{ExecKind, ExecRequest, InteractionQuery, InteractionQueryKind, ServerMessage};
use crate::convert::cursor::proto::raw_wire::decode::fields;
use crate::convert::cursor::proto::wire::{Field, WireError};

/// 解析 InteractionUpdate:收集全部字段(单帧可同时携带 text delta 与
/// token delta,不可 last-field-wins 丢弃)。field 7 是 partial_tool_call
/// (工具参数流式增量),当前不消费,静默忽略
pub fn decode_interaction(data: &[u8]) -> Result<Vec<ServerMessage>, WireError> {
    let mut messages = Vec::new();
    for field in fields(data)? {
        let Field::Bytes { number, value } = field else {
            continue;
        };
        match number {
            1 => messages.push(ServerMessage::TextDelta(parts::string_field(value, 1)?)),
            4 => messages.push(ServerMessage::ThinkingDelta(parts::string_field(value, 1)?)),
            5 => messages.push(ServerMessage::ThinkingCompleted),
            8 => messages.push(ServerMessage::TokenDelta(
                parts::varint_field(value, 1)? as i64,
            )),
            13 => messages.push(ServerMessage::Heartbeat),
            14 => messages.push(ServerMessage::TurnEnded(parts::decode_turn_ended(value)?)),
            _ => {}
        }
    }
    Ok(messages)
}

/// 解析 AgentServerMessage field 7 InteractionQuery:id(field 1)+
/// oneof 查询体(字段号即种类);查询体缺失时按 WebSearch 兜底,
/// 回复按 id 配对,服务端不依赖种类
pub fn decode_interaction_query(data: &[u8]) -> Result<InteractionQuery, WireError> {
    let mut id = 0u32;
    let mut kind = None;
    for field in fields(data)? {
        match field {
            Field::Varint { number: 1, value } => id = value as u32,
            Field::Bytes { number: 2, .. } => kind = Some(InteractionQueryKind::WebSearch),
            Field::Bytes { number: 3, .. } => kind = Some(InteractionQueryKind::AskQuestion),
            Field::Bytes { number: 4, .. } => kind = Some(InteractionQueryKind::SwitchMode),
            Field::Bytes { number: 5, .. } => kind = Some(InteractionQueryKind::ExaSearch),
            Field::Bytes { number: 6, .. } => kind = Some(InteractionQueryKind::ExaFetch),
            Field::Bytes { number: 7, .. } => kind = Some(InteractionQueryKind::CreatePlan),
            Field::Bytes { number: 8, .. } => kind = Some(InteractionQueryKind::SetupVm),
            _ => {}
        }
    }
    Ok(InteractionQuery {
        id,
        kind: kind.unwrap_or(InteractionQueryKind::WebSearch),
    })
}

pub fn decode_exec(data: &[u8]) -> Result<ExecRequest, WireError> {
    let mut id = 0;
    let mut exec_id = String::new();
    let mut kind = ExecKind::Builtin { field_number: 0 };
    for field in fields(data)? {
        match field {
            Field::Varint { number: 1, value } => id = value as u32,
            Field::Bytes { number: 15, value } => {
                exec_id = String::from_utf8_lossy(value).into_owned()
            }
            Field::Bytes { number: 10, .. } => kind = ExecKind::RequestContext,
            Field::Bytes { number: 11, value } => kind = parts::decode_mcp(value)?,
            Field::Bytes { number: 36, value } => {
                // McpStateExecArgs:repeated string server_identifiers(field 1)
                let mut server_identifiers = Vec::new();
                for field in fields(value)? {
                    if let Field::Bytes { number: 1, value } = field {
                        server_identifiers.push(String::from_utf8_lossy(value).into_owned());
                    }
                }
                kind = ExecKind::McpState { server_identifiers };
            }
            Field::Bytes { number: 28, value } => {
                // SubagentArgs:tool_call_id(field 1)
                let mut tool_call_id = String::new();
                for field in fields(value)? {
                    if let Field::Bytes { number: 1, value } = field {
                        tool_call_id = String::from_utf8_lossy(value).into_owned();
                    }
                }
                kind = ExecKind::Subagent { tool_call_id };
            }
            Field::Bytes { number, .. } if parts::is_builtin(number) => {
                kind = ExecKind::Builtin {
                    field_number: number,
                }
            }
            Field::Bytes { number: 19, .. } => {
                // span_context:链路追踪元数据,已知字段,静默忽略
            }
            Field::Bytes { number, value } => {
                // 未知 exec 字段:记录字段号与内容便于排查上游新增的内置工具
                tracing::warn!(
                    "Cursor exec 消息未识别字段 {number} 内容 {:02x?}",
                    &value[..value.len().min(64)]
                );
            }
            _ => {}
        }
    }
    Ok(ExecRequest {
        exec_msg_id: id,
        exec_id,
        kind,
    })
}

pub fn decode_kv(data: &[u8]) -> Result<ServerMessage, WireError> {
    let mut id = 0;
    let mut kind = None;
    let mut blob_id = Vec::new();
    let mut blob_data = Vec::new();
    for field in fields(data)? {
        match field {
            Field::Varint { number: 1, value } => id = value as u32,
            Field::Bytes { number: 2, value } => {
                kind = Some(false);
                blob_id = parts::bytes_field(value, 1)?;
            }
            Field::Bytes { number: 3, value } => {
                kind = Some(true);
                blob_id = parts::bytes_field(value, 1)?;
                blob_data = parts::bytes_field(value, 2)?;
            }
            _ => {}
        }
    }
    Ok(match kind {
        Some(false) => ServerMessage::KvGet { id, blob_id },
        Some(true) => ServerMessage::KvSet {
            id,
            blob_id,
            data: blob_data,
        },
        None => ServerMessage::Heartbeat,
    })
}
