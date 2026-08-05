//! PR-E3：Anthropic `cache_control` 自动放置。
//!
//! Anthropic 的 prompt 缓存是按内容块选择加入的：除非请求体在至少一个
//! 块上显式携带 `cache_control: {"type": "ephemeral"}`，否则什么都不缓存。
//! 复杂的客户端（例如 Claude Code）会自行放置这些标记；而较简单的调用方
//! （手写 SDK 代码、像 Aider/Continue 这样的较小的代理、纯 `curl`）
//! 通常甚至不知道 `cache_control` 的存在。对于*那些*客户端，我们可以通过在请求中
//! 复用率最高的内容块上插入标记，以零学习成本让它们获得缓存命中。
//!
//! # 安全契约
//!
//! 本模块是唯一**修改请求字节**的 Phase E 面 —— 这正是它的意义所在。
//! 我们叠加了两个独立的闸门：
//!
//! 1. **客户放置优先闸门。** 如果在调用方交给我们的请求体的任何位置发现
//!    *任意* `cache_control` 标记，我们就返回 [`AutoPlaceOutcome::Skipped {
//!    reason: SkipReason::MarkerPresent }`] 且绝不修改。这会遍历 `system`
//!    （字符串或块数组）、`messages[].content`（字符串或块数组）以及
//!    `tools[]`（每个工具的顶层）。客户的缓存布局由客户自己掌控。
//! 2. **幂等性。** 对已经带有我们本会添加的标记的请求体重新运行，会通过
//!    闸门 (1) 变成无操作 —— 之前放置的标记在下一次遍历时成为
//!    客户放置优先的信号。
//!
//! 每次跳过/应用都会发出结构化的 `tracing::info!`，带有
//! `event = "e3_skipped"` 或 `event = "e3_applied"`，使运维人员可以
//! 确认闸门按设计触发。
//!
//! # 放置策略（本地优先：最多 3 个标记）
//!
//! Anthropic 的 `cache_control` 语义：块上的标记在规范请求顺序
//! （`tools → system → messages`）中缓存*该块及其之前的所有内容*。
//! 每个缓存的 prefix 持续 5 分钟。一个请求最多可携带 **4** 个标记
//! （Anthropic 的硬性限制）。
//!
//! 此本地 sidecar 最多使用 **3** 个标记，为安全/余量保留第 4 槽位：
//!
//!   1. **最后一个工具定义（顶层）。** 缓存整个 `tools` 数组 ——
//!      跨轮次复用率最高。
//!   2. **system prompt 的最后一块。** 缓存 `tools + system`。
//!      仅在 `system` 已经是块数组时触发；我们**不会**把纯字符串的
//!      `system` 转换为数组。
//!   3. **最后一条用户消息的最后一块**，当对话已有 ≥ 2 条消息时
//!      （缓存最终 assistant/tool_result 尾部之前的所有内容）。
//!   4. 第四个槽位保留 —— 出于安全不放置。
//!
//! # 标记形态
//!
//! ```json
//! {"cache_control": {"type": "ephemeral"}}
//! ```
//!
//! 无 TTL 字段 —— 5 分钟默认值对这些放置是合适的（工具在 5 分钟内
//! 很少更换；更长的 TTL 需要按租户进行容量规划，而此本地 sidecar
//! 并不需要）。

use serde_json::{json, Value};

/// 一次自动放置尝试的结果。
///
/// 由 [`auto_place_anthropic_cache_control`] 返回。调用方使用变体 +
/// 计数发出结构化遥测。变体保持精简 —— 一个带计数的成功路径，
/// 一个带机器可读原因的跳过。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AutoPlaceOutcome {
    /// 至少插入了一个 `cache_control` 标记。`placed_count` 是本次调用
    /// 添加的标记数量。采用三槽位策略时，它在 `0`（未找到有效目标）
    /// 到 `3`（tools、system 和一条符合条件的用户消息都收到了标记）之间。
    Applied {
        /// 本次调用添加的标记数量。
        placed_count: usize,
        /// 标记被放置的 JSON-pointer 风格位置。用于仪表盘识别哪些
        /// 槽位触发最多的稳定标识符。示例：`"tools[3]"`。
        locations: Vec<String>,
    },
    /// 我们没有修改。`reason` 告诉仪表盘哪个闸门触发了。
    Skipped {
        /// 我们为何跳过 —— 完整集合见 [`SkipReason`]。
        reason: SkipReason,
    },
}

/// E3 拒绝放置标记的原因。
///
/// 封闭枚举，使结构化的 `event = "e3_skipped"` 日志携带稳定的
/// `reason` 字段。仪表盘依据这些字符串过滤。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkipReason {
    /// 请求体中已存在至少一个 `cache_control` 标记
    /// （`system`、任意消息块、任意工具顶层）。
    /// 客户放置优先。
    MarkerPresent,
}

impl SkipReason {
    /// 结构化日志 `reason` 字段的稳定字符串。
    pub fn as_str(self) -> &'static str {
        match self {
            SkipReason::MarkerPresent => "marker_present",
        }
    }
}

/// 遍历 `body`，若任何位置出现 `cache_control` 字段则返回 `true`。
/// 公开以便调用方无需经过完整修改路径即可提前跳过
/// （例如在它自己的闸门上想记录不同的 `reason` 字段时）。
///
/// 遍历器检查 Anthropic 请求 schema 允许 `cache_control` 的三个位置：
///
/// - `body.system` —— 当它是内容块数组时（字符串形式无法携带标记）。
/// - `body.messages[].content` —— 当它是块数组时。
/// - `body.tools[]` —— 每个工具定义的顶层。
///
/// 我们**不会**下钻到任意嵌套对象。Anthropic 识别 `cache_control` 的
/// 唯一形态是上述文档化面；下钻到工具 `input_schema` 等会对恰好把
/// 该字段名作为属性键提及的客户 JSON Schema 产生误报。
fn any_anthropic_cache_control(body: &Value) -> bool {
    // ── system：字符串或块数组 ─────────────────────────
    // 只有数组形式能携带标记 —— 字符串形式被显式跳过
    // （无法携带 `cache_control` 字段）。
    if let Some(Value::Array(blocks)) = body.get("system") {
        for block in blocks {
            if block_has_cache_control(block) {
                return true;
            }
        }
    }

    // ── messages[].content：字符串或块数组 ─────────────
    if let Some(Value::Array(messages)) = body.get("messages") {
        for msg in messages {
            if let Some(Value::Array(blocks)) = msg.get("content") {
                for block in blocks {
                    if block_has_cache_control(block) {
                        return true;
                    }
                }
            }
            // 字符串形式：无法携带标记 —— 跳过。
        }
    }

    // ── tools[]：每个工具的顶层字段 ─────────────────────
    if let Some(Value::Array(tools)) = body.get("tools") {
        for tool in tools {
            if block_has_cache_control(tool) {
                return true;
            }
        }
    }

    false
}

/// 使用本地优先的三槽位策略自动放置 Anthropic `cache_control` 标记。
///
/// **行为：**
///
/// - 如果请求体中任何位置已存在 `cache_control` 标记
///   （`system` 块、消息块或任意工具的顶层），返回
///   [`AutoPlaceOutcome::Skipped { reason: SkipReason::MarkerPresent }`]
///   且不修改。
/// - 否则按优先级顺序放置最多三个标记：
///   1. 最后一个工具定义的顶层（缓存 `tools`）。
///   2. 数组形式 `system` 的最后一块（缓存 `system + tools`）。
///   3. 当 `messages` 有 ≥ 2 条时，最后一条用户消息的最后一块
///      （缓存实时尾部之前的对话历史）。
/// - 返回 [`AutoPlaceOutcome::Applied { placed_count, locations }`]，
///   包含每个落下的标记的数量和 JSON-pointer 风格位置。一个没有工具、
///   没有数组形式 system、且少于两条消息的请求返回
///   `Applied { placed_count: 0, locations: vec![] }` —— 我们运行了，
///   但无事可做。
///
/// **幂等性：** 对同一请求体运行两次是无操作。第一次调用插入标记；
/// 第二次调用通过客户放置优先闸门看到它们并返回 `Skipped`。
pub fn auto_place_anthropic_cache_control(body: &mut Value) -> AutoPlaceOutcome {
    // 闸门：任何已存在的标记 → 客户优先，完全跳过。
    if any_anthropic_cache_control(body) {
        return AutoPlaceOutcome::Skipped {
            reason: SkipReason::MarkerPresent,
        };
    }

    let mut locations: Vec<String> = Vec::new();

    // 槽位 1：最后一个工具定义（顶层）。
    if let Some(Value::Array(tools)) = body.get_mut("tools") {
        if !tools.is_empty() {
            let last_idx = tools.len() - 1;
            let last_tool = &mut tools[last_idx];
            if insert_cache_control_on_object(last_tool) {
                locations.push(format!("tools[{last_idx}]"));
            }
        }
    }

    // 槽位 2：数组形式 `system` 的最后一块。
    if let Some(Value::Array(blocks)) = body.get_mut("system") {
        if !blocks.is_empty() {
            let last_idx = blocks.len() - 1;
            let last_block = &mut blocks[last_idx];
            if insert_cache_control_on_object(last_block) {
                locations.push(format!("system[{last_idx}]"));
            }
        }
    }

    // 槽位 3：有历史时，*最后一条用户消息*的最后一块。
    if let Some(Value::Array(messages)) = body.get_mut("messages") {
        if messages.len() >= 2 {
            // 不要使用 messages.last_mut()：字面尾部可能是 assistant 的
            // tool_use 或 assistant 响应。我们想要最后一条*用户*消息，
            // 使缓存的 prefix 在实时尾部之前停止。
            for last_user_idx in (0..messages.len()).rev() {
                let msg = &mut messages[last_user_idx];
                if msg.get("role").and_then(|r| r.as_str()) == Some("user")
                    && is_human_user_message(msg)
                {
                    if let Some(Value::Array(blocks)) = msg.get_mut("content") {
                        if !blocks.is_empty() {
                            let last_block_idx = blocks.len() - 1;
                            let last_block = &mut blocks[last_block_idx];
                            if insert_cache_control_on_object(last_block) {
                                locations.push(format!(
                                    "messages[{last_user_idx}].content[{last_block_idx}]"
                                ));
                            }
                        }
                    }
                    break;
                }
            }
        }
    }

    AutoPlaceOutcome::Applied {
        placed_count: locations.len(),
        locations,
    }
}

/// 当 `msg` 是内容数组仅包含稳定的人类编写块的用户消息时为 `true`。
/// 任何 `tool_result` 块都会使整条消息不符合槽位 3 的条件，因为
/// 后续块上的标记会把其之前易变的工具输出也缓存起来。
fn is_human_user_message(msg: &Value) -> bool {
    if let Some(Value::Array(blocks)) = msg.get("content") {
        return !blocks.is_empty()
            && blocks
                .iter()
                .all(|block| block.get("type").and_then(|t| t.as_str()) != Some("tool_result"));
    }
    false
}

/// 该内容块是否在其顶层携带 `cache_control` 字段？被只读遍历器
/// （[`any_anthropic_cache_control`]）和（间接地）幂等闸门使用。
fn block_has_cache_control(block: &Value) -> bool {
    block.get("cache_control").is_some()
}

/// 如果 `value` 是 JSON 对象，则在其上插入
/// `"cache_control": {"type": "ephemeral"}`。插入时返回 `true`，
/// 若 `value` 不是对象则返回 `false`（这样调用方可以拒绝占用槽位）。
fn insert_cache_control_on_object(value: &mut Value) -> bool {
    match value.as_object_mut() {
        Some(map) => {
            map.insert("cache_control".to_string(), json!({"type": "ephemeral"}));
            true
        }
        None => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// 带一个工具、一条简短用户消息、纯字符串 system 的全新请求体。
    /// 用作"快乐路径放置"测试的种子。
    fn body_one_tool_no_markers() -> Value {
        json!({
            "model": "claude-3-5-sonnet-20241022",
            "system": "You are helpful.",
            "tools": [
                {
                    "name": "search",
                    "description": "search the web",
                    "input_schema": {"type": "object", "properties": {}}
                }
            ],
            "messages": [
                {"role": "user", "content": "hi"}
            ],
        })
    }

    #[test]
    fn places_cache_control_on_last_tool_when_payg_and_no_markers() {
        let mut body = body_one_tool_no_markers();
        let outcome = auto_place_anthropic_cache_control(&mut body);
        match outcome {
            AutoPlaceOutcome::Applied {
                placed_count,
                locations,
            } => {
                assert_eq!(placed_count, 1);
                assert_eq!(locations, vec!["tools[0]"]);
            }
            other => panic!("expected Applied{{1}}, got {other:?}"),
        }
        // 标记在正确路径上可见。
        let cc = body
            .pointer("/tools/0/cache_control")
            .expect("marker inserted on tools[0]");
        assert_eq!(cc, &json!({"type": "ephemeral"}));
    }

    #[test]
    fn places_on_last_tool_when_multiple_tools() {
        // 有多个工具时，标记必须放在最后一个上。
        let mut body = json!({
            "tools": [
                {"name": "a", "description": "a"},
                {"name": "b", "description": "b"},
                {"name": "c", "description": "c"}
            ],
            "messages": [{"role": "user", "content": "hi"}],
        });
        let outcome = auto_place_anthropic_cache_control(&mut body);
        match outcome {
            AutoPlaceOutcome::Applied {
                placed_count,
                locations,
            } => {
                assert_eq!(placed_count, 1);
                assert_eq!(locations, vec!["tools[2]"]);
            }
            other => panic!("expected Applied{{1}}, got {other:?}"),
        }
        assert!(body.pointer("/tools/0/cache_control").is_none());
        assert!(body.pointer("/tools/1/cache_control").is_none());
        assert!(body.pointer("/tools/2/cache_control").is_some());
    }

    #[test]
    fn skips_when_any_tool_already_has_marker() {
        // 客户在第一个工具（不是我们会选的槽位）上放置了标记。
        // 客户放置优先仍然会跳过我们。
        let mut body = json!({
            "tools": [
                {
                    "name": "search",
                    "description": "search",
                    "cache_control": {"type": "ephemeral"}
                },
                {"name": "fetch", "description": "fetch"}
            ],
            "messages": [{"role": "user", "content": "hi"}],
        });
        let before = body.clone();
        let outcome = auto_place_anthropic_cache_control(&mut body);
        assert_eq!(
            outcome,
            AutoPlaceOutcome::Skipped {
                reason: SkipReason::MarkerPresent
            }
        );
        assert_eq!(body, before, "skip path must not mutate");
    }

    #[test]
    fn skips_when_system_block_already_has_marker() {
        // 客户使用了数组形式 `system` 并放置了自己的标记。
        // 客户放置优先。
        let mut body = json!({
            "system": [
                {"type": "text", "text": "you are helpful"},
                {
                    "type": "text",
                    "text": "cite sources",
                    "cache_control": {"type": "ephemeral"}
                }
            ],
            "tools": [{"name": "search", "description": "search"}],
            "messages": [{"role": "user", "content": "hi"}],
        });
        let before = body.clone();
        let outcome = auto_place_anthropic_cache_control(&mut body);
        assert_eq!(
            outcome,
            AutoPlaceOutcome::Skipped {
                reason: SkipReason::MarkerPresent
            }
        );
        assert_eq!(body, before, "skip path must not mutate");
    }

    #[test]
    fn skips_when_message_block_already_has_marker() {
        // 客户在对话中途放置了一个标记。跳过一切。
        let mut body = json!({
            "tools": [{"name": "search", "description": "search"}],
            "messages": [
                {
                    "role": "user",
                    "content": [
                        {
                            "type": "text",
                            "text": "remember this",
                            "cache_control": {"type": "ephemeral"}
                        }
                    ]
                },
                {"role": "assistant", "content": "ok"},
                {"role": "user", "content": "now what?"}
            ],
        });
        let before = body.clone();
        let outcome = auto_place_anthropic_cache_control(&mut body);
        assert_eq!(
            outcome,
            AutoPlaceOutcome::Skipped {
                reason: SkipReason::MarkerPresent
            }
        );
        assert_eq!(body, before, "skip path must not mutate");
    }

    #[test]
    fn places_marker_on_system_array_last_block() {
        // 槽位 2：数组形式 system 在其最后一块上获得标记。
        let mut body = json!({
            "system": [
                {"type": "text", "text": "rule 1"},
                {"type": "text", "text": "rule 2"}
            ],
            "tools": [{"name": "search", "description": "search"}],
            "messages": [{"role": "user", "content": "hi"}],
        });
        let outcome = auto_place_anthropic_cache_control(&mut body);
        assert_eq!(
            outcome,
            AutoPlaceOutcome::Applied {
                placed_count: 2,
                locations: vec!["tools[0]".to_string(), "system[1]".to_string()],
            }
        );
        assert_eq!(
            body.pointer("/system/1/cache_control"),
            Some(&json!({"type": "ephemeral"})),
        );
        assert!(body.pointer("/system/0/cache_control").is_none());
    }

    #[test]
    fn does_not_convert_string_system_to_array() {
        // 字符串形式 system 保持不动；只有工具标记落下。
        let mut body = json!({
            "system": "You are helpful.",
            "tools": [{"name": "search", "description": "search"}],
            "messages": [{"role": "user", "content": "hi"}],
        });
        let outcome = auto_place_anthropic_cache_control(&mut body);
        assert_eq!(
            outcome,
            AutoPlaceOutcome::Applied {
                placed_count: 1,
                locations: vec!["tools[0]".to_string()],
            }
        );
        assert_eq!(body.get("system"), Some(&json!("You are helpful.")));
    }

    #[test]
    fn places_marker_on_last_user_message_last_block_when_history_present() {
        // 槽位 3：对话有历史，标记放在最后一条用户消息的最后一块上。
        let mut body = json!({
            "tools": [{"name": "search", "description": "search"}],
            "messages": [
                {"role": "user", "content": "first"},
                {"role": "assistant", "content": "second"},
                {
                    "role": "user",
                    "content": [
                        {"type": "text", "text": "remember this"},
                        {"type": "text", "text": "and this"}
                    ]
                }
            ],
        });
        let outcome = auto_place_anthropic_cache_control(&mut body);
        assert_eq!(
            outcome,
            AutoPlaceOutcome::Applied {
                placed_count: 2,
                locations: vec!["tools[0]".to_string(), "messages[2].content[1]".to_string(),],
            }
        );
        assert_eq!(
            body.pointer("/messages/2/content/1/cache_control"),
            Some(&json!({"type": "ephemeral"})),
        );
        assert!(body
            .pointer("/messages/2/content/0/cache_control")
            .is_none());
    }

    #[test]
    fn does_not_place_on_user_message_when_single_turn() {
        // 只有一条消息 → 无槽位 3 标记；只有工具标记落下。
        let mut body = json!({
            "tools": [{"name": "search", "description": "search"}],
            "messages": [
                {"role": "user", "content": [{"type": "text", "text": "hi"}]}
            ],
        });
        let outcome = auto_place_anthropic_cache_control(&mut body);
        assert_eq!(
            outcome,
            AutoPlaceOutcome::Applied {
                placed_count: 1,
                locations: vec!["tools[0]".to_string()],
            }
        );
        assert!(body
            .pointer("/messages/0/content/0/cache_control")
            .is_none());
    }

    #[test]
    fn slot3_skips_live_assistant_tail_and_finds_last_user_message() {
        // 对话尾部是 assistant 回复；标记必须落在最后一条用户消息上，
        // 而不是尾部的 assistant 消息。
        let mut body = json!({
            "tools": [{"name": "search", "description": "search"}],
            "messages": [
                {"role": "user", "content": "first"},
                {"role": "assistant", "content": "second"},
                {
                    "role": "user",
                    "content": [
                        {"type": "text", "text": "remember this"},
                        {"type": "text", "text": "and this"}
                    ]
                },
                {"role": "assistant", "content": "latest reply"}
            ],
        });
        let outcome = auto_place_anthropic_cache_control(&mut body);
        assert_eq!(
            outcome,
            AutoPlaceOutcome::Applied {
                placed_count: 2,
                locations: vec!["tools[0]".to_string(), "messages[2].content[1]".to_string(),],
            }
        );
        assert_eq!(
            body.pointer("/messages/2/content/1/cache_control"),
            Some(&json!({"type": "ephemeral"})),
        );
        // 尾部的 assistant 必须保持无标记。
        assert!(body.pointer("/messages/3/cache_control").is_none());
        assert!(body.pointer("/messages/3/content/cache_control").is_none());
    }

    #[test]
    fn slot3_skips_tool_result_tail_and_finds_last_user_message() {
        // 多工具回合：尾部是携带 tool_result 的用户消息。
        // 槽位 3 必须跳过那个实时的 tool_result 尾部，落在更早的人类
        // 用户消息上，使缓存的 prefix 在易变的 assistant/tool_result
        // 回合之前停止。
        let mut body = json!({
            "tools": [{"name": "search", "description": "search"}],
            "messages": [
                {
                    "role": "user",
                    "content": [{"type": "text", "text": "first"}]
                },
                {
                    "role": "assistant",
                    "content": [
                        {
                            "type": "tool_use",
                            "id": "tu_1",
                            "name": "search",
                            "input": {"q": "x"}
                        }
                    ]
                },
                {
                    "role": "user",
                    "content": [
                        {
                            "type": "tool_result",
                            "tool_use_id": "tu_1",
                            "content": "result"
                        }
                    ]
                }
            ],
        });
        let outcome = auto_place_anthropic_cache_control(&mut body);
        assert_eq!(
            outcome,
            AutoPlaceOutcome::Applied {
                placed_count: 2,
                locations: vec!["tools[0]".to_string(), "messages[0].content[0]".to_string(),],
            }
        );
        assert_eq!(
            body.pointer("/messages/0/content/0/cache_control"),
            Some(&json!({"type": "ephemeral"})),
        );
        // tool_result 尾部必须保持无标记。
        assert!(body
            .pointer("/messages/2/content/0/cache_control")
            .is_none());
        assert!(body
            .pointer("/messages/1/content/0/cache_control")
            .is_none());
    }

    #[test]
    fn slot3_skips_user_message_that_contains_tool_result_before_text() {
        // 即使一条 user 角色消息以文本结尾，该消息中任何更早的
        // tool_result 块也是易变的。槽位 3 必须跳过整条消息，并把缓存
        // 边界保持在工具输出之前。
        let mut body = json!({
            "tools": [{"name": "search", "description": "search"}],
            "messages": [
                {
                    "role": "user",
                    "content": [{"type": "text", "text": "first"}]
                },
                {
                    "role": "assistant",
                    "content": [
                        {
                            "type": "tool_use",
                            "id": "tu_1",
                            "name": "search",
                            "input": {"q": "x"}
                        }
                    ]
                },
                {
                    "role": "user",
                    "content": [
                        {
                            "type": "tool_result",
                            "tool_use_id": "tu_1",
                            "content": "result"
                        },
                        {"type": "text", "text": "human follow-up"}
                    ]
                }
            ],
        });
        let outcome = auto_place_anthropic_cache_control(&mut body);
        assert_eq!(
            outcome,
            AutoPlaceOutcome::Applied {
                placed_count: 2,
                locations: vec!["tools[0]".to_string(), "messages[0].content[0]".to_string(),],
            }
        );
        assert_eq!(
            body.pointer("/messages/0/content/0/cache_control"),
            Some(&json!({"type": "ephemeral"})),
        );
        assert!(body
            .pointer("/messages/2/content/0/cache_control")
            .is_none());
        assert!(body
            .pointer("/messages/2/content/1/cache_control")
            .is_none());
    }

    #[test]
    fn slot3_falls_back_to_earliest_user_when_no_later_user_array_block() {
        // messages.len() >= 2 但第一条之后的所有消息都是 assistant。
        // 槽位 3 仍必须落在第一条（也是唯一一条）携带数组形式内容的
        // 用户消息上。
        let mut body = json!({
            "tools": [{"name": "search", "description": "search"}],
            "messages": [
                {
                    "role": "user",
                    "content": [
                        {"type": "text", "text": "only user"}
                    ]
                },
                {"role": "assistant", "content": "reply one"},
                {"role": "assistant", "content": "reply two"}
            ],
        });
        let outcome = auto_place_anthropic_cache_control(&mut body);
        assert_eq!(
            outcome,
            AutoPlaceOutcome::Applied {
                placed_count: 2,
                locations: vec!["tools[0]".to_string(), "messages[0].content[0]".to_string(),],
            }
        );
        assert_eq!(
            body.pointer("/messages/0/content/0/cache_control"),
            Some(&json!({"type": "ephemeral"})),
        );
    }

    #[test]
    fn malformed_system_block_does_not_panic() {
        // 最后一个 system 块不是对象 → 跳过槽位 2，仍放置槽位 1。
        let mut body = json!({
            "system": [
                {"type": "text", "text": "rule 1"},
                "not-an-object"
            ],
            "tools": [{"name": "search", "description": "search"}],
            "messages": [{"role": "user", "content": "hi"}],
        });
        let outcome = auto_place_anthropic_cache_control(&mut body);
        assert_eq!(
            outcome,
            AutoPlaceOutcome::Applied {
                placed_count: 1,
                locations: vec!["tools[0]".to_string()],
            }
        );
        assert!(body.pointer("/tools/0/cache_control").is_some());
    }

    #[test]
    fn malformed_message_content_block_does_not_panic() {
        // 最后一条消息的最后一个内容块不是对象 → 跳过槽位 3。
        let mut body = json!({
            "tools": [{"name": "search", "description": "search"}],
            "messages": [
                {"role": "user", "content": "first"},
                {"role": "assistant", "content": "second"},
                {
                    "role": "user",
                    "content": [
                        {"type": "text", "text": "ok"},
                        "not-an-object"
                    ]
                }
            ],
        });
        let outcome = auto_place_anthropic_cache_control(&mut body);
        assert_eq!(
            outcome,
            AutoPlaceOutcome::Applied {
                placed_count: 1,
                locations: vec!["tools[0]".to_string()],
            }
        );
    }

    #[test]
    fn idempotent_when_we_already_placed_marker_last_run() {
        // 运行一次：放置标记。再次运行：客户放置优先闸门触发
        // （就幂等闸门而言，我们上次放置的标记现在确实是一个
        // 客户侧标记）。
        let mut body = body_one_tool_no_markers();
        let first = auto_place_anthropic_cache_control(&mut body);
        assert!(matches!(
            first,
            AutoPlaceOutcome::Applied {
                placed_count: 1,
                ..
            }
        ));
        let after_first = body.clone();
        let second = auto_place_anthropic_cache_control(&mut body);
        assert_eq!(
            second,
            AutoPlaceOutcome::Skipped {
                reason: SkipReason::MarkerPresent
            }
        );
        assert_eq!(body, after_first, "second run must not mutate");
    }

    #[test]
    fn does_nothing_when_no_tools_present() {
        // 没有 tools 字段的请求体。返回 Applied{0}，不修改。
        let mut body = json!({
            "model": "claude-3-5-sonnet-20241022",
            "system": "You are helpful.",
            "messages": [{"role": "user", "content": "hi"}],
        });
        let before = body.clone();
        let outcome = auto_place_anthropic_cache_control(&mut body);
        assert_eq!(
            outcome,
            AutoPlaceOutcome::Applied {
                placed_count: 0,
                locations: Vec::new(),
            }
        );
        assert_eq!(body, before, "Applied{{0}} path must not mutate");
    }

    #[test]
    fn does_nothing_when_tools_array_is_empty() {
        let mut body = json!({
            "tools": [],
            "messages": [{"role": "user", "content": "hi"}],
        });
        let before = body.clone();
        let outcome = auto_place_anthropic_cache_control(&mut body);
        assert_eq!(
            outcome,
            AutoPlaceOutcome::Applied {
                placed_count: 0,
                locations: Vec::new(),
            }
        );
        assert_eq!(body, before, "empty-tools path must not mutate");
    }

    #[test]
    fn system_string_form_does_not_get_converted_to_array() {
        // 保守的首发策略：纯字符串 `system` 保持为纯字符串。
        // 我们只在工具上放置。
        let mut body = json!({
            "system": "You are helpful. Cite sources.",
            "tools": [{"name": "search", "description": "search"}],
            "messages": [{"role": "user", "content": "hi"}],
        });
        let outcome = auto_place_anthropic_cache_control(&mut body);
        assert!(matches!(
            outcome,
            AutoPlaceOutcome::Applied {
                placed_count: 1,
                ..
            }
        ));
        // system 仍是纯字符串。
        assert_eq!(
            body.get("system"),
            Some(&json!("You are helpful. Cite sources.")),
            "string-form `system` must stay untouched on first ship",
        );
        // 标记反而落在 tools[0] 上。
        assert_eq!(
            body.pointer("/tools/0/cache_control"),
            Some(&json!({"type": "ephemeral"})),
        );
    }

    #[test]
    fn applies_tool_and_system_markers_with_local_policy() {
        // 多轮对话 + 多个工具 + 数组形式 system。
        // 本地优先策略放置工具 + system 标记；消息使用字符串形式内容，
        // 因此槽位 3 没有可针对的数组块。
        let mut body = json!({
            "system": [
                {"type": "text", "text": "rule 1"},
                {"type": "text", "text": "rule 2"}
            ],
            "tools": [
                {"name": "a", "description": "a"},
                {"name": "b", "description": "b"}
            ],
            "messages": [
                {"role": "user", "content": "first"},
                {"role": "assistant", "content": "second"},
                {"role": "user", "content": "third"},
                {"role": "assistant", "content": "fourth"},
                {"role": "user", "content": "fifth"}
            ],
        });
        let outcome = auto_place_anthropic_cache_control(&mut body);
        assert_eq!(
            outcome,
            AutoPlaceOutcome::Applied {
                placed_count: 2,
                locations: vec!["tools[1]".to_string(), "system[1]".to_string(),],
            }
        );
        // 只有选中的槽位携带标记。
        assert_eq!(
            body.pointer("/tools/1/cache_control"),
            Some(&json!({"type": "ephemeral"})),
        );
        assert!(body.pointer("/tools/0/cache_control").is_none());
        assert_eq!(
            body.pointer("/system/1/cache_control"),
            Some(&json!({"type": "ephemeral"})),
        );
        assert!(body.pointer("/system/0/cache_control").is_none());
        // 消息使用字符串形式内容；槽位 3 没有可针对的数组块。
        assert!(body.pointer("/messages/4/cache_control").is_none());
        for (i, msg) in body
            .pointer("/messages")
            .unwrap()
            .as_array()
            .unwrap()
            .iter()
            .enumerate()
        {
            if let Some(Value::Array(blocks)) = msg.get("content") {
                for block in blocks {
                    assert!(
                        !block_has_cache_control(block),
                        "message[{i}] block carries unexpected marker: {block:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn applied_path_preserves_other_tool_fields() {
        // 性质：放置后的请求体与输入请求体一致，仅差选中槽位上的
        // cache_control 键。排序稳定，别处无附带修改。
        let original = body_one_tool_no_markers();
        let mut body = original.clone();
        let _ = auto_place_anthropic_cache_control(&mut body);

        // 移除我们放置的每个标记，并与原始内容比较。
        if let Some(Value::Array(tools)) = body.get_mut("tools") {
            for tool in tools {
                if let Some(map) = tool.as_object_mut() {
                    map.remove("cache_control");
                }
            }
        }
        assert_eq!(
            body, original,
            "Applied path must mutate ONLY cache_control fields on chosen slots",
        );
    }

    #[test]
    fn skip_reason_strings_are_stable() {
        // 仪表盘依据这些字符串过滤。固定它们。
        assert_eq!(SkipReason::MarkerPresent.as_str(), "marker_present");
    }

    #[test]
    fn any_marker_walker_scans_system_array_form_only() {
        // 字符串形式 `system` 无法携带标记 —— 即使字符串包含子串
        // "cache_control"，遍历器也不应标记它。
        let body = json!({
            "system": "Note: cache_control is an Anthropic concept.",
            "messages": [],
        });
        assert!(!any_anthropic_cache_control(&body));

        // 带标记的数组形式 `system` —— 遍历器确实会标记它。
        let body_with_marker = json!({
            "system": [
                {"type": "text", "text": "x", "cache_control": {"type": "ephemeral"}}
            ],
            "messages": [],
        });
        assert!(any_anthropic_cache_control(&body_with_marker));
    }

    #[test]
    fn any_marker_walker_does_not_descend_into_input_schema() {
        // 客户的工具 input_schema 恰好声明了一个字面名为 `cache_control`
        // 的属性。那不是真正的 Anthropic 标记 —— 它是 JSON Schema
        // 属性名。遍历器绝不能误报。
        let body = json!({
            "tools": [{
                "name": "configure",
                "description": "configure something",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "cache_control": {"type": "string"}
                    }
                }
            }],
            "messages": [{"role": "user", "content": "hi"}],
        });
        assert!(
            !any_anthropic_cache_control(&body),
            "walker must scope to documented Anthropic surfaces; \
             schema property keys do not count as markers",
        );
    }

    #[test]
    fn malformed_tool_entry_does_not_panic() {
        // 防御性：一个带非对象工具的 Anthropic 请求会从上游收到 400，
        // 但我们绝不会对畸形请求体 panic。改为跳过该槽位。
        let mut body = json!({
            "tools": ["not-an-object"],
            "messages": [{"role": "user", "content": "hi"}],
        });
        let before = body.clone();
        let outcome = auto_place_anthropic_cache_control(&mut body);
        assert_eq!(
            outcome,
            AutoPlaceOutcome::Applied {
                placed_count: 0,
                locations: Vec::new(),
            }
        );
        assert_eq!(body, before, "malformed-tool path must not mutate");
    }
}
