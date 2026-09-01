// 归一化:编排九模块
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
    normalize_tool_definitions_responses, sort_schema_keys_recursive, sort_tools_deterministically,
};
use crate::cache_stabilization::tool_input_normalize::normalize_tool_use_inputs;
use crate::cache_stabilization::volatile_detector::{
    detect_volatile_content, emit_volatile_warnings, normalize_client_dateline,
    ApiKind as VolatileApiKind,
};

pub use crate::cache_stabilization::truncate_tool_results::UpstreamTruncation;

/// 归一化命中的计数,便于测试与日志
///
/// `volatile_count` 现在计 dateline 指纹句改写个数(撇号/分隔符隐写还原,
/// 对齐 sub2api);占位符式 volatile strip 已下线。检测告警仍跑(full
/// 管线只读 WARN)。pretransform 不跑 dateline,该字段恒 0。
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct NormalizeCounts {
    pub tool_sorted: bool,
    pub smoosh_count: usize,
    pub bookkeeping_count: usize,
    pub tool_input_count: usize,
    pub sort_count: usize,
    pub rstrip_count: usize,
    /// dateline 指纹句改写的文本块个数
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
/// 顺序:
/// 1. tool_def 排序 + schema 键递归排序(仅当无 cache_control 时)
/// 2. smoosh 拆分(剥掉 tool_result.content 里的 reminder)
/// 3. bookkeeping 剥离(历史 user 消息的每轮 reminder)
/// 4. tool_use.input 键序归一化
/// 5. system reminder 列表块排序
/// 6. 尾部 reminder 空白归一化
/// 7. 客户端 dateline 归一化(仅全量;撇号/分隔符隐写还原,对齐 sub2api)
/// 8. cache_control 自动注入(仅全量)
/// 9. volatile 检测→告警(只读,不改 body)
///
/// drift 观测(compute_structural_hash + observe_drift)需要 DriftState,
/// 由 server 层在调用本函数后单独执行。
pub fn normalize_anthropic_full(body: &mut Value) -> NormalizeCounts {
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

    // 7. 客户端 dateline 归一化(撇号/分隔符隐写还原,对齐 sub2api)
    counts.volatile_count = normalize_client_dateline(body, VolatileApiKind::Anthropic);

    // 8. cache_control 自动注入
    if let AutoPlaceOutcome::Applied { placed_count, .. } = auto_place_anthropic_cache_control(body)
    {
        counts.cache_control_placed = placed_count;
    }

    // 9. volatile 检测→告警(只读,不改 body)
    let findings = detect_volatile_content(body, VolatileApiKind::Anthropic);
    if !findings.is_empty() {
        emit_volatile_warnings(&findings, "unknown");
    }

    counts
}

/// 入站 anthropic 转换前归一化:精简子集(openai 转换链路转换前调用)。
///
/// 跑:tool_def 排序 + schema 键排序 → smoosh → content_strip → tool_input → sort → rstrip。
/// tool_def 排序提升 gemini/antigravity 隐式前缀缓存命中率(对齐 full 管线)。
/// schema 键必须在 tool_input 之前排:转换把 `tool_use.input` 冻成
/// `arguments` 字符串,post 无法再改。跳过 volatile strip、auto cache_control、
/// volatile detect warn——cache_control 转换时丢弃;volatile / drift 留转换后。
pub fn normalize_anthropic_pretransform(body: &mut Value) -> NormalizeCounts {
    let mut tool_sorted = false;

    // tool_def 排序 + schema 键递归排序
    if let Some(tools) = body.get_mut("tools").and_then(|t| t.as_array_mut()) {
        if !any_tool_has_cache_control(tools) {
            tool_sorted = sort_tools_deterministically(tools);
        }
        for tool in tools.iter_mut() {
            if let Some(schema) = tool.get_mut("input_schema") {
                sort_schema_keys_recursive(schema);
            }
        }
    }

    NormalizeCounts {
        tool_sorted,
        // 1. smoosh 拆分
        smoosh_count: split_smooshed_reminders(body, DriftApiKind::Anthropic),
        // 2. bookkeeping 剥离
        bookkeeping_count: strip_bookkeeping_content(body, DriftApiKind::Anthropic),
        // 3. tool_use.input 键序归一化
        tool_input_count: normalize_tool_use_inputs(body, DriftApiKind::Anthropic),
        // 4. system reminder 列表块排序
        sort_count: stabilize_block_sort(body, DriftApiKind::Anthropic),
        // 5. 尾部 reminder 空白归一化
        rstrip_count: normalize_reminder_trailing_whitespace(body, DriftApiKind::Anthropic),
        ..Default::default()
    }
}

/// 转换后目标 body 二次归一化:tool_def + sort + rstrip + dateline 归一化
///
/// 顺序(post-transform):
/// 1. tool_def 归一化(按 chat / responses 形状区分)
/// 2. system reminder 列表块排序(CC 注入的 skills/deferred 列表顺序不稳定)
/// 3. 尾部 reminder 空白归一化(CC 重序列化历史内容时字节漂移 #48734)
/// 4. 客户端 dateline 归一化(撇号/分隔符隐写还原,对齐 sub2api)
///
/// tool_result 截断不在本管线:策略须按 payload 覆盖后的出站模型判定,
/// 由 server 层在 apply_payload_overrides 之后单独调用
/// truncate_tool_results::truncate。
///
/// 排序在 rstrip 前,两条归一化都落在 drift 检测前。
pub fn normalize_target_post(body: &mut Value, shape: TargetShape) -> NormalizeCounts {
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

    // 2. system reminder 列表块排序(按 openai 形状匹配 walker)
    let kind = match shape {
        TargetShape::OpenAiChat => DriftApiKind::OpenAiChat,
        TargetShape::OpenAiResponses => DriftApiKind::OpenAiResponses,
    };
    counts.sort_count = stabilize_block_sort(body, kind);

    // 3. 尾部 reminder 空白归一化
    counts.rstrip_count = normalize_reminder_trailing_whitespace(body, kind);

    // 4. 客户端 dateline 归一化(openai 两种形状共用;对齐 sub2api)
    counts.volatile_count = normalize_client_dateline(body, VolatileApiKind::OpenAi);

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

        let counts = normalize_anthropic_full(&mut body);

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

        let counts = normalize_anthropic_full(&mut body);

        assert!(counts.cache_control_placed > 0);
    }

    #[test]
    fn test_pretransform_runs_history_subset() {
        // 转换前:bookkeeping 剥离生效
        let mut body = json!({
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "work"},
                    {"type": "text", "text": "<system-reminder>\nToken usage: 1/2; 1 remaining\n</system-reminder>"}
                ]}
            ]
        });

        let counts = normalize_anthropic_pretransform(&mut body);

        assert!(counts.bookkeeping_count > 0, "bookkeeping should strip");
        // 无 tool-def sort / volatile / cache_control 注入
        assert!(!counts.tool_sorted);
        assert_eq!(counts.volatile_count, 0);
        assert_eq!(counts.cache_control_placed, 0);
    }

    #[test]
    fn test_pretransform_rstrips_reminder_trailing() {
        // 转换前:rstrip 折叠尾部空白(独立块,不经 bookkeeping)
        let mut body = json!({
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "work\n   </system-reminder>   "}
                ]}
            ]
        });

        let counts = normalize_anthropic_pretransform(&mut body);

        assert!(
            counts.rstrip_count > 0,
            "trailing whitespace should collapse"
        );
        let content = body["messages"][0]["content"][0]["text"].as_str().unwrap();
        assert!(
            content.ends_with("</system-reminder>"),
            "collapsed: {content:?}"
        );
    }

    #[test]
    fn test_pretransform_skips_cache_control_injection() {
        // 没有 cache_control 注入(转换前不注入:转换后 cache 语义丢弃)
        let mut body = json!({
            "tools": [
                {"name": "b", "input_schema": {"type": "object"}},
                {"name": "a", "input_schema": {"type": "object"}}
            ],
            "messages": [{"role": "user", "content": "hi"}]
        });

        let counts = normalize_anthropic_pretransform(&mut body);

        assert_eq!(counts.cache_control_placed, 0);
        // pretransform 现在排序 tool_def(提升 gemini/antigravity 缓存命中)
        assert!(
            counts.tool_sorted,
            "tool-def should be sorted for prefix stability"
        );
        assert_eq!(body["tools"][0]["name"], "a");
        assert_eq!(body["tools"][1]["name"], "b");
    }

    #[test]
    fn test_pretransform_sorts_schema_keys_before_tool_input() {
        // properties 键序跨轮抖动时,tool_use.input 必须按排好的
        // schema 重排,否则 convert 冻进 arguments 的字符串会 miss。
        let mk = |props: Value, input: Value| {
            json!({
                "tools": [{
                    "name": "edit_file",
                    "input_schema": {"type": "object", "properties": props}
                }],
                "messages": [{
                    "role": "assistant",
                    "content": [{
                        "type": "tool_use",
                        "id": "t1",
                        "name": "edit_file",
                        "input": input
                    }]
                }]
            })
        };
        let mut a = mk(
            json!({"content": {"type": "string"}, "path": {"type": "string"}}),
            json!({"content": "c", "path": "/p"}),
        );
        let mut b = mk(
            json!({"path": {"type": "string"}, "content": {"type": "string"}}),
            json!({"path": "/p", "content": "c"}),
        );

        let counts_a = normalize_anthropic_pretransform(&mut a);
        normalize_anthropic_pretransform(&mut b);

        let keys = |body: &Value| -> Vec<String> {
            body["messages"][0]["content"][0]["input"]
                .as_object()
                .unwrap()
                .keys()
                .cloned()
                .collect()
        };
        assert_eq!(keys(&a), keys(&b));
        assert_eq!(keys(&a), vec!["content", "path"]);
        let props: Vec<String> = a["tools"][0]["input_schema"]["properties"]
            .as_object()
            .unwrap()
            .keys()
            .cloned()
            .collect();
        assert_eq!(props, vec!["content", "path"]);
        // 单工具场景无需排序(已有序),返回 false
        assert!(!counts_a.tool_sorted);
    }

    #[test]
    fn test_pretransform_sorts_multiple_tools() {
        // 多工具场景验证 pretransform 确实排序 tools 数组
        let mut body = json!({
            "tools": [
                {"name": "z_tool", "input_schema": {"type": "object"}},
                {"name": "a_tool", "input_schema": {"type": "object"}},
                {"name": "m_tool", "input_schema": {"type": "object"}}
            ],
            "messages": [{"role": "user", "content": "hi"}]
        });

        let counts = normalize_anthropic_pretransform(&mut body);

        assert!(counts.tool_sorted, "multiple tools should be sorted");
        assert_eq!(body["tools"][0]["name"], "a_tool");
        assert_eq!(body["tools"][1]["name"], "m_tool");
        assert_eq!(body["tools"][2]["name"], "z_tool");
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

        let counts = normalize_target_post(&mut body, TargetShape::OpenAiChat);

        assert!(counts.tool_sorted);
    }

    #[test]
    fn test_target_post_chat_sorts_skill_listing() {
        // system 里技能列表乱序 → stabilize_block_sort 应排序
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "system", "content": "<system-reminder>\nThe following skills are available:\n- zed\n- alpha\n- mid\n</system-reminder>"},
                {"role": "user", "content": "hi"}
            ]
        });

        let counts = normalize_target_post(&mut body, TargetShape::OpenAiChat);

        assert!(counts.sort_count > 0, "skill listing should be sorted");
        let content = body["messages"][0]["content"].as_str().unwrap();
        assert!(
            content.contains("- alpha\n- mid\n- zed"),
            "items sorted: {content}"
        );
    }

    #[test]
    fn test_target_post_chat_rstrip_reminder_trailing() {
        // 尾部 </system-reminder> 前多余空白 → rstrip 折叠
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "user", "content": "work\n   </system-reminder>   "}
            ]
        });

        let counts = normalize_target_post(&mut body, TargetShape::OpenAiChat);

        assert!(
            counts.rstrip_count > 0,
            "trailing whitespace should collapse"
        );
        let content = body["messages"][0]["content"].as_str().unwrap();
        assert!(
            content.ends_with("</system-reminder>"),
            "collapsed: {content:?}"
        );
    }

    #[test]
    fn test_target_post_is_idempotent() {
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "system", "content": "<system-reminder>\nThe following skills are available:\n- zed\n- alpha\n</system-reminder>"},
                {"role": "user", "content": "work\n   </system-reminder>   "}
            ]
        });

        let c1 = normalize_target_post(&mut body, TargetShape::OpenAiChat);
        let snapshot = body.clone();
        let c2 = normalize_target_post(&mut body, TargetShape::OpenAiChat);

        assert!(c1.sort_count > 0, "skill listing should sort");
        assert!(c1.rstrip_count > 0, "trailing whitespace should collapse");
        assert_eq!(c2.sort_count, 0, "second pass must not re-sort");
        assert_eq!(c2.rstrip_count, 0, "second pass must not re-rstrip");
        assert_eq!(body, snapshot, "second pass must be byte-identical");
    }
}
