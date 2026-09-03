// Grok doom loop 检测纯逻辑(对齐 grok-build xai-grok-sampling-types/src/doom_loop.rs)
//
// 触发器解析 + 置信判定纯函数,server 层调用后决定是否中断流。

pub const RECOVERY_REMINDER: &str = "<system_reminder>Your messages have been flagged as looping. Your response has been flagged as repeating the same text pattern. Avoid excessive repetition. If you are having trouble ask the user for guidance.</system_reminder>";

// grok-build 官方双阶梯阈值常数(对齐 grok-build turn.rs L3491-3495)
pub const NUDGE_AFTER_IDENTICAL_PROBLEMATIC_TOOL_CALLS: u32 = 4;
pub const NUDGE_AFTER_IDENTICAL_TOOL_CALLS: u32 = 8;
pub const MAX_CONSECUTIVE_IDENTICAL_PROBLEMATIC_TOOL_CALLS: u32 = 8;
pub const MAX_CONSECUTIVE_IDENTICAL_TOOL_CALLS: u32 = 12;

/// 工具是否属于高危死循环类型(对齐 grok-build is_problematically_repeating_kind: Read / Plan)
pub fn is_problematically_repeating_tool(tool_name: &str) -> bool {
    let lower = tool_name.to_ascii_lowercase();
    lower.contains("read") || lower.contains("plan") || lower.contains("todo")
}

/// 动作停滞判定结果(对齐 grok-build IdenticalToolCallRun)
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StationarityVerdict {
    /// 正常，无重复
    None,
    /// 软提醒阶段(注入 RECOVERY_REMINDER)
    Nudge { run_len: u32, tool_name: String },
    /// 硬熔断阶段(立即中止 turn，防止无限循环)
    HardStop { run_len: u32, tool_name: String },
}

/// 触发器类型
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DoomLoopSignalKind {
    /// tail_repetition:{threshold},阈值越低循环越紧
    TailRepetition(u32),
    /// exact_repetition:{sequence_tokens}x{repeat_count}@{channel}:连续重复的完全相同 token 序列
    ExactRepetition {
        sequence_tokens: u32,
        repeat_count: u32,
    },
    /// low_logprob(无阈值)
    LowLogprob,
    /// 拆不动的未知类型,保留原始 label
    Unknown(String),
}

/// 单个触发器信号
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DoomLoopSignal {
    pub kind: DoomLoopSignalKind,
    pub channel: String,
}

/// 解析触发器 raw label(对齐 grok-build wire 契约,best-effort 永不报错)
///
/// 语法:
/// - `tail_repetition:{threshold}@{channel}`,如 `tail_repetition:32@thinking`
/// - `low_logprob@{channel}`,如 `low_logprob@response`
///
/// 拆不动记 Unknown(label),不报错(检测是 best-effort,malformed 不弄挂流)。
pub fn parse_trigger(label: &str) -> DoomLoopSignal {
    // @ 前为类型[:阈值],@ 后为 channel
    let parts: Vec<&str> = label.split('@').collect();
    if parts.len() != 2 {
        return DoomLoopSignal {
            kind: DoomLoopSignalKind::Unknown(label.to_string()),
            channel: String::new(),
        };
    }
    let type_part = parts[0];
    let channel = parts[1].to_string();

    if type_part == "low_logprob" {
        return DoomLoopSignal {
            kind: DoomLoopSignalKind::LowLogprob,
            channel,
        };
    }

    // tail_repetition:{threshold}
    if let Some(threshold_str) = type_part.strip_prefix("tail_repetition:") {
        if let Ok(threshold) = threshold_str.parse::<u32>() {
            return DoomLoopSignal {
                kind: DoomLoopSignalKind::TailRepetition(threshold),
                channel,
            };
        }
    }

    // exact_repetition:{sequence_tokens}x{repeat_count}
    if let Some(dimensions) = type_part.strip_prefix("exact_repetition:") {
        let kind = dimensions
            .split_once('x')
            .and_then(|(sequence, count)| {
                Some((sequence.parse::<u32>().ok()?, count.parse::<u32>().ok()?))
            })
            .map_or_else(
                || DoomLoopSignalKind::Unknown(label.to_string()),
                |(sequence_tokens, repeat_count)| DoomLoopSignalKind::ExactRepetition {
                    sequence_tokens,
                    repeat_count,
                },
            );
        return DoomLoopSignal { kind, channel };
    }

    DoomLoopSignal {
        kind: DoomLoopSignalKind::Unknown(label.to_string()),
        channel,
    }
}

/// 置信判定(对齐 grok-build DoomLoopRecoveryPolicy::is_confident)
///
/// 只认 thinking channel 的 tail_repetition(阈值 ≤ 64)。
/// exact_repetition 仅用于遥测统计(grok-build 中无 abort/recovery 消费点),
/// 其余一切(response channel、low_logprob、未知、更松阈值)返回 false。
pub fn is_confident(signal: &DoomLoopSignal) -> bool {
    const MAX_THRESHOLD: u32 = 64;
    const THINKING_CHANNEL: &str = "thinking";

    signal.channel == THINKING_CHANNEL
        && matches!(signal.kind, DoomLoopSignalKind::TailRepetition(t) if t <= MAX_THRESHOLD)
}

/// 递归规范化 JSON 对象的键序（对齐 grok-build canonicalize_json）
pub fn canonicalize_json(value: serde_json::Value) -> serde_json::Value {
    match value {
        serde_json::Value::Object(map) => {
            let mut entries: Vec<_> = map.into_iter().collect();
            entries.sort_by(|(a, _), (b, _)| a.cmp(b));
            serde_json::Value::Object(
                entries
                    .into_iter()
                    .map(|(k, v)| (k, canonicalize_json(v)))
                    .collect(),
            )
        }
        serde_json::Value::Array(items) => {
            serde_json::Value::Array(items.into_iter().map(canonicalize_json).collect())
        }
        other => other,
    }
}

/// 从 tool_use 计算单步签名字符串（对齐 grok-build step_signature）
fn tool_step_signature(tool_calls: &[(&str, &serde_json::Value)]) -> String {
    let mut parts: Vec<String> = tool_calls
        .iter()
        .map(|(name, input)| {
            let args = canonicalize_json((*input).clone()).to_string();
            format!("{}\u{1f}{}", name, args)
        })
        .collect();
    parts.sort();
    parts.join("\u{1e}")
}

/// 提取 assistant 消息中的工具调用 (name, input) 列表
fn extract_assistant_tool_calls(msg: &serde_json::Value) -> Vec<(&str, &serde_json::Value)> {
    let mut calls = Vec::new();
    if let Some(parts) = msg.get("content").and_then(|c| c.as_array()) {
        for part in parts {
            if part.get("type").and_then(|t| t.as_str()) == Some("tool_use") {
                let name = part.get("name").and_then(|n| n.as_str()).unwrap_or("");
                let input = part.get("input").unwrap_or(&serde_json::Value::Null);
                calls.push((name, input));
            }
        }
    }
    calls
}

/// 检查连续相同工具调用运行长度与判定
pub fn check_action_stationarity(body: &serde_json::Value) -> StationarityVerdict {
    let Some(messages) = body.get("messages").and_then(|m| m.as_array()) else {
        return StationarityVerdict::None;
    };
    if messages.len() < 3 {
        return StationarityVerdict::None;
    }

    let last_user_idx = messages.len() - 1;
    if messages[last_user_idx].get("role").and_then(|r| r.as_str()) != Some("user") {
        return StationarityVerdict::None;
    }

    // 从后往前统计连续相同工具调用的 assistant 次数
    let mut target_signature: Option<String> = None;
    let mut tool_name = String::new();
    let mut run_len: u32 = 0;

    for msg in messages.iter().rev() {
        if msg.get("role").and_then(|r| r.as_str()) == Some("assistant") {
            let calls = extract_assistant_tool_calls(msg);
            if calls.is_empty() {
                break;
            }
            let sig = tool_step_signature(&calls);
            if let Some(target) = &target_signature {
                if *target == sig {
                    run_len += 1;
                } else {
                    break;
                }
            } else {
                tool_name = calls[0].0.to_string();
                target_signature = Some(sig);
                run_len = 1;
            }
        }
    }

    if run_len < 2 {
        return StationarityVerdict::None;
    }

    let is_problematic = is_problematically_repeating_tool(&tool_name);
    let hard_stop_threshold = if is_problematic {
        MAX_CONSECUTIVE_IDENTICAL_PROBLEMATIC_TOOL_CALLS
    } else {
        MAX_CONSECUTIVE_IDENTICAL_TOOL_CALLS
    };
    let nudge_threshold = if is_problematic {
        NUDGE_AFTER_IDENTICAL_PROBLEMATIC_TOOL_CALLS
    } else {
        NUDGE_AFTER_IDENTICAL_TOOL_CALLS
    };

    if run_len >= hard_stop_threshold {
        StationarityVerdict::HardStop { run_len, tool_name }
    } else if run_len >= nudge_threshold {
        StationarityVerdict::Nudge { run_len, tool_name }
    } else {
        StationarityVerdict::None
    }
}

/// 检测最近的 assistant 消息是否存在连续重复的工具调用（跨轮 dead loop），
/// 若重复则在尾部 user 消息中注入官方 grok-build RECOVERY_REMINDER。
/// 返回是否注入了提醒。
pub fn inject_loop_recovery_reminder_if_needed(body: &mut serde_json::Value) -> bool {
    let verdict = check_action_stationarity(body);
    if matches!(verdict, StationarityVerdict::None) {
        return false;
    }

    let Some(messages) = body.get_mut("messages").and_then(|m| m.as_array_mut()) else {
        return false;
    };
    let last_user_idx = messages.len() - 1;

    // 检查最新 user 消息中是否已经注入过该 RECOVERY_REMINDER，避免重复堆叠
    let last_user_msg = &mut messages[last_user_idx];
    if let Some(content_str) = last_user_msg.get("content").and_then(|c| c.as_str()) {
        if content_str.contains("Your messages have been flagged as looping") {
            return false;
        }
        let new_text = format!("{}\n\n{}", content_str, RECOVERY_REMINDER);
        last_user_msg["content"] = serde_json::Value::String(new_text);
        return true;
    }

    if let Some(parts) = last_user_msg
        .get_mut("content")
        .and_then(|c| c.as_array_mut())
    {
        for part in parts.iter() {
            if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                if t.contains("Your messages have been flagged as looping") {
                    return false;
                }
            }
        }
        parts.push(serde_json::json!({
            "type": "text",
            "text": RECOVERY_REMINDER,
        }));
        return true;
    }

    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_trigger_tail_repetition() {
        let sig = parse_trigger("tail_repetition:32@thinking");
        assert_eq!(sig.kind, DoomLoopSignalKind::TailRepetition(32));
        assert_eq!(sig.channel, "thinking");

        let sig = parse_trigger("tail_repetition:128@response");
        assert_eq!(sig.kind, DoomLoopSignalKind::TailRepetition(128));
        assert_eq!(sig.channel, "response");
    }

    #[test]
    fn test_parse_trigger_exact_repetition() {
        let sig = parse_trigger("exact_repetition:42x3@thinking");
        assert_eq!(
            sig.kind,
            DoomLoopSignalKind::ExactRepetition {
                sequence_tokens: 42,
                repeat_count: 3,
            }
        );
        assert_eq!(sig.channel, "thinking");

        let sig = parse_trigger("exact_repetition:128x5@response");
        assert_eq!(
            sig.kind,
            DoomLoopSignalKind::ExactRepetition {
                sequence_tokens: 128,
                repeat_count: 5,
            }
        );
        assert_eq!(sig.channel, "response");
    }

    #[test]
    fn test_parse_trigger_low_logprob() {
        let sig = parse_trigger("low_logprob@thinking");
        assert_eq!(sig.kind, DoomLoopSignalKind::LowLogprob);
        assert_eq!(sig.channel, "thinking");
    }

    #[test]
    fn test_parse_trigger_unknown() {
        let sig = parse_trigger("unknown_type@thinking");
        assert!(matches!(sig.kind, DoomLoopSignalKind::Unknown(_)));

        let sig = parse_trigger("malformed");
        assert!(matches!(sig.kind, DoomLoopSignalKind::Unknown(_)));
        assert_eq!(sig.channel, "");
    }

    #[test]
    fn test_is_confident_true() {
        // thinking channel + tail_repetition ≤ 64
        assert!(is_confident(&DoomLoopSignal {
            kind: DoomLoopSignalKind::TailRepetition(32),
            channel: "thinking".to_string(),
        }));
        assert!(is_confident(&DoomLoopSignal {
            kind: DoomLoopSignalKind::TailRepetition(64),
            channel: "thinking".to_string(),
        }));
        assert!(is_confident(&DoomLoopSignal {
            kind: DoomLoopSignalKind::TailRepetition(1),
            channel: "thinking".to_string(),
        }));
    }

    #[test]
    fn test_is_confident_false() {
        // 阈值 > 64
        assert!(!is_confident(&DoomLoopSignal {
            kind: DoomLoopSignalKind::TailRepetition(65),
            channel: "thinking".to_string(),
        }));
        // response channel
        assert!(!is_confident(&DoomLoopSignal {
            kind: DoomLoopSignalKind::TailRepetition(32),
            channel: "response".to_string(),
        }));
        // exact_repetition 仅遥测,不触发中断(对齐 grok-build is_confident)
        assert!(!is_confident(&DoomLoopSignal {
            kind: DoomLoopSignalKind::ExactRepetition {
                sequence_tokens: 42,
                repeat_count: 3,
            },
            channel: "thinking".to_string(),
        }));
        // low_logprob
        assert!(!is_confident(&DoomLoopSignal {
            kind: DoomLoopSignalKind::LowLogprob,
            channel: "thinking".to_string(),
        }));
        // unknown
        assert!(!is_confident(&DoomLoopSignal {
            kind: DoomLoopSignalKind::Unknown("foo".to_string()),
            channel: "thinking".to_string(),
        }));
    }

    #[test]
    fn test_parse_and_confidence_integration() {
        // 置信样本
        let label = "tail_repetition:32@thinking";
        let sig = parse_trigger(label);
        assert!(is_confident(&sig));

        // 非置信:response channel
        let label = "tail_repetition:32@response";
        let sig = parse_trigger(label);
        assert!(!is_confident(&sig));

        // 非置信:阈值松
        let label = "tail_repetition:128@thinking";
        let sig = parse_trigger(label);
        assert!(!is_confident(&sig));

        // 非置信:low_logprob
        let label = "low_logprob@thinking";
        let sig = parse_trigger(label);
        assert!(!is_confident(&sig));
    }

    #[test]
    fn test_inject_loop_recovery_reminder() {
        let mut body = serde_json::json!({
            "messages": [
                {
                    "role": "assistant",
                    "content": [{"type": "tool_use", "id": "call_1", "name": "Read", "input": {"file_path": "/tmp/test"}}]
                },
                {
                    "role": "user",
                    "content": [{"type": "tool_result", "tool_use_id": "call_1", "content": ""}]
                },
                {
                    "role": "assistant",
                    "content": [{"type": "tool_use", "id": "call_2", "name": "Read", "input": {"file_path": "/tmp/test"}}]
                },
                {
                    "role": "user",
                    "content": [{"type": "tool_result", "tool_use_id": "call_2", "content": ""}]
                },
                {
                    "role": "assistant",
                    "content": [{"type": "tool_use", "id": "call_3", "name": "Read", "input": {"file_path": "/tmp/test"}}]
                },
                {
                    "role": "user",
                    "content": [{"type": "tool_result", "tool_use_id": "call_3", "content": ""}]
                },
                {
                    "role": "assistant",
                    "content": [{"type": "tool_use", "id": "call_4", "name": "Read", "input": {"file_path": "/tmp/test"}}]
                },
                {
                    "role": "user",
                    "content": [{"type": "tool_result", "tool_use_id": "call_4", "content": ""}]
                }
            ]
        });

        assert!(inject_loop_recovery_reminder_if_needed(&mut body));
        let msgs = body["messages"].as_array().unwrap();
        let last_user_content = &msgs[7]["content"];
        assert_eq!(last_user_content[1]["type"], "text");
        assert!(last_user_content[1]["text"]
            .as_str()
            .unwrap()
            .contains("Your messages have been flagged as looping"));

        // 再次检测不应重复注入
        assert!(!inject_loop_recovery_reminder_if_needed(&mut body));
    }

    #[test]
    fn test_check_action_stationarity_verdicts() {
        let make_messages = |tool: &str, count: usize| {
            let mut msgs = Vec::new();
            for i in 0..count {
                msgs.push(serde_json::json!({
                    "role": "assistant",
                    "content": [{"type": "tool_use", "id": format!("call_{i}"), "name": tool, "input": {"path": "same"}}]
                }));
                msgs.push(serde_json::json!({
                    "role": "user",
                    "content": [{"type": "tool_result", "tool_use_id": format!("call_{i}"), "content": ""}]
                }));
            }
            serde_json::json!({"messages": msgs})
        };

        // 1 次重复 -> None
        let body1 = make_messages("Read", 1);
        assert_eq!(check_action_stationarity(&body1), StationarityVerdict::None);

        // 2 次 Read (未达到 Nudge 阈值 4) -> None
        let body2 = make_messages("Read", 2);
        assert_eq!(check_action_stationarity(&body2), StationarityVerdict::None);

        // 4 次 Read (达到 NUDGE_AFTER_IDENTICAL_PROBLEMATIC_TOOL_CALLS) -> Nudge
        let body4 = make_messages("Read", 4);
        assert!(matches!(
            check_action_stationarity(&body4),
            StationarityVerdict::Nudge { run_len: 4, .. }
        ));

        // 8 次 Read (达到 MAX_CONSECUTIVE_IDENTICAL_PROBLEMATIC_TOOL_CALLS) -> HardStop
        let body8 = make_messages("Read", 8);
        assert!(matches!(
            check_action_stationarity(&body8),
            StationarityVerdict::HardStop { run_len: 8, .. }
        ));

        // 通用工具 (如 Bash)，8 次仍为 Nudge，12 次才 HardStop
        let bash8 = make_messages("Bash", 8);
        assert!(matches!(
            check_action_stationarity(&bash8),
            StationarityVerdict::Nudge { run_len: 8, .. }
        ));
        let bash12 = make_messages("Bash", 12);
        assert!(matches!(
            check_action_stationarity(&bash12),
            StationarityVerdict::HardStop { run_len: 12, .. }
        ));
    }
}
