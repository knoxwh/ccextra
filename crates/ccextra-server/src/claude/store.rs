// Claude 凭证持久化 (对齐项目 codex/store.rs 形状)

use super::credential::{credential_file_name, ClaudeCredential};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

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

pub fn credential_path(auth_dir: &Path, email_or_account: &str) -> PathBuf {
    auth_dir.join(credential_file_name(email_or_account, ""))
}

pub fn save(auth_dir: &Path, cred: &ClaudeCredential) -> Result<PathBuf> {
    std::fs::create_dir_all(auth_dir)
        .with_context(|| format!("创建凭证目录失败: {}", auth_dir.display()))?;
    let file_name = credential_file_name(&cred.email, &cred.account_uuid);
    let path = auth_dir.join(file_name);
    let json = serde_json::to_string_pretty(cred).with_context(|| "序列化 Claude 凭证失败")?;
    std::fs::write(&path, json).with_context(|| format!("写入凭证文件失败: {}", path.display()))?;
    Ok(path)
}

pub fn load(path: &Path) -> Result<ClaudeCredential> {
    let content = std::fs::read_to_string(path)
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
    for entry in std::fs::read_dir(auth_dir)? {
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
    }
}
