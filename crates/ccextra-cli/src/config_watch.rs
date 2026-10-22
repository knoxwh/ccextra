//! 配置文件监视：保存 config / models.json → debounce → 复用 `handle_reload` 热换。
//! 对齐 CPA：父目录监视 + 150ms debounce + 内容 hash（吸收 secret bcrypt 回写）。

use ccextra_server::http::handlers::reload::{reload_now, ReloadResult};
use ccextra_server::http::AppState;
use hex::encode as hex_encode;
use notify::{Event, EventKind, RecommendedWatcher, RecursiveMode, Watcher};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::sync::mpsc as std_mpsc;
use std::time::Duration;
use tokio::sync::mpsc;

const DEBOUNCE: Duration = Duration::from_millis(150);

/// 对监视目标做内容指纹；缺文件用固定哨兵，避免「删掉 models.json」与空文件混淆不清时仍能感知变化。
pub(crate) fn content_fingerprint(config_path: &Path, models_path: &Path) -> String {
    let mut hasher = Sha256::new();
    hash_one(&mut hasher, b"config\0", config_path);
    hash_one(&mut hasher, b"models\0", models_path);
    hex_encode(hasher.finalize())
}

fn hash_one(hasher: &mut Sha256, tag: &[u8], path: &Path) {
    hasher.update(tag);
    match std::fs::read(path) {
        Ok(bytes) => {
            hasher.update([1u8]);
            hasher.update((bytes.len() as u64).to_le_bytes());
            hasher.update(&bytes);
        }
        Err(_) => {
            hasher.update([0u8]);
        }
    }
}

/// 事件路径是否指向我们盯的两个文件之一（按规范化绝对路径或 basename+父目录比较）。
pub(crate) fn is_watched_path(event_path: &Path, config_path: &Path, models_path: &Path) -> bool {
    path_eq(event_path, config_path) || path_eq(event_path, models_path)
}

fn path_eq(a: &Path, b: &Path) -> bool {
    if a == b {
        return true;
    }
    if let (Ok(ca), Ok(cb)) = (a.canonicalize(), b.canonicalize()) {
        if ca == cb {
            return true;
        }
    }
    match (a.file_name(), b.file_name(), a.parent(), b.parent()) {
        (Some(an), Some(bn), Some(ap), Some(bp)) if an == bn => {
            if ap == bp {
                return true;
            }
            if let (Ok(cap), Ok(cbp)) = (ap.canonicalize(), bp.canonicalize()) {
                return cap == cbp;
            }
            false
        }
        _ => false,
    }
}

pub(crate) fn is_relevant_kind(kind: &EventKind) -> bool {
    matches!(
        kind,
        EventKind::Create(_) | EventKind::Modify(_) | EventKind::Remove(_)
    )
}

/// 决定是否应对当前磁盘内容发起 reload（空 config / hash 未变则跳过）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReloadDecision {
    SkipEmpty,
    SkipUnchanged,
    Reload { new_hash: String },
}

pub(crate) fn decide_reload(
    last_hash: &str,
    config_path: &Path,
    models_path: &Path,
) -> ReloadDecision {
    match std::fs::read(config_path) {
        Ok(data) if data.is_empty() => return ReloadDecision::SkipEmpty,
        Err(_) => {
            // 读不到也走 fingerprint；handle_reload 会失败并保旧
        }
        Ok(_) => {}
    }
    let new_hash = content_fingerprint(config_path, models_path);
    if !last_hash.is_empty() && last_hash == new_hash {
        ReloadDecision::SkipUnchanged
    } else {
        ReloadDecision::Reload { new_hash }
    }
}

/// 快照是否已真正发布（与 [`reload_now`] 写锁更新一致：含 `unfit_registry`）。
/// Ok 但 providers/runtime 回滚、或 Err，均视为未发布，监视侧不得提交 `last_hash`。
pub(crate) fn reload_published(result: &ReloadResult) -> bool {
    result.reloaded.iter().any(|c| c == "unfit_registry")
}

/// 后台监视 `--config` 父目录与启动时钉死的 `models.json`；事件经 debounce 后进程内 `handle_reload`。
/// 失败只打日志，不崩进程；`POST /reload` 仍可用。
pub(crate) fn spawn_config_watcher(state: AppState, config_path: PathBuf, models_path: PathBuf) {
    tokio::spawn(async move {
        if let Err(e) = run_config_watcher(state, config_path, models_path).await {
            tracing::error!("配置文件监视任务退出: {e:#}");
        }
    });
}

async fn run_config_watcher(
    state: AppState,
    config_path: PathBuf,
    models_path: PathBuf,
) -> anyhow::Result<()> {
    let config_path = absolutize(&config_path);
    let models_path = absolutize(&models_path);

    let config_parent = config_path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("config 无父目录: {}", config_path.display()))?
        .to_path_buf();
    let models_parent = models_path
        .parent()
        .unwrap_or(config_parent.as_path())
        .to_path_buf();

    let (tx, mut rx) = mpsc::unbounded_channel::<Event>();
    // notify 回调在同步线程；桥到 tokio mpsc
    let (std_tx, std_rx) = std_mpsc::channel::<Event>();
    let mut watcher = RecommendedWatcher::new(
        move |res: Result<Event, notify::Error>| match res {
            Ok(event) => {
                let _ = std_tx.send(event);
            }
            Err(e) => tracing::warn!("配置监视 notify 错误: {e}"),
        },
        notify::Config::default(),
    )?;

    watcher.watch(&config_parent, RecursiveMode::NonRecursive)?;
    if models_parent != config_parent {
        watcher.watch(&models_parent, RecursiveMode::NonRecursive)?;
    }
    // 持有 watcher 直到任务结束
    let _watcher = watcher;

    // 桥接线程：std channel → tokio mpsc
    std::thread::Builder::new()
        .name("ccextra-config-watch".into())
        .spawn(move || {
            while let Ok(event) = std_rx.recv() {
                if tx.send(event).is_err() {
                    break;
                }
            }
        })?;

    let mut last_hash = content_fingerprint(&config_path, &models_path);
    tracing::info!(
        "监视配置变更: {} 与 {}",
        config_path.display(),
        models_path.display()
    );

    loop {
        let Some(first) = rx.recv().await else {
            break;
        };
        if !event_is_ours(&first, &config_path, &models_path) {
            // 排空无关事件时仍继续等下一条
            continue;
        }

        // debounce：窗口内合并
        tokio::time::sleep(DEBOUNCE).await;
        while let Ok(ev) = rx.try_recv() {
            let _ = ev;
        }

        match decide_reload(&last_hash, &config_path, &models_path) {
            ReloadDecision::SkipEmpty => {
                tracing::debug!("忽略空配置文件写入");
            }
            ReloadDecision::SkipUnchanged => {
                tracing::debug!("配置内容未变 (hash 命中)，跳过热重载");
            }
            ReloadDecision::Reload { .. } => {
                tracing::info!(
                    "检测到配置文件变更，开始热重载 ({}, {})",
                    config_path.display(),
                    models_path.display()
                );
                let published = match reload_now(&state).await {
                    Ok(result) => {
                        tracing::info!(
                            "文件监视热重载完成: {} 成功, {} 失败",
                            result.reloaded.len(),
                            result.failed.len()
                        );
                        for f in &result.failed {
                            tracing::warn!("热重载组件失败 {}: {}", f.component, f.reason);
                        }
                        let ok = reload_published(&result);
                        if !ok {
                            tracing::warn!("热重载未发布快照，保留旧内容 hash 以便同内容可重试");
                        }
                        ok
                    }
                    Err(e) => {
                        // 与 POST /reload 一致：失败保旧快照；不提交 hash
                        tracing::error!("文件监视热重载失败（保留旧配置）: {}", e.err);
                        false
                    }
                };
                // 仅快照真正发布成功才提交 hash；Config::load 可能 bcrypt 回写，重读吸收环
                if published {
                    last_hash = content_fingerprint(&config_path, &models_path);
                }
            }
        }
    }

    Ok(())
}

fn event_is_ours(event: &Event, config_path: &Path, models_path: &Path) -> bool {
    if !is_relevant_kind(&event.kind) {
        return false;
    }
    event
        .paths
        .iter()
        .any(|p| is_watched_path(p, config_path, models_path))
}

fn absolutize(path: &Path) -> PathBuf {
    if path.is_absolute() {
        return path.to_path_buf();
    }
    std::env::current_dir()
        .map(|cwd| cwd.join(path))
        .unwrap_or_else(|_| path.to_path_buf())
}

#[cfg(test)]
mod tests {
    use super::*;
    use ccextra_server::http::handlers::reload::ReloadError;
    use std::fs;
    use tempfile::tempdir;

    #[test]
    fn fingerprint_changes_when_config_changes() {
        let dir = tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        let models = dir.path().join("models.json");
        fs::write(&cfg, "a: 1\n").unwrap();
        fs::write(&models, "[]\n").unwrap();
        let h1 = content_fingerprint(&cfg, &models);
        fs::write(&cfg, "a: 2\n").unwrap();
        let h2 = content_fingerprint(&cfg, &models);
        assert_ne!(h1, h2);
        let h3 = content_fingerprint(&cfg, &models);
        assert_eq!(h2, h3);
    }

    #[test]
    fn fingerprint_changes_when_models_changes() {
        let dir = tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        let models = dir.path().join("models.json");
        fs::write(&cfg, "a: 1\n").unwrap();
        fs::write(&models, "[]\n").unwrap();
        let h1 = content_fingerprint(&cfg, &models);
        fs::write(&models, "[{\"id\":\"x\"}]\n").unwrap();
        assert_ne!(h1, content_fingerprint(&cfg, &models));
    }

    #[test]
    fn fingerprint_missing_models_stable_until_created() {
        let dir = tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        let models = dir.path().join("models.json");
        fs::write(&cfg, "a: 1\n").unwrap();
        let h1 = content_fingerprint(&cfg, &models);
        let h2 = content_fingerprint(&cfg, &models);
        assert_eq!(h1, h2);
        fs::write(&models, "[]\n").unwrap();
        assert_ne!(h1, content_fingerprint(&cfg, &models));
    }

    #[test]
    fn decide_reload_skips_unchanged_and_empty() {
        let dir = tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        let models = dir.path().join("models.json");
        fs::write(&cfg, "host: x\n").unwrap();
        fs::write(&models, "[]\n").unwrap();
        let hash = content_fingerprint(&cfg, &models);
        assert_eq!(
            decide_reload(&hash, &cfg, &models),
            ReloadDecision::SkipUnchanged
        );
        fs::write(&cfg, "").unwrap();
        assert_eq!(
            decide_reload(&hash, &cfg, &models),
            ReloadDecision::SkipEmpty
        );
        fs::write(&cfg, "host: y\n").unwrap();
        match decide_reload(&hash, &cfg, &models) {
            ReloadDecision::Reload { new_hash } => {
                assert_ne!(new_hash, hash);
            }
            other => panic!("expected Reload, got {other:?}"),
        }
    }

    #[test]
    fn decide_reload_first_time_with_empty_last_hash() {
        let dir = tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        let models = dir.path().join("models.json");
        fs::write(&cfg, "host: x\n").unwrap();
        match decide_reload("", &cfg, &models) {
            ReloadDecision::Reload { .. } => {}
            other => panic!("expected Reload on empty last_hash, got {other:?}"),
        }
    }

    #[test]
    fn is_watched_path_matches_basename_in_same_dir() {
        let dir = tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        let models = dir.path().join("models.json");
        fs::write(
            &cfg, "x
",
        )
        .unwrap();
        assert!(is_watched_path(&cfg, &cfg, &models));
        assert!(is_watched_path(
            &dir.path().join("config.yaml"),
            &cfg,
            &models
        ));
        assert!(!is_watched_path(
            &dir.path().join("other.yaml"),
            &cfg,
            &models
        ));
    }

    #[test]
    fn is_relevant_kind_filters() {
        assert!(is_relevant_kind(&EventKind::Modify(
            notify::event::ModifyKind::Data(notify::event::DataChange::Any)
        )));
        assert!(is_relevant_kind(&EventKind::Create(
            notify::event::CreateKind::File
        )));
        assert!(is_relevant_kind(&EventKind::Remove(
            notify::event::RemoveKind::File
        )));
        assert!(!is_relevant_kind(&EventKind::Access(
            notify::event::AccessKind::Any
        )));
    }

    /// secret 回写后指纹变化，成功发布后 commit 再 decide 应 Skip（吸收 bcrypt 环）
    #[test]
    fn hash_commit_absorbs_rewrite() {
        let dir = tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        let models = dir.path().join("models.json");
        fs::write(&cfg, "secret_key: plain\n").unwrap();
        fs::write(&models, "[]\n").unwrap();
        let mut last = content_fingerprint(&cfg, &models);
        // 模拟 Config::load 回写
        fs::write(&cfg, "secret_key: \"$2b$12$abcdef\"\n").unwrap();
        match decide_reload(&last, &cfg, &models) {
            ReloadDecision::Reload { .. } => {}
            other => panic!("{other:?}"),
        }
        // 发布成功后重读指纹并提交
        assert!(reload_published(&published_result()));
        last = content_fingerprint(&cfg, &models);
        assert_eq!(
            decide_reload(&last, &cfg, &models),
            ReloadDecision::SkipUnchanged
        );
    }

    fn published_result() -> ReloadResult {
        ReloadResult {
            reloaded: vec![
                "providers".into(),
                "runtime".into(),
                "unfit_registry".into(),
            ],
            failed: vec![],
        }
    }

    fn unpublished_providers_failed() -> ReloadResult {
        ReloadResult {
            reloaded: vec!["runtime".into()],
            failed: vec![ReloadError {
                component: "providers".into(),
                reason: "校验失败".into(),
            }],
        }
    }

    #[test]
    fn reload_published_requires_unfit_registry() {
        assert!(reload_published(&published_result()));
        assert!(!reload_published(&unpublished_providers_failed()));
        assert!(!reload_published(&ReloadResult::default()));
    }

    /// 未发布成功不得提交 last_hash：同内容指纹仍应 decide Reload。
    #[test]
    fn failed_reload_does_not_commit_last_hash() {
        let dir = tempdir().unwrap();
        let cfg = dir.path().join("config.yaml");
        let models = dir.path().join("models.json");
        fs::write(&cfg, "host: good\n").unwrap();
        fs::write(&models, "[]\n").unwrap();
        let mut last = content_fingerprint(&cfg, &models);

        fs::write(&cfg, "host: bad\n").unwrap();
        let bad_fp = content_fingerprint(&cfg, &models);
        match decide_reload(&last, &cfg, &models) {
            ReloadDecision::Reload { new_hash } => assert_eq!(new_hash, bad_fp),
            other => panic!("expected Reload, got {other:?}"),
        }

        // 模拟 Ok-but-unpublished：不提交 hash
        assert!(!reload_published(&unpublished_providers_failed()));
        // last 仍为旧值 → 同内容仍 Reload
        match decide_reload(&last, &cfg, &models) {
            ReloadDecision::Reload { new_hash } => assert_eq!(new_hash, bad_fp),
            other => panic!("expected Retry Reload after unpublished, got {other:?}"),
        }

        // 若误提交失败指纹，则会 Skip — 对照钉死
        let wrongly_committed = bad_fp.clone();
        assert_eq!(
            decide_reload(&wrongly_committed, &cfg, &models),
            ReloadDecision::SkipUnchanged
        );

        // 发布成功后才提交（含回写吸收）
        assert!(reload_published(&published_result()));
        last = content_fingerprint(&cfg, &models);
        assert_eq!(
            decide_reload(&last, &cfg, &models),
            ReloadDecision::SkipUnchanged
        );
    }
}
