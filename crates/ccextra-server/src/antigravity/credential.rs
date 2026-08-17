// 凭证 JSON:`antigravity-<email>.json`

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AntigravityCredential {
    #[serde(rename = "type", default = "default_type")]
    pub kind: String,
    #[serde(default)]
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: String,
    #[serde(default)]
    pub expires_in: i64,
    /// 写入时的 epoch 毫秒
    #[serde(default)]
    pub timestamp: i64,
    /// RFC3339 过期时刻
    #[serde(default)]
    pub expired: String,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub project_id: String,
    #[serde(default)]
    pub disabled: bool,
    /// 保留未知额外字段(如积分),刷新时不丢
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

fn default_type() -> String {
    "antigravity".into()
}

/// `antigravity-<email>.json`;无邮箱则 `antigravity.json`
pub fn credential_file_name(email: &str) -> String {
    let email = email.trim();
    if email.is_empty() {
        "antigravity.json".into()
    } else {
        format!("antigravity-{email}.json")
    }
}

impl AntigravityCredential {
    pub fn new(
        access_token: String,
        refresh_token: String,
        expires_in: i64,
        email: String,
        project_id: String,
    ) -> Self {
        let now = SystemTime::now();
        let timestamp = now
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        let expired = rfc3339_from_now(now, expires_in);
        Self {
            kind: "antigravity".into(),
            access_token,
            refresh_token,
            expires_in,
            timestamp,
            expired,
            email,
            project_id,
            disabled: false,
            extra: Map::new(),
        }
    }

    /// 刷新后回写 token 字段,保留 email/project/extra
    pub fn apply_tokens(&mut self, access: String, refresh: Option<String>, expires_in: i64) {
        let now = SystemTime::now();
        self.access_token = access;
        if let Some(token) = refresh {
            if !token.trim().is_empty() {
                self.refresh_token = token;
            }
        }
        self.expires_in = expires_in;
        self.timestamp = now
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_millis() as i64)
            .unwrap_or(0);
        self.expired = rfc3339_from_now(now, expires_in);
        self.kind = "antigravity".into();
    }

    pub fn expiry(&self) -> Option<SystemTime> {
        if let Some(t) = parse_rfc3339(&self.expired) {
            return Some(t);
        }
        if self.timestamp > 0 && self.expires_in > 0 {
            let base = UNIX_EPOCH.checked_add(Duration::from_millis(self.timestamp as u64))?;
            return base.checked_add(Duration::from_secs(self.expires_in as u64));
        }
        None
    }

    /// token 非空且过期时刻晚于 now+skew 才算新鲜(对齐 refreshSkew=3000s)
    pub fn is_fresh(&self, now: SystemTime, skew_secs: i64) -> bool {
        if self.access_token.trim().is_empty() {
            return false;
        }
        let Some(exp) = self.expiry() else {
            return false;
        };
        let skew = Duration::from_secs(skew_secs.max(0) as u64);
        match now.checked_add(skew) {
            Some(deadline) => exp > deadline,
            None => false,
        }
    }
}

fn rfc3339_from_now(now: SystemTime, expires_in: i64) -> String {
    let odt = OffsetDateTime::from(now) + time::Duration::seconds(expires_in);
    odt.format(&Rfc3339).unwrap_or_default()
}

fn parse_rfc3339(s: &str) -> Option<SystemTime> {
    let s = s.trim();
    if s.is_empty() {
        return None;
    }
    OffsetDateTime::parse(s, &Rfc3339)
        .ok()
        .map(SystemTime::from)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn file_name_empty_and_email() {
        assert_eq!(credential_file_name(""), "antigravity.json");
        assert_eq!(credential_file_name("  "), "antigravity.json");
        assert_eq!(credential_file_name("a@b.com"), "antigravity-a@b.com.json");
    }

    #[test]
    fn parse_cliproxy_fixture_keeps_extra() {
        let raw = r#"{
            "type":"antigravity",
            "access_token":"tok",
            "refresh_token":"ref",
            "expires_in":3600,
            "timestamp":1700000000000,
            "expired":"2023-11-14T22:13:20Z",
            "email":"a@b.com",
            "project_id":"cogent-snow-4mnnp",
            "disabled":false,
            "credits":12
        }"#;
        let cred: AntigravityCredential = serde_json::from_str(raw).unwrap();
        assert_eq!(cred.kind, "antigravity");
        assert_eq!(cred.email, "a@b.com");
        assert_eq!(cred.project_id, "cogent-snow-4mnnp");
        assert!(!cred.disabled);
        assert_eq!(cred.extra.get("credits").and_then(Value::as_i64), Some(12));
        let back: Value = serde_json::from_str(&serde_json::to_string(&cred).unwrap()).unwrap();
        assert_eq!(back["type"], "antigravity");
        assert_eq!(back["credits"], 12);
        assert!(back.get("kind").is_none());
    }

    #[test]
    fn fresh_respects_3000s_skew() {
        let now = SystemTime::now();
        let mut cred = AntigravityCredential::new(
            "tok".into(),
            "ref".into(),
            3600,
            "a@b.com".into(),
            "p".into(),
        );
        assert!(cred.is_fresh(now, 3000));
        cred.expires_in = 2000;
        cred.expired = rfc3339_from_now(now, 2000);
        assert!(!cred.is_fresh(now, 3000));
        cred.access_token.clear();
        cred.expired = rfc3339_from_now(now, 10_000);
        assert!(!cred.is_fresh(now, 3000));
    }
}
