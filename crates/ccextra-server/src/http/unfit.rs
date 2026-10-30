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

    /// 该 (provider, model) 是否已被标记任意 unfit。
    /// 供首发前预降级做单次读锁快判,避免无标记时逐一探测各 kind。
    pub fn has_any(&self, provider: &str, model: &str) -> bool {
        let key = (provider.to_string(), model.to_string());
        self.state
            .read()
            .unwrap()
            .flags
            .get(&key)
            .is_some_and(|kinds| !kinds.is_empty())
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

    // response_format / text.format 拒绝
    // (Gemini: "response_mime_type", Chat: "response_format", Responses: "text.format")
    if lower.contains("response_mime_type")
        || lower.contains("response_format")
        || lower.contains("text.format")
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

    // tools + reasoning 冲突(源自 magpie Bedrock 场景:thinking 与 tools 不共存)
    // 裸 contains("tool")+"reasoning" 过宽:普通错误含两词即触发,永久剥 reasoning
    // (仅 /reload 清)。要求显式冲突/不支持语义,降低误判。
    if (lower.contains("thinking") || lower.contains("reasoning"))
        && lower.contains("tool")
        && (lower.contains("conflict")
            || lower.contains("incompatible")
            || lower.contains("not supported")
            || lower.contains("unsupported")
            || lower.contains("cannot")
            || lower.contains("not allowed")
            || lower.contains("disabled"))
    {
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

    // RefusesFormat: schema 注入 system/instructions, 注入成功后剥 format 字段
    if registry.is_unfit(provider, model, UnfitKind::RefusesFormat) {
        // 先探测再注入:注入失败(协议无对应字段 / body 缺 messages)时
        // 不得剥掉 format,否则 body 被改却不会重试
        if let Some((schema, slot)) = response_format_schema(body) {
            if inject_schema_instruction(body, &schema, protocol) {
                strip_response_format_slot(body, slot);
                adapted = true;
                tracing::debug!(
                    provider,
                    model,
                    "unfit: response_format → system instruction"
                );
            }
        }
    }

    // OffRefused: reasoning effort "off"/"none" → "low"
    if registry.is_unfit(provider, model, UnfitKind::OffRefused)
        && force_reasoning_effort(body, "low", protocol)
    {
        adapted = true;
        tracing::debug!(provider, model, "unfit: reasoning effort off → low");
    }

    // ToolChoiceRefused: tool_choice → auto
    if registry.is_unfit(provider, model, UnfitKind::ToolChoiceRefused)
        && downgrade_tool_choice(body, protocol)
    {
        adapted = true;
        tracing::debug!(provider, model, "unfit: tool_choice → auto");
    }

    // ToolsWithoutEffort: 移除 thinking/reasoning 配置
    if registry.is_unfit(provider, model, UnfitKind::ToolsWithoutEffort)
        && remove_reasoning_config(body, protocol)
    {
        adapted = true;
        tracing::debug!(provider, model, "unfit: removed reasoning (tools conflict)");
    }

    adapted
}

/// 只读探测 structured-output schema 及剥离目标;不做修改。
/// 移除由调用方在注入成功后执行。
/// slot: `"output_config"` / `"response_format"` 为顶层整键;
/// `"text.format"` 仅剥 `text.format`,保留同级 `text.verbosity`。
fn response_format_schema(body: &serde_json::Value) -> Option<(serde_json::Value, &'static str)> {
    // Anthropic: output_config.format.json_schema
    if let Some(schema) = body.pointer("/output_config/format/json_schema") {
        return Some((schema.clone(), "output_config"));
    }
    // OpenAI Chat: response_format.json_schema.schema(优先取真正 schema)
    if let Some(schema) = body.pointer("/response_format/json_schema/schema") {
        return Some((schema.clone(), "response_format"));
    }
    if let Some(schema) = body.pointer("/response_format/json_schema") {
        return Some((schema.clone(), "response_format"));
    }
    // OpenAI Responses: text.format.schema(Claude→Responses 转换后的位置)
    if let Some(schema) = body.pointer("/text/format/schema") {
        return Some((schema.clone(), "text.format"));
    }
    None
}

/// 按 `response_format_schema` 返回的 slot 剥离 format。
/// `"text.format"` 只删 format;text 空则再删顶层 text,保留 verbosity。
fn strip_response_format_slot(body: &mut serde_json::Value, slot: &str) {
    match slot {
        "text.format" => {
            let text_empty =
                if let Some(text) = body.get_mut("text").and_then(|t| t.as_object_mut()) {
                    text.remove("format");
                    text.is_empty()
                } else {
                    false
                };
            if text_empty {
                if let Some(obj) = body.as_object_mut() {
                    obj.remove("text");
                }
            }
        }
        _ => {
            if let Some(obj) = body.as_object_mut() {
                obj.remove(slot);
            }
        }
    }
}

/// 返回是否真的注入了指令;协议无对应字段时返回 false,调用方不得置 adapted。
fn inject_schema_instruction(
    body: &mut serde_json::Value,
    schema: &serde_json::Value,
    protocol: Protocol,
) -> bool {
    let instruction = format!(
        "\n\nIMPORTANT: Respond with valid JSON matching this schema:\n{}",
        serde_json::to_string_pretty(schema).unwrap_or_else(|_| schema.to_string())
    );

    match protocol {
        Protocol::Claude => {
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
            true
        }
        Protocol::OpenAiChat => {
            let Some(msgs) = body.get_mut("messages").and_then(|m| m.as_array_mut()) else {
                return false;
            };
            // 转换后前缀消息可能是 system 或 developer(o 系列 / GPT-5 族),
            // 两者都算系统位;都未命中才新建,且用 system。
            let sys_pos = msgs.iter().position(|m| {
                matches!(
                    m.get("role").and_then(|r| r.as_str()),
                    Some("system") | Some("developer")
                )
            });
            match sys_pos {
                Some(pos) => {
                    let sys = &mut msgs[pos];
                    match sys.get_mut("content") {
                        Some(serde_json::Value::String(s)) => s.push_str(&instruction),
                        Some(serde_json::Value::Array(arr)) => {
                            arr.push(serde_json::json!({"type": "text", "text": instruction}));
                        }
                        _ => {
                            sys["content"] = serde_json::json!(instruction);
                        }
                    }
                }
                None => msgs.insert(
                    0,
                    serde_json::json!({"role": "system", "content": instruction}),
                ),
            }
            true
        }
        Protocol::OpenAiResponses => {
            match body.get_mut("instructions") {
                Some(serde_json::Value::String(s)) => {
                    s.push_str(&instruction);
                }
                _ => {
                    body["instructions"] = serde_json::json!(instruction.trim_start_matches('\n'));
                }
            }
            true
        }
        // Gemini/AG: 无 responseMimeType 路径,不注入
        Protocol::Gemini | Protocol::Antigravity => false,
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

    /// 首发前预降级依赖幂等:第二次调用不得再改 body,也不得重复报 adapted。
    #[test]
    fn adapt_is_idempotent_for_second_call() {
        let reg = marked(UnfitKind::RefusesFormat);
        let mut body = serde_json::json!({
            "messages": [
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
        let once = body.clone();
        assert!(!adapt_request_for_unfit(
            &mut body,
            "p",
            "m",
            &reg,
            Protocol::OpenAiChat
        ));
        assert_eq!(body, once);
    }

    /// o 系列 / GPT-5 族转换后前缀是 developer,注入须落到该消息,
    /// 不得再插一条 system 造成双前缀。
    #[test]
    fn adapt_chat_refuses_format_injects_into_developer_message() {
        let reg = marked(UnfitKind::RefusesFormat);
        let mut body = serde_json::json!({
            "messages": [
                {"role": "developer", "content": [{"type": "text", "text": "base"}]},
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
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2, "不得插入额外前缀消息");
        assert_eq!(msgs[0]["role"], "developer");
        let parts = msgs[0]["content"].as_array().unwrap();
        assert_eq!(parts[0]["text"], "base");
        assert!(parts[1]["text"]
            .as_str()
            .unwrap()
            .contains("IMPORTANT: Respond with valid JSON"));
    }

    /// Gemini/AG 无注入字段:schema 探测得到但注入失败,不得置 adapted,
    /// 也不得剥掉原 response_format(body 原样返回)。
    #[test]
    fn adapt_gemini_refuses_format_does_not_claim_adaptation() {
        let reg = marked(UnfitKind::RefusesFormat);
        let original = serde_json::json!({
            "contents": [{"role": "user", "parts": [{"text": "hi"}]}],
            "response_format": {
                "type": "json_schema",
                "json_schema": {"schema": {"type": "object"}}
            }
        });
        let mut body = original.clone();
        assert!(!adapt_request_for_unfit(
            &mut body,
            "p",
            "m",
            &reg,
            Protocol::Gemini
        ));
        assert_eq!(body, original, "response_format 不得被剥掉");
    }

    /// Chat 形 body 缺 messages 时注入失败,同样不得剥 response_format。
    #[test]
    fn adapt_chat_refuses_format_without_messages_keeps_response_format() {
        let reg = marked(UnfitKind::RefusesFormat);
        let original = serde_json::json!({
            "response_format": {
                "type": "json_schema",
                "json_schema": {"schema": {"type": "object"}}
            }
        });
        let mut body = original.clone();
        assert!(!adapt_request_for_unfit(
            &mut body,
            "p",
            "m",
            &reg,
            Protocol::OpenAiChat
        ));
        assert_eq!(body, original);
    }

    /// Responses 结构化输出在 text.format;RefusesFormat 须注入 instructions、
    /// 只剥 format,同级 verbosity 保留。
    #[test]
    fn adapt_responses_refuses_format_injects_instructions_keeps_verbosity() {
        let reg = marked(UnfitKind::RefusesFormat);
        let mut body = serde_json::json!({
            "instructions": "base",
            "text": {
                "format": {
                    "type": "json_schema",
                    "name": "cli_proxy_structured_output",
                    "strict": true,
                    "schema": {"type": "object", "properties": {"answer": {"type": "string"}}}
                },
                "verbosity": "low"
            }
        });
        assert!(adapt_request_for_unfit(
            &mut body,
            "p",
            "m",
            &reg,
            Protocol::OpenAiResponses
        ));
        assert!(
            body.pointer("/text/format").is_none(),
            "text.format 须被剥掉"
        );
        assert_eq!(body["text"]["verbosity"], "low", "verbosity 须保留");
        let instr = body["instructions"].as_str().unwrap();
        assert!(instr.starts_with("base"));
        assert!(instr.contains("IMPORTANT: Respond with valid JSON"));
        assert!(instr.contains("answer"));
        // 不得误删顶层 text(verbosity 还在)
        assert!(body.get("text").is_some());
    }

    /// 仅 text.format、无 verbosity 时,剥完后顶层 text 应一并移除。
    #[test]
    fn adapt_responses_refuses_format_removes_empty_text_object() {
        let reg = marked(UnfitKind::RefusesFormat);
        let mut body = serde_json::json!({
            "text": {
                "format": {
                    "type": "json_schema",
                    "name": "out",
                    "strict": true,
                    "schema": {"type": "object"}
                }
            }
        });
        assert!(adapt_request_for_unfit(
            &mut body,
            "p",
            "m",
            &reg,
            Protocol::OpenAiResponses
        ));
        assert!(body.get("text").is_none());
        assert!(body["instructions"]
            .as_str()
            .unwrap()
            .contains("IMPORTANT: Respond with valid JSON"));
    }

    /// 首发预降级幂等:第二次不得再改 body / 报 adapted。
    #[test]
    fn adapt_responses_refuses_format_is_idempotent() {
        let reg = marked(UnfitKind::RefusesFormat);
        let mut body = serde_json::json!({
            "instructions": "base",
            "text": {
                "format": {
                    "type": "json_schema",
                    "schema": {"type": "object"}
                },
                "verbosity": "low"
            }
        });
        assert!(adapt_request_for_unfit(
            &mut body,
            "p",
            "m",
            &reg,
            Protocol::OpenAiResponses
        ));
        let once = body.clone();
        assert!(!adapt_request_for_unfit(
            &mut body,
            "p",
            "m",
            &reg,
            Protocol::OpenAiResponses
        ));
        assert_eq!(body, once);
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

    #[test]
    fn detect_tools_without_effort_requires_conflict_keyword() {
        // 真实误触发反例(收紧前会误判):不得触发
        assert_eq!(
            detect_unfit_from_error(
                400,
                // Anthropic 原文
                "Expected `thinking` content block but found `tool_use`"
            ),
            None,
        );
        assert_eq!(
            detect_unfit_from_error(
                400,
                // Kimi 原文
                "reasoning_content is missing in assistant tool call"
            ),
            None,
        );
        // 含 "tool" + "reasoning" 但无冲突词 → 不触发
        assert_eq!(
            detect_unfit_from_error(
                400,
                "The tool 'analyze' requires additional reasoning context"
            ),
            None,
        );
        // Bedrock 原文正例:含 "not supported" → 触发
        assert_eq!(
            detect_unfit_from_error(400, "reasoning is not supported with tools"),
            Some(UnfitKind::ToolsWithoutEffort),
        );
        assert_eq!(
            detect_unfit_from_error(400, "thinking and tools are incompatible"),
            Some(UnfitKind::ToolsWithoutEffort),
        );
        assert_eq!(
            detect_unfit_from_error(400, "cannot use reasoning when tools are present"),
            Some(UnfitKind::ToolsWithoutEffort),
        );
        // 422 同样触发
        assert_eq!(
            detect_unfit_from_error(422, "tool_calls disabled when thinking is enabled"),
            Some(UnfitKind::ToolsWithoutEffort),
        );
        // 非 400/422 不触发
        assert_eq!(
            detect_unfit_from_error(500, "reasoning not supported with tools"),
            None,
        );
    }
}
