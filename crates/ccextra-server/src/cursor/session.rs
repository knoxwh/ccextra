use super::drive::{CursorDrive, CursorEvent};
use super::journal::{CursorEventJournal, JOURNAL_TTL};
use bytes::Bytes;
use ccextra_core::convert::cursor::proto::{ExecKind, ExecRequest};
use sha2::{Digest, Sha256};
use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, oneshot, watch};

const SESSION_TTL: Duration = Duration::from_secs(5 * 60);
const CHECKPOINT_TTL: Duration = Duration::from_secs(30 * 60);
const DISCONNECT_GRACE_PERIOD: Duration = Duration::from_secs(5);

/// Singleflight 结果：成功或失败
#[derive(Clone)]
pub enum RunOutcome {
    Success(Vec<u8>),
    Failure(String),
}

/// 正在运行的请求状态（用于 singleflight 和断线恢复）
#[derive(Clone)]
pub struct InflightRun {
    /// 结果广播通道
    pub notify: broadcast::Sender<RunOutcome>,
    pub outcome: Arc<Mutex<Option<RunOutcome>>>,
    /// 实时 SSE 帧广播通道
    pub event_tx: broadcast::Sender<Bytes>,
    /// 活跃 consumers 计数
    pub active_consumers: Arc<AtomicUsize>,
    /// consumer epoch，递增用于区分 grace 定时器
    pub consumer_epoch: Arc<AtomicU64>,
}

impl Default for InflightRun {
    fn default() -> Self {
        Self::new()
    }
}

impl InflightRun {
    pub fn new() -> Self {
        let (notify, _) = broadcast::channel(32);
        let (event_tx, _) = broadcast::channel(512);
        Self {
            notify,
            outcome: Arc::new(Mutex::new(None)),
            event_tx,
            active_consumers: Arc::new(AtomicUsize::new(0)),
            consumer_epoch: Arc::new(AtomicU64::new(0)),
        }
    }
}

/// Consumer 活跃连接看门狗，持有引用计数并提供 5 秒断线 grace
pub struct ConsumerGuard {
    pub sessions: CursorSessions,
    pub conversation: String,
    pub identity: String,
    pub generation: u64,
    pub active_consumers: Arc<AtomicUsize>,
    pub consumer_epoch: Arc<AtomicU64>,
}

impl Drop for ConsumerGuard {
    fn drop(&mut self) {
        let mut registry = self.sessions.inner.lock().unwrap();
        let remaining = self.active_consumers.fetch_sub(1, Ordering::SeqCst) - 1;
        if remaining == 0 {
            if let Some(owner) = registry.owners.get_mut(&self.conversation) {
                if owner.identity == self.identity
                    && owner.generation == self.generation
                    && owner.inflight.as_ref().is_some_and(|run| {
                        Arc::ptr_eq(&run.active_consumers, &self.active_consumers)
                    })
                {
                    owner.attached = false;
                }
            }
            let sessions = self.sessions.clone();
            let conversation = self.conversation.clone();
            let identity = self.identity.clone();
            let generation = self.generation;
            let active_consumers = self.active_consumers.clone();
            let consumer_epoch = self.consumer_epoch.clone();
            let my_epoch = consumer_epoch.load(Ordering::SeqCst);
            tokio::spawn(async move {
                tokio::time::sleep(DISCONNECT_GRACE_PERIOD).await;
                // 若 5 秒后仍无活跃 consumer，且没有新连接递增 epoch，则真正取消 session
                let mut registry = sessions.inner.lock().unwrap();
                if active_consumers.load(Ordering::SeqCst) == 0
                    && consumer_epoch.load(Ordering::SeqCst) == my_epoch
                    && registry.owners.get(&conversation).is_some_and(|owner| {
                        owner.identity == identity
                            && owner.generation == generation
                            && owner.state == SessionState::Running
                            && !owner.attached
                            && owner.inflight.as_ref().is_some_and(|run| {
                                Arc::ptr_eq(&run.active_consumers, &active_consumers)
                            })
                    })
                {
                    if let Some(owner) = registry.owners.remove(&conversation) {
                        owner.cancelled.send_replace(true);
                    }
                }
            });
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    Running,
    AwaitingToolResults,
    Completed,
    Failed,
}

pub fn compute_turn_digest(
    conversation: &str,
    upstream_model: &str,
    model_params: &serde_json::Value,
    system: &serde_json::Value,
    messages: &serde_json::Value,
    tools: &serde_json::Value,
) -> String {
    let mut hasher = Sha256::new();
    hasher.update(conversation.as_bytes());
    hasher.update(upstream_model.as_bytes());
    hasher.update(model_params.to_string().as_bytes());
    hasher.update(system.to_string().as_bytes());
    hasher.update(messages.to_string().as_bytes());
    hasher.update(tools.to_string().as_bytes());
    format!("{:x}", hasher.finalize())
}

pub fn compute_tool_catalog_fingerprint(tools: &serde_json::Value) -> String {
    let mut hasher = Sha256::new();
    hasher.update(tools.to_string().as_bytes());
    format!("{:x}", hasher.finalize())
}

#[derive(Clone)]
pub struct CursorSessions {
    inner: Arc<Mutex<Registry>>,
    journal: CursorEventJournal,
}

impl Default for CursorSessions {
    fn default() -> Self {
        let inner = Arc::new(Mutex::new(Registry::default()));
        let journal = CursorEventJournal::new();
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let weak = Arc::downgrade(&inner);
            let journal_clone = journal.clone();
            handle.spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    let Some(inner) = weak.upgrade() else {
                        break;
                    };
                    inner.lock().unwrap().sweep(Instant::now());
                    journal_clone.sweep_expired(JOURNAL_TTL);
                }
            });
        }
        Self { inner, journal }
    }
}

#[derive(Default)]
struct Registry {
    generation: u64,
    owners: HashMap<String, Owner>,
    active: HashMap<String, ParkedSession>,
    checkpoints: HashMap<String, Checkpoint>,
}

struct Owner {
    identity: String,
    generation: u64,
    deadline: Instant,
    cancelled: watch::Sender<bool>,
    upstream_model: String,
    tool_catalog_fingerprint: String,
    turn_digest: String,
    state: SessionState,
    attached: bool,
    response_snapshot: Option<Vec<u8>>,
    /// 正在运行的请求（singleflight）
    inflight: Option<InflightRun>,
}

struct ParkedSession {
    pending: Vec<ExecRequest>,
    generation: u64,
    resume: oneshot::Sender<oneshot::Sender<CursorDrive>>,
    deadline: Instant,
}

struct Checkpoint {
    identity: String,
    generation: u64,
    raw: Vec<u8>,
    blobs: HashMap<String, Vec<u8>>,
    deadline: Instant,
}

pub struct ToolResult {
    pub tool_call_id: String,
    pub content: String,
    pub is_error: bool,
}

type MatchedTools = Vec<(ExecRequest, ToolResult)>;
pub enum ResumedSession {
    Resumed(
        u64,
        oneshot::Receiver<CursorDrive>,
        MatchedTools,
        InflightRun,
    ),
    Joined(u64, InflightRun),
}
type CheckpointData = (Vec<u8>, HashMap<String, Vec<u8>>);

/// 开启请求结果
pub enum BeginOutcome {
    /// 新启动的请求，持有 generation 与 Inflight 状态
    Started(u64, InflightRun),
    /// 已有同 digest 运行中请求（singleflight），直接加入
    AlreadyRunning(u64, InflightRun),
}

fn match_pending(
    pending: &[ExecRequest],
    results: Vec<ToolResult>,
) -> Result<Vec<(&ExecRequest, ToolResult)>, &'static str> {
    if pending.is_empty() || pending.len() != results.len() {
        return Err("Cursor 工具结果数量与待处理调用不匹配");
    }
    let mut by_id = HashMap::new();
    for result in results {
        if result.tool_call_id.is_empty()
            || by_id.insert(result.tool_call_id.clone(), result).is_some()
        {
            return Err("Cursor 工具结果 ID 为空或重复");
        }
    }
    let mut seen = HashSet::new();
    pending
        .iter()
        .map(|exec| {
            let ExecKind::Mcp { tool_call_id, .. } = &exec.kind else {
                return Err("Cursor 待处理调用不是 MCP 工具");
            };
            if tool_call_id.is_empty() || !seen.insert(tool_call_id) {
                return Err("Cursor 待处理工具 ID 为空或重复");
            }
            let result = by_id
                .remove(tool_call_id)
                .ok_or("Cursor 工具结果 ID 不匹配")?;
            Ok((exec, result))
        })
        .collect()
}

impl Registry {
    fn sweep(&mut self, now: Instant) {
        self.active.retain(|_, session| session.deadline > now);
        self.checkpoints
            .retain(|_, checkpoint| checkpoint.deadline > now);
        self.owners.retain(|key, owner| {
            let keep = owner.deadline > now || self.active.contains_key(key);
            if !keep {
                owner.cancelled.send_replace(true);
            }
            keep
        });
    }
}

impl CursorSessions {
    pub fn try_begin(
        &self,
        conversation: &str,
        identity: &str,
        upstream_model: String,
        tool_catalog_fingerprint: String,
        turn_digest: String,
    ) -> Result<BeginOutcome, &'static str> {
        let mut registry = self.inner.lock().unwrap();
        registry.sweep(Instant::now());

        // 检查是否有 running turn
        if let Some(existing) = registry.owners.get_mut(conversation) {
            if existing.identity == identity && existing.state != SessionState::Failed {
                if existing.turn_digest == turn_digest {
                    if let Some(inflight) = existing.inflight.clone() {
                        existing.attached = true;
                        return Ok(BeginOutcome::AlreadyRunning(existing.generation, inflight));
                    }
                }
                if existing.state == SessionState::Running && existing.attached {
                    return Err("session attached to running request");
                }
            }
        }

        registry.generation = registry.generation.wrapping_add(1);
        let generation = registry.generation;
        registry.active.remove(conversation);
        if let Some(previous) = registry.owners.remove(conversation) {
            previous.cancelled.send_replace(true);
        }
        if let Some(checkpoint) = registry.checkpoints.get_mut(conversation) {
            if checkpoint.identity == identity {
                checkpoint.generation = generation;
            } else {
                registry.checkpoints.remove(conversation);
            }
        }
        let inflight = InflightRun::new();
        registry.owners.insert(
            conversation.into(),
            Owner {
                identity: identity.into(),
                generation,
                deadline: Instant::now() + CHECKPOINT_TTL,
                cancelled: watch::channel(false).0,
                upstream_model,
                tool_catalog_fingerprint,
                turn_digest,
                state: SessionState::Running,
                attached: true,
                response_snapshot: None,
                inflight: Some(inflight.clone()),
            },
        );
        Ok(BeginOutcome::Started(generation, inflight))
    }

    pub fn cancellation(
        &self,
        conversation: &str,
        identity: &str,
        generation: u64,
    ) -> Option<watch::Receiver<bool>> {
        let registry = self.inner.lock().unwrap();
        registry
            .owners
            .get(conversation)
            .filter(|owner| owner.identity == identity && owner.generation == generation)
            .map(|owner| owner.cancelled.subscribe())
    }

    pub fn retain_identity(&self, identity: Option<&str>) {
        let mut registry = self.inner.lock().unwrap();
        let stale: Vec<String> = registry
            .owners
            .iter()
            .filter(|(_, owner)| Some(owner.identity.as_str()) != identity)
            .map(|(conversation, _)| conversation.clone())
            .collect();
        for conversation in stale {
            if let Some(owner) = registry.owners.remove(&conversation) {
                owner.cancelled.send_replace(true);
            }
            registry.active.remove(&conversation);
        }
        registry
            .checkpoints
            .retain(|_, checkpoint| Some(checkpoint.identity.as_str()) == identity);
    }

    async fn maintain_parked(
        self,
        conversation: String,
        identity: String,
        generation: u64,
        mut drive: CursorDrive,
        mut cancelled: watch::Receiver<bool>,
        mut resume: oneshot::Receiver<oneshot::Sender<CursorDrive>>,
    ) {
        loop {
            if *cancelled.borrow() {
                break;
            }
            let event = match drive.next_ready_event().await {
                Ok(Some(event)) => event,
                Ok(None) => {
                    tokio::select! {
                        biased;
                        _ = cancelled.changed() => break,
                        handoff = &mut resume => {
                            if let Ok(sender) = handoff {
                                let _ = sender.send(drive);
                                return;
                            }
                            break;
                        }
                        result = drive.read_chunk() => {
                            if let Err(error) = result {
                                tracing::warn!("Cursor 等待工具结果期间上游流中断: {error}");
                                break;
                            }
                        }
                    }
                    continue;
                }
                Err(error) => {
                    tracing::warn!("Cursor 等待工具结果期间上游流中断: {error}");
                    break;
                }
            };
            match event {
                CursorEvent::Checkpoint(raw) => {
                    self.record_checkpoint(
                        &conversation,
                        &identity,
                        generation,
                        raw,
                        drive.blob_store(),
                    );
                }
                // 工具边界已收尾；对齐 Plus 的等待循环，迟到的输出增量不跨回合发送。
                CursorEvent::Text(_) | CursorEvent::Thinking(_) | CursorEvent::Tokens(_) => {}
                CursorEvent::End | CursorEvent::TurnEnded(_) | CursorEvent::ToolUse { .. } => break,
            }
        }
        self.cancel(&conversation, &identity, generation);
    }

    pub fn park(
        &self,
        conversation: &str,
        identity: &str,
        generation: u64,
        drive: CursorDrive,
        pending: Vec<ExecRequest>,
    ) -> Result<(), &'static str> {
        let mut registry = self.inner.lock().unwrap();
        registry.sweep(Instant::now());
        let Some(owner) = registry.owners.get_mut(conversation) else {
            return Err("Cursor 会话已取消");
        };
        if owner.identity != identity || owner.generation != generation {
            return Err("Cursor 会话 owner 已更换");
        }
        let ids: Vec<_> = pending
            .iter()
            .map(|exec| match &exec.kind {
                ExecKind::Mcp { tool_call_id, .. } if !tool_call_id.is_empty() => {
                    Ok(tool_call_id.as_str())
                }
                _ => Err("Cursor 待处理调用不是有效 MCP 工具"),
            })
            .collect::<Result<_, _>>()?;
        if ids.is_empty() || ids.iter().collect::<HashSet<_>>().len() != ids.len() {
            return Err("Cursor 待处理工具 ID 为空或重复");
        }
        owner.state = SessionState::AwaitingToolResults;
        let cancelled = owner.cancelled.subscribe();
        let (resume_sender, resume_receiver) = oneshot::channel();
        registry.active.insert(
            conversation.into(),
            ParkedSession {
                pending,
                generation,
                resume: resume_sender,
                deadline: Instant::now() + SESSION_TTL,
            },
        );
        drop(registry);
        tokio::spawn(self.clone().maintain_parked(
            conversation.to_owned(),
            identity.to_owned(),
            generation,
            drive,
            cancelled,
            resume_receiver,
        ));
        Ok(())
    }

    pub fn take(
        &self,
        conversation: &str,
        identity: &str,
        results: Vec<ToolResult>,
        upstream_model: &str,
        tool_catalog_fingerprint: &str,
        turn_digest: String,
    ) -> Result<ResumedSession, &'static str> {
        let mut registry = self.inner.lock().unwrap();
        registry.sweep(Instant::now());
        {
            let Some(owner) = registry.owners.get(conversation) else {
                return Err("session 不存在");
            };
            if owner.identity != identity {
                return Err("credential 不匹配");
            }
            if owner.upstream_model != upstream_model {
                return Err("模型不匹配");
            }
            if owner.tool_catalog_fingerprint != tool_catalog_fingerprint {
                return Err("工具目录不匹配");
            }
            if owner.turn_digest == turn_digest && owner.state != SessionState::Failed {
                if let Some(run) = &owner.inflight {
                    return Ok(ResumedSession::Joined(owner.generation, run.clone()));
                }
            }
            let Some(session) = registry.active.get(conversation) else {
                return Err("session 未 park");
            };
            if owner.generation != session.generation {
                return Err("generation 不匹配");
            }
        }
        let Some(session) = registry.active.get(conversation) else {
            return Err("session 未 park");
        };
        let matched = match_pending(&session.pending, results)?
            .into_iter()
            .map(|(exec, result)| (exec.clone(), result))
            .collect();
        let session = registry
            .active
            .remove(conversation)
            .expect("validated parked session");
        let (sender, drive) = oneshot::channel();
        if session.resume.send(sender).is_err() {
            if let Some(owner) = registry.owners.remove(conversation) {
                owner.cancelled.send_replace(true);
            }
            return Err("resume channel 已关闭");
        }
        let Some(owner) = registry.owners.get_mut(conversation) else {
            return Err("session 不存在");
        };
        // 原子更新 owner 的 digest、state 与 inflight
        owner.turn_digest = turn_digest;
        owner.state = SessionState::Running;
        owner.attached = true;
        owner.response_snapshot = None;
        let inflight = InflightRun::new();
        owner.inflight = Some(inflight.clone());
        Ok(ResumedSession::Resumed(
            session.generation,
            drive,
            matched,
            inflight,
        ))
    }

    pub fn record_checkpoint(
        &self,
        conversation: &str,
        identity: &str,
        generation: u64,
        raw: Vec<u8>,
        blobs: HashMap<String, Vec<u8>>,
    ) -> bool {
        let mut registry = self.inner.lock().unwrap();
        registry.sweep(Instant::now());
        let Some(owner) = registry.owners.get(conversation) else {
            return false;
        };
        if owner.identity != identity || owner.generation != generation || raw.is_empty() {
            return false;
        }
        registry.checkpoints.insert(
            conversation.into(),
            Checkpoint {
                identity: identity.into(),
                generation,
                raw,
                blobs,
                deadline: Instant::now() + CHECKPOINT_TTL,
            },
        );
        true
    }

    pub fn checkpoint(&self, conversation: &str, identity: &str) -> Option<CheckpointData> {
        let mut registry = self.inner.lock().unwrap();
        registry.sweep(Instant::now());
        registry
            .checkpoints
            .get(conversation)
            .filter(|checkpoint| checkpoint.identity == identity)
            .map(|checkpoint| (checkpoint.raw.clone(), checkpoint.blobs.clone()))
    }

    pub fn cancel(&self, conversation: &str, identity: &str, generation: u64) {
        let mut registry = self.inner.lock().unwrap();
        if registry
            .owners
            .get(conversation)
            .is_some_and(|owner| owner.identity == identity && owner.generation == generation)
        {
            registry.active.remove(conversation);
            if let Some(owner) = registry.owners.remove(conversation) {
                owner.cancelled.send_replace(true);
            }
        }
    }

    pub fn finish(&self, conversation: &str, identity: &str, generation: u64) {
        let mut registry = self.inner.lock().unwrap();
        if registry
            .owners
            .get(conversation)
            .is_some_and(|owner| owner.identity == identity && owner.generation == generation)
        {
            registry.active.remove(conversation);
            if let Some(owner) = registry.owners.get_mut(conversation) {
                owner.state = SessionState::Completed;
            }
        }
    }

    pub fn record_failure(&self, conversation: &str, identity: &str, generation: u64) {
        let mut registry = self.inner.lock().unwrap();
        if let Some(owner) = registry.owners.get_mut(conversation) {
            if owner.identity == identity && owner.generation == generation {
                owner.state = SessionState::Failed;
            }
        }
    }

    /// 创建指定 inflight 的 consumer guard
    pub fn create_consumer_guard(
        &self,
        conversation: &str,
        identity: &str,
        generation: u64,
        inflight: &InflightRun,
    ) -> ConsumerGuard {
        let mut registry = self.inner.lock().unwrap();
        if let Some(owner) = registry.owners.get_mut(conversation) {
            if owner.identity == identity && owner.generation == generation {
                owner.attached = true;
            }
        }
        inflight.consumer_epoch.fetch_add(1, Ordering::SeqCst);
        inflight.active_consumers.fetch_add(1, Ordering::SeqCst);
        ConsumerGuard {
            sessions: self.clone(),
            conversation: conversation.to_string(),
            identity: identity.to_string(),
            generation,
            active_consumers: inflight.active_consumers.clone(),
            consumer_epoch: inflight.consumer_epoch.clone(),
        }
    }

    /// 尝试加入已运行的请求（singleflight / 断线重连）
    pub fn try_join_inflight(
        &self,
        conversation: &str,
        identity: &str,
        turn_digest: &str,
    ) -> Option<(InflightRun, ConsumerGuard)> {
        let mut registry = self.inner.lock().unwrap();
        registry.sweep(Instant::now());
        let owner = registry.owners.get_mut(conversation)?;
        if owner.identity != identity || owner.turn_digest != turn_digest {
            return None;
        }
        if owner.state != SessionState::Running {
            return None;
        }
        let inflight = owner.inflight.clone()?;
        owner.attached = true;
        inflight.consumer_epoch.fetch_add(1, Ordering::SeqCst);
        inflight.active_consumers.fetch_add(1, Ordering::SeqCst);
        let guard = ConsumerGuard {
            sessions: self.clone(),
            conversation: conversation.to_string(),
            identity: identity.to_string(),
            generation: owner.generation,
            active_consumers: inflight.active_consumers.clone(),
            consumer_epoch: inflight.consumer_epoch.clone(),
        };
        Some((inflight, guard))
    }

    /// 检查是否已完成（Completed 状态），返回 response_snapshot
    pub fn try_get_completed(
        &self,
        conversation: &str,
        identity: &str,
        turn_digest: &str,
    ) -> Option<Vec<u8>> {
        let mut registry = self.inner.lock().unwrap();
        registry.sweep(Instant::now());
        let owner = registry.owners.get(conversation)?;
        if owner.identity != identity || owner.turn_digest != turn_digest {
            return None;
        }
        if matches!(
            owner.state,
            SessionState::Completed | SessionState::AwaitingToolResults
        ) {
            owner.response_snapshot.clone()
        } else {
            None
        }
    }

    /// 广播 inflight run 完成结果
    pub fn broadcast_outcome(
        &self,
        conversation: &str,
        identity: &str,
        generation: u64,
        outcome: RunOutcome,
    ) {
        let mut registry = self.inner.lock().unwrap();
        if let Some(owner) = registry.owners.get_mut(conversation) {
            if owner.identity == identity && owner.generation == generation {
                if let Some(run) = owner.inflight.as_ref() {
                    *run.outcome.lock().unwrap() = Some(outcome.clone());
                    let _ = run.notify.send(outcome.clone());
                    // 标记 state 为 Completed/Failed，保存 response_snapshot
                    match &outcome {
                        RunOutcome::Success(snapshot) => {
                            if owner.state != SessionState::AwaitingToolResults {
                                owner.state = SessionState::Completed;
                            }
                            owner.response_snapshot = Some(snapshot.clone());
                        }
                        RunOutcome::Failure(_) => {
                            owner.state = SessionState::Failed;
                        }
                    }
                }
            }
        }
    }

    /// 在 owner 锁内发布帧，防止旧 producer 污染同 digest 的新执行。
    pub fn publish_if_current(
        &self,
        conversation: &str,
        identity: &str,
        generation: u64,
        publish: impl FnOnce(&CursorEventJournal),
    ) -> bool {
        let registry = self.inner.lock().unwrap();
        if !registry
            .owners
            .get(conversation)
            .is_some_and(|owner| owner.identity == identity && owner.generation == generation)
        {
            return false;
        }
        publish(&self.journal);
        true
    }

    /// 获取 journal 引用（用于在 handler 中直接记录事件）
    pub fn journal(&self) -> &CursorEventJournal {
        &self.journal
    }

    /// 清理 journal 中的旧 turn
    pub fn cleanup_turn_journal(&self, turn_digest: &str) {
        self.journal.remove(turn_digest);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn begin(sessions: &CursorSessions, conversation: &str, identity: &str, digest: &str) -> u64 {
        let BeginOutcome::Started(generation, _) = sessions
            .try_begin(
                conversation,
                identity,
                String::new(),
                String::new(),
                digest.into(),
            )
            .unwrap()
        else {
            panic!("expected new run")
        };
        generation
    }

    fn exec(id: u32, name: &str) -> ExecRequest {
        ExecRequest {
            exec_msg_id: id,
            exec_id: format!("exec-{id}"),
            kind: ExecKind::Mcp {
                name: "lookup".into(),
                tool_call_id: name.into(),
                args: Default::default(),
            },
        }
    }

    fn result(id: &str) -> ToolResult {
        ToolResult {
            tool_call_id: id.into(),
            content: "ok".into(),
            is_error: false,
        }
    }

    #[tokio::test(start_paused = true)]
    async fn last_consumer_drop_starts_fresh_grace_and_parked_turn_survives() {
        let sessions = CursorSessions::default();
        let BeginOutcome::Started(generation, run) = sessions
            .try_begin(
                "conv",
                "account",
                "model".into(),
                String::new(),
                "digest".into(),
            )
            .unwrap()
        else {
            panic!("expected new run")
        };
        let first = sessions.create_consumer_guard("conv", "account", generation, &run);
        let second = sessions.create_consumer_guard("conv", "account", generation, &run);
        drop(second);
        tokio::time::advance(Duration::from_secs(6)).await;
        assert!(sessions
            .cancellation("conv", "account", generation)
            .is_some());
        drop(first);
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(4)).await;
        let (_, reconnect) = sessions
            .try_join_inflight("conv", "account", "digest")
            .unwrap();
        tokio::time::advance(Duration::from_secs(2)).await;
        assert!(sessions
            .cancellation("conv", "account", generation)
            .is_some());
        drop(reconnect);
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(5)).await;
        tokio::task::yield_now().await;
        assert!(sessions
            .cancellation("conv", "account", generation)
            .is_none());

        let generation = begin(&sessions, "parked", "account", "");
        let (_, guard) = sessions.try_join_inflight("parked", "account", "").unwrap();
        sessions
            .inner
            .lock()
            .unwrap()
            .owners
            .get_mut("parked")
            .unwrap()
            .state = SessionState::AwaitingToolResults;
        drop(guard);
        tokio::task::yield_now().await;
        tokio::time::advance(Duration::from_secs(6)).await;
        tokio::task::yield_now().await;
        assert!(sessions
            .cancellation("parked", "account", generation)
            .is_some());
    }

    #[test]
    fn matches_multiple_tool_calls_by_id_not_position() {
        let pending = [exec(7, "call-a"), exec(9, "call-b")];
        let pairs = match_pending(&pending, vec![result("call-b"), result("call-a")]).unwrap();
        assert_eq!(pairs[0].0.exec_msg_id, 7);
        assert_eq!(pairs[0].0.exec_id, "exec-7");
        assert_eq!(pairs[0].1.tool_call_id, "call-a");
        assert_eq!(pairs[1].0.exec_msg_id, 9);
        assert_eq!(pairs[1].1.tool_call_id, "call-b");
        assert!(match_pending(&pending, vec![result("call-a")]).is_err());
        assert!(match_pending(&pending, vec![result("call-a"), result("call-a")]).is_err());
        assert!(match_pending(&pending, vec![result("call-a"), result("call-c")]).is_err());
        assert!(match_pending(
            &[exec(1, "same"), exec(2, "same")],
            vec![result("same"), result("other")]
        )
        .is_err());
    }

    #[test]
    fn checkpoint_rejects_stale_owner_and_account_replacement() {
        let sessions = CursorSessions::default();
        let old = begin(&sessions, "conv", "account-a", "first");
        assert!(sessions.record_checkpoint(
            "conv",
            "account-a",
            old,
            vec![1, 2, 3],
            HashMap::new()
        ));
        assert!(sessions.checkpoint("conv", "account-b").is_none());
        sessions.finish("conv", "account-a", old);
        let new = begin(&sessions, "conv", "account-a", "next");
        assert_ne!(new, old);
        assert!(!sessions.record_checkpoint("conv", "account-a", old, vec![9], HashMap::new()));
        assert_eq!(
            sessions.checkpoint("conv", "account-a").unwrap().0,
            vec![1, 2, 3]
        );
        sessions.cancel("conv", "account-a", old);
        assert!(sessions.record_checkpoint("conv", "account-a", new, vec![4], HashMap::new()));
        begin(&sessions, "conv", "account-b", "first");
        assert!(sessions.checkpoint("conv", "account-a").is_none());
        assert!(sessions.checkpoint("conv", "account-b").is_none());
    }

    #[tokio::test]
    async fn replacing_owner_cancels_previous_stream() {
        let sessions = CursorSessions::default();
        let first = begin(&sessions, "conv", "account-a", "first");
        let receiver = sessions.cancellation("conv", "account-a", first).unwrap();
        assert!(sessions
            .try_begin(
                "conv",
                "account-a",
                String::new(),
                String::new(),
                "next".into()
            )
            .is_err());
        let (_, guard) = sessions
            .try_join_inflight("conv", "account-a", "first")
            .unwrap();
        drop(guard);
        let second = begin(&sessions, "conv", "account-a", "next");
        assert!(*receiver.borrow());
        assert!(sessions.cancellation("conv", "account-a", first).is_none());
        assert!(!*sessions
            .cancellation("conv", "account-a", second)
            .unwrap()
            .borrow());
        sessions.cancel("conv", "account-a", second);
        assert!(sessions.cancellation("conv", "account-a", second).is_none());
    }

    #[test]
    fn credential_replacement_evicts_old_stream_and_checkpoint() {
        let sessions = CursorSessions::default();
        let old = begin(&sessions, "old", "account-a", "first");
        let receiver = sessions.cancellation("old", "account-a", old).unwrap();
        sessions.record_checkpoint("old", "account-a", old, vec![1], HashMap::new());
        let current = begin(&sessions, "current", "account-b", "first");
        sessions.record_checkpoint("current", "account-b", current, vec![2], HashMap::new());
        sessions.retain_identity(Some("account-b"));
        assert!(*receiver.borrow());
        assert!(sessions.checkpoint("old", "account-a").is_none());
        assert_eq!(
            sessions.checkpoint("current", "account-b").unwrap().0,
            vec![2]
        );
        sessions.retain_identity(None);
        assert!(sessions.checkpoint("current", "account-b").is_none());
    }

    #[test]
    fn expired_checkpoint_and_owner_cannot_resume() {
        let sessions = CursorSessions::default();
        let owner = begin(&sessions, "conv", "account-a", "first");
        sessions.record_checkpoint("conv", "account-a", owner, vec![1], HashMap::new());
        {
            let mut registry = sessions.inner.lock().unwrap();
            registry.owners.get_mut("conv").unwrap().deadline =
                Instant::now() - Duration::from_secs(1);
            registry.checkpoints.get_mut("conv").unwrap().deadline =
                Instant::now() - Duration::from_secs(1);
        }
        assert!(sessions.checkpoint("conv", "account-a").is_none());
        assert!(!sessions.record_checkpoint("conv", "account-a", owner, vec![2], HashMap::new()));
    }
}
