// 凭证目录读写:根目录用项目 `.cache/antigravity`

use super::constants::DEFAULT_AUTH_DIR;
use super::credential::{credential_file_name, AntigravityCredential};
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

fn home_dir() -> PathBuf {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
        .unwrap_or_else(|| PathBuf::from("."))
}

/// 默认 `.cache/antigravity`(相对路径,由 CLI 钉到配置文件所在目录)
pub fn default_auth_dir() -> PathBuf {
    resolve_auth_dir(None)
}

/// 空 → `.cache/antigravity`;单独 `~` → $HOME;其余展开 `~` 或原样
pub fn resolve_auth_dir(raw: Option<&str>) -> PathBuf {
    let s = raw.map(str::trim).unwrap_or("");
    if s.is_empty() {
        return PathBuf::from(DEFAULT_AUTH_DIR);
    }
    if let Some(rest) = s.strip_prefix('~') {
        let rest = rest.trim_start_matches(['/', '\\']);
        if rest.is_empty() {
            return home_dir();
        }
        return home_dir().join(rest.replace('\\', "/"));
    }
    PathBuf::from(s)
}

pub fn load(path: &Path) -> Result<AntigravityCredential> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("read antigravity credential {}", path.display()))?;
    serde_json::from_str(&raw)
        .with_context(|| format!("parse antigravity credential {}", path.display()))
}

fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    let dir = path.parent().context("credential path missing parent")?;
    let mut nonce = [0u8; 8];
    getrandom::fill(&mut nonce).context("credential temp nonce")?;
    let tmp = dir.join(format!(
        ".antigravity.{:016x}.tmp",
        u64::from_be_bytes(nonce)
    ));
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

/// 目录 0700,文件 0600;JSON 即 metadata 本身(原子写 + 进程内 path 锁)
pub fn save(dir: &Path, cred: &AntigravityCredential) -> Result<PathBuf> {
    let path = dir.join(credential_file_name(&cred.email));
    save_to(&path, cred)?;
    Ok(path)
}

/// 写回指定凭证文件:运行时刷新落盘到扫描到的原路径,不按 email 重新推导文件名
/// (对齐 CPA FileTokenStore.resolveAuthPath 优先使用 path 属性)
pub fn save_to(path: &Path, cred: &AntigravityCredential) -> Result<()> {
    let dir = path.parent().context("credential path missing parent")?;
    fs::create_dir_all(dir).with_context(|| format!("create auth dir {}", dir.display()))?;
    #[cfg(unix)]
    {
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    let raw = serde_json::to_vec(cred).context("serialize antigravity credential")?;

    let inproc = path_lock(path);
    let _guard = inproc.lock().unwrap_or_else(|p| p.into_inner());

    write_atomic(path, &raw)?;
    #[cfg(unix)]
    {
        let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
    }
    Ok(())
}

/// 列出目录下 `antigravity*.json`(忽略坏文件和 client_id 不匹配的凭证)
pub fn list(dir: &Path) -> Result<Vec<(PathBuf, AntigravityCredential)>> {
    if !dir.exists() {
        return Ok(Vec::new());
    }
    let mut out = Vec::new();
    for entry in fs::read_dir(dir).with_context(|| format!("read auth dir {}", dir.display()))? {
        let entry = entry?;
        let name = entry.file_name();
        let name = name.to_string_lossy();
        if !name.starts_with("antigravity") || !name.ends_with(".json") {
            continue;
        }
        let path = entry.path();
        match load(&path) {
            Ok(cred) => {
                // 验证 client_id(对齐 magpie geminiOwnLogin)
                if let Err(reason) = verify_client_id(&cred) {
                    tracing::warn!("skip {} (email: {}): {reason}", path.display(), cred.email);
                    continue;
                }
                out.push((path, cred));
            }
            Err(err) => tracing::warn!("skip {}: {err}", path.display()),
        }
    }
    out.sort_by(|a, b| a.0.cmp(&b.0));
    Ok(out)
}

/// 验证凭证的 id_token client_id 是否匹配官方 CLIENT_ID
///
/// 对齐 magpie `geminiOwnLogin` 逻辑:
/// - 无 id_token 或解析失败 → 通过(旧凭证或外部工具生成)
/// - 有 client_id 且不匹配 → 拒绝(非官方 OAuth client)
fn verify_client_id(cred: &AntigravityCredential) -> Result<(), String> {
    // 从 extra 字段读取 id_token(老格式可能存在)
    let id_token = cred
        .extra
        .get("id_token")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    if id_token.trim().is_empty() {
        // 无 id_token,无法验证但不拒绝(兼容旧凭证)
        return Ok(());
    }

    let Some(client_id) = crate::jwt::extract_client_id(id_token) else {
        // JWT 解析失败,警告但不拒绝
        tracing::debug!("antigravity: id_token parse failed, skip client_id check");
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
    use crate::antigravity::AntigravityCredential;

    #[test]
    fn save_load_list_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let cred = AntigravityCredential::new(
            "tok".into(),
            "ref".into(),
            3600,
            "a@b.com".into(),
            "proj".into(),
        );
        let path = save(dir.path(), &cred).unwrap();
        assert_eq!(path.file_name().unwrap(), "antigravity-a@b.com.json");
        let loaded = load(&path).unwrap();
        assert_eq!(loaded.email, "a@b.com");
        assert_eq!(loaded.project_id, "proj");
        assert_eq!(loaded.access_token, "tok");
        let listed = list(dir.path()).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].1.email, "a@b.com");
    }

    #[test]
    fn resolve_empty_and_tilde() {
        let def = default_auth_dir();
        assert_eq!(def, PathBuf::from(".cache/antigravity"));
        assert_eq!(resolve_auth_dir(None), def);
        assert_eq!(resolve_auth_dir(Some("")), def);
        assert!(!def.to_string_lossy().contains(".cli-proxy-api"));
        // 显式 `~` 仍是 $HOME
        assert_eq!(resolve_auth_dir(Some("~")), home_dir());
        assert_eq!(resolve_auth_dir(Some("/tmp/ag")), PathBuf::from("/tmp/ag"));
    }
}
