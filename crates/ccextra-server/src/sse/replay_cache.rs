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
    request_fingerprint: String,
    parser: SseParser,
    /// 带 output_index 的 done 项(对齐 outputItemsByIndex)
    items_by_index: HashMap<i64, Value>,
    /// 无 output_index 的 done 项(对齐 outputItemsFallback)
    items_fallback: Vec<Value>,
    /// 流式 reasoning 文本累积器(对齐 grok-build stream/responses.rs
    /// L273 reasoning_acc.push_str:不分 output_index 单一累积)
    reasoning_acc: String,
}

impl StreamReplayExtractor {
    pub fn new(cache: ReplayCache, session_key: String, request_fingerprint: String) -> Self {
        Self {
            cache,
            session_key,
            request_fingerprint,
            parser: SseParser::new(),
            items_by_index: HashMap::new(),
            items_fallback: Vec::new(),
            reasoning_acc: String::new(),
        }
    }

    /// 喂入上游 chunk,收集 output_item.done + reasoning_text.delta,
    /// 遇 completed 补空 output 后缓存(对齐 grok-build stream_responses
    /// L273 reasoning_acc 累积 + L549-550 inject_streaming_reasoning_fallback)
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
                // 上游真实事件名带 response. 前缀(见 sse/responses.rs 的
                // event_type 判别,data.type 同为全名)
                Some("response.reasoning_text.delta") => {
                    self.collect_reasoning_delta(&value);
                }
                Some("response.output_item.done") => {
                    self.collect_output_item_done(&value);
                }
                Some("response.completed") => {
                    let patched = self.patch_completed_output(value);
                    self.cache.store_from_completed(
                        &self.session_key,
                        &patched,
                        &self.request_fingerprint,
                    );
                }
                _ => {}
            }
        }
    }

    /// 收集 reasoning_text.delta(对齐 grok-build L273 reasoning_acc.push_str,
    /// 官方不分 output_index 单一累积)
    fn collect_reasoning_delta(&mut self, event_data: &Value) {
        let Some(delta) = event_data.get("delta").and_then(|v| v.as_str()) else {
            return;
        };
        self.reasoning_acc.push_str(delta);
    }

    /// 收集 output_item.done 的 item 字段(对齐 xaiCollectOutputItemDone,
    /// 原样存 item,不做 per-item 兜底——reasoning 缺失文本统一由
    /// completed 时的 merge_streamed_reasoning_summaries 处理,对齐
    /// grok-build 只在最终 response 上 inject_streaming_reasoning_fallback)
    fn collect_output_item_done(&mut self, event_data: &Value) {
        let Some(item) = event_data.get("item").cloned() else {
            return;
        };
        if let Some(idx) = event_data.get("output_index").and_then(|v| v.as_i64()) {
            self.items_by_index.insert(idx, item);
        } else {
            self.items_fallback.push(item);
        }
    }

    /// completed 若无 output,用收集的 items 补上(对齐 xaiPatchCompletedOutput);
    /// 之后统一注入流式 reasoning 兜底(对齐 grok-build 流式路径在最终
    /// response 上调用 inject_streaming_reasoning_fallback L549-550)
    fn patch_completed_output(&self, mut completed: Value) -> Value {
        let has_output = completed
            .pointer("/response/output")
            .and_then(|v| v.as_array())
            .map(|arr| !arr.is_empty())
            .unwrap_or(false);
        if !has_output && !(self.items_by_index.is_empty() && self.items_fallback.is_empty()) {
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
        }
        self.merge_streamed_reasoning_summaries(&mut completed);
        completed
    }

    /// 流式 reasoning 兜底注入(对齐 grok-build
    /// inject_streaming_reasoning_fallback L1493-1526):
    /// - 无累积文本 → 不动
    /// - output 任一 reasoning 已带 summary 文本 → 不动
    /// - 有 reasoning 无文本 → 把累积文本 push 进第一个的 summary(不覆盖)
    /// - 无 reasoning → 在尾 assistant 前插入合成 reasoning 项
    fn merge_streamed_reasoning_summaries(&self, completed: &mut Value) {
        if self.reasoning_acc.is_empty() {
            return;
        }
        let Some(output) = completed
            .pointer_mut("/response/output")
            .and_then(|v| v.as_array_mut())
        else {
            return;
        };
        let any_with_text = output.iter().any(|item| {
            item.get("type").and_then(|v| v.as_str()) == Some("reasoning")
                && reasoning_item_has_text(item)
        });
        if any_with_text {
            return;
        }
        if let Some(item) = output
            .iter_mut()
            .find(|i| i.get("type").and_then(|v| v.as_str()) == Some("reasoning"))
        {
            let text = self.reasoning_acc.clone();
            if let Some(obj) = item.as_object_mut() {
                if let Some(arr) = obj
                    .get("summary")
                    .and_then(|v| v.as_array())
                    .map(|a| a.to_vec())
                {
                    let mut pushable = arr;
                    pushable.push(serde_json::json!({"type": "summary_text", "text": text}));
                    obj.insert("summary".to_string(), Value::Array(pushable));
                } else {
                    obj.insert(
                        "summary".to_string(),
                        serde_json::json!([{"type": "summary_text", "text": text}]),
                    );
                }
            }
            return;
        }
        // 无 reasoning:在尾 assistant 组首项前插入合成项(对齐官方
        // rposition(Assistant);转入 output 形状,assistant 组由 message/
        // function_call 折叠而成,首项即组起点)
        let pos = output
            .iter()
            .position(|i| {
                matches!(
                    i.get("type").and_then(|v| v.as_str()).unwrap_or(""),
                    "message" | "function_call" | "custom_tool_call"
                )
            })
            .unwrap_or(output.len());
        output.insert(
            pos,
            serde_json::json!({"type": "reasoning", "summary": [
                {"type": "summary_text", "text": self.reasoning_acc}
            ]}),
        );
    }
}

/// reasoning item 是否已带 summary 文本(对齐官方
/// inject_streaming_reasoning_fallback 的 any_with_text,只查 summary;
/// encrypted 项 summary 为空数组视为无文本)
fn reasoning_item_has_text(item: &Value) -> bool {
    item.get("summary")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter().any(|sp| {
                sp.get("text")
                    .and_then(|t| t.as_str())
                    .map(|s| !s.is_empty())
                    .unwrap_or(false)
            })
        })
        .unwrap_or(false)
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
    /// request_fingerprint 由请求侧 input 前缀指纹传入,用于 marker 精确锚定。
    pub fn store_from_completed(
        &self,
        session_key: &str,
        completed: &Value,
        request_fingerprint: &str,
    ) {
        if session_key.trim().is_empty() {
            return;
        }
        let Some(turn) = build_replay_turn(completed, request_fingerprint) else {
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
    /// keep_plain_reasoning=true 时保留无 encrypted_content 但带摘要文本
    /// 的 reasoning(grok 上游启用,对齐 grok-build 官方行为)。
    pub fn apply_to_body(
        &self,
        session_key: &str,
        body: &mut Value,
        keep_plain_reasoning: bool,
    ) -> bool {
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
        insert_replay_turns(body, items, keep_plain_reasoning)
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
        cache.store_from_completed("sess-1", &completed(), "");
        let mut body = json!({"input": [
            {"type": "message", "role": "user", "content": "q"},
            {"type": "function_call_output", "call_id": "call_1", "output": "ok"}
        ]});
        assert!(cache.apply_to_body("sess-1", &mut body, false));
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 4);
        assert_eq!(input[1]["type"], "reasoning");
    }

    #[test]
    fn test_no_replayable_output_keeps_cache() {
        let cache = ReplayCache::new(Duration::from_secs(60), 128);
        cache.store_from_completed("sess-1", &completed(), "");
        // 纯文本轮(无可回放项)→ 不动缓存(对齐 CPA 直接 return;
        // 累积语义下清空会丢全部历史 reasoning)
        cache.store_from_completed(
            "sess-1",
            &json!({"type": "response.completed", "response": {"output": [
                {"type": "web_search_call"}
            ]}}),
            "",
        );
        let mut body = json!({"input": [
            {"type": "message", "role": "user", "content": "q"},
            {"type": "function_call_output", "call_id": "call_1", "output": "ok"}
        ]});
        assert!(cache.apply_to_body("sess-1", &mut body, false));
        let input = body["input"].as_array().unwrap();
        assert!(input.iter().any(|i| i["type"] == "reasoning"));
    }

    #[test]
    fn test_ttl_expiry() {
        let cache = ReplayCache::new(Duration::from_millis(0), 128);
        cache.store_from_completed("sess-1", &completed(), "");
        std::thread::sleep(Duration::from_millis(2));
        let mut body = json!({"input": []});
        assert!(!cache.apply_to_body("sess-1", &mut body, false));
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
        cache.store_from_completed("sess-1", &round1, "");
        cache.store_from_completed("sess-1", &round2, "");
        // 两轮 reasoning 都注入(input 含 c1/c2 的 output)
        let mut body = json!({"input": [
            {"type": "message", "role": "user", "content": "q"},
            {"type": "function_call_output", "call_id": "c1", "output": "ok1"},
            {"type": "function_call_output", "call_id": "c2", "output": "ok2"}
        ]});
        assert!(cache.apply_to_body("sess-1", &mut body, false));
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
        cache.store_from_completed("sess-1", &round1, "");
        cache.store_from_completed("sess-1", &round1, ""); // 同轮重复(重试场景)
                                                           // marker id 相同,不重复累积
        let mut body = json!({"input": [
            {"type": "message", "role": "user", "content": "q"}
        ]});
        if cache.apply_to_body("sess-1", &mut body, false) {
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
        let mut extractor = StreamReplayExtractor::new(cache.clone(), key.clone(), String::new());

        // 喂 output_item.done 事件
        let done1 = b"event: response.output_item.done\ndata: {\"output_index\":0,\"item\":{\"type\":\"reasoning\",\"encrypted_content\":\"dGVzdA\"}}\n\n";
        let done2 = b"event: response.output_item.done\ndata: {\"item\":{\"type\":\"function_call\",\"call_id\":\"c1\",\"name\":\"f\",\"arguments\":\"{}\"}}\n\n";
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
        assert!(cache.apply_to_body(&key, &mut body, false));
        let input = body["input"].as_array().unwrap();
        assert!(input.iter().any(|i| i["type"] == "reasoning"));
        assert!(input.iter().any(|i| i["type"] == "function_call"));
    }

    #[test]
    fn test_patch_completed_keeps_existing_output() {
        let cache = ReplayCache::new(Duration::from_secs(60), 128);
        let key = "sess-keep".to_string();
        let mut extractor = StreamReplayExtractor::new(cache.clone(), key.clone(), String::new());

        extractor.push(b"event: response.output_item.done\ndata: {\"item\":{\"type\":\"reasoning\",\"encrypted_content\":\"orphan\"}}\n\n");

        // completed 已有 output
        let completed_with = b"event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"type\":\"reasoning\",\"encrypted_content\":\"original\"}]}}\n\n";
        extractor.push(completed_with);

        let mut body = json!({"input": []});
        assert!(cache.apply_to_body(&key, &mut body, false));
        let input = body["input"].as_array().unwrap();
        // 应该用 original 不是 orphan
        assert_eq!(input[0]["encrypted_content"], "original");
    }

    #[test]
    fn test_inject_streaming_reasoning_fallback_with_deltas() {
        let cache = ReplayCache::new(Duration::from_secs(60), 128);
        let key = "sess-deltas".to_string();
        let mut extractor = StreamReplayExtractor::new(cache.clone(), key.clone(), String::new());

        // 流式 reasoning 文本累积(对齐 grok-build L273 reasoning_acc.push_str)
        extractor.push(
            b"event: response.reasoning_text.delta\ndata: {\"output_index\":0,\"delta\":\"thinking \"}\n\n",
        );
        extractor.push(
            b"event: response.reasoning_text.delta\ndata: {\"output_index\":0,\"delta\":\"hard\"}\n\n",
        );

        // output_item.done 无 content/summary → 兜底补 summary(对齐 inject_streaming_reasoning_fallback)
        extractor.push(b"event: response.output_item.done\ndata: {\"output_index\":0,\"item\":{\"type\":\"reasoning\",\"encrypted_content\":\"gAAA\"}}\n\n");
        extractor.push(b"event: response.output_item.done\ndata: {\"item\":{\"type\":\"function_call\",\"call_id\":\"c1\",\"name\":\"f\",\"arguments\":\"{}\"}}\n\n");

        // completed 无 output → patch 补上
        let completed_empty = b"event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"r3\"}}\n\n";
        extractor.push(completed_empty);

        // 验证注入的 reasoning item 带 summary(流式文本兜底)
        let mut body = json!({"input": [
            {"type": "message", "role": "user", "content": "q"},
            {"type": "function_call_output", "call_id": "c1", "output": "ok"}
        ]});
        assert!(cache.apply_to_body(&key, &mut body, false));
        let input = body["input"].as_array().unwrap();
        let reasoning = input.iter().find(|i| i["type"] == "reasoning").unwrap();
        assert_eq!(reasoning["encrypted_content"], "gAAA");
        let summary = reasoning["summary"].as_array().unwrap();
        assert_eq!(summary[0]["text"], "thinking hard");
    }

    #[test]
    fn test_reasoning_delta_fallback_no_output_index() {
        let cache = ReplayCache::new(Duration::from_secs(60), 128);
        let key = "sess-fallback".to_string();
        let mut extractor = StreamReplayExtractor::new(cache.clone(), key.clone(), String::new());

        // 无 output_index 的 delta → fallback 槽
        extractor
            .push(b"event: response.reasoning_text.delta\ndata: {\"delta\":\"fallback text\"}\n\n");

        // output_item.done 无 output_index 且无 content/summary → 用 fallback 槽兜底
        extractor.push(b"event: response.output_item.done\ndata: {\"item\":{\"type\":\"reasoning\",\"encrypted_content\":\"xAI\"}}\n\n");

        let completed_empty = b"event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"r4\"}}\n\n";
        extractor.push(completed_empty);

        let mut body = json!({"input": []});
        assert!(cache.apply_to_body(&key, &mut body, false));
        let input = body["input"].as_array().unwrap();
        let reasoning = input.iter().find(|i| i["type"] == "reasoning").unwrap();
        assert_eq!(reasoning["summary"][0]["text"], "fallback text");
    }

    #[test]
    fn test_fallback_inserts_synthetic_reasoning_before_assistant() {
        let cache = ReplayCache::new(Duration::from_secs(60), 128);
        let key = "sess-synth".to_string();
        let mut extractor = StreamReplayExtractor::new(cache.clone(), key.clone(), String::new());

        extractor.push(b"event: response.reasoning_text.delta\ndata: {\"delta\":\"think\"}\n\n");
        // completed 自带 output 且无 reasoning 项 → 合成 reasoning 前插
        // (对齐官方 L1518-1525 无 Reasoning 时 insert 合成项)
        extractor.push(b"event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"type\":\"function_call\",\"call_id\":\"c2\",\"name\":\"f\",\"arguments\":\"{}\"}]}}\n\n");

        let mut body = json!({"input": [
            {"type": "function_call_output", "call_id": "c2", "output": "ok"}
        ]});
        assert!(cache.apply_to_body(&key, &mut body, true));
        let input = body["input"].as_array().unwrap();
        let reasoning = input.iter().find(|i| i["type"] == "reasoning").unwrap();
        assert_eq!(reasoning["summary"][0]["text"], "think");
        assert!(input.iter().any(|i| i["type"] == "function_call"));
    }

    #[test]
    fn test_fallback_fills_first_reasoning_only() {
        let cache = ReplayCache::new(Duration::from_secs(60), 128);
        let key = "sess-first".to_string();
        let mut extractor = StreamReplayExtractor::new(cache.clone(), key.clone(), String::new());

        // 两条 delta 单一累积;两个 reasoning 无文本 → 只塞第一个(对齐官方
        // items.iter().position(Reasoning) 取第一个)
        extractor.push(
            b"event: response.reasoning_text.delta\ndata: {\"output_index\":0,\"delta\":\"a\"}\n\n",
        );
        extractor.push(
            b"event: response.reasoning_text.delta\ndata: {\"output_index\":1,\"delta\":\"b\"}\n\n",
        );
        extractor.push(b"event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"output\":[{\"type\":\"reasoning\",\"encrypted_content\":\"e1\"},{\"type\":\"reasoning\",\"encrypted_content\":\"e2\"}]}}\n\n");

        let mut body = json!({"input": []});
        assert!(cache.apply_to_body(&key, &mut body, false));
        let input = body["input"].as_array().unwrap();
        let reasonings: Vec<_> = input.iter().filter(|i| i["type"] == "reasoning").collect();
        assert_eq!(reasonings.len(), 2);
        assert_eq!(reasonings[0]["summary"][0]["text"], "ab");
        assert!(reasonings[1].get("summary").is_none());
    }

    #[test]
    fn test_reasoning_with_existing_summary_not_overwritten() {
        let cache = ReplayCache::new(Duration::from_secs(60), 128);
        let key = "sess-keep-summary".to_string();
        let mut extractor = StreamReplayExtractor::new(cache.clone(), key.clone(), String::new());

        // 累积流式文本
        extractor.push(b"event: response.reasoning_text.delta\ndata: {\"output_index\":0,\"delta\":\"orphan delta\"}\n\n");

        // output_item.done 已带 summary → 不用流式文本覆盖(对齐 grok-build L1503 any_with_text return)
        extractor.push(b"event: response.output_item.done\ndata: {\"output_index\":0,\"item\":{\"type\":\"reasoning\",\"encrypted_content\":\"gAAA\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"original summary\"}]}}\n\n");

        let completed_empty = b"event: response.completed\ndata: {\"type\":\"response.completed\",\"response\":{\"id\":\"r5\"}}\n\n";
        extractor.push(completed_empty);

        let mut body = json!({"input": []});
        assert!(cache.apply_to_body(&key, &mut body, false));
        let input = body["input"].as_array().unwrap();
        let reasoning = input.iter().find(|i| i["type"] == "reasoning").unwrap();
        // 应保留 original summary 不是 orphan delta
        assert_eq!(reasoning["summary"][0]["text"], "original summary");
    }
}
