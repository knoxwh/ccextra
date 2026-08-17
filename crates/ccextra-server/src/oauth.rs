// OAuth 凭证管理

use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OAuthCredential {
    pub access_token: String,
    pub refresh_token: Option<String>,
    pub expires_at: Option<i64>,
}

/// OAuth 凭证存储
pub struct OAuthStore {
    auth_dir: PathBuf,
}

impl OAuthStore {
    /// 创建新的 OAuth 存储
    pub fn new(auth_dir: impl AsRef<Path>) -> Result<Self> {
        let auth_dir = auth_dir.as_ref().to_path_buf();
        fs::create_dir_all(&auth_dir)
            .with_context(|| format!("创建 auth_dir 失败: {:?}", auth_dir))?;
        Ok(Self { auth_dir })
    }

    /// 获取凭证文件路径
    fn credential_path(&self, provider: &str) -> PathBuf {
        self.auth_dir.join(format!("{}.json", provider))
    }

    /// 加载凭证
    pub fn load(&self, provider: &str) -> Result<OAuthCredential> {
        let path = self.credential_path(provider);
        let content =
            fs::read_to_string(&path).with_context(|| format!("读取凭证文件失败: {:?}", path))?;
        let cred: OAuthCredential = serde_json::from_str(&content)
            .with_context(|| format!("解析凭证文件失败: {:?}", path))?;
        Ok(cred)
    }

    /// 保存凭证
    pub fn save(&self, provider: &str, credential: &OAuthCredential) -> Result<()> {
        let path = self.credential_path(provider);
        let content = serde_json::to_string_pretty(credential).context("序列化凭证失败")?;
        fs::write(&path, content).with_context(|| format!("写入凭证文件失败: {:?}", path))?;
        Ok(())
    }

    /// 检查凭证是否存在
    pub fn exists(&self, provider: &str) -> bool {
        self.credential_path(provider).exists()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn test_oauth_store_save_and_load() {
        let temp = TempDir::new().unwrap();
        let store = OAuthStore::new(temp.path()).unwrap();

        let cred = OAuthCredential {
            access_token: "test-token".to_string(),
            refresh_token: Some("refresh-token".to_string()),
            expires_at: Some(1234567890),
        };

        store.save("test-provider", &cred).unwrap();
        assert!(store.exists("test-provider"));

        let loaded = store.load("test-provider").unwrap();
        assert_eq!(loaded.access_token, "test-token");
        assert_eq!(loaded.refresh_token, Some("refresh-token".to_string()));
        assert_eq!(loaded.expires_at, Some(1234567890));
    }

    #[test]
    fn test_oauth_store_not_exists() {
        let temp = TempDir::new().unwrap();
        let store = OAuthStore::new(temp.path()).unwrap();

        assert!(!store.exists("nonexistent"));
    }

    #[test]
    fn test_oauth_store_load_missing() {
        let temp = TempDir::new().unwrap();
        let store = OAuthStore::new(temp.path()).unwrap();

        let result = store.load("missing");
        assert!(result.is_err());
    }
}
