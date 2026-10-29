//! Kimi thinking replay 缓存（Claude Messages / protocol:claude）。
//!
//! key = "{modelFamily}:{session}"（家族对齐 CPA kimiThinkingReplayModelFamily；
//! session 同源 `extract_claude_code_session`）。
//! value = 完整 assistant `content` 数组（带 signature 的 thinking + tool_use）。

use ccextra_core::convert::{
    content_from_message_response, content_is_replayable, normalize_cached_content,
    restore_kimi_thinking_replay, KimiThinkingReplayStreamAccumulator,
};
use serde_json::Value;
use std::time::Duration;

use super::parser::SseParser;
use super::ttl_lru::TtlLru;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KimiReplayApplyOutcome {
    Miss,
    /// 账本存在但未改写（已有 thinking / 指纹不匹配 / 已完整）
    Unchanged,
    Applied,
}

impl KimiReplayApplyOutcome {
    pub fn applied(self) -> bool {
        matches!(self, Self::Applied)
    }
}

/// 进程内 Kimi thinking 账本。
#[derive(Clone)]
pub struct KimiThinkingReplayCache {
    inner: TtlLru<Value>,
}

impl KimiThinkingReplayCache {
    pub fn new(ttl: Duration, capacity: usize) -> Self {
        Self {
            inner: TtlLru::new(ttl, capacity),
        }
    }

    pub fn replace_content(&self, session_key: &str, content: Value) {
        match normalize_cached_content(&content) {
            Some(normalized) => self.inner.insert(session_key, normalized),
            None => self.inner.remove(session_key),
        }
    }

    pub fn invalidate(&self, session_key: &str) {
        self.inner.remove(session_key);
    }

    fn get_content(&self, session_key: &str) -> Option<Value> {
        self.inner.get_touch(session_key)
    }

    /// 将缓存 thinking 写回 Anthropic `messages` body。
    pub fn apply_to_claude_body(
        &self,
        session_key: &str,
        body: &mut Value,
    ) -> KimiReplayApplyOutcome {
        let Some(cached) = self.get_content(session_key) else {
            return KimiReplayApplyOutcome::Miss;
        };
        if restore_kimi_thinking_replay(body, &cached) {
            KimiReplayApplyOutcome::Applied
        } else {
            KimiReplayApplyOutcome::Unchanged
        }
    }

    /// 非流：从上游 Anthropic message JSON 入库；不可回放则清空。
    pub fn store_from_message_response(&self, session_key: &str, response: &Value) {
        if let Some(content) = content_from_message_response(response) {
            self.replace_content(session_key, content);
        } else if response.get("content").is_some() {
            // 完整响应但不可回放（纯文本收尾等）→ 清掉旧账本，对齐 CPA
            self.invalidate(session_key);
        }
    }

    /// 流结束提交：可回放则写入；不可回放则清空。
    pub fn store_from_stream_content(&self, session_key: &str, content: Option<Value>) {
        match content {
            Some(c) if content_is_replayable(&c) => self.replace_content(session_key, c),
            Some(_) => self.invalidate(session_key),
            None => {}
        }
    }
}

/// 旁路解析 Claude/Anthropic SSE，流结束时提交 Kimi thinking 账本。
pub struct KimiStreamReplayExtractor {
    cache: KimiThinkingReplayCache,
    session_key: String,
    /// 本请求是否注入过 replay；上游 error 且曾注入时清空（对齐 CPA）
    replay_applied: bool,
    accumulator: KimiThinkingReplayStreamAccumulator,
    parser: SseParser,
    committed: bool,
}

impl KimiStreamReplayExtractor {
    pub fn new(cache: KimiThinkingReplayCache, session_key: String, replay_applied: bool) -> Self {
        Self {
            cache,
            session_key,
            replay_applied,
            accumulator: KimiThinkingReplayStreamAccumulator::new(),
            parser: SseParser::new(),
            committed: false,
        }
    }

    pub fn push(&mut self, bytes: &[u8]) {
        for event in self.parser.push(bytes) {
            let Ok(value) = serde_json::from_str::<Value>(&event.data) else {
                continue;
            };
            self.accumulator.observe_payload(&value);
        }
    }

    pub fn finish(&mut self) {
        if self.committed {
            return;
        }
        for event in self.parser.finish() {
            let Ok(value) = serde_json::from_str::<Value>(&event.data) else {
                continue;
            };
            self.accumulator.observe_payload(&value);
        }
        self.commit();
    }

    fn commit(&mut self) {
        if self.committed {
            return;
        }
        self.committed = true;
        if self.accumulator.upstream_error() {
            if self.replay_applied {
                self.cache.invalidate(&self.session_key);
            }
            return;
        }
        let content = self.accumulator.take_content();
        self.cache
            .store_from_stream_content(&self.session_key, content);
    }
}

impl Drop for KimiStreamReplayExtractor {
    fn drop(&mut self) {
        self.finish();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn roundtrip_restores_stripped_thinking() {
        let cache = KimiThinkingReplayCache::new(Duration::from_secs(60), 128);
        let key = "k3:sess-1";
        let response = json!({
            "id":"msg_1","type":"message","role":"assistant",
            "content":[
                {"type":"thinking","thinking":"full reasoning","signature":"kimi-signature"},
                {"type":"text","text":"Inspecting."},
                {"type":"tool_use","id":"toolu_1","name":"Read","input":{"path":"README.md"}}
            ],
            "stop_reason":"tool_use"
        });
        cache.store_from_message_response(key, &response);

        let mut body = json!({"messages":[
            {"role":"user","content":"inspect"},
            {"role":"assistant","content":[
                {"type":"text","text":"Inspecting."},
                {"type":"tool_use","id":"toolu_1","name":"Read","input":{"path":"README.md"}}
            ]},
            {"role":"user","content":[{"type":"tool_result","tool_use_id":"toolu_1","content":"ok"}]}
        ]});
        assert_eq!(
            cache.apply_to_claude_body(key, &mut body),
            KimiReplayApplyOutcome::Applied
        );
        assert_eq!(
            body["messages"][1]["content"][0]["signature"],
            "kimi-signature"
        );
    }

    #[test]
    fn non_replayable_response_clears_cache() {
        let cache = KimiThinkingReplayCache::new(Duration::from_secs(60), 128);
        let key = "k3:sess-2";
        cache.replace_content(
            key,
            json!([
                {"type":"thinking","thinking":"t","signature":"sig"},
                {"type":"tool_use","id":"toolu_1","name":"Read","input":{}}
            ]),
        );
        cache.store_from_message_response(
            key,
            &json!({
                "content":[{"type":"text","text":"done"}],
                "stop_reason":"end_turn"
            }),
        );
        let mut body = json!({"messages":[{"role":"assistant","content":[
            {"type":"tool_use","id":"toolu_1","name":"Read","input":{}}
        ]}]});
        assert_eq!(
            cache.apply_to_claude_body(key, &mut body),
            KimiReplayApplyOutcome::Miss
        );
    }

    #[test]
    fn stream_extractor_commits_on_drop() {
        let cache = KimiThinkingReplayCache::new(Duration::from_secs(60), 128);
        let key = "k3:sess-stream".to_string();
        {
            let mut ext = KimiStreamReplayExtractor::new(cache.clone(), key.clone(), false);
            let chunk = concat!(
                "event: message_start\n",
                "data: {\"type\":\"message_start\",\"message\":{\"id\":\"m\"}}\n\n",
                "event: content_block_start\n",
                "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\",\"signature\":\"\"}}\n\n",
                "event: content_block_delta\n",
                "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"plan\"}}\n\n",
                "event: content_block_delta\n",
                "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"signature_delta\",\"signature\":\"sig\"}}\n\n",
                "event: content_block_stop\n",
                "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
                "event: content_block_start\n",
                "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"tool_use\",\"id\":\"toolu_1\",\"name\":\"Read\",\"input\":{}}}\n\n",
                "event: content_block_delta\n",
                "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"input_json_delta\",\"partial_json\":\"{\\\"p\\\":1}\"}}\n\n",
                "event: content_block_stop\n",
                "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
                "event: message_stop\n",
                "data: {\"type\":\"message_stop\"}\n\n",
            );
            ext.push(chunk.as_bytes());
        }
        let mut body = json!({"messages":[{"role":"assistant","content":[
            {"type":"tool_use","id":"toolu_1","name":"Read","input":{"p":1}}
        ]}]});
        assert!(cache.apply_to_claude_body(&key, &mut body).applied());
        assert_eq!(body["messages"][0]["content"][0]["signature"], "sig");
    }

    #[test]
    fn stream_error_clears_when_replay_applied() {
        let cache = KimiThinkingReplayCache::new(Duration::from_secs(60), 128);
        let key = "k3:sess-err".to_string();
        cache.replace_content(
            &key,
            json!([
                {"type":"thinking","thinking":"t","signature":"sig"},
                {"type":"tool_use","id":"toolu_1","name":"Read","input":{}}
            ]),
        );
        {
            let mut ext = KimiStreamReplayExtractor::new(cache.clone(), key.clone(), true);
            ext.push(
                b"event: error\ndata: {\"type\":\"error\",\"error\":{\"message\":\"bad\"}}\n\n",
            );
        }
        let mut body = json!({"messages":[{"role":"assistant","content":[
            {"type":"tool_use","id":"toolu_1","name":"Read","input":{}}
        ]}]});
        assert_eq!(
            cache.apply_to_claude_body(&key, &mut body),
            KimiReplayApplyOutcome::Miss
        );
    }
}
