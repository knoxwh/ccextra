// 归一化:编排 tklite 九模块
//
// 三条管线:
// - normalize_anthropic_full:  入站 anthropic 转换前,九模块全跑
// - normalize_target_post:     转换后目标 body 二次归一化(openai chat / responses)
//
// 说明:
// - 本模块只做纯函数变更(改 body),不持有 DriftState。
// - drift 观测(observe_drift)需要状态,由 server 层调用,此处不处理。

use serde_json::Value;

use crate::cache_stabilization::anthropic_cache_control::{
    auto_place_anthropic_cache_control, AutoPlaceOutcome,
};
use crate::cache_stabilization::content_strip::strip_bookkeeping_content;
use crate::cache_stabilization::drift_detector::ApiKind as DriftApiKind;
use crate::cache_stabilization::reminder_rstrip::normalize_reminder_trailing_whitespace;
use crate::cache_stabilization::smoosh_split::split_smooshed_reminders;
use crate::cache_stabilization::sort_stabilization::stabilize_block_sort;
use crate::cache_stabilization::tool_def_normalize::{
    any_tool_has_cache_control, normalize_tool_definitions_openai_chat,
    normalize_tool_definitions_responses, sort_schema_keys_recursive,
    sort_tools_deterministically,
};
use crate::cache_stabilization::tool_input_normalize::normalize_tool_use_inputs;
use crate::cache_stabilization::volatile_detector::{
    detect_volatile_content, emit_volatile_warnings, strip_volatile_from_prefix,
    ApiKind as VolatileApiKind,
};

/// 归一化命中的计数,便于测试与日志
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct NormalizeCounts {
    pub tool_sorted: bool,
    pub smoosh_count: usize,
    pub bookkeeping_count: usize,
    pub tool_input_count: usize,
    pub sort_count: usize,
    pub rstrip_count: usize,
    pub volatile_count: usize,
    pub cache_control_placed: usize,
}

/// 转换后目标 body 的形状,决定 post-transform 用哪套工具归一化
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TargetShape {
    OpenAiChat,
    OpenAiResponses,
}

/// 入站 anthropic 转换前归一化:九模块全跑
///
/// 顺序与 tklite Full 一致:
/// 1. tool_def 排序 + schema 键递归排序(仅当无 cache_control 时)
/// 2. smoosh 拆分(剥掉 tool_result.content 里的 reminder)
/// 3. bookkeeping 剥离(历史 user 消息的每轮 reminder)
/// 4. tool_use.input 键序归一化
/// 5. system reminder 列表块排序
/// 6. 尾部 reminder 空白归一化
/// 7. 前缀 volatile 剥离(仅全量)
/// 8. cache_control 自动注入(仅全量)
/// 9. volatile 检测→告警(只读,不改 body)
///
/// drift 观测(compute_structural_hash + observe_drift)需要 DriftState,
/// 由 server 层在调用本函数后单独执行。
pub fn normalize_anthropic_full(body: &mut Value, _session_key: &str) -> NormalizeCounts {
    let mut counts = NormalizeCounts::default();

    // 1. tool_def 排序 + schema 键递归排序
    if let Some(tools) = body.get_mut("tools").and_then(|t| t.as_array_mut()) {
        if !any_tool_has_cache_control(tools) {
            counts.tool_sorted = sort_tools_deterministically(tools);
        }
        for tool in tools.iter_mut() {
            if let Some(schema) = tool.get_mut("input_schema") {
                sort_schema_keys_recursive(schema);
            }
        }
    }

    // 2. smoosh 拆分
    counts.smoosh_count = split_smooshed_reminders(body, DriftApiKind::Anthropic);

    // 3. bookkeeping 剥离
    counts.bookkeeping_count = strip_bookkeeping_content(body, DriftApiKind::Anthropic);

    // 4. tool_use.input 键序归一化
    counts.tool_input_count = normalize_tool_use_inputs(body, DriftApiKind::Anthropic);

    // 5. system reminder 列表块排序
    counts.sort_count = stabilize_block_sort(body, DriftApiKind::Anthropic);

    // 6. 尾部 reminder 空白归一化
    counts.rstrip_count = normalize_reminder_trailing_whitespace(body, DriftApiKind::Anthropic);

    // 7. 前缀 volatile 剥离
    counts.volatile_count = strip_volatile_from_prefix(body, VolatileApiKind::Anthropic);

    // 8. cache_control 自动注入
    if let AutoPlaceOutcome::Applied { placed_count, .. } =
        auto_place_anthropic_cache_control(body)
    {
        counts.cache_control_placed = placed_count;
    }

    // 9. volatile 检测→告警(只读)
    let findings = detect_volatile_content(body, VolatileApiKind::Anthropic);
    if !findings.is_empty() {
        emit_volatile_warnings(&findings, "unknown");
    }

    counts
}

/// 转换后目标 body 二次归一化:tool_def + volatile 剥离
///
/// 顺序与 tklite openai 管线一致:
/// 1. tool_def 归一化(按 chat / responses 形状区分)
/// 2. volatile 剥离(工具参数里残留的时间戳/UUID)
pub fn normalize_target_post(body: &mut Value, _session_key: &str, shape: TargetShape) -> NormalizeCounts {
    let mut counts = NormalizeCounts::default();

    // 1. tool_def 归一化
    match shape {
        TargetShape::OpenAiChat => {
            let applied = normalize_tool_definitions_openai_chat(body);
            counts.tool_sorted = applied.e1_tool_sort || applied.e2_schema_sort;
        }
        TargetShape::OpenAiResponses => {
            let applied = normalize_tool_definitions_responses(body);
            counts.tool_sorted = applied.e1_tool_sort || applied.e2_schema_sort;
        }
    }

    // 2. volatile 剥离(openai 两种形状共用同一 walker)
    counts.volatile_count = strip_volatile_from_prefix(body, VolatileApiKind::OpenAi);

    counts
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_anthropic_full_sorts_tools() {
        let mut body = json!({
            "model": "test",
            "tools": [
                {"name": "b_tool", "input_schema": {"type": "object"}},
                {"name": "a_tool", "input_schema": {"type": "object"}}
            ],
            "messages": [{"role": "user", "content": "hi"}]
        });

        let counts = normalize_anthropic_full(&mut body, "key");

        assert!(counts.tool_sorted);
        assert_eq!(body["tools"][0]["name"], "a_tool");
    }

    #[test]
    fn test_anthropic_full_places_cache_control() {
        let mut body = json!({
            "model": "test",
            "tools": [{"name": "a", "input_schema": {"type": "object"}}],
            "messages": [{"role": "user", "content": "hi"}]
        });

        let counts = normalize_anthropic_full(&mut body, "key");

        assert!(counts.cache_control_placed > 0);
    }

    #[test]
    fn test_target_post_chat_sorts_tools() {
        let mut body = json!({
            "model": "test",
            "tools": [
                {"type": "function", "function": {"name": "b", "parameters": {"type": "object"}}},
                {"type": "function", "function": {"name": "a", "parameters": {"type": "object"}}}
            ],
            "messages": [{"role": "user", "content": "hi"}]
        });

        let counts = normalize_target_post(&mut body, "key", TargetShape::OpenAiChat);

        assert!(counts.tool_sorted);
    }
}