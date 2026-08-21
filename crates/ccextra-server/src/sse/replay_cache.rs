// reasoning replay 缓存(server 层状态,对齐 CPA internal/cache 的
// codex 累积 replay 存取;core 无 IO,状态由本层持有)
//
// key = "{model}:{session key}"(对齐 codexReasoningReplayCacheKey 的
// model+session 连续性边界;session 与 x-grok-conv-id 同源 cc_session)。
// value = 累积的 replay turns(每轮 [marker, ...items],256 turn/16MB
// 上限丢最老,marker id 幂等去重)。带 TTL 与容量上限,防泄漏。
// SSE 解析器不落本层:每条流各自持有(对齐 CPA 每请求局部收集),
// 同会话并发流互不串扰,流结束随闭包释放。

use ccextra_core::convert::{append_replay_turn, build_replay_turn, insert_replay_turns};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use super::parser::SseParser;

/// 每流一个的 SSE 提取器:累积解析本流 chunk,收集 output_item.done,
/// 命中 response.completed 补空 output 后写入缓存(对齐 CPA 流式路径的
/// 局部收集 + xaiPatchCompletedOutput,流结束随闭包释放)。
pub struct StreamReplayExtractor {
    cache: ReplayCache,
    session_key: String,
    parser: SseParser,
    /// 带 output_index 的 done 项(对齐 outputItemsByIndex)
    items_by_index: HashMap<i64, Value>,
    /// 无 output_index 的 done 项(对齐 outputItemsFallback)
    items_fallback: Vec<Value>,
}

impl StreamReplayExtractor {
    pub fn new(cache: ReplayCache, session_key: String) -> Self {
        Self {
            cache,
            session_key,
            parser: SseParser::new(),
            items_by_index: HashMap::new(),
            items_fallback: Vec::new(),
        }
    }

    /// 喂入上游 chunk,收集 output_item.done,遇 completed 补空 output 后缓存
    pub fn push(&mut self, bytes: &[u8]) {
        for event in self.parser.push(bytes) {
            let value: Value = match serde_json::from_str(&event.data) {
                Ok(v) => v,
                Err(_) => continue,
            };
            let event_type = event
                .event
                .as_deref()
                .or_else(|| value.get("type").and_then(|v| v.as_str()));
            match event_type {
                Some("output_item.done") => {
                    self.collect_output_item_done(&value);
                }
                Some("response.completed") => {
                    let patched = self.patch_completed_output(value);
                    self.cache.store_from_completed(&self.session_key, &patched);
                }
                _ => {}
            }
        }
    }

    /// 收集 output_item.done 的 item 字段(对齐 xaiCollectOutputItemDone)
    fn collect_output_item_done(&mut self, event_data: &Value) {
        let Some(item) = event_data.get("item") else {
            return;
        };
        if let Some(idx) = event_data.get("output_index").and_then(|v| v.as_i64()) {
            self.items_by_index.insert(idx, item.clone());
        } else {
            self.items_fallback.push(item.clone());
        }
    }

    /// completed 若无 output,用收集的 items 补上(对齐 xaiPatchCompletedOutput)
    fn patch_completed_output(&self, mut completed: Value) -> Value {
        let has_output = completed
            .pointer("/response/output")
            .and_then(|v| v.as_array())
            .map(|arr| !arr.is_empty())
            .unwrap_or(false);
        if has_output || (self.items_by_index.is_empty() && self.items_fallback.is_empty()) {
            return completed;
        }
        let mut indexes: Vec<i64> = self.items_by_index.keys().copied().collect();
        indexes.sort_unstable();
        let mut output = Vec::with_capacity(indexes.len() + self.items_fallback.len());
        for idx in indexes {
            if let Some(item) = self.items_by_index.get(&idx) {
                output.push(item.clone());
            }
        }
        output.extend(self.items_fallback.iter().cloned());
        if let Some(obj) = completed.as_object_mut() {
            if let Some(response) = obj.get_mut("response").and_then(|v| v.as_object_mut()) {
                response.insert("output".to_string(), Value::Array(output));
            }
        }
        completed
    }
}

/// 缓存条目
struct Entry {
    items: Vec<Value>,
    stored_at: Instant,
}

/// replay 缓存(会话 → 上一轮 replay 项)
#[derive(Clone)]
pub struct ReplayCache {
    inner: Arc<Mutex<HashMap<String, Entry>>>,
    ttl: Duration,
    capacity: usize,
}

impl ReplayCache {
    /// capacity 上限(超限清空,对齐 secret 缓存的简单策略)
    pub fn new(ttl: Duration, capacity: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(HashMap::new())),
            ttl,
            capacity,
        }
    }

    /// 从 response.completed 事件 data 累积缓存 replay 项。
    /// 每轮构造 [marker, ...items] 追加(对齐 CPA
    /// AppendCodexReasoningReplayItemsBestEffort + appendCodexReasoningReplayTurn):
    /// marker id 含 request 指纹,同轮重放幂等去重;超 256 turn/16MB 丢最老。
    /// 无可回放项时不动缓存(对齐 CPA cacheCodexReasoningReplayFromCompleted
    /// 直接 return;累积语义下纯文本轮清空会丢全部历史 reasoning)。
    ///
    /// request_fingerprint 传空串:真值需请求侧 input 前缀指纹,本函数无 body
    /// 上下文(StreamReplayExtractor 亦无)。取舍:锚定退化为 call_ids/
    /// assistant_fingerprint 双通道,已覆盖主场景;防错位增强留待需要时给
    /// StreamReplayExtractor::new 加参数从 http.rs 传入。
    pub fn store_from_completed(&self, session_key: &str, completed: &Value) {
        if session_key.trim().is_empty() {
            return;
        }
        let Some(turn) = build_replay_turn(completed, "") else {
            return;
        };
        let mut map = self.inner.lock().unwrap();
        if map.len() >= self.capacity && !map.contains_key(session_key) {
            map.clear();
        }
        let existing = map
            .get(session_key)
            .filter(|e| e.stored_at.elapsed() <= self.ttl)
            .map(|e| e.items.clone())
            .unwrap_or_default();
        let merged = append_replay_turn(&existing, &turn);
        map.insert(
            session_key.to_string(),
            Entry {
                items: merged,
                stored_at: Instant::now(),
            },
        );
    }

    /// 删除该会话缓存(上游拒绝 replay 项时调用,对齐
    /// DeleteXAIReasoningReplayItemRequired)
    pub fn invalidate(&self, session_key: &str) {
        let trimmed = session_key.trim();
        if trimmed.is_empty() {
            return;
        }
        self.inner.lock().unwrap().remove(trimmed);
    }

    /// 取上一轮 replay 项并注入 body.input(过滤 + 对齐 + 插入全在 core)。
    /// 无缓存/过滤后为空返回 false。命中时刷新时间戳(对齐 CPA 读时
    /// 滑动续期:GetXAIReasoningReplayItemsRequired 更新 entry.Timestamp)。
    pub fn apply_to_body(&self, session_key: &str, body: &mut Value) -> bool {
        let trimmed = session_key.trim();
        if trimmed.is_empty() {
            return false;
        }
        let session_key = trimmed;
        let items = {
            let mut map = self.inner.lock().unwrap();
            match map.get_mut(session_key) {
                Some(entry) if entry.stored_at.elapsed() <= self.ttl => {
                    entry.stored_at = Instant::now();
                    entry.items.clone()
                }
                Some(_) => {
                    map.remove(session_key);
                    return false;
                }
                None => return false,
            }
        };
        insert_replay_turns(body, items)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn completed() -> Value {
        json!({"type": "response.completed", "response": {"id": "r1", "output": [
            {"type": "reasoning", "encrypted_content": "gAAA"},
            {"type": "function_call", "call_id": "call_1", "name": "f", "arguments": "{}"}
        ]}})
    }

    #[test]
    fn test_store_and_apply_roundtrip() {
        let cache = ReplayCache::new(Duration::from_secs(60), 128);
        cache.store_from_completed("sess-1", &completed());
        let mut body = json!({"input": [
            {"type": "message", "role": "user", "content": "q"},
            {"type": "function_call_output", "call_id": "call_1", "output": "ok"}
        ]});
        assert!(cache.apply_to_body("sess-1", &mut body));
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 4);
        assert_eq!(input[1]["type"], "reasoning");
    }

    #[test]
    fn test_no_replayable_output_keeps_cache() {
        let cache = ReplayCache::new(Duration::from_secs(60), 128);
        cache.store_from_completed("sess-1", &completed());
        // 纯文本轮(无可回放项)→ 不动缓存(对齐 CPA 直接 return;
        // 累积语义下清空会丢全部历史 reasoning)
        cache.store_from_completed(
            "sess-1",
            &json!({"type": "response.completed", "response": {"output": [
                {"type": "web_search_call"}
            ]}}),
        );
        let mut body = json!({"input": [
            {"type": "message", "role": "user", "content": "q"},
            {"type": "function_call_output", "call_id": "call_1", "output": "ok"}
        ]});
        assert!(cache.apply_to_body("sess-1", &mut body));
        let input = body["input"].as_array().unwrap();
        assert!(input.iter().any(|i| i["type"] == "reasoning"));
    }

    #[test]
    fn test_ttl_expiry() {
        let cache = ReplayCache::new(Duration::from_millis(0), 128);
        cache.store_from_completed("sess-1", &completed());
        std::thread::sleep(Duration::from_millis(2));
        let mut body = json!({"input": []});
        assert!(!cache.apply_to_body("sess-1", &mut body));
    }

    #[test]
    fn test_cumulative_store_two_rounds() {
        let cache = ReplayCache::new(Duration::from_secs(60), 128);
        let round1 = json!({"type": "response.completed", "response": {"output": [
            {"type": "reasoning", "encrypted_content": "g1"},
            {"type": "function_call", "call_id": "c1", "name": "f", "arguments": "{}"}
        ]}});
        let round2 = json!({"type": "response.completed", "response": {"output": [
            {"type": "reasoning", "encrypted_content": "g2"},
            {"type": "function_call", "call_id": "c2", "name": "f", "arguments": "{}"}
        ]}});
        cache.store_from_completed("sess-1", &round1);
        cache.store_from_completed("sess-1", &round2);
        // 两轮 reasoning 都注入(input 含 c1/c2 的 output)
        let mut body = json!({"input": [
            {"type": "message", "role": "user", "content": "q"},
            {"type": "function_call_output", "call_id": "c1", "output": "ok1"},
            {"type": "function_call_output", "call_id": "c2", "output": "ok2"}
        ]});
        assert!(cache.apply_to_body("sess-1", &mut body));
        let input = body["input"].as_array().unwrap();
        assert!(input.iter().any(|i| i["encrypted_content"] == "g1"));
        assert!(input.iter().any(|i| i["encrypted_content"] == "g2"));
    }

    #[test]
    fn test_store_idempotent_same_completed() {
        let cache = ReplayCache::new(Duration::from_secs(60), 128);
        let round1 = json!({"type": "response.completed", "response": {"output": [
            {"type": "reasoning", "encrypted_content": "g1"}
        ]}});
        cache.store_from_completed("sess-1", &round1);
        cache.store_from_completed("sess-1", &round1); // 同轮重复(重试场景)
                                                       // marker id 相同,不重复累积
        let mut body = json!({"input": [
            {"type": "message", "role": "user", "content": "q"}
        ]});
        if cache.apply_to_body("sess-1", &mut body) {
            let count = body["input"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|i| i["encrypted_content"] == "g1")
                .count();
            assert_eq!(count, 1);
        }
    }

    #[test]
    fn test_patch_completed_empty_output() {
        let cache = ReplayCache::new(Duration::from_secs(60), 128);
        let key = "sess-patch".to_string();
        let mut extractor = StreamReplayExtractor::new(cache.clone(), key.clone());

        // 喂 output_item.done 事件
        let done1 = b"event: output_item.done\ndata: {\"output_index\":0,\"item\":{\"type\":\"reasoning\",\"encrypted_content\":\"dGVzdA\"}}\n\n";
        let done2 = b"event: output_item.done\ndata: {\"item\":{\"type\":\"function_call\",\"call_id\":\"c1\",\"name\":\"f\",\"arguments\":\"{}\"}}\n\n";
        extractor.push(done1);
        extractor.push(done2);

        // completed 无 output
        let completed_empty = b"event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"r2\"}}\n\n";
        extractor.push(completed_empty);

        // 验证缓存已补 output
        let mut body = json!({"input": [
            {"type": "message", "role": "user", "content": "q"},
            {"type": "function_call_output", "call_id": "c1", "output": "ok"}
        ]});
        assert!(cache.apply_to_body(&key, &mut body));
        let input = body["input"].as_array().unwrap();
        assert!(input.iter().any(|i| i["type"] == "reasoning"));
        assert!(input.iter().any(|i| i["type"] == "function_call"));
    }

    #[test]
    fn test_patch_completed_keeps_existing_output() {
        let cache = ReplayCache::new(Duration::from_secs(60), 128);
        let key = "sess-keep".to_string();
        let mut extractor = StreamReplayExtractor::new(cache.clone(), key.clone());

        extractor.push(b"event: output_item.done\ndata: {\"item\":{\"type\":\"reasoning\",\"encrypted_content\":\"orphan\"}}\n\n");

        // completed 已有 output
        let completed_with = b"event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"type\":\"reasoning\",\"encrypted_content\":\"original\"}]}}\n\n";
        extractor.push(completed_with);

        let mut body = json!({"input": []});
        assert!(cache.apply_to_body(&key, &mut body));
        let input = body["input"].as_array().unwrap();
        // 应该用 original 不是 orphan
        assert_eq!(input[0]["encrypted_content"], "original");
    }
}
