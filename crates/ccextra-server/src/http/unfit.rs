// Unfit 状态机:自适应运行时错误探针(对齐 magpie gateway.go:3440-3490)
//
// 当上游报 400/422 拒绝时,自动识别不兼容特性并降级重试:
// - response_format 不支持 → schema 注入 system prompt
// - reasoning effort "off" 拒绝 → 强制 "low"
// - tool_choice 不支持 → 降级 "auto"
// - tools + reasoning 冲突 → 关闭 reasoning
//
// 状态记忆:(provider, model, feature) 三元组标记,避免重复试探

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum UnfitKind {
    RefusesFormat,      // response_format 不支持
    OffRefused,         // reasoning effort "off" 被拒
    ToolChoiceRefused,  // tool_choice 不支持
    ToolsWithoutEffort, // tools + reasoning 冲突
}

type UnfitKey = (String, String); // (provider, model)

#[derive(Default)]
pub struct UnfitRegistry {
    flags: RwLock<HashMap<UnfitKey, Vec<UnfitKind>>>,
}

impl UnfitRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 标记 (provider, model) 不兼容某特性
    pub fn mark(&self, provider: &str, model: &str, kind: UnfitKind) {
        let key = (provider.to_string(), model.to_string());
        let mut flags = self.flags.write().unwrap();
        flags.entry(key).or_default().push(kind);
    }

    /// 检查是否已标记为不兼容
    pub fn is_unfit(&self, provider: &str, model: &str, kind: UnfitKind) -> bool {
        let key = (provider.to_string(), model.to_string());
        self.flags
            .read()
            .unwrap()
            .get(&key)
            .is_some_and(|kinds| kinds.contains(&kind))
    }
}

/// 全局 unfit 注册表(lazy 初始化)
static UNFIT_REGISTRY: once_cell::sync::Lazy<Arc<UnfitRegistry>> =
    once_cell::sync::Lazy::new(|| Arc::new(UnfitRegistry::new()));

pub fn global_registry() -> &'static Arc<UnfitRegistry> {
    &UNFIT_REGISTRY
}

/// 从上游 400/422 错误体探测不兼容特性(对齐 magpie detectUnfit)
pub fn detect_unfit_from_error(status: u16, body: &str) -> Option<UnfitKind> {
    if status != 400 && status != 422 {
        return None;
    }
    let lower = body.to_lowercase();

    // response_format 拒绝(Gemini: "response_mime_type", OpenAI: "response_format")
    if lower.contains("response_mime_type")
        || lower.contains("response_format")
        || lower.contains("json_schema")
    {
        return Some(UnfitKind::RefusesFormat);
    }

    // reasoning effort "off" 拒绝
    if lower.contains("reasoning") && lower.contains("off") {
        return Some(UnfitKind::OffRefused);
    }

    // tool_choice 拒绝
    if lower.contains("tool_choice") || lower.contains("toolchoice") {
        return Some(UnfitKind::ToolChoiceRefused);
    }

    // tools + reasoning 冲突(Gemini: "thinking" + "tools")
    if (lower.contains("thinking") || lower.contains("reasoning")) && lower.contains("tool") {
        return Some(UnfitKind::ToolsWithoutEffort);
    }

    None
}

/// 根据 unfit 标记降级请求体(对齐 magpie adaptForUnfit)
pub fn adapt_request_for_unfit(
    body: &mut serde_json::Value,
    provider: &str,
    model: &str,
    registry: &UnfitRegistry,
) -> bool {
    let mut adapted = false;

    // RefusesFormat: schema 注入 system prompt, 移除 response_format
    if registry.is_unfit(provider, model, UnfitKind::RefusesFormat) {
        if let Some(schema) = extract_and_remove_response_format(body) {
            inject_schema_to_system(body, &schema);
            adapted = true;
            tracing::info!(
                provider,
                model,
                "unfit: response_format → system instruction"
            );
        }
    }

    // OffRefused: reasoning effort "off" → "low"
    if registry.is_unfit(provider, model, UnfitKind::OffRefused)
        && force_reasoning_effort(body, "low")
    {
        adapted = true;
        tracing::info!(provider, model, "unfit: reasoning effort off → low");
    }

    // ToolChoiceRefused: tool_choice → "auto"
    if registry.is_unfit(provider, model, UnfitKind::ToolChoiceRefused)
        && downgrade_tool_choice(body)
    {
        adapted = true;
        tracing::info!(provider, model, "unfit: tool_choice → auto");
    }

    // ToolsWithoutEffort: 移除 thinking/reasoning 配置
    if registry.is_unfit(provider, model, UnfitKind::ToolsWithoutEffort)
        && remove_reasoning_config(body)
    {
        adapted = true;
        tracing::info!(provider, model, "unfit: removed reasoning (tools conflict)");
    }

    adapted
}

fn extract_and_remove_response_format(body: &mut serde_json::Value) -> Option<serde_json::Value> {
    // Anthropic: output_config.format.json_schema
    if let Some(schema) = body.pointer("/output_config/format/json_schema") {
        let schema = schema.clone();
        body.as_object_mut()?.remove("output_config");
        return Some(schema);
    }
    // OpenAI: response_format.json_schema
    if let Some(schema) = body.pointer("/response_format/json_schema") {
        let schema = schema.clone();
        body.as_object_mut()?.remove("response_format");
        return Some(schema);
    }
    None
}

fn inject_schema_to_system(body: &mut serde_json::Value, schema: &serde_json::Value) {
    let instruction = format!(
        "\n\nIMPORTANT: Respond with valid JSON matching this schema:\n{}",
        serde_json::to_string_pretty(schema).unwrap_or_else(|_| schema.to_string())
    );

    match body.get_mut("system") {
        Some(serde_json::Value::String(s)) => {
            s.push_str(&instruction);
        }
        Some(serde_json::Value::Array(arr)) => {
            arr.push(serde_json::json!({"type": "text", "text": instruction}));
        }
        _ => {
            body["system"] = serde_json::json!(instruction);
        }
    }
}

fn force_reasoning_effort(body: &mut serde_json::Value, effort: &str) -> bool {
    // Anthropic: thinking.output_config.effort
    if let Some(config) = body.pointer_mut("/thinking/output_config/effort") {
        if config.as_str() == Some("off") {
            *config = serde_json::json!(effort);
            return true;
        }
    }
    false
}

fn downgrade_tool_choice(body: &mut serde_json::Value) -> bool {
    if let Some(choice) = body.get_mut("tool_choice") {
        if choice.get("type").and_then(|t| t.as_str()) != Some("auto") {
            *choice = serde_json::json!({"type": "auto"});
            return true;
        }
    }
    false
}

fn remove_reasoning_config(body: &mut serde_json::Value) -> bool {
    let mut removed = false;
    if body
        .as_object_mut()
        .and_then(|m| m.remove("thinking"))
        .is_some()
    {
        removed = true;
    }
    if let Some(config) = body.pointer_mut("/output_config") {
        if config
            .as_object_mut()
            .and_then(|m| m.remove("effort"))
            .is_some()
        {
            removed = true;
        }
    }
    removed
}
