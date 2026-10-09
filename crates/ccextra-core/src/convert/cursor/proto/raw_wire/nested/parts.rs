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

/// `TurnEndedUpdate` 解析 token usage 字段。
/// 字段号来自 @cursor/sdk 1.0.32 protobuf-es type info:
/// 1 input_tokens|2 output_tokens|3 cache_read_tokens|4 cache_write_tokens|5 reasoning_tokens。
/// 实测 field 1 是本轮完整输入(≈ context 大小);field 3/4 全量解码,
/// 是否采用由 CursorSse 自校验(见 TurnEnded 处理)。
pub fn decode_turn_ended(data: &[u8]) -> Result<TurnUsage, WireError> {
    use crate::convert::cursor::proto::wire::Field;

    let fields_result = fields(data)?;
    let mut usage = TurnUsage::default();

    for field in &fields_result {
        if let Field::Varint { number, value } = field {
            match number {
                1 => {
                    usage.input_tokens = Some(*value as i64);
                    tracing::info!("TurnEnded input_tokens: {}", value);
                }
                2 => {
                    usage.output_tokens = Some(*value as i64);
                    tracing::debug!("TurnEnded output_tokens: {}", value);
                }
                3 => {
                    usage.cache_read_tokens = Some(*value as i64);
                    tracing::debug!("TurnEnded cache_read_tokens: {}", value);
                }
                4 => {
                    usage.cache_write_tokens = Some(*value as i64);
                    tracing::debug!("TurnEnded cache_write_tokens: {}", value);
                }
                _ => {
                    tracing::debug!("TurnEnded 未知字段 {}: {}", number, value);
                }
            }
        }
    }

    Ok(usage)
}

pub fn is_builtin(number: u64) -> bool {
    matches!(
        number,
        2 | 3 | 4 | 5 | 7 | 8 | 9 | 14 | 16 | 17 | 18 | 20 | 21 | 22 | 23
    )
}
