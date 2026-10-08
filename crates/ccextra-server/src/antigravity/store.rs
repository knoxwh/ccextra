// 凭证目录读写:根目录用项目 `.cache/antigravity`

use super::constants::DEFAULT_AUTH_DIR;
use super::credential::{credential_file_name, AntigravityCredential};
use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

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

pub fn credential_path(dir: &Path, email: &str) -> PathBuf {
    dir.join(credential_file_name(email))
}

pub fn load(path: &Path) -> Result<AntigravityCredential> {
    let raw = fs::read_to_string(path)
        .with_context(|| format!("read antigravity credential {}", path.display()))?;
    serde_json::from_str(&raw)
        .with_context(|| format!("parse antigravity credential {}", path.display()))
}

/// 目录 0700,文件 0600;JSON 即 metadata 本身
pub fn save(dir: &Path, cred: &AntigravityCredential) -> Result<PathBuf> {
    fs::create_dir_all(dir).with_context(|| format!("create auth dir {}", dir.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(dir, fs::Permissions::from_mode(0o700))?;
    }
    let path = credential_path(dir, &cred.email);
    let raw = serde_json::to_vec(cred).context("serialize antigravity credential")?;
    fs::write(&path, raw)
        .with_context(|| format!("write antigravity credential {}", path.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    }
    Ok(path)
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
                    tracing::warn!(
                        "skip {} (email: {}): {reason}",
                        path.display(),
                        cred.email
                    );
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
