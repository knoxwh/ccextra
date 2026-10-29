// Claude 凭证结构 (对齐 CLIProxyAPI ClaudeTokenStorage)

use crate::oauth_common::sanitize_file_segment;
use serde::{Deserialize, Serialize};
use std::time::{Duration, SystemTime};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ClaudeCredential {
    #[serde(default = "default_type")]
    pub r#type: String,
    #[serde(default)]
    pub id_token: String,
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: String,
    #[serde(default)]
    pub last_refresh: String,
    #[serde(default)]
    pub email: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub account_uuid: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub organization_uuid: String,
    #[serde(default, skip_serializing_if = "String::is_empty")]
    pub organization_name: String,
    /// 单机固定 1 个 device id (对齐 CPA claude_device_ids 池大小 1)
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub claude_device_ids: Vec<String>,
    /// RFC3339 过期时间
    #[serde(default)]
    pub expired: String,
    #[serde(default)]
    pub disabled: bool,
}

fn default_type() -> String {
    "claude".to_string()
}

/// 是否 Anthropic 订阅 OAuth access token
pub fn is_claude_oauth_token(key: &str) -> bool {
    key.contains("sk-ant-oat")
}

impl ClaudeCredential {
    /// 检查 token 是否有效 (now + skew_secs < expire_time)
    pub fn is_fresh(&self, now: SystemTime, skew_secs: i64) -> bool {
        if self.access_token.trim().is_empty() {
            return false;
        }
        // 空/坏 expired 视为不新鲜,强制走刷新(对齐 CPA / Codex)
        if self.expired.trim().is_empty() {
            return false;
        }
        let exp_time = match OffsetDateTime::parse(self.expired.trim(), &Rfc3339) {
            Ok(dt) => SystemTime::from(dt),
            Err(_) => return false,
        };
        let skew = Duration::from_secs(skew_secs.max(0) as u64);
        match now.checked_add(skew) {
            Some(deadline) => exp_time > deadline,
            None => false,
        }
    }

    /// 应用刷新 / 换码返回的 token
    pub fn apply_tokens(
        &mut self,
        access_token: String,
        refresh_token: Option<String>,
        expires_in: i64,
    ) {
        self.access_token = access_token;
        if let Some(r) = refresh_token {
            if !r.trim().is_empty() {
                self.refresh_token = r;
            }
        }
        let now = OffsetDateTime::now_utc();
        self.last_refresh = now.format(&Rfc3339).unwrap_or_default();
        let exp = now + time::Duration::seconds(expires_in.max(60));
        self.expired = exp.format(&Rfc3339).unwrap_or_default();
    }
}

/// 根据 email 或 account_uuid 生成文件名
pub fn credential_file_name(email: &str, account_uuid: &str) -> String {
    let seg_email = sanitize_file_segment(email);
    if !seg_email.is_empty() {
        return format!("claude-{seg_email}.json");
    }
    let seg_account = sanitize_file_segment(account_uuid);
    if !seg_account.is_empty() {
        return format!("claude-{seg_account}.json");
    }
    let ms = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("claude-{ms}.json")
}

/// 生成单个 64 hex device id (32 随机字节)
pub fn generate_device_id() -> anyhow::Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::fill(&mut bytes).map_err(|e| anyhow::anyhow!("generate device id: {e}"))?;
    Ok(hex::encode(bytes))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_file_name_sanitization() {
        assert_eq!(
            credential_file_name("user@example.com", ""),
            "claude-user@example.com.json"
        );
        assert_eq!(credential_file_name("", "acct-123"), "claude-acct-123.json");
    }

    #[test]
    fn test_is_claude_oauth_token() {
        assert!(is_claude_oauth_token("sk-ant-oat01-abc"));
        assert!(is_claude_oauth_token("Bearer sk-ant-oat-x"));
        assert!(!is_claude_oauth_token("sk-ant-api03-abc"));
        assert!(!is_claude_oauth_token("sk-ccextra-xxx"));
    }

    #[test]
    fn test_generate_device_id_hex64() {
        let id = generate_device_id().unwrap();
        assert_eq!(id.len(), 64);
        assert!(id.chars().all(|c| c.is_ascii_hexdigit()));
    }

    #[test]
    fn is_fresh_rejects_empty_or_bad_expired() {
        let mut cred = ClaudeCredential {
            r#type: "claude".into(),
            id_token: String::new(),
            access_token: "sk-ant-oat-tok".into(),
            refresh_token: "r".into(),
            last_refresh: String::new(),
            email: "u@e.com".into(),
            account_uuid: "a".into(),
            organization_uuid: String::new(),
            organization_name: String::new(),
            claude_device_ids: vec![],
            expired: String::new(),
            disabled: false,
        };
        let now = SystemTime::now();
        assert!(!cred.is_fresh(now, 0));
        cred.expired = "not-rfc3339".into();
        assert!(!cred.is_fresh(now, 0));
        cred.expired = "2999-01-01T00:00:00Z".into();
        assert!(cred.is_fresh(now, 0));
    }
}
