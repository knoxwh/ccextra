use super::{nested, ServerMessage};
use crate::convert::cursor::proto::wire::{decode_fields, Field, WireError};

const INTERACTION_UPDATE: u64 = 1;
const EXEC_SERVER_MESSAGE: u64 = 2;
const CONVERSATION_CHECKPOINT: u64 = 3;
const KV_SERVER_MESSAGE: u64 = 4;
/// AgentServerMessage field 5:ExecServerControlMessage(服务端主动中止)
const EXEC_SERVER_CONTROL: u64 = 5;
/// AgentServerMessage field 7:InteractionQuery(服务端交互查询)
const INTERACTION_QUERY: u64 = 7;

pub fn decode_agent_server_message(data: &[u8]) -> Result<Vec<ServerMessage>, WireError> {
    let mut messages = Vec::new();
    for field in decode_fields(data)? {
        let Field::Bytes { number, value } = field else {
            continue;
        };
        match number {
            INTERACTION_UPDATE => {
                messages.extend(nested::decode_interaction(value)?);
            }
            EXEC_SERVER_MESSAGE => messages.push(ServerMessage::Exec(nested::decode_exec(value)?)),
            CONVERSATION_CHECKPOINT => {
                messages.push(ServerMessage::Checkpoint(super::RawCheckpoint(
                    value.to_vec(),
                )));
            }
            KV_SERVER_MESSAGE => messages.push(nested::decode_kv(value)?),
            INTERACTION_QUERY => messages.push(ServerMessage::InteractionQuery(
                nested::decode_interaction_query(value)?,
            )),
            EXEC_SERVER_CONTROL => {
                // 服务端主动中止:立即失败,不等 90s idle 超时
                return Err(WireError::ServerAbort);
            }
            _ => {}
        }
    }
    Ok(messages)
}

pub(crate) fn fields(data: &[u8]) -> Result<Vec<Field<'_>>, WireError> {
    decode_fields(data)
}
