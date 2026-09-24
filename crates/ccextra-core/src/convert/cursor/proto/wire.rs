use thiserror::Error;

#[derive(Debug, Error, PartialEq, Eq)]
pub enum WireError {
    #[error("protobuf varint truncated")]
    VarintTruncated,
    #[error("protobuf varint overflow")]
    VarintOverflow,
    #[error("protobuf field {0} has invalid wire type")]
    InvalidWireType(u64),
    #[error("protobuf field number is zero")]
    ZeroFieldNumber,
    #[error("protobuf length exceeds remaining bytes")]
    LengthOverflow,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Field<'a> {
    Varint { number: u64, value: u64 },
    Bytes { number: u64, value: &'a [u8] },
    Fixed64 { number: u64, value: &'a [u8; 8] },
    Fixed32 { number: u64, value: &'a [u8; 4] },
}

pub fn encode_varint(mut value: u64, out: &mut Vec<u8>) {
    while value >= 0x80 {
        out.push((value as u8) | 0x80);
        value >>= 7;
    }
    out.push(value as u8);
}

pub fn encode_tag(number: u64, wire_type: u8, out: &mut Vec<u8>) {
    encode_varint((number << 3) | u64::from(wire_type), out);
}

pub fn encode_bytes(number: u64, value: &[u8], out: &mut Vec<u8>) {
    encode_tag(number, 2, out);
    encode_varint(value.len() as u64, out);
    out.extend_from_slice(value);
}

pub fn decode_fields(mut data: &[u8]) -> Result<Vec<Field<'_>>, WireError> {
    let mut fields = Vec::new();
    while !data.is_empty() {
        let tag = take_varint(&mut data)?;
        let number = tag >> 3;
        let wire_type = (tag & 7) as u8;
        if number == 0 {
            return Err(WireError::ZeroFieldNumber);
        }
        match wire_type {
            0 => fields.push(Field::Varint {
                number,
                value: take_varint(&mut data)?,
            }),
            1 => {
                if data.len() < 8 {
                    return Err(WireError::LengthOverflow);
                }
                let value = data[..8].try_into().expect("checked fixed width");
                data = &data[8..];
                fields.push(Field::Fixed64 { number, value });
            }
            2 => {
                let len = take_varint(&mut data)? as usize;
                if len > data.len() {
                    return Err(WireError::LengthOverflow);
                }
                let value = &data[..len];
                data = &data[len..];
                fields.push(Field::Bytes { number, value });
            }
            5 => {
                if data.len() < 4 {
                    return Err(WireError::LengthOverflow);
                }
                let value = data[..4].try_into().expect("checked fixed width");
                data = &data[4..];
                fields.push(Field::Fixed32 { number, value });
            }
            other => return Err(WireError::InvalidWireType(u64::from(other))),
        }
    }
    Ok(fields)
}

fn take_varint(data: &mut &[u8]) -> Result<u64, WireError> {
    let mut value = 0u64;
    for shift in (0..70).step_by(7) {
        let byte = *data.first().ok_or(WireError::VarintTruncated)?;
        *data = &data[1..];
        if shift == 63 && byte > 1 {
            return Err(WireError::VarintOverflow);
        }
        value |= u64::from(byte & 0x7f) << shift;
        if byte & 0x80 == 0 {
            return Ok(value);
        }
    }
    Err(WireError::VarintOverflow)
}
