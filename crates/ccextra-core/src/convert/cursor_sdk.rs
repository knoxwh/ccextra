// Cursor SDK sidecar 请求转换:anthropic body → /run body
//
// 纯逻辑无 IO。输出字段:model / modelParams / systemPrompt / workspaceDir /
// messages / tools。apiKey 由 server 侧 client 注入,不进 converter。
// 历史 thinking 块剥离(对齐 sidecar stripThinkingBlocks);图片原样保留,
// 由 sidecar 校验并转换为 SDK 附件;system 复用 passthrough 的
// 非 Claude 清洗(剥计费归属指纹/身份声明/触发块)。

use std::path::Path;

use serde_json::{json, Map, Value};
use thiserror::Error;

use crate::thinking::{forced_effort, resolve_effort_from_body, ModelCapability};

#[derive(Debug, Error)]
pub enum CursorSdkConvertError {
    /// messages 缺失或不是非空数组
    #[error("messages must be a non-empty array")]
    InvalidMessages,
}

/// SDK 模型参数词表(目录刷新时从 sidecar /models 带回)
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct CursorParamVocab {
    pub id: String,
    pub values: Vec<String>,
}

/// 转换为 sidecar /run body
///
/// 输入:归一化后的 anthropic body(含 system / messages / tools)
/// 输出:sidecar /run 请求体(不含 apiKey)
/// vocab/registry 为空时 modelParams 退化为白名单固定参数(兼容旧目录)
pub fn convert_to_cursor_sdk(
    body: Value,
    upstream_model: &str,
    workspace_dir: &Path,
    vocab: &[CursorParamVocab],
    registry: &[ModelCapability],
) -> Result<Value, CursorSdkConvertError> {
    let mut body = body;
    // system 清洗:Cursor 非 Claude 上游,剥归属指纹/身份声明/触发块
    super::passthrough::sanitize_passthrough_prompt(&mut body, upstream_model);
    let system_prompt = flatten_system(body.get("system"));

    let messages = body
        .get("messages")
        .and_then(|m| m.as_array())
        .cloned()
        .ok_or(CursorSdkConvertError::InvalidMessages)?;
    if messages.is_empty() {
        return Err(CursorSdkConvertError::InvalidMessages);
    }
    let messages = strip_thinking_blocks(messages);

    // tools 原样透传(Anthropic 形状);缺省输出空数组,与 sidecar 缺省语义一致
    let tools = body
        .get("tools")
        .and_then(|t| t.as_array())
        .cloned()
        .unwrap_or_default();

    let (model_id, model_params) = resolve_model_params(upstream_model, &body, vocab, registry);

    Ok(json!({
        "model": model_id,
        "modelParams": model_params,
        "systemPrompt": system_prompt,
        "workspaceDir": workspace_dir.to_string_lossy(),
        "messages": messages,
        "tools": tools,
    }))
}

/// 解析 upstream_model 并组装 modelParams
///
/// upstream_model 可携带白名单固定参数("base:param=value,..."),固定值优先;
/// 入站 thinking.type=enabled 映射词表 thinking 参数;effort(force_effort
/// 优先,回退入站)钳到词表最近档,映射到 effort/reasoning/reasoning_effort。
/// 参数按 id 排序,保证会话哈希稳定。
fn resolve_model_params(
    upstream_model: &str,
    body: &Value,
    vocab: &[CursorParamVocab],
    registry: &[ModelCapability],
) -> (String, Vec<Value>) {
    let (base, pinned) = split_pinned_params(upstream_model);
    let mut params = pinned
        .into_iter()
        .map(|(id, value)| (canonicalize_param_id(&base, &id, vocab), value))
        .collect::<Vec<_>>();

    let thinking_enabled = body
        .get("thinking")
        .and_then(|t| t.get("type"))
        .and_then(|v| v.as_str())
        == Some("enabled");
    if thinking_enabled
        && vocab
            .iter()
            .any(|p| p.id == "thinking" && p.values.iter().any(|v| v == "true"))
        && !params.iter().any(|(k, _)| k == "thinking")
    {
        params.push(("thinking".to_string(), "true".to_string()));
    }

    let effort = forced_effort(&base, registry)
        .map(str::to_string)
        .or_else(|| resolve_effort_from_body(body).map(str::to_string));
    if let Some(effort) = effort {
        if let Some(param) = vocab
            .iter()
            .find(|p| matches!(p.id.as_str(), "effort" | "reasoning" | "reasoning_effort"))
        {
            if !params.iter().any(|(k, _)| k == &param.id) {
                if let Some(value) = clamp_to_vocab_values(&effort, &param.values) {
                    params.push((param.id.clone(), value));
                }
            }
        }
    }

    params.sort_by(|a, b| a.0.cmp(&b.0));
    let values = params
        .into_iter()
        .map(|(id, value)| json!({ "id": id, "value": value }))
        .collect();
    (base, values)
}

/// 拆 "base:param=value,param=value" → (base, 固定参数对)
///
/// 无 ":" 时整串即 base;参数段格式非法时整串退回 base,不把垃圾发给 SDK
fn split_pinned_params(upstream_model: &str) -> (String, Vec<(String, String)>) {
    let Some((base, tail)) = upstream_model.split_once(':') else {
        return (upstream_model.to_string(), Vec::new());
    };
    let mut pinned = Vec::new();
    for pair in tail.split(',') {
        match pair.split_once('=') {
            Some((k, v)) if !k.is_empty() && !v.is_empty() => {
                pinned.push((k.to_string(), v.to_string()));
            }
            // 非法参数段:整串退回 base
            _ => return (upstream_model.to_string(), Vec::new()),
        }
    }
    (base.to_string(), pinned)
}

/// 按当前模型词表归一 Cursor 参数别名,避免固定参数使用旧名称
fn canonicalize_param_id(model: &str, id: &str, vocab: &[CursorParamVocab]) -> String {
    let supports = |candidate: &str| vocab.iter().any(|param| param.id == candidate);
    match id {
        "effort" if !supports("effort") && supports("reasoning_effort") => {
            "reasoning_effort".to_string()
        }
        "reasoning_effort" if !supports("reasoning_effort") && supports("effort") => {
            "effort".to_string()
        }
        "mode" if model == "auto-smart" && !supports("mode") && supports("optimize_for") => {
            "optimize_for".to_string()
        }
        _ => id.to_string(),
    }
}

/// effort 钳到词表最近值(tie 取低);词表无可排序值或 effort 非法 → None。
/// effort 为 none 而词表不含 none 时不钳制(钳上去会强行开思考),返回 None。
fn clamp_to_vocab_values(effort: &str, values: &[String]) -> Option<String> {
    if values.iter().any(|v| v == effort) {
        return Some(effort.to_string());
    }
    if effort == "none" {
        return None;
    }
    let target = param_value_rank(effort)?;
    let mut best: Option<(usize, u8)> = None;
    for (index, value) in values.iter().enumerate() {
        let Some(rank) = param_value_rank(value) else {
            continue;
        };
        let dist = target.abs_diff(rank);
        let take = match best {
            None => true,
            Some((_, best_rank)) => {
                let best_dist = target.abs_diff(best_rank);
                dist < best_dist || (dist == best_dist && rank < best_rank)
            }
        };
        if take {
            best = Some((index, rank));
        }
    }
    best.map(|(index, _)| values[index].clone())
}

/// 思考档排序(none 最低,max 最高;extra-high 介于 high 与 xhigh)
/// effort 档位序(钳位距离用);`auto` 不参与排序,仅经精确匹配透传
fn param_value_rank(value: &str) -> Option<u8> {
    Some(match value {
        "none" => 0,
        "minimal" => 1,
        "low" => 2,
        "medium" => 3,
        "high" => 4,
        "extra-high" => 5,
        "xhigh" => 6,
        "max" => 7,
        _ => return None,
    })
}

/// 顶层 system(字符串或 text 块数组)压平为单个字符串
///
/// 块数组用空行连接;清洗后无 system 输出空串(sidecar 恒收 systemPrompt)
fn flatten_system(system: Option<&Value>) -> String {
    match system {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(blocks)) => blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join("\n\n"),
        _ => String::new(),
    }
}

/// 递归剥 thinking 块与 cache_control 标记;图片块原样保留。
///
/// 对齐 sidecar stripThinkingBlocks 的递归语义(含 tool_result 嵌套内容)。
/// cache_control 是 Anthropic 缓存断点标记,客户端随轮次移动(当前消息打标、
/// 历史回显剥标),非内容语义;剥掉避免 journal 快照携带瞬态噪声。
fn strip_thinking_blocks(messages: Vec<Value>) -> Vec<Value> {
    messages.into_iter().map(strip_value).collect()
}

fn strip_value(value: Value) -> Value {
    match value {
        Value::Array(items) => Value::Array(
            items
                .into_iter()
                .filter(|item| item.get("type").and_then(|t| t.as_str()) != Some("thinking"))
                .map(strip_value)
                .collect(),
        ),
        Value::Object(map) => {
            let mut out = Map::with_capacity(map.len());
            for (key, child) in map {
                if key == "cache_control" {
                    continue;
                }
                out.insert(key, strip_value(child));
            }
            Value::Object(out)
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn workspace() -> std::path::PathBuf {
        std::path::PathBuf::from("/tmp/workspace")
    }

    #[test]
    fn keeps_tools_and_message_order_without_images() {
        let body = json!({
            "model": "composer-2.5",
            "system": "You are helpful.",
            "messages": [
                { "role": "user", "content": [{ "type": "text", "text": "hi" }] },
                { "role": "assistant", "content": [{ "type": "text", "text": "hello" }] },
                { "role": "user", "content": [{ "type": "text", "text": "go on" }] }
            ],
            "tools": [
                { "name": "Bash", "description": "run shell", "input_schema": { "type": "object" } }
            ]
        });
        let out = convert_to_cursor_sdk(body, "composer-2.5", &workspace(), &[], &[]).unwrap();
        assert_eq!(out["model"], "composer-2.5");
        assert_eq!(out["systemPrompt"], "You are helpful.");
        assert_eq!(out["workspaceDir"], "/tmp/workspace");
        let messages = out["messages"].as_array().unwrap();
        assert_eq!(messages.len(), 3);
        assert_eq!(messages[0]["content"][0]["text"], "hi");
        assert_eq!(messages[2]["content"][0]["text"], "go on");
        assert_eq!(out["tools"].as_array().unwrap().len(), 1);
        assert_eq!(out["tools"][0]["name"], "Bash");
    }

    #[test]
    fn strips_cache_control_markers() {
        let body = json!({
            "model": "auto",
            "messages": [
                { "role": "user", "content": [
                    { "type": "text", "text": "hi", "cache_control": { "type": "ephemeral" } }
                ] },
                { "role": "assistant", "content": [{ "type": "text", "text": "hello" }] },
                { "role": "user", "content": [
                    { "type": "tool_result", "tool_use_id": "t1", "content": "done",
                      "cache_control": { "type": "ephemeral" } }
                ] }
            ]
        });
        let out = convert_to_cursor_sdk(body, "auto", &workspace(), &[], &[]).unwrap();
        let messages = out["messages"].as_array().unwrap();
        assert_eq!(messages[0]["content"][0].get("cache_control"), None);
        assert_eq!(messages[0]["content"][0]["text"], "hi");
        assert_eq!(messages[2]["content"][0].get("cache_control"), None);
        assert_eq!(messages[2]["content"][0]["tool_use_id"], "t1");
    }

    #[test]
    fn strips_history_thinking_blocks() {
        let body = json!({
            "model": "auto",
            "messages": [
                { "role": "user", "content": "hi" },
                { "role": "assistant", "content": [
                    { "type": "thinking", "thinking": "internal" },
                    { "type": "text", "text": "answer" }
                ] },
                { "role": "user", "content": [
                    { "type": "tool_result", "tool_use_id": "t1", "content": [
                        { "type": "thinking", "thinking": "nested" },
                        { "type": "text", "text": "result" }
                    ] }
                ] }
            ]
        });
        let out = convert_to_cursor_sdk(body, "auto", &workspace(), &[], &[]).unwrap();
        let messages = out["messages"].as_array().unwrap();
        let assistant = messages[1]["content"].as_array().unwrap();
        assert_eq!(assistant.len(), 1);
        assert_eq!(assistant[0]["type"], "text");
        let tool_result = messages[2]["content"][0]["content"].as_array().unwrap();
        assert_eq!(tool_result.len(), 1);
        assert_eq!(tool_result[0]["text"], "result");
    }

    #[test]
    fn sanitizes_top_level_and_message_system() {
        let body = json!({
            "model": "auto",
            "system": [
                { "type": "text", "text": "x-anthropic-billing-header: abc" },
                { "type": "text", "text": "You are Claude Code, Anthropic's official CLI for Claude." },
                { "type": "text", "text": "Real instructions." }
            ],
            "messages": [
                { "role": "system", "content": "x-anthropic-billing-header: xyz" },
                { "role": "user", "content": "hi" }
            ]
        });
        let out = convert_to_cursor_sdk(body, "auto", &workspace(), &[], &[]).unwrap();
        // 归属指纹与身份声明块被剥,只留真实指令
        assert_eq!(out["systemPrompt"], "Real instructions.");
        let messages = out["messages"].as_array().unwrap();
        // 消息内 system 清洗后整条丢弃
        assert_eq!(messages.len(), 1);
        assert_eq!(messages[0]["role"], "user");
    }

    #[test]
    fn preserves_image_block_for_sidecar() {
        let body = json!({
            "model": "auto",
            "messages": [
                { "role": "user", "content": [
                    { "type": "image", "source": {
                        "type": "base64", "media_type": "image/png", "data": "AQID"
                    } }
                ] }
            ]
        });
        let out = convert_to_cursor_sdk(body, "auto", &workspace(), &[], &[]).unwrap();
        assert_eq!(out["messages"][0]["content"][0]["type"], "image");
        assert_eq!(out["messages"][0]["content"][0]["source"]["data"], "AQID");
    }

    #[test]
    fn preserves_image_inside_tool_result_for_sidecar() {
        let body = json!({
            "model": "auto",
            "messages": [
                { "role": "user", "content": [
                    { "type": "tool_result", "tool_use_id": "t1", "content": [
                        { "type": "image", "source": {
                            "type": "base64", "media_type": "image/jpeg", "data": "BAUG"
                        } }
                    ] }
                ] }
            ]
        });
        let out = convert_to_cursor_sdk(body, "auto", &workspace(), &[], &[]).unwrap();
        assert_eq!(
            out["messages"][0]["content"][0]["content"][0]["source"]["media_type"],
            "image/jpeg"
        );
    }

    #[test]
    fn missing_or_empty_messages_is_invalid() {
        let body = json!({ "model": "auto", "messages": [] });
        let err = convert_to_cursor_sdk(body, "auto", &workspace(), &[], &[]).unwrap_err();
        assert!(matches!(err, CursorSdkConvertError::InvalidMessages));

        let body = json!({ "model": "auto" });
        let err = convert_to_cursor_sdk(body, "auto", &workspace(), &[], &[]).unwrap_err();
        assert!(matches!(err, CursorSdkConvertError::InvalidMessages));
    }

    #[test]
    fn emits_empty_model_params_and_empty_tools_consistently() {
        let body = json!({
            "model": "auto",
            "messages": [{ "role": "user", "content": "hi" }]
        });
        let out = convert_to_cursor_sdk(body, "auto", &workspace(), &[], &[]).unwrap();
        assert_eq!(out["modelParams"], json!([]));
        assert_eq!(out["tools"], json!([]));
        // temperature / top_p / stop_sequences / thinking 不进输出
        for dropped in ["temperature", "top_p", "stop_sequences", "thinking"] {
            assert!(out.get(dropped).is_none());
        }
    }

    #[test]
    fn conversion_is_idempotent_for_same_input() {
        let body = json!({
            "model": "auto",
            "system": "Keep this.",
            "messages": [
                { "role": "user", "content": "hi" },
                { "role": "assistant", "content": [
                    { "type": "thinking", "thinking": "x" },
                    { "type": "text", "text": "answer" }
                ] }
            ],
            "tools": [{ "name": "Bash", "input_schema": { "type": "object" } }]
        });
        let first = convert_to_cursor_sdk(body.clone(), "auto", &workspace(), &[], &[]).unwrap();
        let second = convert_to_cursor_sdk(body, "auto", &workspace(), &[], &[]).unwrap();
        assert_eq!(first, second);
    }

    fn vocab(id: &str, values: &[&str]) -> CursorParamVocab {
        CursorParamVocab {
            id: id.to_string(),
            values: values.iter().map(|v| v.to_string()).collect(),
        }
    }

    #[test]
    fn pinned_params_flow_and_model_strips_suffix() {
        let body = json!({
            "model": "auto-smart",
            "messages": [{ "role": "user", "content": "hi" }]
        });
        let out = convert_to_cursor_sdk(
            body,
            "auto-smart:optimize_for=intelligence",
            &workspace(),
            &[],
            &[],
        )
        .unwrap();
        assert_eq!(out["model"], "auto-smart");
        assert_eq!(
            out["modelParams"],
            json!([{ "id": "optimize_for", "value": "intelligence" }])
        );
    }

    #[test]
    fn invalid_pinned_tail_falls_back_to_whole_model() {
        let body = json!({
            "model": "auto",
            "messages": [{ "role": "user", "content": "hi" }]
        });
        let out = convert_to_cursor_sdk(body, "auto:oops", &workspace(), &[], &[]).unwrap();
        assert_eq!(out["model"], "auto:oops");
        assert_eq!(out["modelParams"], json!([]));
    }

    #[test]
    fn thinking_enabled_maps_to_true_and_effort_clamps() {
        let body = json!({
            "model": "grok-4.7",
            "thinking": { "type": "enabled" },
            "output_config": { "effort": "max" },
            "messages": [{ "role": "user", "content": "hi" }]
        });
        let vocab = vec![
            vocab("thinking", &["false", "true"]),
            vocab("reasoning_effort", &["low", "medium", "high", "xhigh"]),
        ];
        let out = convert_to_cursor_sdk(body, "grok-4.7", &workspace(), &vocab, &[]).unwrap();
        // max 不在词表,钳到最近档 xhigh;参数按 id 排序
        assert_eq!(
            out["modelParams"],
            json!([
                { "id": "reasoning_effort", "value": "xhigh" },
                { "id": "thinking", "value": "true" }
            ])
        );
    }

    #[test]
    fn thinking_disabled_omits_params() {
        let body = json!({
            "model": "grok-4.7",
            "thinking": { "type": "disabled" },
            "messages": [{ "role": "user", "content": "hi" }]
        });
        let vocab = vec![
            vocab("thinking", &["false", "true"]),
            vocab("reasoning_effort", &["low", "high"]),
        ];
        let out = convert_to_cursor_sdk(body, "grok-4.7", &workspace(), &vocab, &[]).unwrap();
        assert_eq!(out["modelParams"], json!([]));
    }

    #[test]
    fn force_effort_overrides_inbound_and_clamps() {
        let body = json!({
            "model": "muse-spark-1.3",
            "thinking": { "type": "enabled", "output_config": { "effort": "low" } },
            "messages": [{ "role": "user", "content": "hi" }]
        });
        let vocab = vec![vocab(
            "effort",
            &["minimal", "low", "medium", "high", "xhigh", "max"],
        )];
        let registry = vec![ModelCapability {
            id: "muse-spark-1.3".to_string(),
            reasoning_levels: vec!["low".to_string(), "max".to_string()],
            force_effort: Some("max".to_string()),
        }];
        let out =
            convert_to_cursor_sdk(body, "muse-spark-1.3", &workspace(), &vocab, &registry).unwrap();
        assert_eq!(
            out["modelParams"],
            json!([{ "id": "effort", "value": "max" }])
        );
    }

    #[test]
    fn pinned_effort_alias_uses_catalog_parameter_name() {
        let body = json!({
            "model": "grok-4.7",
            "messages": [{ "role": "user", "content": "hi" }]
        });
        let vocab = vec![vocab("reasoning_effort", &["low", "high"])];
        let out =
            convert_to_cursor_sdk(body, "grok-4.7:effort=high", &workspace(), &vocab, &[]).unwrap();
        assert_eq!(
            out["modelParams"],
            json!([{ "id": "reasoning_effort", "value": "high" }])
        );
    }

    #[test]
    fn auto_smart_mode_alias_uses_optimize_for() {
        let body = json!({
            "model": "auto-smart",
            "messages": [{ "role": "user", "content": "hi" }]
        });
        let vocab = vec![vocab("optimize_for", &["speed", "quality"])];
        let out = convert_to_cursor_sdk(body, "auto-smart:mode=speed", &workspace(), &vocab, &[])
            .unwrap();
        assert_eq!(
            out["modelParams"],
            json!([{ "id": "optimize_for", "value": "speed" }])
        );
    }

    #[test]
    fn pinned_overrides_mapped_param() {
        let body = json!({
            "model": "grok-4.7",
            "thinking": { "type": "enabled", "output_config": { "effort": "low" } },
            "messages": [{ "role": "user", "content": "hi" }]
        });
        let vocab = vec![vocab("reasoning_effort", &["low", "high"])];
        let out = convert_to_cursor_sdk(
            body,
            "grok-4.7:reasoning_effort=high",
            &workspace(),
            &vocab,
            &[],
        )
        .unwrap();
        assert_eq!(
            out["modelParams"],
            json!([{ "id": "reasoning_effort", "value": "high" }])
        );
    }

    #[test]
    fn effort_clamps_tie_takes_lower() {
        // high 不在词表,low 与 xhigh 距离相等时取低
        let values: Vec<String> = ["low", "xhigh"].iter().map(|v| v.to_string()).collect();
        assert_eq!(
            clamp_to_vocab_values("high", &values).as_deref(),
            Some("low")
        );
        // extra-high 介于 high 与 xhigh:词表只有 high/xhigh 时 tie 取低 → high
        let values: Vec<String> = ["high", "xhigh"].iter().map(|v| v.to_string()).collect();
        assert_eq!(
            clamp_to_vocab_values("extra-high", &values).as_deref(),
            Some("high")
        );
        // effort none 而词表不含 none:不钳上去,跳过参数
        let values: Vec<String> = ["low", "high"].iter().map(|v| v.to_string()).collect();
        assert_eq!(clamp_to_vocab_values("none", &values), None);
        // 词表无可排序值 → None
        let values: Vec<String> = ["false", "true"].iter().map(|v| v.to_string()).collect();
        assert_eq!(clamp_to_vocab_values("high", &values), None);
    }
}
