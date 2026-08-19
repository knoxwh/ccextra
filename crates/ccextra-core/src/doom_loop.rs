// Grok doom loop 检测纯逻辑(对齐 grok-build xai-grok-sampling-types/src/doom_loop.rs)
//
// 触发器解析 + 置信判定纯函数,server 层调用后决定是否中断流。

/// 触发器类型
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DoomLoopSignalKind {
    /// tail_repetition:{threshold},阈值越低循环越紧
    TailRepetition(u32),
    /// low_logprob(无阈值)
    LowLogProb,
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
            kind: DoomLoopSignalKind::LowLogProb,
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

    DoomLoopSignal {
        kind: DoomLoopSignalKind::Unknown(label.to_string()),
        channel,
    }
}

/// 置信判定(对齐 grok-build DoomLoopRecoveryPolicy::is_confident)
///
/// 只认 thinking channel 的 tail_repetition 且阈值 ≤ 64。
/// 其他一切(response channel、low_logprob、未知、更松阈值)返回 false。
pub fn is_confident(signal: &DoomLoopSignal) -> bool {
    const MAX_THRESHOLD: u32 = 64;
    const THINKING_CHANNEL: &str = "thinking";

    signal.channel == THINKING_CHANNEL
        && matches!(signal.kind, DoomLoopSignalKind::TailRepetition(t) if t <= MAX_THRESHOLD)
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
    fn test_parse_trigger_low_logprob() {
        let sig = parse_trigger("low_logprob@thinking");
        assert_eq!(sig.kind, DoomLoopSignalKind::LowLogProb);
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
        // low_logprob
        assert!(!is_confident(&DoomLoopSignal {
            kind: DoomLoopSignalKind::LowLogProb,
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
}
