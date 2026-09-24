mod parts;

use super::{ExecKind, ExecRequest, ServerMessage};
use crate::convert::cursor::proto::raw_wire::decode::fields;
use crate::convert::cursor::proto::wire::{Field, WireError};

pub fn decode_interaction(data: &[u8]) -> Result<Option<ServerMessage>, WireError> {
    let mut result = None;
    for field in fields(data)? {
        let Field::Bytes { number, value } = field else {
            continue;
        };
        result = match number {
            1 => Some(ServerMessage::TextDelta(parts::string_field(value, 1)?)),
            4 => Some(ServerMessage::ThinkingDelta(parts::string_field(value, 1)?)),
            5 => Some(ServerMessage::ThinkingCompleted),
            8 => Some(ServerMessage::TokenDelta(
                parts::varint_field(value, 1)? as i64
            )),
            13 => Some(ServerMessage::Heartbeat),
            14 => Some(ServerMessage::TurnEnded),
            _ => result,
        };
    }
    Ok(result)
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
            Field::Bytes { number, .. } if parts::is_builtin(number) => {
                kind = ExecKind::Builtin {
                    field_number: number,
                }
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
