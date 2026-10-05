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

/// `TurnEndedUpdate` 解析 token usage 字段（逆向工程）。
/// 根据实测日志与官方 SDK TokenUsage 接口推断字段映射：
/// - 字段 1: 可能是 input_tokens（但值异常大，需验证）
/// - 字段 2: output_tokens（与实际输出 token 数匹配）
/// - 字段 3: cache_read_tokens
/// - 字段 4: cache_write_tokens
/// - 字段 5/6: 未知（可能是内部元数据或时间戳）
pub fn decode_turn_ended(data: &[u8]) -> Result<TurnUsage, WireError> {
    use crate::convert::cursor::proto::wire::Field;

    let fields_result = fields(data)?;
    let mut usage = TurnUsage::default();

    for field in &fields_result {
        if let Field::Varint { number, value } = field {
            match number {
                1 => {
                    // 字段 1 值异常大（如 11732），可能是累计或其他含义。
                    // 暂不使用，避免误报。待验证后启用。
                    tracing::debug!("TurnEnded 字段 1 (疑似 input): {}", value);
                }
                2 => {
                    usage.output_tokens = Some(*value as i64);
                    tracing::debug!("TurnEnded output_tokens: {}", value);
                }
                3 => {
                    if *value > 0 {
                        usage.cache_read_tokens = Some(*value as i64);
                        tracing::info!("TurnEnded cache_read_tokens: {}", value);
                    }
                }
                4 => {
                    if *value > 0 {
                        usage.cache_write_tokens = Some(*value as i64);
                        tracing::info!("TurnEnded cache_write_tokens: {}", value);
                    }
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
