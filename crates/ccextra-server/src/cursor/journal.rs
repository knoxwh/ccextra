use bytes::Bytes;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// Journal 默认存活时间（10分钟）
pub const JOURNAL_TTL: Duration = Duration::from_secs(10 * 60);

/// Replay 查询状态
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReplayStatus {
    /// 可用并返回所有帧
    Available(Vec<Bytes>),
    /// 已被驱逐或过期淘汰
    Evicted,
    /// 从未记录
    NotFound,
}

/// 单个 turn 的事件日志
#[derive(Debug, Clone)]
struct TurnJournal {
    /// 已发布的事件序列
    events: Vec<Bytes>,
    /// 是否已进入终态（completed/failed）
    terminated: bool,
    /// 总字节数
    total_bytes: usize,
    /// 创建时间
    created_at: Instant,
}

impl Default for TurnJournal {
    fn default() -> Self {
        Self {
            events: Vec::new(),
            terminated: false,
            total_bytes: 0,
            created_at: Instant::now(),
        }
    }
}

/// Cursor 事件日志：按 turn digest 保存已发布事件
#[derive(Clone)]
pub struct CursorEventJournal {
    inner: Arc<Mutex<JournalInner>>,
}

struct JournalInner {
    journals: HashMap<String, TurnJournal>,
    total_bytes: usize,
    max_memory_bytes: usize,
    /// 被淘汰/驱逐的 digest 记录（带时间戳）
    evicted: HashMap<String, Instant>,
}

impl CursorEventJournal {
    pub fn new() -> Self {
        Self::with_limit(64 * 1024 * 1024) // 默认 64MB
    }

    pub fn with_limit(max_memory_bytes: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(JournalInner {
                journals: HashMap::new(),
                total_bytes: 0,
                max_memory_bytes,
                evicted: HashMap::new(),
            })),
        }
    }

    fn record_evicted(inner: &mut JournalInner, digest: String) {
        if inner.evicted.len() >= 2048 {
            // 简单淘汰最老的记录
            if let Some(oldest) = inner
                .evicted
                .iter()
                .min_by_key(|(_, t)| *t)
                .map(|(k, _)| k.clone())
            {
                inner.evicted.remove(&oldest);
            }
        }
        inner.evicted.insert(digest, Instant::now());
    }

    fn evict_if_needed(inner: &mut JournalInner) {
        if inner.total_bytes <= inner.max_memory_bytes {
            return;
        }
        // 1. 优先驱逐最老的 terminated journal
        let mut terminated: Vec<_> = inner
            .journals
            .iter()
            .filter(|(_, j)| j.terminated)
            .map(|(k, j)| (k.clone(), j.created_at, j.total_bytes))
            .collect();
        terminated.sort_by_key(|(_, created, _)| *created);
        for (digest, _, bytes) in terminated {
            if inner.total_bytes <= inner.max_memory_bytes {
                break;
            }
            inner.journals.remove(&digest);
            inner.total_bytes = inner.total_bytes.saturating_sub(bytes);
            Self::record_evicted(inner, digest);
        }

        // 2. 若仍然超限，严格驱逐最老的 active journal
        if inner.total_bytes > inner.max_memory_bytes {
            let mut active: Vec<_> = inner
                .journals
                .iter()
                .map(|(k, j)| (k.clone(), j.created_at, j.total_bytes))
                .collect();
            active.sort_by_key(|(_, created, _)| *created);
            for (digest, _, bytes) in active {
                if inner.total_bytes <= inner.max_memory_bytes {
                    break;
                }
                inner.journals.remove(&digest);
                inner.total_bytes = inner.total_bytes.saturating_sub(bytes);
                Self::record_evicted(inner, digest);
            }
        }
    }

    /// 保存原始 SSE 帧；只有 message_start 能创建日志，终态后拒绝追加。
    pub fn record(&self, turn_digest: &str, frame: Bytes) {
        let start = frame.starts_with(b"event: message_start\n");
        let terminal =
            frame.starts_with(b"event: message_stop\n") || frame.starts_with(b"event: error\n");
        let mut inner = self.inner.lock().unwrap();
        if inner.evicted.contains_key(turn_digest) {
            return;
        }
        if start {
            if inner.journals.contains_key(turn_digest) {
                return;
            }
            inner
                .journals
                .insert(turn_digest.to_owned(), TurnJournal::default());
        }
        let Some(journal) = inner.journals.get_mut(turn_digest) else {
            return;
        };
        if journal.terminated {
            return;
        }
        let size = frame.len();
        journal.events.push(frame);
        journal.total_bytes += size;
        journal.terminated = terminal;
        inner.total_bytes += size;
        Self::evict_if_needed(&mut inner);
    }

    /// 查询 replay 状态（支持检测已驱逐状态）
    pub fn replay_status(&self, turn_digest: &str) -> ReplayStatus {
        let inner = self.inner.lock().unwrap();
        if let Some(journal) = inner.journals.get(turn_digest) {
            ReplayStatus::Available(journal.events.clone())
        } else if inner.evicted.contains_key(turn_digest) {
            ReplayStatus::Evicted
        } else {
            ReplayStatus::NotFound
        }
    }

    /// 定期按 TTL 清理过期日志，超期条目计入 evicted
    pub fn sweep_expired(&self, ttl: Duration) {
        let mut inner = self.inner.lock().unwrap();
        let now = Instant::now();
        let expired: Vec<_> = inner
            .journals
            .iter()
            .filter(|(_, j)| now.duration_since(j.created_at) >= ttl)
            .map(|(k, j)| (k.clone(), j.total_bytes))
            .collect();

        for (digest, bytes) in expired {
            inner.journals.remove(&digest);
            inner.total_bytes = inner.total_bytes.saturating_sub(bytes);
            Self::record_evicted(&mut inner, digest);
        }

        // 清理超长期的 evicted 记录（例如 2 倍 TTL）
        let evicted_ttl = ttl.saturating_mul(2);
        inner
            .evicted
            .retain(|_, evicted_at| now.duration_since(*evicted_at) < evicted_ttl);
    }

    /// 清理指定 turn 的 journal
    pub fn remove(&self, turn_digest: &str) {
        let mut inner = self.inner.lock().unwrap();
        inner.evicted.remove(turn_digest);
        if let Some(journal) = inner.journals.remove(turn_digest) {
            inner.total_bytes = inner.total_bytes.saturating_sub(journal.total_bytes);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn frame(event: &str, text: &str) -> Bytes {
        crate::sse::emit::sse(event, &serde_json::json!({"text": text}))
    }

    #[test]
    fn active_overflow_does_not_recreate_truncated_replay() {
        let start = frame("message_start", "start");
        let journal = CursorEventJournal::with_limit(start.len());
        journal.record("turn", start.clone());
        journal.record("turn", frame("content_block_delta", "overflow"));
        assert_eq!(journal.replay_status("turn"), ReplayStatus::Evicted);
        journal.record("turn", frame("content_block_delta", "tail"));
        journal.record("turn", frame("message_stop", "stop"));
        assert_eq!(journal.replay_status("turn"), ReplayStatus::Evicted);
        assert!(journal.inner.lock().unwrap().total_bytes <= start.len());
    }

    #[test]
    fn test_journal_replay() {
        let journal = CursorEventJournal::new();
        let frames: Vec<_> = [
            "message_start",
            "content_block_start",
            "content_block_delta",
            "content_block_delta",
            "content_block_stop",
            "message_delta",
            "message_stop",
        ]
        .into_iter()
        .enumerate()
        .map(|(i, kind)| frame(kind, &i.to_string()))
        .collect();
        for frame in &frames {
            journal.record("turn", frame.clone());
        }
        assert_eq!(
            journal.replay_status("turn"),
            ReplayStatus::Available(frames)
        );
    }

    #[test]
    fn test_journal_no_duplicate_message_start() {
        let journal = CursorEventJournal::new();
        let first = frame("message_start", "first");
        journal.record("turn", first.clone());
        journal.record("turn", frame("message_start", "duplicate"));
        assert_eq!(
            journal.replay_status("turn"),
            ReplayStatus::Available(vec![first])
        );
    }

    #[test]
    fn test_journal_terminated_blocks_new_events() {
        for terminal in ["message_stop", "error"] {
            let journal = CursorEventJournal::new();
            let frames = vec![frame("message_start", "start"), frame(terminal, "end")];
            for frame in &frames {
                journal.record("turn", frame.clone());
            }
            journal.record("turn", frame("content_block_delta", "late"));
            assert_eq!(
                journal.replay_status("turn"),
                ReplayStatus::Available(frames)
            );
        }
    }

    #[test]
    fn test_journal_eviction_and_replay_unavailable() {
        let start = frame("message_start", "first");
        let stop = frame("message_stop", "end");
        let journal = CursorEventJournal::with_limit(start.len() + stop.len());
        journal.record("first", start.clone());
        journal.record("first", stop.clone());
        assert_eq!(
            journal.replay_status("first"),
            ReplayStatus::Available(vec![start, stop])
        );
        let next = frame("message_start", "next");
        journal.record("next", next.clone());
        assert_eq!(journal.replay_status("first"), ReplayStatus::Evicted);
        assert_eq!(
            journal.replay_status("next"),
            ReplayStatus::Available(vec![next])
        );
    }
}
