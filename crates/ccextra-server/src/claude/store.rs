// Claude 凭证持久化 (对齐 Codex 原子写 + CPA 文件锁精神)

use super::credential::{credential_file_name, ClaudeCredential};
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
    PathBuf::from(".cache/claude")
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

fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let dir = path.parent().context("credential path missing parent")?;
    let mut nonce = [0u8; 8];
    getrandom::fill(&mut nonce).context("credential temp nonce")?;
    let tmp = dir.join(format!(".claude.{:016x}.tmp", u64::from_be_bytes(nonce)));
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

pub fn save(auth_dir: &Path, cred: &ClaudeCredential) -> Result<PathBuf> {
    let path = auth_dir.join(credential_file_name(&cred.email, &cred.account_uuid));
    save_to(&path, cred)?;
    Ok(path)
}

/// 写回指定凭证文件:运行时刷新落盘到扫描到的原路径,不按 email 重新推导文件名
/// (对齐 CPA FileTokenStore.resolveAuthPath 优先使用 path 属性)
pub fn save_to(path: &Path, cred: &ClaudeCredential) -> Result<()> {
    let auth_dir = path.parent().context("credential path missing parent")?;
    fs::create_dir_all(auth_dir)
        .with_context(|| format!("创建凭证目录失败: {}", auth_dir.display()))?;
    #[cfg(unix)]
    {
        fs::set_permissions(auth_dir, fs::Permissions::from_mode(0o700))?;
    }
    let json = serde_json::to_vec_pretty(cred).context("序列化 Claude 凭证失败")?;

    let inproc = path_lock(path);
    let _guard = inproc.lock().unwrap_or_else(|p| p.into_inner());

    let lock_path = PathBuf::from(format!("{}.lock", path.display()));
    let mut file_lock = fslock::LockFile::open(&lock_path)
        .with_context(|| format!("open credential lock {}", lock_path.display()))?;
    file_lock
        .lock()
        .with_context(|| format!("acquire credential lock {}", lock_path.display()))?;

    write_atomic(path, &json)?;
    #[cfg(unix)]
    {
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
        let _ = fs::set_permissions(&lock_path, fs::Permissions::from_mode(0o600));
    }
    let _ = file_lock.unlock();
    Ok(())
}

pub fn load(path: &Path) -> Result<ClaudeCredential> {
    let content = fs::read_to_string(path)
        .with_context(|| format!("读取凭证文件失败: {}", path.display()))?;
    let cred: ClaudeCredential = serde_json::from_str(&content)
        .with_context(|| format!("解析凭证文件失败: {}", path.display()))?;
    Ok(cred)
}

pub fn list(auth_dir: &Path) -> Result<Vec<(PathBuf, ClaudeCredential)>> {
    if !auth_dir.exists() {
        return Ok(Vec::new());
    }
    let mut entries = Vec::new();
    for entry in fs::read_dir(auth_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_file() && path.extension().is_some_and(|ext| ext == "json") {
            if let Ok(cred) = load(&path) {
                if cred.r#type == "claude" || cred.r#type.is_empty() {
                    entries.push((path, cred));
                }
            }
        }
    }
    Ok(entries)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_save_and_load_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let cred = ClaudeCredential {
            r#type: "claude".to_string(),
            id_token: String::new(),
            access_token: "sk-ant-oat-test".to_string(),
            refresh_token: "test_refresh".to_string(),
            last_refresh: String::new(),
            email: "user@example.com".to_string(),
            account_uuid: "acct-123".to_string(),
            organization_uuid: "org-1".to_string(),
            organization_name: "Org".to_string(),
            claude_device_ids: vec!["abcd".repeat(16)],
            expired: "2026-01-01T00:00:00Z".to_string(),
            disabled: false,
        };

        let path = save(dir.path(), &cred).unwrap();
        assert_eq!(path.file_name().unwrap(), "claude-user@example.com.json");

        let loaded = load(&path).unwrap();
        assert_eq!(loaded.access_token, "sk-ant-oat-test");
        assert_eq!(loaded.account_uuid, "acct-123");
        assert_eq!(loaded.claude_device_ids.len(), 1);

        let list_res = list(dir.path()).unwrap();
        assert_eq!(list_res.len(), 1);

        #[cfg(unix)]
        {
            assert_eq!(
                fs::metadata(dir.path()).unwrap().permissions().mode() & 0o777,
                0o700
            );
            assert_eq!(
                fs::metadata(&path).unwrap().permissions().mode() & 0o777,
                0o600
            );
        }
    }
}
