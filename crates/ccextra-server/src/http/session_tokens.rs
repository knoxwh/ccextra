use std::collections::HashMap;
use std::time::{Duration, Instant};

const CAPACITY: usize = 512;
const TTL: Duration = Duration::from_secs(30 * 60);

type Entry = (usize, Instant, u64);

pub struct SessionTokenCache {
    entries: HashMap<String, Entry>,
    next_sequence: u64,
}

impl SessionTokenCache {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
            next_sequence: 0,
        }
    }

    pub(crate) fn insert(&mut self, session: impl Into<String>, tokens: usize) {
        self.insert_at(session, tokens, Instant::now());
    }

    fn insert_at(&mut self, session: impl Into<String>, tokens: usize, written_at: Instant) {
        let session = session.into();
        let sequence = self.next_sequence;
        self.next_sequence = self.next_sequence.wrapping_add(1);
        self.entries.insert(session, (tokens, written_at, sequence));
        if self.entries.len() > CAPACITY {
            if let Some(oldest) = self
                .entries
                .iter()
                .min_by_key(|(_, (_, _, sequence))| *sequence)
                .map(|(session, _)| session.clone())
            {
                self.entries.remove(&oldest);
            }
        }
    }

    pub(crate) fn get(&mut self, session: &str) -> Option<usize> {
        let (tokens, written_at, _) = self.entries.get(session).copied()?;
        if written_at.elapsed() >= TTL {
            self.entries.remove(session);
            return None;
        }
        Some(tokens)
    }
}

impl Default for SessionTokenCache {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stores_and_reads_tokens() {
        let mut cache = SessionTokenCache::new();
        cache.insert("session", 42);
        assert_eq!(cache.get("session"), Some(42));
    }

    #[test]
    fn expired_entry_is_missing_without_refresh_on_read() {
        let mut cache = SessionTokenCache::new();
        cache.insert_at("session", 42, Instant::now() - TTL - Duration::from_secs(1));
        assert_eq!(cache.get("session"), None);
    }

    #[test]
    fn evicts_oldest_entry_at_capacity() {
        let mut cache = SessionTokenCache::new();
        for index in 0..CAPACITY {
            cache.insert_at(index.to_string(), index, Instant::now());
        }
        cache.insert_at("new", 999, Instant::now() + Duration::from_secs(1));
        assert_eq!(cache.get("0"), None);
        assert_eq!(cache.get("new"), Some(999));
    }

    #[test]
    fn updating_session_replaces_value_and_refreshes_write_time() {
        let mut cache = SessionTokenCache::new();
        cache.insert_at("session", 1, Instant::now() - TTL + Duration::from_secs(1));
        cache.insert("session", 2);
        assert_eq!(cache.get("session"), Some(2));
    }
}
