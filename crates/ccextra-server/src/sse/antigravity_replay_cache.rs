//! Antigravity thoughtSignature replay 缓存（server 层状态）。
//!
//! 与 Responses `ReplayCache`（input/encrypted_content）分离：
//! key = "{model}:{session}"，value = 规范化的 AG replay items 账本。

use ccextra_core::convert::{
    apply_ag_replay_items, collect_ag_replay_from_response, normalize_ag_replay_items,
    AgReplayAccumulator,
};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

const EVICT_BATCH_SIZE: usize = 128;

struct Entry {
    items: Vec<Value>,
    stored_at: Instant,
}

/// Antigravity / Gemini thoughtSignature 账本缓存。
#[derive(Clone)]
pub struct AgReplayCache {
    inner: Arc<Mutex<HashMap<String, Entry>>>,
    ttl: Duration,
    capacity: usize,
}

impl AgReplayCache {
    pub fn new(ttl: Duration, capacity: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
            ttl,
            capacity,
        }
    }

    /// 用规范化 items 覆盖会话账本；空切片表示清空。
    pub fn replace_items(&self, session_key: &str, items: Vec<Value>) {
        let trimmed = session_key.trim();
        if trimmed.is_empty() {
            return;
        }
        let mut map = self.inner.lock().unwrap();
        if items.is_empty() {
            map.remove(trimmed);
            return;
        }
        let Some(normalized) = normalize_ag_replay_items(&items) else {
            map.remove(trimmed);
            return;
        };
        if map.len() >= self.capacity && !map.contains_key(trimmed) {
            Self::evict_oldest_locked(&mut map, EVICT_BATCH_SIZE);
        }
        map.insert(
            trimmed.to_string(),
            Entry {
                items: normalized,
                stored_at: Instant::now(),
            },
        );
    }

    pub fn invalidate(&self, session_key: &str) {
        let trimmed = session_key.trim();
        if trimmed.is_empty() {
            return;
        }
        self.inner.lock().unwrap().remove(trimmed);
    }

    fn get_items(&self, session_key: &str) -> Option<Vec<Value>> {
        let trimmed = session_key.trim();
        if trimmed.is_empty() {
            return None;
        }
        let mut map = self.inner.lock().unwrap();
        match map.get_mut(trimmed) {
            Some(entry) if entry.stored_at.elapsed() <= self.ttl => {
                entry.stored_at = Instant::now();
                Some(entry.items.clone())
            }
            Some(_) => {
                map.remove(trimmed);
                None
            }
            None => None,
        }
    }

    /// 将缓存签名回填到 Antigravity 外层 body 的 `request` 对象。
    pub fn apply_to_antigravity_body(&self, session_key: &str, body: &mut Value) -> bool {
        let Some(items) = self.get_items(session_key) else {
            return false;
        };
        let Some(request) = body.get_mut("request") else {
            return false;
        };
        apply_ag_replay_items(request, &items)
    }

    /// 非流：从上游响应收集并提交账本。
    /// `request` 为 Antigravity body 的 `request` 子对象（含 contents）。
    pub fn store_from_response(&self, session_key: &str, request: &Value, response_body: &Value) {
        match collect_ag_replay_from_response(request, response_body) {
            Some(items) => self.replace_items(session_key, items),
            None => {} // 非 terminal：不动缓存
        }
    }

    fn evict_oldest_locked(map: &mut HashMap<String, Entry>, count: usize) {
        if count == 0 || map.is_empty() {
            return;
        }
        let mut candidates: Vec<(String, Instant)> =
            map.iter().map(|(k, v)| (k.clone(), v.stored_at)).collect();
        candidates.sort_by_key(|(_, t)| *t);
        for (key, _) in candidates.into_iter().take(count) {
            map.remove(&key);
        }
    }
}

/// 流式提取：旁路解析上游 Gemini/AG SSE。
///
/// 对齐 CPA：`finishReason` 只标记 terminal，不立即落库；signature / usage
/// 可能跟在后续帧。仅在流结束（`finish` / `Drop`）时提交。
pub struct AgStreamReplayExtractor {
    cache: AgReplayCache,
    session_key: String,
    accumulator: AgReplayAccumulator,
    parser: super::parser::SseParser,
    committed: bool,
}

impl AgStreamReplayExtractor {
    pub fn new(cache: AgReplayCache, session_key: String, request: Value) -> Self {
        let accumulator = AgReplayAccumulator::new_from_request(&request);
        Self {
            cache,
            session_key,
            accumulator,
            parser: super::parser::SseParser::new(),
            committed: false,
        }
    }

    /// 只观察，不提交（finishReason 帧之后仍可能有 thoughtSignature）。
    pub fn push(&mut self, bytes: &[u8]) {
        for event in self.parser.push(bytes) {
            let Ok(value) = serde_json::from_str::<Value>(&event.data) else {
                continue;
            };
            self.accumulator.observe_response(&value);
        }
    }

    /// 流结束：flush SSE 残留并在 terminal 时提交账本。
    pub fn finish(&mut self) {
        if self.committed {
            return;
        }
        for event in self.parser.finish() {
            let Ok(value) = serde_json::from_str::<Value>(&event.data) else {
                continue;
            };
            self.accumulator.observe_response(&value);
        }
        self.commit();
    }

    fn commit(&mut self) {
        if self.committed {
            return;
        }
        if let Some(items) = self.accumulator.take_commit_items() {
            self.cache.replace_items(&self.session_key, items);
            self.committed = true;
        }
    }
}

impl Drop for AgStreamReplayExtractor {
    fn drop(&mut self) {
        self.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn nonstream_roundtrip_merges_function_call_signature() {
        let cache = AgReplayCache::new(Duration::from_secs(60), 128);
        let key = "gemini-flash:sess-1";
        let request = json!({
            "contents": [{"role": "user", "parts": [{"text": "hi"}]}]
        });
        let response = json!({
            "response": {
                "candidates": [{
                    "content": {"parts": [{
                        "thoughtSignature": "sig-first-long-enough",
                        "functionCall": {"id": "id1", "name": "Read", "args": {"file_path": "/a"}}
                    }]},
                    "finishReason": "STOP"
                }]
            }
        });
        cache.store_from_response(key, &request, &response);

        let mut body = json!({
            "request": {
                "contents": [
                    {"role": "user", "parts": [{"text": "hi"}]},
                    {"role": "model", "parts": [{
                        "functionCall": {"id": "id1", "name": "Read", "args": {"file_path": "/a"}}
                    }]},
                    {"role": "user", "parts": [{
                        "functionResponse": {"id": "id1", "name": "Read", "response": {"result": "ok"}}
                    }]}
                ]
            }
        });
        assert!(cache.apply_to_antigravity_body(key, &mut body));
        assert_eq!(
            body["request"]["contents"][1]["parts"][0]["thoughtSignature"],
            "sig-first-long-enough"
        );
    }

    #[test]
    fn nonstream_partial_does_not_overwrite_cache() {
        let cache = AgReplayCache::new(Duration::from_secs(60), 128);
        let key = "gemini-flash:sess-2";
        cache.replace_items(
            key,
            vec![json!({
                "type": "thought_signature",
                "thoughtSignature": "old-signature-123456789",
                "contentIndex": 1,
                "partIndex": 0,
            })],
        );
        let request = json!({"contents": [{"role": "user", "parts": [{"text": "x"}]}]});
        let partial = json!({
            "response": {"candidates": [{"content": {"parts": [
                {"text": "partial"},
                {"text": "", "thoughtSignature": "partial-signature-123456789"}
            ]}}]}
        });
        cache.store_from_response(key, &request, &partial);
        // 仍保留旧账本
        let mut body = json!({
            "request": {"contents": [
                {"role": "user", "parts": [{"text": "x"}]},
                {"role": "model", "parts": [{"text": "old"}]}
            ]}
        });
        // old item 无 targetHash 且 partIndex 可能匹配 model text → 可能注入；
        // 关键是缓存未被 partial 清空。取 items 非空即可。
        assert!(cache.get_items(key).is_some());
        let _ = cache.apply_to_antigravity_body(key, &mut body);
    }

    #[test]
    fn terminal_empty_clears_cache() {
        let cache = AgReplayCache::new(Duration::from_secs(60), 128);
        let key = "gemini-flash:sess-3";
        cache.replace_items(
            key,
            vec![json!({
                "type": "thought_signature",
                "thoughtSignature": "old-signature-123456789",
                "contentIndex": 1,
                "partIndex": 0,
            })],
        );
        let request = json!({"contents": [{"role": "user", "parts": [{"text": "new"}]}]});
        let response = json!({
            "response": {"candidates": [{"content": {"parts": [
                {"text": "answer without signature"}
            ]}, "finishReason": "STOP"}]}
        });
        cache.store_from_response(key, &request, &response);
        assert!(cache.get_items(key).is_none());
    }

    #[test]
    fn stream_extractor_commits_on_finish_at_eof() {
        let cache = AgReplayCache::new(Duration::from_secs(60), 128);
        let key = "gemini-flash:sess-stream".to_string();
        let request = json!({"contents": [{"role": "user", "parts": [{"text": "hi"}]}]});
        let mut extractor = AgStreamReplayExtractor::new(cache.clone(), key.clone(), request);
        extractor.push(
            br#"data: {"response":{"candidates":[{"content":{"parts":[{"thoughtSignature":"sig-first-long-enough","functionCall":{"name":"Read","args":{"file_path":"/a"},"id":"id1"}}]}}]}}

"#,
        );
        extractor.push(
            br#"data: {"response":{"candidates":[{"content":{"parts":[{"functionCall":{"name":"Read","args":{"file_path":"/b"},"id":"id2"}}]},"finishReason":"STOP"}]}}

"#,
        );
        // finishReason 到达时尚未落库（对齐 CPA：签名帧可能跟在后面）
        assert!(cache.get_items(&key).is_none());
        extractor.finish();
        let items = cache.get_items(&key).expect("committed at eof");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["thoughtSignature"], "sig-first-long-enough");
    }

    /// 对齐 CPA issue6332_review7：finishReason 先到，thoughtSignature 后到。
    #[test]
    fn stream_extractor_keeps_observing_after_finish_reason() {
        let cache = AgReplayCache::new(Duration::from_secs(60), 128);
        let key = "gemini-flash:sess-split".to_string();
        let request = json!({"contents": [{"role": "user", "parts": [{"text": "hi"}]}]});
        let mut extractor = AgStreamReplayExtractor::new(cache.clone(), key.clone(), request);
        let sig = "s".repeat(128);
        extractor.push(
            br#"data: {"response":{"candidates":[{"content":{"parts":[{"functionCall":{"id":"native-call","name":"lookup","args":{"key":"value"}}}]},"finishReason":"STOP"}]}}

"#,
        );
        assert!(
            cache.get_items(&key).is_none(),
            "must not commit on first finishReason"
        );
        let late_sig = format!(
            "data: {{\"response\":{{\"candidates\":[{{\"content\":{{\"parts\":[{{\"text\":\"\",\"thoughtSignature\":\"{sig}\"}}]}}}}]}}}}\n\n"
        );
        extractor.push(late_sig.as_bytes());
        extractor.push(
            br#"data: {"response":{"usageMetadata":{"promptTokenCount":11,"candidatesTokenCount":22,"totalTokenCount":33}}}

"#,
        );
        extractor.finish();
        let items = cache
            .get_items(&key)
            .expect("committed with late signature");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["type"], "function_call_part");
        assert_eq!(items[0]["thoughtSignature"], sig);
        assert_eq!(items[0]["name"], "lookup");
    }

    #[test]
    fn stream_extractor_drop_commits_terminal() {
        let cache = AgReplayCache::new(Duration::from_secs(60), 128);
        let key = "gemini-flash:sess-drop".to_string();
        let request = json!({"contents": [{"role": "user", "parts": [{"text": "hi"}]}]});
        {
            let mut extractor = AgStreamReplayExtractor::new(cache.clone(), key.clone(), request);
            extractor.push(
                br#"data: {"response":{"candidates":[{"content":{"parts":[{"thoughtSignature":"sig-drop-long-enough12","functionCall":{"name":"Read","args":{"p":"/a"},"id":"id1"}}]},"finishReason":"STOP"}]}}

"#,
            );
            assert!(cache.get_items(&key).is_none());
            // drop → finish
        }
        let items = cache.get_items(&key).expect("Drop commits");
        assert_eq!(items[0]["thoughtSignature"], "sig-drop-long-enough12");
    }
}
