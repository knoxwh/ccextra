use super::{nested, ServerMessage};
use crate::convert::cursor::proto::wire::{decode_fields, Field, WireError};

const INTERACTION_UPDATE: u64 = 1;
const EXEC_SERVER_MESSAGE: u64 = 2;
const CONVERSATION_CHECKPOINT: u64 = 3;
const KV_SERVER_MESSAGE: u64 = 4;

pub fn decode_agent_server_message(data: &[u8]) -> Result<Vec<ServerMessage>, WireError> {
    let mut messages = Vec::new();
    for field in decode_fields(data)? {
        let Field::Bytes { number, value } = field else {
            continue;
        };
        match number {
            INTERACTION_UPDATE => {
                if let Some(message) = nested::decode_interaction(value)? {
                    messages.push(message);
                }
            }
            EXEC_SERVER_MESSAGE => messages.push(ServerMessage::Exec(nested::decode_exec(value)?)),
            CONVERSATION_CHECKPOINT => {
                messages.push(ServerMessage::Checkpoint(super::RawCheckpoint(
                    value.to_vec(),
                )));
            }
            KV_SERVER_MESSAGE => messages.push(nested::decode_kv(value)?),
            _ => {}
        }
    }
    Ok(messages)
}

pub(crate) fn fields(data: &[u8]) -> Result<Vec<Field<'_>>, WireError> {
    decode_fields(data)
}
