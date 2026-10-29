// Claude 凭证持久化 (对齐 Codex 原子写 + CPA 文件锁精神)

use super::credential::{credential_file_name, ClaudeCredential};
use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

use crate::oauth_common;

const DEFAULT_AUTH_DIR: &str = ".cache/claude";

pub fn default_auth_dir() -> PathBuf {
    PathBuf::from(DEFAULT_AUTH_DIR)
}

pub fn resolve_auth_dir(dir: Option<&str>) -> PathBuf {
    oauth_common::resolve_auth_dir(dir, DEFAULT_AUTH_DIR)
}

pub fn save(auth_dir: &Path, cred: &ClaudeCredential) -> Result<PathBuf> {
    let path = auth_dir.join(credential_file_name(&cred.email, &cred.account_uuid));
    save_to(&path, cred)?;
    Ok(path)
}

/// 写回指定凭证文件:运行时刷新落盘到扫描到的原路径,不按 email 重新推导文件名
/// (对齐 CPA FileTokenStore.resolveAuthPath 优先使用 path 属性)
pub fn save_to(path: &Path, cred: &ClaudeCredential) -> Result<()> {
    let json = serde_json::to_vec_pretty(cred).context("序列化 Claude 凭证失败")?;
    oauth_common::write_credential_file(path, &json, "claude")
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
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

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
