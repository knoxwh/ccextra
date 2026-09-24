use super::drive::{CursorDrive, CursorEvent};
use ccextra_core::convert::cursor::proto::{ExecKind, ExecRequest};
use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::sync::{oneshot, watch};

const SESSION_TTL: Duration = Duration::from_secs(5 * 60);
const CHECKPOINT_TTL: Duration = Duration::from_secs(30 * 60);

#[derive(Clone)]
pub struct CursorSessions {
    inner: Arc<Mutex<Registry>>,
}

impl Default for CursorSessions {
    fn default() -> Self {
        let inner = Arc::new(Mutex::new(Registry::default()));
        if let Ok(handle) = tokio::runtime::Handle::try_current() {
            let weak = Arc::downgrade(&inner);
            handle.spawn(async move {
                loop {
                    tokio::time::sleep(Duration::from_secs(30)).await;
                    let Some(inner) = weak.upgrade() else {
                        break;
                    };
                    inner.lock().unwrap().sweep(Instant::now());
                }
            });
        }
        Self { inner }
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
type ResumedSession = (u64, oneshot::Receiver<CursorDrive>, MatchedTools);
type CheckpointData = (Vec<u8>, HashMap<String, Vec<u8>>);

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
            owner.deadline > now
                || self.active.contains_key(key)
                || self.checkpoints.contains_key(key)
        });
    }
}

impl CursorSessions {
    pub fn begin(&self, conversation: &str, identity: &str) -> u64 {
        let mut registry = self.inner.lock().unwrap();
        registry.sweep(Instant::now());
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
        registry.owners.insert(
            conversation.into(),
            Owner {
                identity: identity.into(),
                generation,
                deadline: Instant::now() + CHECKPOINT_TTL,
                cancelled: watch::channel(false).0,
            },
        );
        generation
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
                CursorEvent::End | CursorEvent::TurnEnded | CursorEvent::ToolUse { .. } => break,
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
        let Some(owner) = registry.owners.get(conversation) else {
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
    ) -> Result<Option<ResumedSession>, &'static str> {
        let mut registry = self.inner.lock().unwrap();
        registry.sweep(Instant::now());
        let Some(owner) = registry.owners.get(conversation) else {
            return Ok(None);
        };
        if owner.identity != identity {
            return Ok(None);
        }
        let Some(session) = registry.active.get(conversation) else {
            return Ok(None);
        };
        if owner.generation != session.generation {
            return Ok(None);
        }
        let matched = match_pending(&session.pending, results)?
            .into_iter()
            .map(|(exec, result)| (exec.clone(), result))
            .collect();
        let session = registry
            .active
            .remove(conversation)
            .expect("validated active session");
        let (sender, drive) = oneshot::channel();
        if session.resume.send(sender).is_err() {
            if let Some(owner) = registry.owners.remove(conversation) {
                owner.cancelled.send_replace(true);
            }
            return Ok(None);
        }
        Ok(Some((session.generation, drive, matched)))
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
            registry.owners.remove(conversation);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        let old = sessions.begin("conv", "account-a");
        assert!(sessions.record_checkpoint(
            "conv",
            "account-a",
            old,
            vec![1, 2, 3],
            HashMap::new()
        ));
        assert!(sessions.checkpoint("conv", "account-b").is_none());
        let new = sessions.begin("conv", "account-a");
        assert_ne!(new, old);
        assert!(!sessions.record_checkpoint("conv", "account-a", old, vec![9], HashMap::new()));
        assert_eq!(
            sessions.checkpoint("conv", "account-a").unwrap().0,
            vec![1, 2, 3]
        );
        sessions.cancel("conv", "account-a", old);
        assert!(sessions.record_checkpoint("conv", "account-a", new, vec![4], HashMap::new()));
        sessions.begin("conv", "account-b");
        assert!(sessions.checkpoint("conv", "account-a").is_none());
        assert!(sessions.checkpoint("conv", "account-b").is_none());
    }

    #[test]
    fn replacing_owner_cancels_previous_stream() {
        let sessions = CursorSessions::default();
        let first = sessions.begin("conv", "account-a");
        let receiver = sessions.cancellation("conv", "account-a", first).unwrap();
        let second = sessions.begin("conv", "account-a");
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
        let old = sessions.begin("old", "account-a");
        let receiver = sessions.cancellation("old", "account-a", old).unwrap();
        sessions.record_checkpoint("old", "account-a", old, vec![1], HashMap::new());
        let current = sessions.begin("current", "account-b");
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
        let owner = sessions.begin("conv", "account-a");
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
