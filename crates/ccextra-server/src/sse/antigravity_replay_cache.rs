//! Antigravity thoughtSignature replay 缓存（server 层状态）。
//!
//! 与 Responses `ReplayCache`（input/encrypted_content）分离：
//! key 由 [`ag_replay_session_key`] 统一构造，value = 规范化的 AG replay items 账本。

use ccextra_core::convert::{
    collect_ag_replay_from_response, normalize_ag_replay_items,
    prepare_ag_replay_request_with_tools, AgReplayAccumulator, AgReplayPrepareReport,
    AgReplayToolContext,
};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

/// 对齐 CPA AntigravityReasoningReplayCacheEvictBatchSize
const EVICT_BATCH_SIZE: usize = 128;
/// 对齐 CPA AntigravityReasoningReplayCacheMaxEntries
pub const AG_REPLAY_CACHE_MAX_ENTRIES: usize = 10240;
/// 对齐 CPA AntigravityReasoningReplayCacheTTL
pub const AG_REPLAY_CACHE_TTL: Duration = Duration::from_secs(3600);
/// 对齐 CPA CacheCleanupInterval（signature_cache.go:34，后台 purgeExpired 周期）
pub const AG_REPLAY_CACHE_CLEANUP_INTERVAL: Duration = Duration::from_secs(600);

/// AG 账本 key：`{model}:claude:{session}:agent:{agent}[:context:{system lane}]`。
///
/// 对齐 CPA `ClaudeCodeExecutionScope`（`claude:{session}:agent:{agent}`，缺省 `main`）
/// 加 `:context:{lane}`（`antigravityClaudeReplaySystemLane`，见
/// [`ccextra_core::convert::claude_replay_system_lane`]；无 system 时省略），
/// 前缀 model 对应 CPA scope 的 modelName。读、写、清空必须共用本函数。
pub fn ag_replay_session_key(
    upstream_model: &str,
    session: &str,
    agent_id: &str,
    system_lane: &str,
) -> String {
    if system_lane.is_empty() {
        format!("{upstream_model}:claude:{session}:agent:{agent_id}")
    } else {
        format!("{upstream_model}:claude:{session}:agent:{agent_id}:context:{system_lane}")
    }
}

/// 账本条目（对齐 CPA antigravityReasoningReplayEntry 本地模式）。`deleted` 为 tombstone：
/// 删除 / 读 miss 都写 tombstone 而非直接移除，使旧快照的条件写入无法越过它。
struct Entry {
    items: Vec<Value>,
    stored_at: Instant,
    /// 每次写入单调递增（对齐 CPA entry.Revision）
    revision: u64,
    /// 链分支代号（对齐 CPA entry.Branch，CPA 用随机 nonce，这里用进程内单调计数，仅比较相等）
    branch: u64,
    deleted: bool,
}

/// 读账本时的快照（对齐 CPA AntigravityReasoningReplaySnapshot 本地字段：
/// loaded / found / revision / branch / evictionEpoch / items）。默认值 = 未读（!loaded）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AgReplaySnapshot {
    loaded: bool,
    found: bool,
    revision: u64,
    branch: u64,
    eviction_epoch: u64,
    items: Vec<Value>,
}

impl AgReplaySnapshot {
    #[cfg(test)]
    pub fn is_loaded(&self) -> bool {
        self.loaded
    }
}

#[derive(Default)]
struct State {
    entries: HashMap<String, Entry>,
    next_revision: u64,
    next_branch: u64,
    /// 对齐 CPA antigravityReasoningReplayEvictionEpoch：任何条目被移除（淘汰 / TTL）时递增
    eviction_epoch: u64,
}

impl State {
    fn bump_revision(&mut self) -> u64 {
        self.next_revision += 1;
        self.next_revision
    }

    fn new_branch(&mut self) -> u64 {
        self.next_branch += 1;
        self.next_branch
    }

    /// 写入新条目并在超限时批量淘汰（对齐 CPA 写后 `len > Max` 才 evict）。
    fn put(&mut self, key: &str, items: Vec<Value>, branch: u64, deleted: bool, capacity: usize) {
        let revision = self.bump_revision();
        self.entries.insert(
            key.to_string(),
            Entry {
                items,
                stored_at: Instant::now(),
                revision,
                branch,
                deleted,
            },
        );
        if self.entries.len() > capacity {
            self.evict_oldest(EVICT_BATCH_SIZE);
        }
    }

    fn put_tombstone(&mut self, key: &str, capacity: usize) {
        let branch = self.new_branch();
        self.put(key, Vec::new(), branch, true, capacity);
    }

    /// 对齐 CPA purgeExpiredAntigravityReasoningReplayCache：移除全部过期条目（含 tombstone），
    /// 每移除一条 evictionEpoch 递增。返回移除条数。
    fn purge_expired(&mut self, ttl: Duration) -> usize {
        let expired: Vec<String> = self
            .entries
            .iter()
            .filter(|(_, entry)| entry.stored_at.elapsed() > ttl)
            .map(|(key, _)| key.clone())
            .collect();
        for key in &expired {
            self.eviction_epoch += 1;
            self.entries.remove(key);
        }
        expired.len()
    }

    fn evict_oldest(&mut self, count: usize) {
        if count == 0 || self.entries.is_empty() {
            return;
        }
        let mut candidates: Vec<(String, Instant)> = self
            .entries
            .iter()
            .map(|(k, v)| (k.clone(), v.stored_at))
            .collect();
        candidates.sort_by_key(|(_, t)| *t);
        for (key, _) in candidates.into_iter().take(count) {
            self.eviction_epoch += 1;
            self.entries.remove(&key);
        }
    }

    /// 对齐 CPA reserveAntigravityReasoningReplayAbsentLocked：miss 时写 per-key tombstone，
    /// 使无关 key 的淘汰不会让本次 miss 的快照失效。
    fn reserve_absent(&mut self, key: &str, capacity: usize) -> AgReplaySnapshot {
        if self.entries.len() >= capacity {
            self.evict_oldest(EVICT_BATCH_SIZE);
        }
        let branch = self.new_branch();
        let revision = self.bump_revision();
        self.entries.insert(
            key.to_string(),
            Entry {
                items: Vec::new(),
                stored_at: Instant::now(),
                revision,
                branch,
                deleted: true,
            },
        );
        AgReplaySnapshot {
            loaded: true,
            found: true,
            revision,
            branch,
            eviction_epoch: self.eviction_epoch,
            items: Vec::new(),
        }
    }

    /// 对齐 CPA 本地分支 matchesSnapshot。
    fn matches_snapshot(&self, key: &str, snapshot: &AgReplaySnapshot) -> bool {
        match self.entries.get(key) {
            Some(entry) => snapshot.found && entry.revision == snapshot.revision,
            None => !snapshot.found && snapshot.eviction_epoch == self.eviction_epoch,
        }
    }
}

/// 对齐 CPA antigravityReasoningReplayItemsPrefix
fn items_prefix(prefix: &[Value], items: &[Value]) -> bool {
    prefix.len() <= items.len() && prefix.iter().zip(items).all(|(a, b)| a == b)
}

/// `apply_to_antigravity_body` 结果，便于调用侧区分 miss / 失败 / 未改写 / 命中。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgReplayApplyOutcome {
    /// 无账本或 TTL 过期
    Miss,
    /// body 缺少 `request` 字段
    MissingRequest,
    /// 账本存在但规范化失败（超限等）
    Invalid { items: usize },
    /// 账本存在且规范化成功，但未改写任何 part（无匹配 / 已有 native 签名）
    Unchanged { items: usize },
    /// 成功注入至少一处 thoughtSignature / 原生 id
    Applied { items: usize },
    /// 回放破坏 functionCall/functionResponse 配对：已退回原始 payload 并按快照清账本
    RolledBack { items: usize },
    /// 历史配对无效（对齐 CPA 本地 400）；`message` 为完整 CPA 文案。不清账本。
    PairingInvalid { message: String },
}

impl AgReplayApplyOutcome {
    pub fn applied(&self) -> bool {
        matches!(self, Self::Applied { .. })
    }
}

/// Antigravity / Gemini thoughtSignature 账本缓存（对齐 CPA
/// internal/cache/antigravity_reasoning_replay_cache.go 本地模式；无 Home KV）。
#[derive(Clone)]
pub struct AgReplayCache {
    inner: Arc<Mutex<State>>,
    ttl: Duration,
    capacity: usize,
}

impl AgReplayCache {
    pub fn new(ttl: Duration, capacity: usize) -> Self {
        Self {
            inner: Arc::new(Mutex::new(State::default())),
            ttl,
            capacity,
        }
    }

    /// 无条件写入（对齐 CPA CacheAntigravityReasoningReplayItemsBestEffort）：规范化失败 / 空
    /// 不改动；成功则新 revision + 新 branch。返回是否写入。
    pub fn replace_items(&self, session_key: &str, items: Vec<Value>) -> bool {
        let trimmed = session_key.trim();
        if trimmed.is_empty() {
            return false;
        }
        let Some(normalized) = normalize_ag_replay_items(&items) else {
            return false;
        };
        let mut state = self.inner.lock().unwrap();
        let branch = state.new_branch();
        state.put(trimmed, normalized, branch, false, self.capacity);
        true
    }

    /// 仅当账本仍是快照那一版时才发布新链，或新链是当前同分支链的后继（对齐 CPA
    /// ReplaceAntigravityReasoningReplayItemsIfUnchanged 本地分支）。`None` = items 规范化失败（CPA 返回 error）。
    pub fn replace_if_unchanged(
        &self,
        session_key: &str,
        snapshot: &AgReplaySnapshot,
        items: Vec<Value>,
    ) -> Option<bool> {
        let trimmed = session_key.trim();
        if trimmed.is_empty() {
            return Some(false);
        }
        let normalized = normalize_ag_replay_items(&items)?;
        if !snapshot.loaded {
            return Some(self.replace_items(trimmed, normalized));
        }
        let mut state = self.inner.lock().unwrap();
        let matches = state.matches_snapshot(trimmed, snapshot);
        let is_descendant = state.entries.get(trimmed).is_some_and(|entry| {
            !entry.deleted
                && snapshot.branch != 0
                && entry.branch == snapshot.branch
                && items_prefix(&entry.items, &normalized)
        });
        if !matches && !is_descendant {
            return Some(false);
        }
        let mut branch = snapshot.branch;
        if branch == 0 || (matches && !items_prefix(&snapshot.items, &normalized)) {
            branch = state.new_branch();
        }
        state.put(trimmed, normalized, branch, false, self.capacity);
        Some(true)
    }

    /// 仅当账本仍是快照那一版时写 tombstone（对齐 CPA
    /// DeleteAntigravityReasoningReplayItemsIfUnchanged 本地分支）；快照未加载 → 无条件删除。
    /// 返回是否删除。
    pub fn invalidate_if_unchanged(&self, session_key: &str, snapshot: &AgReplaySnapshot) -> bool {
        let trimmed = session_key.trim();
        if trimmed.is_empty() {
            return false;
        }
        if !snapshot.loaded {
            self.invalidate(trimmed);
            return true;
        }
        let mut state = self.inner.lock().unwrap();
        if !state.matches_snapshot(trimmed, snapshot) {
            return false;
        }
        state.put_tombstone(trimmed, self.capacity);
        true
    }

    /// 立即清一次过期条目（对齐 CPA purgeExpiredAntigravityReasoningReplayCache）。
    #[cfg(test)]
    pub fn purge_expired(&self) -> usize {
        self.inner.lock().unwrap().purge_expired(self.ttl)
    }

    /// 后台定期清过期条目（对齐 CPA startCacheCleanup：ticker 每 `interval` 调一次
    /// purgeExpired）。任务只持 `Weak`：缓存全部句柄 drop 后下一拍退出，不延长缓存寿命；
    /// 进程关停时随 runtime 一起结束。须在 tokio runtime 内调用。
    pub fn spawn_purge_task(&self, interval: Duration) -> tokio::task::JoinHandle<()> {
        let weak = Arc::downgrade(&self.inner);
        let ttl = self.ttl;
        tokio::spawn(async move {
            let mut ticker = tokio::time::interval(interval);
            ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // interval 首拍立即返回；CPA ticker 首次在一个周期后触发
            ticker.tick().await;
            loop {
                ticker.tick().await;
                let Some(inner) = weak.upgrade() else {
                    break;
                };
                let purged = inner.lock().unwrap().purge_expired(ttl);
                drop(inner);
                if purged > 0 {
                    tracing::debug!(purged, "AG replay 账本后台清理过期条目");
                }
            }
        })
    }

    /// 无条件删除 = 写 tombstone（对齐 CPA DeleteAntigravityReasoningReplayItemRequired）。
    pub fn invalidate(&self, session_key: &str) {
        let trimmed = session_key.trim();
        if trimmed.is_empty() {
            return;
        }
        self.inner
            .lock()
            .unwrap()
            .put_tombstone(trimmed, self.capacity);
    }

    /// 响应结束提交（对齐 CPA antigravityReasoningReplayAccumulator.Commit 的 terminal 分支）：
    /// 空（overflow / 无条目）→ DeleteIfUnchanged；否则 ReplaceIfUnchanged，规范化失败再
    /// DeleteIfUnchanged。
    pub fn commit_if_unchanged(
        &self,
        session_key: &str,
        snapshot: &AgReplaySnapshot,
        items: Vec<Value>,
    ) {
        if items.is_empty() {
            self.invalidate_if_unchanged(session_key, snapshot);
            return;
        }
        if self
            .replace_if_unchanged(session_key, snapshot, items)
            .is_none()
        {
            self.invalidate_if_unchanged(session_key, snapshot);
        }
    }

    /// 读账本并取快照（对齐 CPA GetAntigravityReasoningReplayItemsWithSnapshotRequired 本地分支：
    /// miss / 过期 → 预留 tombstone；命中刷新 TTL；tombstone / 空 → 无 items）。
    fn load(&self, session_key: &str) -> (Option<Vec<Value>>, AgReplaySnapshot) {
        let trimmed = session_key.trim();
        if trimmed.is_empty() {
            return (None, AgReplaySnapshot::default());
        }
        let mut state = self.inner.lock().unwrap();
        let expired = match state.entries.get(trimmed) {
            None => return (None, state.reserve_absent(trimmed, self.capacity)),
            Some(entry) => entry.stored_at.elapsed() > self.ttl,
        };
        if expired {
            state.eviction_epoch += 1;
            state.entries.remove(trimmed);
            return (None, state.reserve_absent(trimmed, self.capacity));
        }
        let epoch = state.eviction_epoch;
        let entry = state.entries.get_mut(trimmed).expect("checked above");
        entry.stored_at = Instant::now();
        let mut snapshot = AgReplaySnapshot {
            loaded: true,
            found: true,
            revision: entry.revision,
            branch: entry.branch,
            eviction_epoch: epoch,
            items: Vec::new(),
        };
        if entry.deleted || entry.items.is_empty() {
            return (None, snapshot);
        }
        snapshot.items = entry.items.clone();
        (Some(entry.items.clone()), snapshot)
    }

    /// 测试用：只读查看活条目（不预留 tombstone、不刷新 TTL）。
    #[cfg(test)]
    fn get_items(&self, session_key: &str) -> Option<(Vec<Value>, u64)> {
        let state = self.inner.lock().unwrap();
        state
            .entries
            .get(session_key.trim())
            .filter(|e| !e.deleted && !e.items.is_empty() && e.stored_at.elapsed() <= self.ttl)
            .map(|e| (e.items.clone(), e.revision))
    }

    /// 将缓存签名 / 原生 id 回填到 Antigravity 外层 body 的 `request` 对象，并执行
    /// 降级 / 首 functionCall 哨兵 / 配对回退兜底（[`prepare_ag_replay_request_with_tools`]）。
    /// 账本 miss 时兜底照跑：历史里的稳定 id 仍须降级为合成 id。
    #[cfg(test)]
    pub fn apply_to_antigravity_body(
        &self,
        session_key: &str,
        body: &mut Value,
    ) -> AgReplayApplyOutcome {
        self.apply_to_antigravity_body_with_tools(session_key, body, AgReplayToolContext::default())
            .0
    }

    /// 将缓存签名 / 原生 id 回填到 Antigravity body 并执行外发前处理链，带工具上下文（schema 归一 / 旧式 id 回写）；
    /// 另返回本请求读账本时的快照，供后续 IfUnchanged 提交 / 清理（对齐 CPA scope.cacheSnapshot）。
    pub fn apply_to_antigravity_body_with_tools(
        &self,
        session_key: &str,
        body: &mut Value,
        tools: AgReplayToolContext,
    ) -> (AgReplayApplyOutcome, AgReplaySnapshot) {
        let (items, snapshot) = self.load(session_key);
        let outcome = self.apply_loaded(session_key, &snapshot, items, body, tools);
        (outcome, snapshot)
    }

    fn apply_loaded(
        &self,
        session_key: &str,
        snapshot: &AgReplaySnapshot,
        items: Option<Vec<Value>>,
        body: &mut Value,
        tools: AgReplayToolContext,
    ) -> AgReplayApplyOutcome {
        let Some(request) = body.get_mut("request") else {
            return if items.is_some() {
                AgReplayApplyOutcome::MissingRequest
            } else {
                AgReplayApplyOutcome::Miss
            };
        };
        let Some(items) = items else {
            let report = prepare_ag_replay_request_with_tools(request, None, tools);
            log_prepare_report(&report);
            return match report.pairing_error {
                Some(message) => AgReplayApplyOutcome::PairingInvalid { message },
                None => AgReplayApplyOutcome::Miss,
            };
        };
        let item_count = items.len();
        // 先探测规范化：失败与「有账本但未匹配」分开，便于日志分级
        if normalize_ag_replay_items(&items).is_none() {
            let report = prepare_ag_replay_request_with_tools(request, None, tools);
            log_prepare_report(&report);
            if let Some(message) = report.pairing_error {
                return AgReplayApplyOutcome::PairingInvalid { message };
            }
            return AgReplayApplyOutcome::Invalid { items: item_count };
        }
        let report = prepare_ag_replay_request_with_tools(request, Some(&items), tools);
        log_prepare_report(&report);
        if let Some(message) = report.pairing_error {
            // 对齐 CPA：只回 400，不删账本（CPA 仅回退分支删）
            AgReplayApplyOutcome::PairingInvalid { message }
        } else if report.rolled_back {
            // 对齐 CPA：回放破坏配对 → DeleteIfUnchanged 删账本，发原始请求
            self.invalidate_if_unchanged(session_key, snapshot);
            AgReplayApplyOutcome::RolledBack { items: item_count }
        } else if report.applied {
            AgReplayApplyOutcome::Applied { items: item_count }
        } else {
            AgReplayApplyOutcome::Unchanged { items: item_count }
        }
    }

    /// 非流：从上游响应收集并按快照条件提交账本（对齐 CPA
    /// cacheAntigravityReasoningReplayFromResponse → Commit）。
    /// `request` 为 Antigravity body 的 `request` 子对象（含 contents）。
    pub fn store_from_response(
        &self,
        session_key: &str,
        snapshot: &AgReplaySnapshot,
        request: &Value,
        response_body: &Value,
    ) {
        if let Some(items) = collect_ag_replay_from_response(request, response_body) {
            self.commit_if_unchanged(session_key, snapshot, items);
        }
        // None: 非 terminal，不动缓存
    }
}

/// 无会话（无账本 key）时的请求侧处理：角色归一 / 降级稳定 id / 首 functionCall 哨兵 /
/// 配对校验；配对无效时返回 CPA 同款 400 文案。
pub fn prepare_antigravity_body_without_ledger(
    body: &mut Value,
    tools: AgReplayToolContext,
) -> Option<String> {
    let request = body.get_mut("request")?;
    let report = prepare_ag_replay_request_with_tools(request, None, tools);
    log_prepare_report(&report);
    report.pairing_error
}

/// 降级 / 补哨兵只打 debug（不新增 info 级逐请求日志）。
fn log_prepare_report(report: &AgReplayPrepareReport) {
    if report.degraded > 0 || report.repaired > 0 {
        tracing::debug!(
            degraded = report.degraded,
            repaired = report.repaired,
            "antigravity replay 兜底：稳定 id 降级 / 首 functionCall 补哨兵"
        );
    }
}

/// 流式提取：旁路解析上游 Gemini/AG SSE。
///
/// 对齐 CPA：`finishReason` 只标记 terminal，不立即落库；signature / usage
/// 可能跟在后续帧。仅在流结束（`finish` / `Drop`）时提交。
pub struct AgStreamReplayExtractor {
    cache: AgReplayCache,
    session_key: String,
    snapshot: AgReplaySnapshot,
    accumulator: AgReplayAccumulator,
    parser: super::parser::SseParser,
    committed: bool,
}

impl AgStreamReplayExtractor {
    pub fn new(
        cache: AgReplayCache,
        session_key: String,
        snapshot: AgReplaySnapshot,
        request: Value,
    ) -> Self {
        let accumulator = AgReplayAccumulator::new_from_request(&request);
        Self {
            cache,
            session_key,
            snapshot,
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
            // 对齐 CPA Commit：按读账本时的快照 ReplaceIfUnchanged / DeleteIfUnchanged
            self.cache
                .commit_if_unchanged(&self.session_key, &self.snapshot, items);
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
        cache.store_from_response(key, &AgReplaySnapshot::default(), &request, &response);

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
        assert!(cache.apply_to_antigravity_body(key, &mut body).applied());
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
        cache.store_from_response(key, &AgReplaySnapshot::default(), &request, &partial);
        // 非 terminal：旧账本原样保留，partial 的签名不入账
        let (items, _) = cache.get_items(key).expect("旧账本仍在");
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["thoughtSignature"], "old-signature-123456789");
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
        cache.store_from_response(key, &AgReplaySnapshot::default(), &request, &response);
        assert!(cache.get_items(key).is_none());
    }

    #[test]
    fn stream_extractor_commits_on_finish_at_eof() {
        let cache = AgReplayCache::new(Duration::from_secs(60), 128);
        let key = "gemini-flash:sess-stream".to_string();
        let request = json!({"contents": [{"role": "user", "parts": [{"text": "hi"}]}]});
        let mut extractor = AgStreamReplayExtractor::new(
            cache.clone(),
            key.clone(),
            AgReplaySnapshot::default(),
            request,
        );
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
        let (items, _) = cache.get_items(&key).expect("committed at eof");
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["thoughtSignature"], "sig-first-long-enough");
    }

    /// 对齐 CPA issue6332_review7：finishReason 先到，thoughtSignature 后到。
    #[test]
    fn stream_extractor_keeps_observing_after_finish_reason() {
        let cache = AgReplayCache::new(Duration::from_secs(60), 128);
        let key = "gemini-flash:sess-split".to_string();
        let request = json!({"contents": [{"role": "user", "parts": [{"text": "hi"}]}]});
        let mut extractor = AgStreamReplayExtractor::new(
            cache.clone(),
            key.clone(),
            AgReplaySnapshot::default(),
            request,
        );
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
        let (items, _) = cache
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
            let mut extractor = AgStreamReplayExtractor::new(
                cache.clone(),
                key.clone(),
                AgReplaySnapshot::default(),
                request,
            );
            extractor.push(
                br#"data: {"response":{"candidates":[{"content":{"parts":[{"thoughtSignature":"sig-drop-long-enough12","functionCall":{"name":"Read","args":{"p":"/a"},"id":"id1"}}]},"finishReason":"STOP"}]}}

"#,
            );
            assert!(cache.get_items(&key).is_none());
            // drop → finish
        }
        let (items, _) = cache.get_items(&key).expect("Drop commits");
        assert_eq!(items[0]["thoughtSignature"], "sig-drop-long-enough12");
    }

    #[test]
    fn apply_outcome_distinguishes_miss_and_applied() {
        let cache = AgReplayCache::new(Duration::from_secs(60), 128);
        let key = "gemini-flash:sess-outcome";
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
        assert_eq!(
            cache.apply_to_antigravity_body(key, &mut body),
            AgReplayApplyOutcome::Miss
        );
        cache.replace_items(
            key,
            vec![json!({
                "type": "function_call_part",
                "contentIndex": 1,
                "partIndex": 0,
                "name": "Read",
                "call_id": "id1",
                "args": {"file_path": "/a"},
                "thoughtSignature": "sig-first-long-enough"
            })],
        );
        assert_eq!(
            cache.apply_to_antigravity_body(key, &mut body),
            AgReplayApplyOutcome::Applied { items: 1 }
        );
        assert_eq!(
            body["request"]["contents"][1]["parts"][0]["thoughtSignature"],
            "sig-first-long-enough"
        );
    }

    #[test]
    fn overflow_store_clears_existing_ledger() {
        let cache = AgReplayCache::new(Duration::from_secs(60), 128);
        let key = "gemini-flash:sess-overflow";
        cache.replace_items(
            key,
            vec![json!({
                "type": "thought_signature",
                "thoughtSignature": "old-signature-123456789",
                "contentIndex": 1,
                "partIndex": 0,
            })],
        );
        assert!(cache.get_items(key).is_some());
        let request = json!({"contents": [{"role": "user", "parts": [{"text": "hi"}]}]});
        let huge = "s".repeat(ccextra_core::convert::MAX_BYTES_PER_ENTRY);
        let response = json!({
            "response": {"candidates": [{"content": {"parts": [
                {"thoughtSignature": huge, "functionCall": {"name": "Read", "args": {"p": "/a"}, "id": "id1"}}
            ]}, "finishReason": "STOP"}]}
        });
        cache.store_from_response(key, &AgReplaySnapshot::default(), &request, &response);
        assert!(
            cache.get_items(key).is_none(),
            "terminal overflow must clear ledger"
        );
    }

    fn fc_item(sig: &str) -> Value {
        json!({
            "type": "function_call_part",
            "contentIndex": 1,
            "partIndex": 0,
            "name": "Read",
            "call_id": "id1",
            "args": {"file_path": "/a"},
            "thoughtSignature": sig
        })
    }

    #[test]
    fn session_key_has_agent_dimension() {
        assert_eq!(
            ag_replay_session_key("gemini-flash", "sess", "main", ""),
            "gemini-flash:claude:sess:agent:main"
        );
        // 缺省头 → main，与显式 main 同一账本
        let missing =
            ccextra_core::session::extract_claude_code_agent_id(&axum::http::HeaderMap::new());
        assert_eq!(
            ag_replay_session_key("gemini-flash", "sess", &missing, ""),
            ag_replay_session_key("gemini-flash", "sess", "main", "")
        );
        assert_ne!(
            ag_replay_session_key("gemini-flash", "sess", "main", ""),
            ag_replay_session_key("gemini-flash", "sess", "agent-sub", "")
        );
    }

    /// 主代理与子代理同 session 不互相覆盖；清空只影响对应 agent。
    #[test]
    fn agent_ledgers_are_isolated_for_store_and_invalidate() {
        let cache = AgReplayCache::new(Duration::from_secs(60), 128);
        let main = ag_replay_session_key("gemini-flash", "sess-a", "main", "");
        let sub = ag_replay_session_key("gemini-flash", "sess-a", "agent-sub", "");
        cache.replace_items(&main, vec![fc_item("sig-main-long-enough")]);
        cache.replace_items(&sub, vec![fc_item("sig-sub-long-enough-")]);
        assert_eq!(
            cache.get_items(&main).unwrap().0[0]["thoughtSignature"],
            "sig-main-long-enough"
        );
        assert_eq!(
            cache.get_items(&sub).unwrap().0[0]["thoughtSignature"],
            "sig-sub-long-enough-"
        );

        cache.invalidate(&sub);
        assert!(cache.get_items(&sub).is_none());
        assert!(cache.get_items(&main).is_some(), "main ledger must survive");

        // terminal 空响应清空同样只作用于本 agent
        cache.replace_items(&sub, vec![fc_item("sig-sub-long-enough-")]);
        let request = json!({"contents": [{"role": "user", "parts": [{"text": "x"}]}]});
        let empty = json!({"response": {"candidates": [{"content": {"parts": [
            {"text": "no signature"}
        ]}, "finishReason": "STOP"}]}});
        cache.store_from_response(&main, &AgReplaySnapshot::default(), &request, &empty);
        assert!(cache.get_items(&main).is_none());
        assert!(cache.get_items(&sub).is_some(), "sub ledger must survive");
    }

    /// system lane 进 key：不同 system → 各自账本；读 / 写 / 清空共用同一 key。
    #[test]
    fn system_lane_separates_ledgers_and_clear_paths_stay_consistent() {
        use ccextra_core::convert::claude_replay_system_lane;
        let lane_a = claude_replay_system_lane(&json!({"system": "You are A."}));
        let lane_b = claude_replay_system_lane(&json!({"system": "You are B."}));
        assert_ne!(lane_a, lane_b);
        let key_a = ag_replay_session_key("gemini-flash", "sess", "main", &lane_a);
        let key_b = ag_replay_session_key("gemini-flash", "sess", "main", &lane_b);
        assert_eq!(
            key_a,
            format!("gemini-flash:claude:sess:agent:main:context:{lane_a}")
        );
        cache_roundtrip_isolated(&key_a, &key_b);
    }

    fn cache_roundtrip_isolated(key_a: &str, key_b: &str) {
        let cache = AgReplayCache::new(Duration::from_secs(60), 128);
        cache.replace_items(key_a, vec![fc_item("sig-lane-a-long-enough")]);
        cache.replace_items(key_b, vec![fc_item("sig-lane-b-long-enough")]);
        cache.invalidate(key_a);
        assert!(cache.get_items(key_a).is_none());
        assert!(cache.get_items(key_b).is_some(), "other lane must survive");
    }

    /// 回放破坏配对 → 发原始请求，并清空该 key 账本。
    #[test]
    fn rolled_back_replay_clears_ledger() {
        let cache = AgReplayCache::new(Duration::from_secs(60), 128);
        let key = "gemini-flash:claude:sess-rb:agent:main";
        let stable = ccextra_core::convert::tool_id::gemini_claude_tool_use_id(
            "id1",
            "Read",
            Some(&json!({"file_path": "/a"})),
        );
        cache.replace_items(key, vec![fc_item("sig-first-long-enough")]);
        let fc =
            json!({"functionCall": {"id": stable, "name": "Read", "args": {"file_path": "/a"}}});
        let fr = json!({"functionResponse": {"id": stable, "name": "Read", "response": {"result": "ok"}}});
        let mut body = json!({"request": {"contents": [
            {"role": "user", "parts": [{"text": "hi"}]},
            {"role": "model", "parts": [fc.clone(), fc]},
            {"role": "user", "parts": [fr.clone(), fr]}
        ]}});
        let mut original = body.clone();
        // 退回的是 sanitize 阶段输出：首 functionCall 补哨兵（SanitizeGeminiRequestThoughtSignatures）
        // + 纯 functionResponse 回合 role → model（角色归一）
        original["request"]["contents"][1]["parts"][0]["thoughtSignature"] =
            json!("skip_thought_signature_validator");
        original["request"]["contents"][2]["role"] = json!("model");
        assert_eq!(
            cache.apply_to_antigravity_body(key, &mut body),
            AgReplayApplyOutcome::RolledBack { items: 1 }
        );
        assert_eq!(body, original);
        assert!(
            cache.get_items(key).is_none(),
            "ledger cleared after rollback"
        );
    }

    /// 账本 miss / 无会话时仍降级稳定 id，请求保持可发。
    #[test]
    fn miss_and_no_session_still_degrade_stable_ids() {
        let stable = ccextra_core::convert::tool_id::gemini_claude_tool_use_id(
            "id1",
            "Read",
            Some(&json!({"file_path": "/a"})),
        );
        let make = || {
            json!({"request": {"contents": [
                {"role": "user", "parts": [{"text": "hi"}]},
                {"role": "model", "parts": [{"functionCall": {"id": stable, "name": "Read", "args": {"file_path": "/a"}}}]},
                {"role": "user", "parts": [{"functionResponse": {"id": stable, "name": "Read", "response": {"result": "ok"}}}]}
            ]}})
        };
        let cache = AgReplayCache::new(Duration::from_secs(60), 128);
        let mut miss = make();
        assert_eq!(
            cache.apply_to_antigravity_body("gemini-flash:claude:none:agent:main", &mut miss),
            AgReplayApplyOutcome::Miss
        );
        let mut no_session = make();
        assert!(prepare_antigravity_body_without_ledger(
            &mut no_session,
            AgReplayToolContext::default()
        )
        .is_none());
        for body in [&miss, &no_session] {
            let id = body["request"]["contents"][1]["parts"][0]["functionCall"]["id"]
                .as_str()
                .unwrap();
            assert!(id.starts_with("call_"));
            assert_eq!(
                body["request"]["contents"][2]["parts"][0]["functionResponse"]["id"],
                id
            );
            assert_eq!(
                body["request"]["contents"][1]["parts"][0]["thoughtSignature"],
                "skip_thought_signature_validator"
            );
        }
        assert_eq!(miss, no_session);
    }

    /// 端到端：非流响应带原生 id → 下一轮稳定 id 历史经缓存还原为原生 id + 签名。
    #[test]
    fn nonstream_stable_id_roundtrip_through_cache() {
        let cache = AgReplayCache::new(Duration::from_secs(60), 128);
        let key = "gemini-flash:claude:sess-st:agent:main";
        let request = json!({"contents": [{"role": "user", "parts": [{"text": "hi"}]}]});
        let response = json!({"response": {"candidates": [{
            "content": {"parts": [{
                "thoughtSignature": "sig-first-long-enough",
                "functionCall": {"id": "id1", "name": "Read", "args": {"file_path": "/a"}}
            }]},
            "finishReason": "STOP"
        }]}});
        cache.store_from_response(key, &AgReplaySnapshot::default(), &request, &response);
        let stable = ccextra_core::convert::tool_id::gemini_claude_tool_use_id(
            "id1",
            "Read",
            Some(&json!({"file_path": "/a"})),
        );
        let mut body = json!({"request": {"contents": [
            {"role": "user", "parts": [{"text": "hi"}]},
            {"role": "model", "parts": [{"thoughtSignature": "skip_thought_signature_validator",
                "functionCall": {"id": stable, "name": "Read", "args": {"file_path": "/a"}}}]},
            {"role": "user", "parts": [{"functionResponse": {"id": stable, "name": "Read", "response": {"result": "ok"}}}]}
        ]}});
        assert!(cache.apply_to_antigravity_body(key, &mut body).applied());
        let contents = &body["request"]["contents"];
        assert_eq!(contents[1]["parts"][0]["functionCall"]["id"], "id1");
        assert_eq!(
            contents[1]["parts"][0]["thoughtSignature"],
            "sig-first-long-enough"
        );
        assert_eq!(contents[2]["parts"][0]["functionResponse"]["id"], "id1");
        assert_eq!(contents[2]["parts"][0]["functionResponse"]["name"], "Read");
    }

    /// IfUnchanged 删除：慢请求按旧快照删除时，不得误删并发请求刚提交的新账本。
    #[test]
    fn invalidate_if_unchanged_spares_concurrent_commit() {
        let cache = AgReplayCache::new(Duration::from_secs(60), 128);
        let key = "gemini-flash:claude:sess-cas:agent:main";
        cache.replace_items(key, vec![fc_item("sig-old-long-enough-")]);
        let (_, stale) = cache.load(key);
        // 并发请求提交新账本
        cache.replace_items(key, vec![fc_item("sig-new-long-enough-")]);
        assert!(!cache.invalidate_if_unchanged(key, &stale));
        assert_eq!(
            cache.get_items(key).unwrap().0[0]["thoughtSignature"],
            "sig-new-long-enough-"
        );
        // 快照仍是当前版 → 写 tombstone
        let (_, current) = cache.load(key);
        assert!(cache.invalidate_if_unchanged(key, &current));
        assert!(cache.get_items(key).is_none());
        // 快照未加载 → 无条件删除（对齐 CPA !snapshot.loaded 分支）
        cache.replace_items(key, vec![fc_item("sig-again-long-enough")]);
        assert!(cache.invalidate_if_unchanged(key, &AgReplaySnapshot::default()));
        assert!(cache.get_items(key).is_none());
        // 读 TTL 刷新不改 revision
        cache.replace_items(key, vec![fc_item("sig-again-long-enough")]);
        let (_, a) = cache.load(key);
        let (_, b) = cache.load(key);
        assert_eq!(a.revision, b.revision);
    }

    fn broken_pairing_body(stable: &str) -> Value {
        json!({"request": {"contents": [
            {"role": "user", "parts": [{"text": "hi"}]},
            {"role": "model", "parts": [
                {"functionCall": {"id": stable, "name": "Read", "args": {"file_path": "/a"}}},
                {"functionCall": {"id": "other", "name": "Read", "args": {"file_path": "/b"}}}
            ]},
            {"role": "user", "parts": [{"functionResponse": {"id": stable, "name": "Read", "response": {}}}]}
        ]}})
    }

    /// 配对无效 → PairingInvalid（CPA 400 文案），对齐 CPA 不清账本；门控只看模型，
    /// 无账本 / 无稳定 id / 无会话的请求同样 400。
    #[test]
    fn invalid_pairing_returns_400_message_and_keeps_ledger() {
        let cache = AgReplayCache::new(Duration::from_secs(60), 128);
        let key = "gemini-flash:claude:sess-400:agent:main";
        let stable = ccextra_core::convert::tool_id::gemini_claude_tool_use_id(
            "id1",
            "Read",
            Some(&json!({"file_path": "/a"})),
        );
        let want = "antigravity executor: invalid Gemini function call history: \
             request.contents[2]: functionResponse count 1 does not match pending functionCall count 2";
        cache.replace_items(key, vec![fc_item("sig-first-long-enough")]);
        let (_, rev_before) = cache.get_items(key).unwrap();
        let mut body = broken_pairing_body(&stable);
        let (outcome, snapshot) = cache.apply_to_antigravity_body_with_tools(
            key,
            &mut body,
            AgReplayToolContext::default(),
        );
        assert!(snapshot.found && snapshot.revision == rev_before);
        assert_eq!(
            outcome,
            AgReplayApplyOutcome::PairingInvalid {
                message: want.to_string()
            }
        );
        let (_, rev_after) = cache.get_items(key).expect("ledger kept on 400");
        assert_eq!(rev_after, rev_before);

        // 账本 miss + 普通 id → 同样 400
        let mut body = broken_pairing_body("plain-id");
        assert_eq!(
            cache.apply_to_antigravity_body("gemini-flash:claude:none:agent:main", &mut body),
            AgReplayApplyOutcome::PairingInvalid {
                message: want.to_string()
            }
        );

        // 无会话（稳定 id / 普通 id）→ 同样 400 文案
        for id in [stable.as_str(), "plain-id"] {
            let mut body = broken_pairing_body(id);
            let msg =
                prepare_antigravity_body_without_ledger(&mut body, AgReplayToolContext::default());
            assert_eq!(msg.as_deref(), Some(want));
        }

        // 合法历史无会话照常放行
        let mut ok = json!({"request": {"contents": [
            {"role": "user", "parts": [{"text": "hi"}]},
            {"role": "model", "parts": [{"functionCall": {"id": "x", "name": "Read", "args": {}}}]},
            {"role": "user", "parts": [{"functionResponse": {"id": "x", "name": "Read", "response": {}}}]}
        ]}});
        assert!(
            prepare_antigravity_body_without_ledger(&mut ok, AgReplayToolContext::default())
                .is_none()
        );
    }

    // ---- ReplaceIfUnchanged / tombstone（移植 CPA antigravity_reasoning_replay_cache_test.go 本地用例）----

    fn item(sig: &str) -> Value {
        fc_item(&format!("{sig}-long-enough-signature"))
    }

    /// 对齐 TestAntigravityReasoningReplayConditionalMutationRejectsStaleLocalSnapshot
    #[test]
    fn conditional_mutation_rejects_stale_snapshot() {
        let cache = AgReplayCache::new(Duration::from_secs(60), 128);
        let key = "gemini-flash:claude:stale:agent:main";
        assert!(cache.replace_items(key, vec![item("old")]));
        let (_, snapshot) = cache.load(key);
        assert!(cache.replace_items(key, vec![item("new")]));
        assert_eq!(
            cache.replace_if_unchanged(key, &snapshot, vec![item("stale")]),
            Some(false)
        );
        assert!(!cache.invalidate_if_unchanged(key, &snapshot));
        let (items, _) = cache.get_items(key).expect("newer state kept");
        assert_eq!(
            items,
            vec![normalize_ag_replay_items(&[item("new")]).unwrap()[0].clone()]
        );
    }

    /// 对齐 ...NonPrefixReplaceRotatesLocalBranch：非前缀替换换分支，旧快照的「后继」被拒。
    #[test]
    fn non_prefix_replace_rotates_branch() {
        let cache = AgReplayCache::new(Duration::from_secs(60), 128);
        let key = "gemini-flash:claude:non-prefix:agent:main";
        cache.replace_items(key, vec![item("old")]);
        let (_, first) = cache.load(key);
        let (_, stale) = cache.load(key);
        assert_eq!(
            cache.replace_if_unchanged(key, &first, vec![item("new")]),
            Some(true)
        );
        assert_eq!(
            cache.replace_if_unchanged(key, &stale, vec![item("new"), item("latest")]),
            Some(false)
        );
    }

    /// 对齐 ...ConditionalReplaceAcceptsDescendantLocalChain：同分支后继链可越过旧快照。
    #[test]
    fn conditional_replace_accepts_descendant_chain() {
        let cache = AgReplayCache::new(Duration::from_secs(60), 128);
        let key = "gemini-flash:claude:descendant:agent:main";
        cache.replace_items(key, vec![item("prefix")]);
        let (_, stale) = cache.load(key);
        let (_, first) = cache.load(key);
        assert_eq!(
            cache.replace_if_unchanged(key, &first, vec![item("prefix"), item("middle")]),
            Some(true)
        );
        assert_eq!(
            cache.replace_if_unchanged(
                key,
                &stale,
                vec![item("prefix"), item("middle"), item("latest")]
            ),
            Some(true)
        );
        assert_eq!(cache.get_items(key).unwrap().0.len(), 3);
    }

    /// 对齐 ...DescendantMergeRejectsResetBranchABA：删除后重建同前缀的新分支，旧快照不得并入。
    #[test]
    fn descendant_merge_rejects_reset_branch_aba() {
        let cache = AgReplayCache::new(Duration::from_secs(60), 128);
        let key = "gemini-flash:claude:aba:agent:main";
        cache.replace_items(key, vec![item("prefix")]);
        let (_, stale) = cache.load(key);
        let (_, first) = cache.load(key);
        assert_eq!(
            cache.replace_if_unchanged(key, &first, vec![item("prefix"), item("middle")]),
            Some(true)
        );
        let (_, current) = cache.load(key);
        assert!(cache.invalidate_if_unchanged(key, &current));
        let (_, reset) = cache.load(key);
        assert_eq!(
            cache.replace_if_unchanged(key, &reset, vec![item("prefix")]),
            Some(true)
        );
        assert_eq!(
            cache.replace_if_unchanged(key, &stale, vec![item("prefix"), item("stale")]),
            Some(false)
        );
    }

    /// 对齐 ...ConditionalDeleteTombstoneBlocksStaleFirstWriter / ...EvictedTombstoneStillBlocks：
    /// miss 快照之后的条件清理写 tombstone；即使 tombstone 被淘汰，旧 miss 快照也写不进。
    #[test]
    fn tombstone_blocks_stale_first_writer_even_after_eviction() {
        for evict in [false, true] {
            let cache = AgReplayCache::new(Duration::from_secs(60), 128);
            let key = "gemini-flash:claude:first-writer:agent:main";
            let (items, stale) = cache.load(key);
            assert!(items.is_none() && stale.is_loaded());
            let (_, clear) = cache.load(key);
            assert!(cache.invalidate_if_unchanged(key, &clear));
            if evict {
                cache.inner.lock().unwrap().evict_oldest(1);
            }
            assert_eq!(
                cache.replace_if_unchanged(key, &stale, vec![item("stale-first")]),
                Some(false)
            );
        }
    }

    /// 对齐 ...UnrelatedEvictionDoesNotBlockAbsentSnapshot：miss 预留 per-key tombstone，
    /// 淘汰别的 key 不影响本 key 的首写。
    #[test]
    fn unrelated_eviction_does_not_block_absent_snapshot() {
        let cache = AgReplayCache::new(Duration::from_secs(60), 128);
        cache.replace_items("gemini-flash:claude:older:agent:main", vec![item("live")]);
        std::thread::sleep(Duration::from_millis(2));
        let key = "gemini-flash:claude:untouched:agent:main";
        let (_, snapshot) = cache.load(key);
        cache.inner.lock().unwrap().evict_oldest(1);
        assert!(cache
            .get_items("gemini-flash:claude:older:agent:main")
            .is_none());
        assert_eq!(
            cache.replace_if_unchanged(key, &snapshot, vec![item("first")]),
            Some(true)
        );
    }

    /// 对齐 ...LocalTombstonesStayWithinEntryBound / ...AbsenceReservationsStayWithinEntryBound
    #[test]
    fn tombstones_and_reservations_stay_within_capacity() {
        let capacity = 16;
        let cache = AgReplayCache::new(Duration::from_secs(60), capacity);
        for index in 0..=capacity {
            cache.invalidate(&format!("gemini-flash:claude:tomb-{index}:agent:main"));
        }
        assert!(cache.inner.lock().unwrap().entries.len() <= capacity);
        let mut latest = String::new();
        for index in 0..=capacity {
            latest = format!("gemini-flash:claude:absent-{index}:agent:main");
            assert!(cache.load(&latest).0.is_none());
        }
        let state = cache.inner.lock().unwrap();
        assert!(state.entries.len() <= capacity);
        assert!(state.entries.get(&latest).is_some_and(|e| e.deleted));
    }

    /// 并发：N 个请求读到同一快照后各自提交不同的后继链，恰好一个成功（其余既不匹配快照，
    /// 也不是新链的同分支后继）；最终账本是胜者的链，提交过程不丢失也不交叉覆盖。
    #[test]
    fn concurrent_commits_from_same_snapshot_single_winner() {
        let cache = AgReplayCache::new(Duration::from_secs(60), 128);
        let key = "gemini-flash:claude:race:agent:main";
        cache.replace_items(key, vec![item("base")]);
        let snapshots: Vec<_> = (0..8).map(|_| cache.load(key).1).collect();
        let barrier = std::sync::Arc::new(std::sync::Barrier::new(snapshots.len()));
        let handles: Vec<_> = snapshots
            .into_iter()
            .enumerate()
            .map(|(i, snapshot)| {
                let cache = cache.clone();
                let barrier = barrier.clone();
                std::thread::spawn(move || {
                    barrier.wait();
                    let won = cache
                        .replace_if_unchanged(
                            key,
                            &snapshot,
                            vec![item("base"), item(&format!("turn-{i}"))],
                        )
                        .unwrap();
                    (i, won)
                })
            })
            .collect();
        let results: Vec<(usize, bool)> = handles.into_iter().map(|h| h.join().unwrap()).collect();
        let winners: Vec<usize> = results
            .iter()
            .filter(|(_, w)| *w)
            .map(|(i, _)| *i)
            .collect();
        assert_eq!(winners.len(), 1, "{results:?}");
        let (items, _) = cache.get_items(key).unwrap();
        let expected =
            normalize_ag_replay_items(&[item("base"), item(&format!("turn-{}", winners[0]))])
                .unwrap();
        assert_eq!(items, expected);
    }

    /// 流式提取器按读账本时的快照提交：期间有更新的并发提交 → 旧流不覆盖；
    /// 终止空响应的条件清理同样不误删新账本。
    #[test]
    fn stream_extractor_commit_respects_snapshot() {
        let cache = AgReplayCache::new(Duration::from_secs(60), 128);
        let key = "gemini-flash:claude:stream-cas:agent:main".to_string();
        cache.replace_items(&key, vec![item("old")]);
        let (_, snapshot) = cache.load(&key);
        let request = json!({"contents": [{"role": "user", "parts": [{"text": "hi"}]}]});
        let mut slow = AgStreamReplayExtractor::new(
            cache.clone(),
            key.clone(),
            snapshot.clone(),
            request.clone(),
        );
        // 并发请求先提交
        cache.replace_items(&key, vec![item("newer")]);
        slow.push(
            br#"data: {"response":{"candidates":[{"content":{"parts":[{"thoughtSignature":"sig-slow-long-enough-1","functionCall":{"name":"Read","args":{"p":"/a"},"id":"id1"}}]},"finishReason":"STOP"}]}}

"#,
        );
        slow.finish();
        let newer = normalize_ag_replay_items(&[item("newer")]).unwrap();
        assert_eq!(cache.get_items(&key).unwrap().0, newer);

        let empty = json!({"response": {"candidates": [{"content": {"parts": [
            {"text": "no signature"}
        ]}, "finishReason": "STOP"}]}});
        cache.store_from_response(&key, &snapshot, &request, &empty);
        assert_eq!(
            cache.get_items(&key).unwrap().0,
            newer,
            "stale clear must not delete"
        );

        // 快照仍当前 → 正常提交
        let (_, current) = cache.load(&key);
        let mut fresh = AgStreamReplayExtractor::new(cache.clone(), key.clone(), current, request);
        fresh.push(
            br#"data: {"response":{"candidates":[{"content":{"parts":[{"thoughtSignature":"sig-fresh-long-enough","functionCall":{"name":"Read","args":{"p":"/a"},"id":"id1"}}]},"finishReason":"STOP"}]}}

"#,
        );
        fresh.finish();
        assert_eq!(
            cache.get_items(&key).unwrap().0[0]["thoughtSignature"],
            "sig-fresh-long-enough"
        );
    }

    // ---- 容量 / 后台清理（对齐 CPA MaxEntries=10240、EvictBatch=128、10 分钟 purgeExpired）----

    #[test]
    fn purge_expired_removes_entries_and_tombstones_and_bumps_epoch() {
        let cache = AgReplayCache::new(Duration::ZERO, 16);
        assert!(cache.replace_items("live", vec![item("a")]));
        cache.invalidate("dead");
        let epoch_before = cache.inner.lock().unwrap().eviction_epoch;
        std::thread::sleep(Duration::from_millis(5));
        assert_eq!(cache.purge_expired(), 2);
        let state = cache.inner.lock().unwrap();
        assert!(state.entries.is_empty());
        assert_eq!(state.eviction_epoch, epoch_before + 2);
    }

    #[test]
    fn purge_keeps_fresh_entries() {
        let cache = AgReplayCache::new(AG_REPLAY_CACHE_TTL, AG_REPLAY_CACHE_MAX_ENTRIES);
        assert!(cache.replace_items("k", vec![item("a")]));
        assert_eq!(cache.purge_expired(), 0);
        assert!(cache.get_items("k").is_some());
    }

    /// 超出容量时按批淘汰最旧条目（10240 + 1 → 淘汰 128）。
    #[test]
    fn capacity_overflow_evicts_one_batch() {
        let cache = AgReplayCache::new(AG_REPLAY_CACHE_TTL, AG_REPLAY_CACHE_MAX_ENTRIES);
        for i in 0..=AG_REPLAY_CACHE_MAX_ENTRIES {
            cache.invalidate(&format!("k{i}"));
        }
        assert_eq!(
            cache.inner.lock().unwrap().entries.len(),
            AG_REPLAY_CACHE_MAX_ENTRIES + 1 - EVICT_BATCH_SIZE
        );
    }

    /// 后台任务按周期清理；缓存 drop 后任务自行退出（不挂住测试 / 关停）。
    #[tokio::test(start_paused = true)]
    async fn purge_task_runs_periodically_and_stops_after_cache_dropped() {
        let cache = AgReplayCache::new(Duration::ZERO, 16);
        assert!(cache.replace_items("k", vec![item("a")]));
        std::thread::sleep(Duration::from_millis(5));
        let handle = cache.spawn_purge_task(AG_REPLAY_CACHE_CLEANUP_INTERVAL);
        tokio::time::sleep(AG_REPLAY_CACHE_CLEANUP_INTERVAL / 2).await;
        assert_eq!(cache.inner.lock().unwrap().entries.len(), 1, "首拍不清理");
        tokio::time::sleep(AG_REPLAY_CACHE_CLEANUP_INTERVAL).await;
        assert!(cache.inner.lock().unwrap().entries.is_empty());
        drop(cache);
        tokio::time::timeout(AG_REPLAY_CACHE_CLEANUP_INTERVAL * 2, handle)
            .await
            .expect("purge task exits after cache dropped")
            .unwrap();
    }
}
