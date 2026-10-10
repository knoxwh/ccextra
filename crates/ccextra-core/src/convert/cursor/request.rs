use super::proto::{encode_bytes, generated};
use super::{history, input, schema, CursorConvertError};
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

/// 零工具直连会话约束(对齐 cursor-cpa-plugin request.go 文案):无工具环境
/// 下防止模型幻觉调用不存在的工具或 web search
const SYSTEM_CONSTRAINT: &str = "<system_constraint>\nNOTE: This is a direct conversational session with no tool execution environment. Please answer directly in text using your knowledge without attempting to invoke tools or web search.\n</system_constraint>";

/// 无 reasoning 注册表的转换入口(家族模型只按目录等级钳制)
pub fn build_run_request(
    body: &Value,
    model: &str,
    conversation_id: &str,
    message_id: &str,
    checkpoint: Option<&[u8]>,
) -> Result<CursorRunRequest, CursorConvertError> {
    build_run_request_with(body, model, conversation_id, message_id, checkpoint, &[])
}

/// 带 reasoning 注册表的转换入口(HTTP 热重载快照注入)
///
/// 家族变体模型(name 带 `effort_levels` 标记)的思考档由 models.json 决定:
/// `force_effort` 固定档,否则入站档按 `reasoning_levels` 钳制,再钳到目录
/// 实际变体后拼进 model id,白名单思考钉参对其不生效。非家族模型不读注册表,
/// 仍由白名单钉参/入站档写入 RequestedModel.parameters
pub fn build_run_request_with(
    body: &Value,
    model: &str,
    conversation_id: &str,
    message_id: &str,
    checkpoint: Option<&[u8]>,
    registry: &[crate::thinking::ModelCapability],
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
    let tools = schema::tools(body)?;
    // 零工具时注入直连会话约束(对齐 cursor-cpa-plugin:两种模式都拼在
    // UserText 头部;CC 主路径恒带工具,不受影响)
    let text = if tools.is_empty() {
        format!("{SYSTEM_CONSTRAINT}\n\n{text}")
    } else {
        text
    };
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
    // 非家族模型的目录思考参数有两种名字:reasoning_effort / effort。白名单钉住
    // 任一即覆盖入站档;钉 auto 等于不钉,回退 body effort。家族模型不读此值
    let pinned_effort = pinned
        .iter()
        .find(|(id, _)| matches!(*id, "reasoning_effort" | "effort"))
        .map(|(_, value)| *value)
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
        // effort_levels/effort_default 是内部标记;家族模式下思考档由 models.json
        // 决定并已拼进 model id,白名单残留的思考钉参不下发;钉 auto 等于不钉
        // (避免与回退的 body effort 重复下发)
        .filter(|(id, value)| {
            let effort_pin = matches!(*id, "reasoning_effort" | "effort");
            *id != "effort_levels"
                && *id != "effort_default"
                && !(effort_pin && (!effort_levels.is_empty() || *value == "auto"))
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
        // 家族:思考档策略来自 models.json(按 base 精确匹配,查不到不钳)。
        // force_effort 固定档优先且不受 reasoning_levels 钳制(与其他协议一致);
        // 否则入站档(无则目录缺省档)按 reasoning_levels 钳制。最后再钳到目录
        // 实际变体,保证拼出的 id 上游存在(目录无裸 base 条目,裸 id 会被拒绝)
        let level = match crate::thinking::forced_effort(model_base, registry) {
            Some(forced) => Some(forced),
            None => body_effort
                .or(effort_default)
                .map(|effort| crate::thinking::clamp_effort(effort, model_base, registry)),
        };
        level
            .map(|effort| {
                format!(
                    "{model_base}-{}",
                    crate::thinking::clamp_effort_to_levels(effort, &effort_levels)
                )
            })
            // 仅手工构造的家族名缺 effort_default 才会回退裸 base(catalog 总会下发该标记)
            .unwrap_or_else(|| model_base.to_string())
    };
    let model_details = generated::ModelDetails {
        model_id: upstream_model_id.clone(),
        display_model_id: upstream_model_id.clone(),
        display_name: upstream_model_id.clone(),
        ..Default::default()
    };
    let mut blobs = HashMap::new();
    let state = match checkpoint {
        Some(raw) => raw.to_vec(),
        None => {
            // 对齐 Plus:真 system 进 root blob(键序 content 先,对齐 Go json.Marshal
            // 字母序;blob id 是 sha256,需字节稳定),UserText 只留用户输入。
            // 多轮历史走 history::fabricate:Anthropic 消息编成 root prompt blob
            // 序列 + turns 结构(2026-10-09 探针 F/G 实证:服务端经 kv getBlob
            // 取回未知 blob,伪造历史被模型接受),替代 flatten 全量 UserText
            // 重放(64KiB RST 风险)
            let system = serde_json::json!({ "content": system, "role": "system" });
            let bytes = serde_json::to_vec(&system)?;
            let digest = Sha256::digest(&bytes);
            blobs.insert(hex::encode(digest), bytes);
            let history = history::fabricate(body)?;
            blobs.extend(history.blobs);
            let mut root_prompt_ids = vec![digest.to_vec()];
            root_prompt_ids.extend(history.root_prompt_ids);
            generated::ConversationStateStructure {
                root_prompt_messages_json: root_prompt_ids,
                turns: history.turn_ids,
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
