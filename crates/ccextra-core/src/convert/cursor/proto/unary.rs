use super::{
    generated::GetUsableModelsRequest, generated::GetUsableModelsResponse,
    parse_connect_end_stream, ConnectFrameDecoder, ConnectFrameError, TrailerError,
    CONNECT_COMPRESSION_FLAG, CONNECT_END_STREAM_FLAG, DEFAULT_MAX_FRAME_SIZE,
};
use prost::Message;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum UnaryError {
    #[error(transparent)]
    Frame(#[from] ConnectFrameError),
    #[error(transparent)]
    Trailer(#[from] TrailerError),
    #[error("Cursor Connect error {code}: {message}")]
    Remote { code: String, message: String },
    #[error("unary response has no protobuf data frame")]
    MissingResponse,
    #[error("unary response has multiple protobuf data frames")]
    MultipleResponses,
    #[error(transparent)]
    Decode(#[from] prost::DecodeError),
}

pub fn encode_get_usable_models_request(custom_model_ids: &[String]) -> Vec<u8> {
    GetUsableModelsRequest {
        custom_model_ids: custom_model_ids.to_vec(),
    }
    .encode_to_vec()
}

pub fn decode_get_usable_models_response(
    data: &[u8],
) -> Result<GetUsableModelsResponse, UnaryError> {
    let payload = extract_response_payload(data)?;
    Ok(GetUsableModelsResponse::decode(payload.as_slice())?)
}

fn extract_response_payload(data: &[u8]) -> Result<Vec<u8>, UnaryError> {
    if data.len() < 5 || data[0] & !(CONNECT_END_STREAM_FLAG | CONNECT_COMPRESSION_FLAG) != 0 {
        return Ok(data.to_vec());
    }
    let mut decoder = ConnectFrameDecoder::new(DEFAULT_MAX_FRAME_SIZE);
    let frames = decoder.push(data)?;
    if frames.is_empty() || !decoder.is_empty() {
        return Ok(data.to_vec());
    }

    let mut response = None;
    for frame in frames {
        if frame.flags & CONNECT_END_STREAM_FLAG != 0 {
            if let Some(error) = parse_connect_end_stream(&frame.payload)? {
                return Err(UnaryError::Remote {
                    code: error.code,
                    message: error.message,
                });
            }
            continue;
        }
        if response.is_some() {
            return Err(UnaryError::MultipleResponses);
        }
        response = Some(frame.decoded_payload(DEFAULT_MAX_FRAME_SIZE)?);
    }
    response.ok_or(UnaryError::MissingResponse)
}
