// xAI 凭证持久化 (对齐 codex/store 原子写 + path 锁)

use super::credential::{credential_file_name, XAICredential};
use anyhow::{Context, Result};
use std::fs;
use std::path::{Path, PathBuf};

use crate::oauth_common;

const DEFAULT_AUTH_DIR: &str = ".cache/xai";

pub fn default_auth_dir() -> PathBuf {
    PathBuf::from(DEFAULT_AUTH_DIR)
}

pub fn resolve_auth_dir(dir: Option<&str>) -> PathBuf {
    oauth_common::resolve_auth_dir(dir, DEFAULT_AUTH_DIR)
}

pub fn save(auth_dir: &Path, cred: &XAICredential) -> Result<PathBuf> {
    let path = auth_dir.join(credential_file_name(&cred.email, &cred.sub));
    save_to(&path, cred)?;
    Ok(path)
}

/// 写回指定凭证文件:运行时刷新落盘到扫描到的原路径,不按 email 重新推导文件名
/// (对齐 CPA FileTokenStore.resolveAuthPath 优先使用 path 属性)
pub fn save_to(path: &Path, cred: &XAICredential) -> Result<()> {
    let json = serde_json::to_vec_pretty(cred).context("序列化 xAI 凭证失败")?;
    oauth_common::write_credential_file(path, &json, "xai")
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
