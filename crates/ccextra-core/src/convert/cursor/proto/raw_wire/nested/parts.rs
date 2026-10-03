use super::super::{ExecKind, TurnUsage};
use crate::convert::cursor::proto::raw_wire::decode::fields;
use crate::convert::cursor::proto::wire::{Field, WireError};
use std::collections::BTreeMap;

pub fn string_field(data: &[u8], wanted: u64) -> Result<String, WireError> {
    for field in fields(data)? {
        if let Field::Bytes { number, value } = field {
            if number == wanted {
                return Ok(String::from_utf8_lossy(value).into_owned());
            }
        }
    }
    Ok(String::new())
}

pub fn bytes_field(data: &[u8], wanted: u64) -> Result<Vec<u8>, WireError> {
    for field in fields(data)? {
        if let Field::Bytes { number, value } = field {
            if number == wanted {
                return Ok(value.to_vec());
            }
        }
    }
    Ok(Vec::new())
}

pub fn varint_field(data: &[u8], wanted: u64) -> Result<u64, WireError> {
    for field in fields(data)? {
        if let Field::Varint { number, value } = field {
            if number == wanted {
                return Ok(value);
            }
        }
    }
    Ok(0)
}

pub fn decode_mcp(data: &[u8]) -> Result<ExecKind, WireError> {
    let mut name = String::new();
    let mut tool_call_id = String::new();
    let mut args = BTreeMap::new();
    for field in fields(data)? {
        match field {
            Field::Bytes { number: 1, value } => name = String::from_utf8_lossy(value).into_owned(),
            Field::Bytes { number: 2, value } => {
                let mut key = String::new();
                let mut item = Vec::new();
                for entry in fields(value)? {
                    match entry {
                        Field::Bytes { number: 1, value } => {
                            key = String::from_utf8_lossy(value).into_owned()
                        }
                        Field::Bytes { number: 2, value } => item = value.to_vec(),
                        _ => {}
                    }
                }
                if !key.is_empty() {
                    args.insert(key, item);
                }
            }
            Field::Bytes { number: 3, value } => {
                tool_call_id = String::from_utf8_lossy(value).into_owned()
            }
            Field::Bytes { number: 5, value } if name.is_empty() => {
                name = String::from_utf8_lossy(value).into_owned()
            }
            _ => {}
        }
    }
    Ok(ExecKind::Mcp {
        name,
        tool_call_id,
        args,
    })
}

/// `TurnEndedUpdate` 在 agent.proto 里是空消息。Plus 只把它当生成结束信号。
/// 子字段里的 varint 先不当 token:空帧与带未知字段的帧都返回全 None,
/// 避免把以后的协议字段误读成用量。仍校验 payload 是合法 protobuf。
pub fn decode_turn_ended(data: &[u8]) -> Result<TurnUsage, WireError> {
    let _ = fields(data)?;
    Ok(TurnUsage::default())
}

pub fn is_builtin(number: u64) -> bool {
    matches!(
        number,
        2 | 3 | 4 | 5 | 7 | 8 | 9 | 14 | 16 | 17 | 18 | 20 | 21 | 22 | 23
    )
}
