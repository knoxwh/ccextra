// xAI 凭证持久化 (对齐 codex/store 原子写 + path 锁)

use super::credential::{credential_file_name, XAICredential};
use anyhow::{Context, Result};
use std::collections::HashMap;
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock};

#[cfg(unix)]
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

static PATH_LOCKS: OnceLock<Mutex<HashMap<PathBuf, Arc<Mutex<()>>>>> = OnceLock::new();

fn path_lock(path: &Path) -> Arc<Mutex<()>> {
    let key = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
    let mut map = PATH_LOCKS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .unwrap_or_else(|p| p.into_inner());
    map.entry(key)
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

pub fn default_auth_dir() -> PathBuf {
    PathBuf::from(".cache/xai")
}

pub fn resolve_auth_dir(dir: Option<&str>) -> PathBuf {
    match dir {
        Some(d) if !d.trim().is_empty() => {
            if let Some(rest) = d.strip_prefix("~/") {
                if let Some(home) =
                    dirs_next().or_else(|| std::env::var_os("HOME").map(PathBuf::from))
                {
                    return home.join(rest);
                }
            }
            PathBuf::from(d)
        }
        _ => default_auth_dir(),
    }
}

fn dirs_next() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

pub fn credential_path(auth_dir: &Path, email_or_sub: &str) -> PathBuf {
    auth_dir.join(credential_file_name(email_or_sub, ""))
}

fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let dir = path.parent().context("credential path missing parent")?;
    let mut nonce = [0u8; 8];
    getrandom::fill(&mut nonce).context("credential temp nonce")?;
    let tmp = dir.join(format!(".xai.{:016x}.tmp", u64::from_be_bytes(nonce)));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    options.mode(0o600);
    let mut file = options
        .open(&tmp)
        .with_context(|| format!("create temp credential {}", tmp.display()))?;
    let result = (|| -> Result<()> {
        file.write_all(data)?;
        file.sync_all()?;
        fs::rename(&tmp, path).with_context(|| format!("replace credential {}", path.display()))?;
        Ok(())
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

pub fn save(auth_dir: &Path, cred: &XAICredential) -> Result<PathBuf> {
    fs::create_dir_all(auth_dir)
        .with_context(|| format!("创建凭证目录失败: {}", auth_dir.display()))?;
    #[cfg(unix)]
    {
        fs::set_permissions(auth_dir, fs::Permissions::from_mode(0o700))?;
    }
    let file_name = credential_file_name(&cred.email, &cred.sub);
    let path = auth_dir.join(file_name);
    let json = serde_json::to_vec_pretty(cred).context("序列化 xAI 凭证失败")?;

    let inproc = path_lock(&path);
    let _guard = inproc.lock().unwrap_or_else(|p| p.into_inner());

    let lock_path = PathBuf::from(format!("{}.lock", path.display()));
    let mut file_lock = fslock::LockFile::open(&lock_path)
        .with_context(|| format!("open credential lock {}", lock_path.display()))?;
    file_lock
        .lock()
        .with_context(|| format!("acquire credential lock {}", lock_path.display()))?;

    write_atomic(&path, &json)?;
    #[cfg(unix)]
    {
        let _ = fs::set_permissions(&path, fs::Permissions::from_mode(0o600));
        let _ = fs::set_permissions(&lock_path, fs::Permissions::from_mode(0o600));
    }
    let _ = file_lock.unlock();
    Ok(path)
}

pub fn load(path: &Path) -> Result<XAICredential> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("读取凭证文件失败: {}", path.display()))?;
    let cred: XAICredential = serde_json::from_str(&content)
        .with_context(|| format!("解析凭证文件失败: {}", path.display()))?;
    Ok(cred)
}

pub fn list(auth_dir: &Path) -> Result<Vec<(PathBuf, XAICredential)>> {
    if !auth_dir.exists() {
        return Ok(Vec::new());
    }
    let mut entries = Vec::new();
    for entry in fs::read_dir(auth_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_file() && path.extension().is_some_and(|ext| ext == "json") {
            if let Ok(cred) = load(&path) {
                if cred.r#type == "xai" || cred.r#type.is_empty() {
                    if let Err(reason) = verify_client_id(&cred) {
                        tracing::warn!("skip {} (email: {}): {reason}", path.display(), cred.email);
                        continue;
                    }
                    entries.push((path, cred));
                }
            }
        }
    }
    Ok(entries)
}

/// 验证 id_token 的 client_id 是否匹配官方 CLIENT_ID
fn verify_client_id(cred: &XAICredential) -> Result<(), String> {
    let id_token = cred.id_token.trim();
    if id_token.is_empty() {
        return Ok(());
    }

    let Some(client_id) = crate::jwt::extract_client_id(id_token) else {
        tracing::debug!("xai: id_token parse failed, skip client_id check");
        return Ok(());
    };

    if client_id != super::constants::CLIENT_ID {
        return Err(format!(
            "client_id mismatch: got {}, expected {}",
            client_id,
            super::constants::CLIENT_ID
        ));
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_save_and_load_roundtrip() {
        let dir = tempdir().unwrap();
        let mut cred = XAICredential {
            r#type: "xai".to_string(),
            auth_kind: "oauth".to_string(),
            access_token: "test_access".to_string(),
            refresh_token: "test_refresh".to_string(),
            id_token: "".to_string(),
            token_type: "Bearer".to_string(),
            expires_in: 3600,
            expired: "2026-01-01T00:00:00Z".to_string(),
            last_refresh: "2025-01-01T00:00:00Z".to_string(),
            email: "user@x.ai".to_string(),
            sub: "sub_123".to_string(),
            base_url: "https://cli-chat-proxy.grok.com/v1".to_string(),
            token_endpoint: "https://auth.x.ai/token".to_string(),
            disabled: false,
        };

        let path = save(dir.path(), &cred).unwrap();
        assert_eq!(path.file_name().unwrap(), "xai-user@x.ai.json");

        let loaded = load(&path).unwrap();
        assert_eq!(loaded.access_token, "test_access");
        assert_eq!(loaded.email, "user@x.ai");

        let list_res = list(dir.path()).unwrap();
        assert_eq!(list_res.len(), 1);

        cred.email = "".to_string();
        cred.sub = "sub_only".to_string();
        let path2 = save(dir.path(), &cred).unwrap();
        assert_eq!(path2.file_name().unwrap(), "xai-sub_only.json");
        let list_res2 = list(dir.path()).unwrap();
        assert_eq!(list_res2.len(), 2);
    }
}
