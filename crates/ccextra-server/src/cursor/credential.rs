use serde::{Deserialize, Serialize};
use std::time::{SystemTime, UNIX_EPOCH};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CursorCredential {
    #[serde(rename = "accessToken")]
    pub access_token: String,
    #[serde(rename = "refreshToken")]
    pub refresh_token: String,
    #[serde(default)]
    pub sub: String,
    #[serde(default)]
    pub expires_at: Option<i64>,
}

impl CursorCredential {
    pub fn is_fresh(&self, now: SystemTime, skew_secs: i64) -> bool {
        if self.access_token.trim().is_empty() {
            return false;
        }
        let expiry = self.expires_at.or_else(|| jwt_expiry(&self.access_token));
        let Some(expiry) = expiry else {
            return false;
        };
        let now = now
            .duration_since(UNIX_EPOCH)
            .map(|d| d.as_secs() as i64)
            .unwrap_or(i64::MAX);
        now.saturating_add(skew_secs) < expiry
    }

    pub fn apply_tokens(&mut self, access_token: String, refresh_token: String) {
        self.access_token = access_token;
        if !refresh_token.trim().is_empty() {
            self.refresh_token = refresh_token;
        }
        self.expires_at = Some(jwt_expiry(&self.access_token).unwrap_or_else(|| {
            SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .map(|d| d.as_secs() as i64 + 3600)
                .unwrap_or_default()
        }));
        self.sub = jwt_sub(&self.access_token).unwrap_or_default();
    }
}

pub fn jwt_sub(token: &str) -> Option<String> {
    jwt_claims(token).and_then(|v| v.get("sub").and_then(|v| v.as_str()).map(str::to_owned))
}

pub fn jwt_expiry(token: &str) -> Option<i64> {
    jwt_claims(token).and_then(|v| v.get("exp").and_then(|v| v.as_i64()))
}

fn jwt_claims(token: &str) -> Option<serde_json::Value> {
    use base64::Engine;
    let part = token.split('.').nth(1)?;
    let bytes = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(part)
        .ok()?;
    serde_json::from_slice(&bytes).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn refresh_preserves_token_and_account_identity() {
        use base64::Engine;
        let now = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;
        let claims = serde_json::json!({ "sub": "user-a", "exp": now + 3600 });
        let token = format!(
            "x.{}.x",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(claims.to_string())
        );
        let mut cred = CursorCredential {
            access_token: "stale".into(),
            refresh_token: "keep-this".into(),
            sub: "user-a".into(),
            expires_at: None,
        };
        assert!(!cred.is_fresh(SystemTime::now(), 600));
        cred.apply_tokens(token, String::new());
        assert_eq!(cred.sub, "user-a");
        assert_eq!(cred.refresh_token, "keep-this");
        assert!(cred.is_fresh(SystemTime::now(), 600));
    }
}
