// ccextra-core: 纯逻辑,无 IO
//
// 职责:
// - normalize/: 九个归一化模块
// - convert/: 三条 body-to-body 转换
// - route/: model → provider 路由决策
// - session/: 会话身份派生
// - doom_loop: Grok doom loop 检测纯逻辑

pub mod cache_stabilization;
pub mod convert;
pub mod doom_loop;
pub mod normalize;
pub mod prompt_cache;
pub mod route;
pub mod secret;
pub mod session;
pub mod thinking;

pub use doom_loop::{is_confident, parse_trigger, DoomLoopSignal, DoomLoopSignalKind};
pub use route::{Protocol, RouteDecision};

/// 判断 antigravity 模型是否使用 reasoning replay(对齐 CPA
/// antigravityUsesReasoningReplayCache:gemini/flash/agent 启用,claude 不启用)
pub fn antigravity_uses_reasoning_replay(model_name: &str) -> bool {
    let lower = model_name.to_lowercase();
    if lower.contains("claude") {
        return false;
    }
    lower.contains("gemini") || lower.contains("flash") || lower.contains("agent")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_antigravity_uses_reasoning_replay() {
        // gemini/flash/agent 启用
        assert!(antigravity_uses_reasoning_replay("gemini-2.0-flash"));
        assert!(antigravity_uses_reasoning_replay("gemini-3.5-pro"));
        assert!(antigravity_uses_reasoning_replay("flash-2.5"));
        assert!(antigravity_uses_reasoning_replay("agent-v1"));
        assert!(antigravity_uses_reasoning_replay("GEMINI-FLASH")); // 大小写

        // claude 不启用
        assert!(!antigravity_uses_reasoning_replay("claude-opus-5"));
        assert!(!antigravity_uses_reasoning_replay("claude-sonnet-4"));
        assert!(!antigravity_uses_reasoning_replay("CLAUDE-OPUS-5"));

        // 其他模型不启用
        assert!(!antigravity_uses_reasoning_replay("gpt-5"));
        assert!(!antigravity_uses_reasoning_replay("llama-3"));
    }
}
