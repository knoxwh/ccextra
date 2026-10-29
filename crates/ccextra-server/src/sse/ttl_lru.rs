// 会话级 replay 账本共用的 TTL + 容量上限缓存(Responses replay 与 Kimi thinking 共用)。
//
// 语义对齐 CPA internal/cache 的 reasoning replay 存取:
// - 读命中滑动续期(GetXAIReasoningReplayItemsRequired 更新 Timestamp),过期即删;
// - 新 key 写入时若已满,按时间戳驱逐最老的 EVICT_BATCH_SIZE 条
//   (evictOldestCodexReasoningReplayEntries,不整表清空);
// - key 统一 trim,空 key 不读不写。

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// 驱逐批次(对齐 CPA CodexReasoningReplayCacheEvictBatchSize)
const EVICT_BATCH_SIZE: usize = 128;

struct Entry<V> {
    value: V,
    stored_at: Instant,
}

pub(crate) struct TtlLru<V> {
    inner: Arc<Mutex<HashMap<String, Entry<V>>>>,
    ttl: Duration,
    capacity: usize,
}

impl<V> Clone for TtlLru<V> {
    fn clone(&self) -> Self {
        Self {
            inner: Arc::clone(&self.inner),
            ttl: self.ttl,
            capacity: self.capacity,
        }
    }
}

impl<V: Clone> TtlLru<V> {
    pub(crate) fn new(ttl: Duration, capacity: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
            ttl,
            capacity,
        }
    }

    /// 读取未过期值并续期;过期条目顺带删除
    pub(crate) fn get_touch(&self, key: &str) -> Option<V> {
        let key = key.trim();
        if key.is_empty() {
            return None;
        }
        let mut map = self.inner.lock().unwrap();
        match map.get_mut(key) {
            Some(entry) if entry.stored_at.elapsed() <= self.ttl => {
                entry.stored_at = Instant::now();
                Some(entry.value.clone())
            }
            Some(_) => {
                map.remove(key);
                None
            }
            None => None,
        }
    }

    /// 在同一把锁内基于现值(过期视为无)计算新值并写入
    pub(crate) fn upsert(&self, key: &str, f: impl FnOnce(Option<&V>) -> V) {
        let key = key.trim();
        if key.is_empty() {
            return;
        }
        let mut map = self.inner.lock().unwrap();
        if map.len() >= self.capacity && !map.contains_key(key) {
            evict_oldest_locked(&mut map, EVICT_BATCH_SIZE);
        }
        let existing = map
            .get(key)
            .filter(|e| e.stored_at.elapsed() <= self.ttl)
            .map(|e| &e.value);
        let value = f(existing);
        map.insert(
            key.to_string(),
            Entry {
                value,
                stored_at: Instant::now(),
            },
        );
    }

    pub(crate) fn insert(&self, key: &str, value: V) {
        self.upsert(key, |_| value);
    }

    pub(crate) fn remove(&self, key: &str) {
        let key = key.trim();
        if key.is_empty() {
            return;
        }
        self.inner.lock().unwrap().remove(key);
    }
}

fn evict_oldest_locked<V>(map: &mut HashMap<String, Entry<V>>, count: usize) {
    if count == 0 || map.is_empty() {
        return;
    }
    let mut candidates: Vec<(String, Instant)> =
        map.iter().map(|(k, v)| (k.clone(), v.stored_at)).collect();
    candidates.sort_by_key(|(_, ts)| *ts);
    for (key, _) in candidates.into_iter().take(count) {
        map.remove(&key);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_trimmed_and_empty_ignored() {
        let cache = TtlLru::new(Duration::from_secs(60), 10);
        cache.insert(" s1 ", 1);
        assert_eq!(cache.get_touch("s1"), Some(1));
        cache.insert("  ", 2);
        assert_eq!(cache.get_touch(""), None);
        cache.remove(" s1");
        assert_eq!(cache.get_touch("s1"), None);
    }

    #[test]
    fn expired_entry_is_dropped_and_not_seen_by_upsert() {
        let cache = TtlLru::new(Duration::from_millis(0), 10);
        cache.insert("s", 1);
        std::thread::sleep(Duration::from_millis(2));
        cache.upsert("s", |old| {
            assert!(old.is_none());
            2
        });
        std::thread::sleep(Duration::from_millis(2));
        assert_eq!(cache.get_touch("s"), None);
    }

    #[test]
    fn full_cache_evicts_oldest_batch_on_new_key_only() {
        let cache = TtlLru::new(Duration::from_secs(60), 200);
        for i in 0..200 {
            cache.insert(&format!("k{i}"), i);
            std::thread::sleep(Duration::from_micros(100));
        }
        cache.insert("k0", 0); // 已有 key 不触发驱逐
        assert_eq!(cache.get_touch("k1"), Some(1));
        cache.insert("new", 999);
        assert_eq!(cache.get_touch("k2"), None);
        assert_eq!(cache.get_touch("k199"), Some(199));
        assert_eq!(cache.get_touch("new"), Some(999));
    }
}
