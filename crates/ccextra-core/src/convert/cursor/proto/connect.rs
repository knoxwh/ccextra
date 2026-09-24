use flate2::read::GzDecoder;
use std::io::Read;
use thiserror::Error;

pub const CONNECT_COMPRESSION_FLAG: u8 = 0x01;
pub const CONNECT_END_STREAM_FLAG: u8 = 0x02;
pub const CONNECT_FRAME_HEADER_SIZE: usize = 5;
pub const DEFAULT_MAX_FRAME_SIZE: usize = 16 * 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConnectFrame {
    pub flags: u8,
    pub payload: Vec<u8>,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum ConnectFrameError {
    #[error("Connect frame payload exceeds limit")]
    FrameTooLarge,
    #[error("Connect frame length overflows usize")]
    LengthOverflow,
    #[error("Connect frame compression decode failed")]
    Compression,
    #[error("Connect frame decompressed payload exceeds limit")]
    DecompressedTooLarge,
}

impl ConnectFrame {
    pub fn encode(payload: &[u8], flags: u8) -> Result<Vec<u8>, ConnectFrameError> {
        if payload.len() > u32::MAX as usize {
            return Err(ConnectFrameError::FrameTooLarge);
        }
        let mut frame = Vec::with_capacity(CONNECT_FRAME_HEADER_SIZE + payload.len());
        frame.push(flags);
        frame.extend_from_slice(&(payload.len() as u32).to_be_bytes());
        frame.extend_from_slice(payload);
        Ok(frame)
    }

    pub fn decoded_payload(&self, max_size: usize) -> Result<Vec<u8>, ConnectFrameError> {
        if self.flags & CONNECT_COMPRESSION_FLAG == 0 {
            if self.payload.len() > max_size {
                return Err(ConnectFrameError::FrameTooLarge);
            }
            return Ok(self.payload.clone());
        }
        let decoder = GzDecoder::new(self.payload.as_slice());
        let mut output = Vec::new();
        decoder
            .take((max_size as u64).saturating_add(1))
            .read_to_end(&mut output)
            .map_err(|_| ConnectFrameError::Compression)?;
        if output.len() > max_size {
            return Err(ConnectFrameError::DecompressedTooLarge);
        }
        Ok(output)
    }
}

pub struct ConnectFrameDecoder {
    buffer: Vec<u8>,
    max_frame_size: usize,
}

impl ConnectFrameDecoder {
    pub fn new(max_frame_size: usize) -> Self {
        Self {
            buffer: Vec::new(),
            max_frame_size,
        }
    }

    pub fn push(&mut self, data: &[u8]) -> Result<Vec<ConnectFrame>, ConnectFrameError> {
        self.buffer.extend_from_slice(data);
        let mut frames = Vec::new();
        loop {
            if self.buffer.len() < CONNECT_FRAME_HEADER_SIZE {
                break;
            }
            let flags = self.buffer[0];
            let len = u32::from_be_bytes(self.buffer[1..5].try_into().unwrap()) as usize;
            if len > self.max_frame_size {
                return Err(ConnectFrameError::FrameTooLarge);
            }
            let total = CONNECT_FRAME_HEADER_SIZE
                .checked_add(len)
                .ok_or(ConnectFrameError::LengthOverflow)?;
            if self.buffer.len() < total {
                break;
            }
            let payload = self.buffer[5..total].to_vec();
            self.buffer.drain(..total);
            frames.push(ConnectFrame { flags, payload });
        }
        Ok(frames)
    }

    pub fn is_empty(&self) -> bool {
        self.buffer.is_empty()
    }
}
