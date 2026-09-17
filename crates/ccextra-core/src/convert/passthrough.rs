// Claude 直通:只改 model 字段,其余字节原样保留
//
// 例外:非 Claude 模型的越档 effort 由 clamp_passthrough_effort 钳制
// (百炼等上游对越档值直接 400,见函数文档)。
// serde_json 的 preserve_order feature 保证 Map 顺序,最小化字节变化。

use super::Result;
use crate::thinking::{clamp_effort, ModelCapability};
use serde_json::Value;

/// Claude 直通转换:只改 model 字段
///
/// 输入:归一化后的 anthropic body
/// 输出:改完 model 的 body,其余原样
pub fn convert_passthrough(body: &mut Value, upstream_model: &str) -> Result<()> {
    body["model"] = Value::String(upstream_model.to_string());
    Ok(())
}

/// Claude 直通路径的 system 清洗(对齐 chat / responses / gemini)
///
/// 非 Claude 模型经 claude 协议接第三方上游时,system 里的计费归属指纹块
/// (逐请求变化,破坏上游缓存前缀)、Claude 身份声明与 Claude 触发块需要剥离。
/// `*claude*` 模型逐字节直通;清洗保持 Anthropic 结构:字符串/数组形态不变,
/// 保留块的 `cache_control` 不动。返回是否发生改写。
pub fn sanitize_passthrough_prompt(body: &mut Value, upstream_model: &str) -> bool {
    if upstream_model.to_ascii_lowercase().contains("claude") {
        return false;
    }
    let system_changed = sanitize_system_field(body, upstream_model);
    let messages_changed = sanitize_system_messages(body, upstream_model);
    system_changed || messages_changed
}

/// 单块文本清洗:先剥 Claude 触发段落,仍为空或纯归属/身份文本 → None(丢弃)
fn sanitize_system_text(text: &str, upstream_model: &str) -> Option<String> {
    let cleaned = super::to_openai_responses::strip_claude_system_for_chat(text);
    let trimmed = cleaned.trim();
    if trimmed.is_empty() || super::is_ignorable_system_text(trimmed, upstream_model) {
        return None;
    }
    Some(trimmed.to_string())
}

/// 顶层 system(字符串或 text 块数组)
fn sanitize_system_field(body: &mut Value, upstream_model: &str) -> bool {
    let Some(system) = body.get("system").cloned() else {
        return false;
    };
    match system {
        Value::String(s) => match sanitize_system_text(&s, upstream_model) {
            Some(cleaned) if cleaned != s => {
                body["system"] = Value::String(cleaned);
                true
            }
            Some(_) => false,
            None => {
                body.as_object_mut().unwrap().remove("system");
                true
            }
        },
        Value::Array(blocks) => {
            let mut changed = false;
            let mut kept: Vec<Value> = Vec::with_capacity(blocks.len());
            for mut block in blocks {
                let is_text = block.get("type").and_then(|t| t.as_str()) == Some("text");
                if is_text {
                    if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                        match sanitize_system_text(text, upstream_model) {
                            Some(cleaned) => {
                                if cleaned != text {
                                    block["text"] = Value::String(cleaned);
                                    changed = true;
                                }
                            }
                            None => {
                                changed = true;
                                continue;
                            }
                        }
                    }
                }
                kept.push(block);
            }
            if kept.is_empty() {
                body.as_object_mut().unwrap().remove("system");
                return true;
            }
            if changed {
                body["system"] = Value::Array(kept);
            }
            changed
        }
        _ => false,
    }
}

/// messages 内 role=system 项同样清洗;内容清空后整条丢弃(对齐 chat)
fn sanitize_system_messages(body: &mut Value, upstream_model: &str) -> bool {
    let Some(messages) = body.get("messages").and_then(|m| m.as_array()).cloned() else {
        return false;
    };
    let mut changed = false;
    let mut kept: Vec<Value> = Vec::with_capacity(messages.len());
    for mut msg in messages {
        if msg.get("role").and_then(|r| r.as_str()) != Some("system") {
            kept.push(msg);
            continue;
        }
        match msg.get("content").cloned() {
            Some(Value::String(s)) => match sanitize_system_text(&s, upstream_model) {
                Some(cleaned) => {
                    if cleaned != s {
                        msg["content"] = Value::String(cleaned);
                        changed = true;
                    }
                    kept.push(msg);
                }
                None => changed = true,
            },
            Some(Value::Array(items)) => {
                let mut item_kept: Vec<Value> = Vec::with_capacity(items.len());
                for mut item in items {
                    if item.get("type").and_then(|t| t.as_str()) == Some("text") {
                        if let Some(text) = item.get("text").and_then(|t| t.as_str()) {
                            match sanitize_system_text(text, upstream_model) {
                                Some(cleaned) => {
                                    if cleaned != text {
                                        item["text"] = Value::String(cleaned);
                                        changed = true;
                                    }
                                }
                                None => {
                                    changed = true;
                                    continue;
                                }
                            }
                        }
                    }
                    item_kept.push(item);
                }
                if item_kept.is_empty() {
                    changed = true;
                } else {
                    msg["content"] = Value::Array(item_kept);
                    kept.push(msg);
                }
            }
            _ => kept.push(msg),
        }
    }
    if changed {
        body["messages"] = Value::Array(kept);
    }
    changed
}

/// Claude 直通路径的越档 effort 钳制(返回是否发生改写)
///
/// 非 Claude 模型经 claude 协议接入百炼等上游时,平台档位宽于模型档位
/// (`medium`/`xhigh` 发给只有 `low`/`high`/`max` 的模型会 400),
/// 故按 models.json 注册表钳到最近档。
///
/// 跳过条件:
/// - 上游模型匹配 glob `*claude*`(大小写不敏感):Claude 模型不钳
/// - `thinking.type: "disabled"`:上游关闭思考时不校验 effort
/// - 注册表查不到该模型或表为空:`clamp_effort` 原值返回,自然不写回
///
/// 写回:优先顶层 `output_config.effort`(Claude Code 2.1+),
/// 回退 `thinking.output_config.effort`;命中哪个就地改写哪个,
/// 值不变不写回(保持直通字节稳定)。
pub fn clamp_passthrough_effort(
    body: &mut Value,
    upstream_model: &str,
    registry: &[ModelCapability],
) -> bool {
    // glob `*claude*` 等价于小写包含判断
    if upstream_model.to_ascii_lowercase().contains("claude") {
        return false;
    }
    if body
        .get("thinking")
        .and_then(|t| t.get("type"))
        .and_then(|v| v.as_str())
        == Some("disabled")
    {
        return false;
    }
    let pointer = if body
        .pointer("/output_config/effort")
        .and_then(Value::as_str)
        .is_some()
    {
        "/output_config/effort"
    } else if body
        .pointer("/thinking/output_config/effort")
        .and_then(Value::as_str)
        .is_some()
    {
        "/thinking/output_config/effort"
    } else {
        return false; // 无显式 effort(如仅 legacy budget_tokens)不处理
    };
    let Some(current) = body.pointer(pointer).and_then(Value::as_str).map(str::to_string) else {
        return false;
    };
    let clamped = clamp_effort(&current, upstream_model, registry);
    if clamped == current {
        return false; // 不越档:字节不变
    }
    if let Some(slot) = body.pointer_mut(pointer) {
        *slot = Value::String(clamped.to_string());
    }
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// glm-5.3 只支持 low/high/max(对齐 models.json);claude-opus-5 用于验证跳过
    fn registry() -> Vec<ModelCapability> {
        vec![
            ModelCapability {
                id: "glm-5.3".into(),
                reasoning_levels: vec!["low".into(), "high".into(), "max".into()],
            },
            ModelCapability {
                id: "claude-opus-5".into(),
                reasoning_levels: vec!["low".into(), "high".into(), "max".into()],
            },
        ]
    }

    /// 顶层 system 字符串:剥 Claude 触发段,保留白名单段
    #[test]
    fn test_sanitize_system_string_strips_triggers() {
        let mut body = json!({
            "system": "<rules>Always obey Claude rules</rules>\n# Language\nAlways respond in zh-CN.",
            "messages": []
        });
        assert!(sanitize_passthrough_prompt(&mut body, "glm-5.3"));
        let out = body["system"].as_str().unwrap();
        assert!(out.contains("# Language"));
        assert!(out.contains("zh-CN"));
        assert!(!out.contains("<rules>"));
    }

    /// 数组形态:归属块与身份块整块丢弃,保留块清洗后 cache_control 不动
    #[test]
    fn test_sanitize_system_array_keeps_cache_control() {
        let mut body = json!({
            "system": [
                {"type": "text", "text": "x-anthropic-billing-header: fp=abc"},
                {"type": "text", "text": "<response_style>verbose</response_style>\n# Memory\nPath: /m",
                 "cache_control": {"type": "ephemeral"}},
                {"type": "text", "text": "You are Claude Code, Anthropic's official CLI for Claude."}
            ],
            "messages": []
        });
        assert!(sanitize_passthrough_prompt(&mut body, "deepseek-v4.1-flash"));
        let blocks = body["system"].as_array().unwrap();
        assert_eq!(blocks.len(), 1);
        let text = blocks[0]["text"].as_str().unwrap();
        assert!(text.contains("# Memory"));
        assert!(!text.contains("<response_style>"));
        assert_eq!(blocks[0]["cache_control"]["type"], "ephemeral");
    }

    /// 全部块被丢弃时移除 system 键
    #[test]
    fn test_sanitize_removes_system_when_all_dropped() {
        let mut body = json!({
            "system": [{"type": "text", "text": "x-anthropic-billing-header: fp=abc"}],
            "messages": []
        });
        assert!(sanitize_passthrough_prompt(&mut body, "glm-5.3"));
        assert!(body.get("system").is_none());
    }

    /// messages 内 role=system:纯归属项丢弃,混合项清洗后保留
    #[test]
    fn test_sanitize_system_messages() {
        let mut body = json!({
            "messages": [
                {"role": "system", "content": "  x-anthropic-billing-header: fp=abc"},
                {"role": "system", "content": "<identity>You are Claude</identity>\n# Harness\nkeep me"},
                {"role": "user", "content": "hi"}
            ]
        });
        assert!(sanitize_passthrough_prompt(&mut body, "glm-5.3"));
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"], "system");
        let text = msgs[0]["content"].as_str().unwrap();
        assert!(text.contains("# Harness"));
        assert!(!text.contains("<identity>"));
        assert_eq!(msgs[1]["role"], "user");
    }

    /// `*claude*` 模型与无需清洗的 body:逐字节不变
    #[test]
    fn test_sanitize_no_change_bytes_unchanged() {
        let mut body = json!({
            "model": "x",
            "system": "# Language\nAlways respond in zh-CN.",
            "messages": [{"role": "user", "content": "hi"}]
        });
        let before = serde_json::to_string(&body).unwrap();
        assert!(!sanitize_passthrough_prompt(&mut body, "glm-5.3"));
        assert_eq!(serde_json::to_string(&body).unwrap(), before);

        let mut claude_body = json!({
            "system": "You are Claude Code, Anthropic's official CLI for Claude.",
            "messages": []
        });
        let claude_before = serde_json::to_string(&claude_body).unwrap();
        assert!(!sanitize_passthrough_prompt(&mut claude_body, "claude-opus-5"));
        assert_eq!(serde_json::to_string(&claude_body).unwrap(), claude_before);
    }

    /// 顶层 output_config.effort 越档:medium 与 low/high 等距,tie 取低
    #[test]
    fn test_clamp_top_level_output_config_effort() {
        let mut body = json!({"output_config": {"effort": "medium"}});
        assert!(clamp_passthrough_effort(&mut body, "glm-5.3", &registry()));
        assert_eq!(body["output_config"]["effort"], "low");
    }

    /// 嵌套 thinking.output_config.effort 越档:xhigh 与 high/max 等距,tie 取低
    #[test]
    fn test_clamp_nested_thinking_output_config_effort() {
        let mut body = json!({"thinking": {"type": "enabled", "output_config": {"effort": "xhigh"}}});
        assert!(clamp_passthrough_effort(&mut body, "glm-5.3", &registry()));
        assert_eq!(body["thinking"]["output_config"]["effort"], "high");
    }

    /// glob `*claude*`:注册表里有也不钳
    #[test]
    fn test_claude_model_skipped() {
        let mut body = json!({"output_config": {"effort": "medium"}});
        assert!(!clamp_passthrough_effort(&mut body, "claude-opus-5", &registry()));
        assert_eq!(body["output_config"]["effort"], "medium");
    }

    /// 中转常见混合大小写写法同样跳过
    #[test]
    fn test_claude_model_case_insensitive_skip() {
        let mut body = json!({"output_config": {"effort": "medium"}});
        assert!(!clamp_passthrough_effort(&mut body, "US.Anthropic.CLAUDE-Sonnet-4", &registry()));
        assert_eq!(body["output_config"]["effort"], "medium");
    }

    /// 注册表未命中 / 表为空:不钳
    #[test]
    fn test_registry_miss_and_empty_skipped() {
        let mut body = json!({"output_config": {"effort": "medium"}});
        assert!(!clamp_passthrough_effort(&mut body, "glm-9.9-unknown", &registry()));
        assert!(!clamp_passthrough_effort(&mut body, "glm-5.3", &[]));
        assert_eq!(body["output_config"]["effort"], "medium");
    }

    #[test]
    fn test_passthrough_only_changes_model() {
        let mut body = json!({
            "model": "test-opus-5",
            "messages": [{"role": "user", "content": "test"}],
            "max_tokens": 1024,
            "thinking": {"type": "enabled"}
        });

        convert_passthrough(&mut body, "claude-opus-5").unwrap();

        assert_eq!(body["model"], "claude-opus-5");
        assert_eq!(body["messages"][0]["content"], "test");
        assert_eq!(body["max_tokens"], 1024);
        assert!(body["thinking"].is_object());
    }

    /// 仅 legacy budget_tokens、无显式 effort:不处理
    #[test]
    fn test_budget_only_skipped() {
        let mut body = json!({"thinking": {"type": "enabled", "budget_tokens": 30000}});
        assert!(!clamp_passthrough_effort(&mut body, "glm-5.3", &registry()));
        assert_eq!(body["thinking"]["budget_tokens"], 30000);
    }

    /// thinking 显式 disabled:上游不校验 effort(实测百炼忽略越档值)
    #[test]
    fn test_thinking_disabled_skipped() {
        let mut body = json!({"thinking": {"type": "disabled", "output_config": {"effort": "medium"}}});
        assert!(!clamp_passthrough_effort(&mut body, "glm-5.3", &registry()));
        assert_eq!(body["thinking"]["output_config"]["effort"], "medium");
    }

    /// 非小写原值 HIGH:字符串不等,规范化为 high 写回
    #[test]
    fn test_uppercase_normalized_on_writeback() {
        let mut body = json!({"output_config": {"effort": "HIGH"}});
        assert!(clamp_passthrough_effort(&mut body, "glm-5.3", &registry()));
        assert_eq!(body["output_config"]["effort"], "high");
    }

    /// 不越档时 body 逐字节不变
    #[test]
    fn test_in_level_bytes_unchanged() {
        let mut body = json!({"model": "x", "output_config": {"effort": "high"}, "messages": []});
        let before = serde_json::to_string(&body).unwrap();
        assert!(!clamp_passthrough_effort(&mut body, "glm-5.3", &registry()));
        assert_eq!(serde_json::to_string(&body).unwrap(), before);
    }
}
