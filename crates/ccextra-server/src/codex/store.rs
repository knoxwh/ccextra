// Codex 凭证持久化 (对齐项目 xai/store.rs 形状)

use super::credential::{credential_file_name, CodexCredential};
use anyhow::{Context, Result};
use std::path::{Path, PathBuf};

pub fn default_auth_dir() -> PathBuf {
    PathBuf::from(".cache/codex")
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

pub fn save(auth_dir: &Path, cred: &CodexCredential) -> Result<PathBuf> {
    std::fs::create_dir_all(auth_dir)
        .with_context(|| format!("创建凭证目录失败: {}", auth_dir.display()))?;
    let file_name = credential_file_name(&cred.email, &cred.account_id);
    let path = auth_dir.join(file_name);
    let json = serde_json::to_string_pretty(cred).with_context(|| "序列化 Codex 凭证失败")?;
    std::fs::write(&path, json).with_context(|| format!("写入凭证文件失败: {}", path.display()))?;
    Ok(path)
}

pub fn load(path: &Path) -> Result<CodexCredential> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("读取凭证文件失败: {}", path.display()))?;
    let cred: CodexCredential = serde_json::from_str(&content)
        .with_context(|| format!("解析凭证文件失败: {}", path.display()))?;
    Ok(cred)
}

pub fn list(auth_dir: &Path) -> Result<Vec<(PathBuf, CodexCredential)>> {
    if !auth_dir.exists() {
        return Ok(Vec::new());
    }
    let mut entries = Vec::new();
    for entry in std::fs::read_dir(auth_dir)? {
        let entry = entry?;
        let path = entry.path();
        if path.is_file() && path.extension().is_some_and(|ext| ext == "json") {
            if let Ok(cred) = load(&path) {
                if cred.r#type == "codex" || cred.r#type.is_empty() {
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
        let cred = CodexCredential {
            r#type: "codex".to_string(),
            id_token: String::new(),
            access_token: "test_access".to_string(),
            refresh_token: "test_refresh".to_string(),
            account_id: "acct-123".to_string(),
            last_refresh: String::new(),
            email: "user@example.com".to_string(),
            plan_type: "pro".to_string(),
            expired: "2026-01-01T00:00:00Z".to_string(),
            disabled: false,
        };

        let path = save(dir.path(), &cred).unwrap();
        assert_eq!(path.file_name().unwrap(), "codex-user@example.com.json");

        let loaded = load(&path).unwrap();
        assert_eq!(loaded.access_token, "test_access");
        assert_eq!(loaded.account_id, "acct-123");

        let list_res = list(dir.path()).unwrap();
        assert_eq!(list_res.len(), 1);
    }
}
