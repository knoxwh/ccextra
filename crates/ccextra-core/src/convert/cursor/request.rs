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

/// model 名固定参数段拆分:`id:k=v,k=v` → (base, 参数对);无段时原样返回
fn split_pinned_params(model: &str) -> (&str, Vec<(&str, &str)>) {
    match model.split_once(':') {
        None => (model, Vec::new()),
        Some((base, tail)) => {
            let params = tail
                .split(',')
                .filter_map(|pair| pair.split_once('='))
                .filter(|(k, v)| !k.is_empty() && !v.is_empty())
                .collect();
            (base, params)
        }
    }
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
        ..
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
    // model 名可带白名单固定参数段(catalog pinned params);家族变体条目
    // 额外携带 effort_levels 标记:effort 钳制后拼进 model id(上游目录以
    // base-level 形态发布变体,裸 base 会被上游当 default/auto 处理)
    let (model_base, pinned) = split_pinned_params(model);
    let effort_levels: Vec<&str> = pinned
        .iter()
        .find(|(id, _)| *id == "effort_levels")
        .map(|(_, value)| value.split('+').collect())
        .unwrap_or_default();
    // 家族缺省等级:目录无裸 base 条目时,无 effort 的请求也必须拼等级,
    // 否则上游 not_found(实测 muse-spark-1.3 裸 id 被拒)
    let effort_default = pinned
        .iter()
        .find(|(id, _)| *id == "effort_default")
        .map(|(_, value)| *value);
    let pinned_effort = pinned
        .iter()
        .find(|(id, _)| *id == "reasoning_effort")
        .map(|(_, value)| *value)
        // 钉 auto 等于不钉:不钳制、不拼接,回退 body effort
        .filter(|value| *value != "auto");
    // body effort 走全局解析:thinking.budget_tokens / adaptive / output_config 全覆盖;
    // 顶层 reasoning_effort(OpenAI 风格入站)作回退,auto 不干预
    let body_effort = crate::thinking::resolve_effort_from_body(body)
        .filter(|e| *e != "auto")
        .or_else(|| {
            body.get("reasoning_effort")
                .and_then(Value::as_str)
                .and_then(crate::thinking::Level::parse)
                .filter(|level| *level != crate::thinking::Level::Auto)
                .map(|level| level.as_str())
        });
    let mut parameters: Vec<generated::RequestedModelModelParameterbytes> = pinned
        .iter()
        // effort_levels 是内部标记;家族模式下 reasoning_effort 已消费进 model id,
        // 钉 auto 等于不钉(避免与回退的 body effort 重复下发)
        .filter(|(id, value)| {
            *id != "effort_levels"
                && *id != "effort_default"
                && !(*id == "reasoning_effort" && (!effort_levels.is_empty() || *value == "auto"))
        })
        .map(|(id, value)| generated::RequestedModelModelParameterbytes {
            id: (*id).to_string(),
            value: (*value).to_string(),
        })
        .collect();
    let upstream_model_id = if effort_levels.is_empty() {
        // 非家族:effort 走 RequestedModel.parameters(固定参数优先)
        if let Some(effort) = body_effort {
            if pinned_effort.is_none() {
                parameters.push(generated::RequestedModelModelParameterbytes {
                    id: "reasoning_effort".to_string(),
                    value: effort.to_string(),
                });
            }
        }
        model_base.to_string()
    } else {
        // 家族:钳制到目录变体等级后拼接;无 effort 时用缺省等级
        // (目录无裸 base 条目,裸 id 会被上游拒绝)
        pinned_effort
            .or(body_effort)
            .or(effort_default)
            .map(|effort| {
                format!(
                    "{model_base}-{}",
                    crate::thinking::clamp_effort_to_levels(effort, &effort_levels)
                )
            })
            .unwrap_or_else(|| model_base.to_string())
    };
    let model_details = generated::ModelDetails {
        model_id: upstream_model_id.clone(),
        display_model_id: upstream_model_id.clone(),
        display_name: upstream_model_id.clone(),
        ..Default::default()
    };
    let tools = schema::tools(body)?;
    let mut blobs = HashMap::new();
    let state = match checkpoint {
        Some(raw) => raw.to_vec(),
        None => {
            // 对齐 Plus:真 system 进 root blob(键序 content 先,对齐 Go json.Marshal
            // 字母序;blob id 是 sha256,需字节稳定),UserText 只留用户输入。
            // 无 checkpoint 时不发结构化 turns(Plus flatten 路径 Turns=nil):
            // Run 端点对无服务端状态的会话拒绝/掐断大 turns,UserText 才是可靠通道
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
    if !parameters.is_empty() {
        let requested = generated::RequestedModel {
            model_id: upstream_model_id,
            parameters,
            ..Default::default()
        };
        encode_bytes(9, &requested.encode_to_vec(), &mut run);
    }
    let mut payload = Vec::new();
    encode_bytes(1, &run, &mut payload);
    Ok(CursorRunRequest {
        payload,
        blob_store: blobs,
        mcp_tools: tools,
    })
}
