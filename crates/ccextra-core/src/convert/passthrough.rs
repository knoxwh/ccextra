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
