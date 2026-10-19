// Unfit 状态机:自适应运行时错误探针(对齐 magpie gateway.go:3440-3490)
//
// 当上游报 400/422 拒绝时,自动识别不兼容特性并降级重试:
// - response_format 不支持 → schema 注入 system prompt
// - reasoning effort "off" 拒绝 → 强制 "low"
// - tool_choice 不支持 → 降级 "auto"
// - tools + reasoning 冲突 → 关闭 reasoning
//
// 状态记忆:(provider, model, feature) 三元组标记,避免重复试探

use ccextra_core::route::Protocol;
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
struct UnfitState {
    flags: HashMap<UnfitKey, Vec<UnfitKind>>,
    /// /reload clear 时递增;持旧世代的飞行请求 mark 会被丢弃
    generation: u64,
}

#[derive(Default)]
pub struct UnfitRegistry {
    state: RwLock<UnfitState>,
}

impl UnfitRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// 当前世代。仅供同 crate 内与 config 读锁成对捕获
    /// (见 AppState::snapshot_and_unfit_generation); 勿单独对外暴露。
    pub(crate) fn generation(&self) -> u64 {
        self.state.read().unwrap().generation
    }

    /// 标记 (provider, model) 不兼容某特性;已存在同 kind 时不重复插入。
    /// `at_generation` 与当前世代不一致时丢弃(防止 /reload clear 后旧飞行请求写回)。
    pub fn mark(&self, provider: &str, model: &str, kind: UnfitKind, at_generation: u64) {
        let mut state = self.state.write().unwrap();
        if at_generation != state.generation {
            return;
        }
        let key = (provider.to_string(), model.to_string());
        let kinds = state.flags.entry(key).or_default();
        if !kinds.contains(&kind) {
            kinds.push(kind);
        }
    }

    /// 检查是否已标记为不兼容
    pub fn is_unfit(&self, provider: &str, model: &str, kind: UnfitKind) -> bool {
        let key = (provider.to_string(), model.to_string());
        self.state
            .read()
            .unwrap()
            .flags
            .get(&key)
            .is_some_and(|kinds| kinds.contains(&kind))
    }

    /// /reload 时清空并增代;调用方须在持有 config 写锁时调用,
    /// 与请求侧同读锁捕获的 generation 成对原子。
    pub fn clear(&self) {
        let mut state = self.state.write().unwrap();
        state.flags.clear();
        state.generation = state.generation.wrapping_add(1);
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

/// 根据 unfit 标记降级请求体(对齐 magpie adaptForUnfit)。
/// `protocol` 决定改已转换 body 的字段形状(Chat/Responses 与 Claude 不同)。
pub fn adapt_request_for_unfit(
    body: &mut serde_json::Value,
    provider: &str,
    model: &str,
    registry: &UnfitRegistry,
    protocol: Protocol,
) -> bool {
    let mut adapted = false;

    // RefusesFormat: schema 注入 system/instructions, 移除 response_format
    if registry.is_unfit(provider, model, UnfitKind::RefusesFormat) {
        if let Some(schema) = extract_and_remove_response_format(body) {
            inject_schema_instruction(body, &schema, protocol);
            adapted = true;
            tracing::info!(
                provider,
                model,
                "unfit: response_format → system instruction"
            );
        }
    }

    // OffRefused: reasoning effort "off"/"none" → "low"
    if registry.is_unfit(provider, model, UnfitKind::OffRefused)
        && force_reasoning_effort(body, "low", protocol)
    {
        adapted = true;
        tracing::info!(provider, model, "unfit: reasoning effort off → low");
    }

    // ToolChoiceRefused: tool_choice → auto
    if registry.is_unfit(provider, model, UnfitKind::ToolChoiceRefused)
        && downgrade_tool_choice(body, protocol)
    {
        adapted = true;
        tracing::info!(provider, model, "unfit: tool_choice → auto");
    }

    // ToolsWithoutEffort: 移除 thinking/reasoning 配置
    if registry.is_unfit(provider, model, UnfitKind::ToolsWithoutEffort)
        && remove_reasoning_config(body, protocol)
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
    // OpenAI Chat: response_format.json_schema.schema(优先取真正 schema)
    if let Some(schema) = body.pointer("/response_format/json_schema/schema") {
        let schema = schema.clone();
        body.as_object_mut()?.remove("response_format");
        return Some(schema);
    }
    if let Some(schema) = body.pointer("/response_format/json_schema") {
        let schema = schema.clone();
        body.as_object_mut()?.remove("response_format");
        return Some(schema);
    }
    None
}

fn inject_schema_instruction(
    body: &mut serde_json::Value,
    schema: &serde_json::Value,
    protocol: Protocol,
) {
    let instruction = format!(
        "\n\nIMPORTANT: Respond with valid JSON matching this schema:\n{}",
        serde_json::to_string_pretty(schema).unwrap_or_else(|_| schema.to_string())
    );

    match protocol {
        Protocol::Claude => match body.get_mut("system") {
            Some(serde_json::Value::String(s)) => {
                s.push_str(&instruction);
            }
            Some(serde_json::Value::Array(arr)) => {
                arr.push(serde_json::json!({"type": "text", "text": instruction}));
            }
            _ => {
                body["system"] = serde_json::json!(instruction);
            }
        },
        Protocol::OpenAiChat => {
            if let Some(msgs) = body.get_mut("messages").and_then(|m| m.as_array_mut()) {
                if let Some(sys) = msgs
                    .iter_mut()
                    .find(|m| m.get("role").and_then(|r| r.as_str()) == Some("system"))
                {
                    match sys.get_mut("content") {
                        Some(serde_json::Value::String(s)) => s.push_str(&instruction),
                        Some(serde_json::Value::Array(arr)) => {
                            arr.push(serde_json::json!({"type": "text", "text": instruction}));
                        }
                        _ => {
                            sys["content"] = serde_json::json!(instruction);
                        }
                    }
                } else {
                    msgs.insert(
                        0,
                        serde_json::json!({"role": "system", "content": instruction}),
                    );
                }
            }
        }
        Protocol::OpenAiResponses => match body.get_mut("instructions") {
            Some(serde_json::Value::String(s)) => {
                s.push_str(&instruction);
            }
            _ => {
                body["instructions"] = serde_json::json!(instruction.trim_start_matches('\n'));
            }
        },
        // Gemini/AG: 本批不扩 thinkingConfig 大改;无匹配字段时 adapt 保持 false
        Protocol::Gemini | Protocol::Antigravity => {}
    }
}

fn force_reasoning_effort(body: &mut serde_json::Value, effort: &str, protocol: Protocol) -> bool {
    match protocol {
        Protocol::Claude => {
            if let Some(config) = body.pointer_mut("/thinking/output_config/effort") {
                if config.as_str() == Some("off") {
                    *config = serde_json::json!(effort);
                    return true;
                }
            }
            false
        }
        Protocol::OpenAiChat => {
            if let Some(re) = body.get_mut("reasoning_effort") {
                if matches!(re.as_str(), Some("off") | Some("none")) {
                    *re = serde_json::json!(effort);
                    return true;
                }
            }
            // Kimi: thinking.effort
            if let Some(te) = body.pointer_mut("/thinking/effort") {
                if matches!(te.as_str(), Some("off") | Some("none")) {
                    *te = serde_json::json!(effort);
                    return true;
                }
            }
            false
        }
        Protocol::OpenAiResponses => {
            if let Some(e) = body.pointer_mut("/reasoning/effort") {
                if matches!(e.as_str(), Some("off") | Some("none")) {
                    *e = serde_json::json!(effort);
                    return true;
                }
            }
            false
        }
        Protocol::Gemini | Protocol::Antigravity => false,
    }
}

fn downgrade_tool_choice(body: &mut serde_json::Value, protocol: Protocol) -> bool {
    match protocol {
        Protocol::Claude => {
            if let Some(choice) = body.get_mut("tool_choice") {
                if choice.get("type").and_then(|t| t.as_str()) != Some("auto") {
                    *choice = serde_json::json!({"type": "auto"});
                    return true;
                }
            }
            false
        }
        Protocol::OpenAiChat | Protocol::OpenAiResponses => {
            if let Some(choice) = body.get_mut("tool_choice") {
                if choice.as_str() == Some("auto") {
                    return false;
                }
                *choice = serde_json::json!("auto");
                return true;
            }
            false
        }
        Protocol::Gemini | Protocol::Antigravity => false,
    }
}

fn remove_reasoning_config(body: &mut serde_json::Value, protocol: Protocol) -> bool {
    match protocol {
        Protocol::Claude => {
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
        Protocol::OpenAiChat => {
            let mut removed = false;
            if let Some(obj) = body.as_object_mut() {
                if obj.remove("reasoning_effort").is_some() {
                    removed = true;
                }
                if obj.remove("thinking").is_some() {
                    removed = true;
                }
            }
            removed
        }
        Protocol::OpenAiResponses => body
            .as_object_mut()
            .and_then(|m| m.remove("reasoning"))
            .is_some(),
        Protocol::Gemini | Protocol::Antigravity => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mark_does_not_duplicate_kinds() {
        let reg = UnfitRegistry::new();
        let gen = reg.generation();
        reg.mark("p", "m", UnfitKind::RefusesFormat, gen);
        reg.mark("p", "m", UnfitKind::RefusesFormat, gen);
        reg.mark("p", "m", UnfitKind::OffRefused, gen);
        let state = reg.state.read().unwrap();
        assert_eq!(state.flags[&("p".into(), "m".into())].len(), 2);
    }

    #[test]
    fn clear_empties_registry() {
        let reg = UnfitRegistry::new();
        let gen = reg.generation();
        reg.mark("p", "m", UnfitKind::ToolChoiceRefused, gen);
        reg.clear();
        assert!(!reg.is_unfit("p", "m", UnfitKind::ToolChoiceRefused));
    }

    #[test]
    fn stale_generation_mark_does_not_pollute_after_clear() {
        let reg = UnfitRegistry::new();
        let stale = reg.generation();
        reg.clear();
        assert_ne!(stale, reg.generation());
        reg.mark("p", "m", UnfitKind::RefusesFormat, stale);
        assert!(!reg.is_unfit("p", "m", UnfitKind::RefusesFormat));
        let current = reg.generation();
        reg.mark("p", "m", UnfitKind::RefusesFormat, current);
        assert!(reg.is_unfit("p", "m", UnfitKind::RefusesFormat));
    }

    fn marked(kind: UnfitKind) -> UnfitRegistry {
        let reg = UnfitRegistry::new();
        let gen = reg.generation();
        reg.mark("p", "m", kind, gen);
        reg
    }

    #[test]
    fn adapt_claude_off_refused_still_uses_thinking_path() {
        let reg = marked(UnfitKind::OffRefused);
        let mut body = serde_json::json!({
            "thinking": {"output_config": {"effort": "off"}}
        });
        assert!(adapt_request_for_unfit(
            &mut body,
            "p",
            "m",
            &reg,
            Protocol::Claude
        ));
        assert_eq!(body["thinking"]["output_config"]["effort"], "low");
    }

    #[test]
    fn adapt_chat_off_refused_sets_reasoning_effort_low() {
        let reg = marked(UnfitKind::OffRefused);
        let mut body = serde_json::json!({"reasoning_effort": "off", "messages": []});
        assert!(adapt_request_for_unfit(
            &mut body,
            "p",
            "m",
            &reg,
            Protocol::OpenAiChat
        ));
        assert_eq!(body["reasoning_effort"], "low");
        // 不得误写 Anthropic 残留字段
        assert!(body.get("thinking").is_none());
    }

    #[test]
    fn adapt_chat_tools_without_effort_removes_reasoning_effort() {
        let reg = marked(UnfitKind::ToolsWithoutEffort);
        let mut body = serde_json::json!({
            "reasoning_effort": "high",
            "thinking": {"type": "enabled", "effort": "high"},
            "messages": []
        });
        assert!(adapt_request_for_unfit(
            &mut body,
            "p",
            "m",
            &reg,
            Protocol::OpenAiChat
        ));
        assert!(body.get("reasoning_effort").is_none());
        assert!(body.get("thinking").is_none());
    }

    #[test]
    fn adapt_chat_tool_choice_downgrades_to_string_auto() {
        let reg = marked(UnfitKind::ToolChoiceRefused);
        let mut body = serde_json::json!({
            "tool_choice": {"type": "function", "function": {"name": "x"}},
            "messages": []
        });
        assert!(adapt_request_for_unfit(
            &mut body,
            "p",
            "m",
            &reg,
            Protocol::OpenAiChat
        ));
        assert_eq!(body["tool_choice"], "auto");
    }

    #[test]
    fn adapt_chat_refuses_format_injects_into_system_message() {
        let reg = marked(UnfitKind::RefusesFormat);
        let mut body = serde_json::json!({
            "messages": [
                {"role": "system", "content": "base"},
                {"role": "user", "content": "hi"}
            ],
            "response_format": {
                "type": "json_schema",
                "json_schema": {"schema": {"type": "object"}}
            }
        });
        assert!(adapt_request_for_unfit(
            &mut body,
            "p",
            "m",
            &reg,
            Protocol::OpenAiChat
        ));
        assert!(body.get("response_format").is_none());
        let sys = body["messages"][0]["content"].as_str().unwrap();
        assert!(sys.starts_with("base"));
        assert!(sys.contains("IMPORTANT: Respond with valid JSON"));
        // 不得写顶层 Anthropic system
        assert!(body.get("system").is_none());
    }

    #[test]
    fn adapt_responses_off_refused_sets_reasoning_effort() {
        let reg = marked(UnfitKind::OffRefused);
        let mut body = serde_json::json!({
            "reasoning": {"effort": "off", "summary": "auto"}
        });
        assert!(adapt_request_for_unfit(
            &mut body,
            "p",
            "m",
            &reg,
            Protocol::OpenAiResponses
        ));
        assert_eq!(body["reasoning"]["effort"], "low");
    }
}
