use super::proto::{encode_bytes, generated};
use super::{input, schema, CursorConvertError};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use prost::Message;
use serde_json::Value;
use sha2::{Digest, Sha256};
use std::collections::HashMap;

pub struct CursorRunRequest {
    pub payload: Vec<u8>,
    pub blob_store: HashMap<String, Vec<u8>>,
    pub mcp_tools: Vec<generated::McpToolDefinition>,
}

pub fn conversation_id(identity: &str, session_id: &str) -> String {
    hex::encode(Sha256::digest(
        format!("cursor-conv:{identity}:{session_id}").as_bytes(),
    ))
}

/// 图片 uuid(对齐 Plus generateId 的 32 位 hex;确定性派生自 message_id 与序号)
fn image_uuid(message_id: &str, index: usize) -> String {
    hex::encode(&Sha256::digest(format!("{message_id}:{index}").as_bytes())[..16])
}

pub fn build_run_request(
    body: &Value,
    model: &str,
    conversation_id: &str,
    message_id: &str,
    checkpoint: Option<&[u8]>,
) -> Result<CursorRunRequest, CursorConvertError> {
    if model.is_empty() || conversation_id.is_empty() || message_id.is_empty() {
        return Err(CursorConvertError::Invalid(
            "model、conversation_id 和 message_id 不能为空".into(),
        ));
    }
    let input::UserInput {
        text,
        system,
        images,
    } = input::user_text(body, checkpoint.is_some())?;
    if text.is_empty() {
        return Err(CursorConvertError::Invalid("缺少用户消息".into()));
    }
    // 对齐 Plus:图片进 SelectedContext.selected_images(base64 解码为 bytes)
    let mut decoded_images = Vec::with_capacity(images.len());
    for (index, image) in images.iter().enumerate() {
        let data = STANDARD.decode(image.data.as_bytes()).map_err(|error| {
            CursorConvertError::Invalid(format!("图片 base64 解码失败: {error}"))
        })?;
        decoded_images.push((image_uuid(message_id, index), image.mime_type.clone(), data));
    }
    let selected_context = if decoded_images.is_empty() {
        None
    } else {
        Some(generated::SelectedContext {
            selected_images: decoded_images
                .into_iter()
                .map(|(uuid, mime_type, data)| generated::SelectedImage {
                    data_or_blob_id: Some(generated::selected_image::DataOrBlobId::Data(data)),
                    uuid,
                    mime_type,
                    ..Default::default()
                })
                .collect(),
            ..Default::default()
        })
    };
    let action = generated::ConversationAction {
        action: Some(generated::conversation_action::Action::UserMessageAction(
            generated::UserMessageAction {
                user_message: Some(generated::UserMessage {
                    text,
                    message_id: message_id.into(),
                    selected_context,
                    ..Default::default()
                }),
                ..Default::default()
            },
        )),
    };
    let model_details = generated::ModelDetails {
        model_id: model.into(),
        display_model_id: model.into(),
        display_name: model.into(),
        ..Default::default()
    };
    let tools = schema::tools(body)?;
    let mut blobs = HashMap::new();
    let state = match checkpoint {
        Some(raw) => raw.to_vec(),
        None => {
            // 对齐 Plus:真 system 进 root blob(键序 content 先,对齐 Go json.Marshal
            // 字母序;blob id 是 sha256,需字节稳定),UserText 只留用户输入
            let system = serde_json::json!({ "content": system, "role": "system" });
            let bytes = serde_json::to_vec(&system)?;
            let digest = Sha256::digest(&bytes);
            blobs.insert(hex::encode(digest), bytes);
            generated::ConversationStateStructure {
                root_prompt_messages_json: vec![digest.to_vec()],
                ..Default::default()
            }
            .encode_to_vec()
        }
    };
    let mut run = Vec::new();
    encode_bytes(1, &state, &mut run);
    encode_bytes(2, &action.encode_to_vec(), &mut run);
    encode_bytes(3, &model_details.encode_to_vec(), &mut run);
    if !tools.is_empty() {
        encode_bytes(
            4,
            &generated::McpTools {
                mcp_tools: tools.clone(),
            }
            .encode_to_vec(),
            &mut run,
        );
    }
    encode_bytes(5, conversation_id.as_bytes(), &mut run);
    let mut payload = Vec::new();
    encode_bytes(1, &run, &mut payload);
    Ok(CursorRunRequest {
        payload,
        blob_store: blobs,
        mcp_tools: tools,
    })
}
