// xAI 凭证结构与 JWT Payload 解析 (对齐 CLIProxyAPI TokenStorage)

use serde::{Deserialize, Serialize};
use std::time::{Duration, SystemTime};
use time::format_description::well_known::Rfc3339;
use time::OffsetDateTime;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct XAICredential {
    #[serde(default = "default_type")]
    pub r#type: String,
    #[serde(default = "default_auth_kind")]
    pub auth_kind: String,
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: String,
    #[serde(default)]
    pub id_token: String,
    #[serde(default)]
    pub token_type: String,
    #[serde(default)]
    pub expires_in: i64,
    #[serde(default)]
    pub expired: String,
    #[serde(default)]
    pub last_refresh: String,
    #[serde(default)]
    pub email: String,
    #[serde(default)]
    pub sub: String,
    #[serde(default = "default_base_url")]
    pub base_url: String,
    #[serde(default)]
    pub token_endpoint: String,
    #[serde(default)]
    pub disabled: bool,
}

fn default_type() -> String {
    "xai".to_string()
}

fn default_auth_kind() -> String {
    "oauth".to_string()
}

fn default_base_url() -> String {
    super::constants::CLI_CHAT_PROXY_BASE_URL.to_string()
}

impl XAICredential {
    /// 检查 token 是否有效 (now + skew_secs < expire_time)
    pub fn is_fresh(&self, now: SystemTime, skew_secs: i64) -> bool {
        if self.access_token.trim().is_empty() {
            return false;
        }
        if self.expired.trim().is_empty() {
            return true;
        }
        let exp_time = match OffsetDateTime::parse(self.expired.trim(), &Rfc3339) {
            Ok(dt) => SystemTime::from(dt),
            Err(_) => return true,
        };
        let skew = Duration::from_secs(skew_secs.max(0) as u64);
        match now.checked_add(skew) {
            Some(deadline) => exp_time > deadline,
            None => false,
        }
    }

    /// 应用刷新返回的 token
    pub fn apply_tokens(
        &mut self,
        access_token: String,
        refresh_token: Option<String>,
        id_token: Option<String>,
        expires_in: i64,
    ) {
        self.access_token = access_token;
        if let Some(r) = refresh_token {
            if !r.trim().is_empty() {
                self.refresh_token = r;
            }
        }
        if let Some(id) = id_token {
            if !id.trim().is_empty() {
                let (email, sub) = parse_jwt_identity(&id);
                if !email.is_empty() {
                    self.email = email;
                }
                if !sub.is_empty() {
                    self.sub = sub;
                }
                self.id_token = id;
            }
        }
        self.expires_in = expires_in;
        let now = OffsetDateTime::now_utc();
        self.last_refresh = now.format(&Rfc3339).unwrap_or_default();
        let exp = now + time::Duration::seconds(expires_in.max(60));
        self.expired = exp.format(&Rfc3339).unwrap_or_default();
    }
}

/// 解析 JWT ID Token 提取 email 与 sub
pub fn parse_jwt_identity(id_token: &str) -> (String, String) {
    let parts: Vec<&str> = id_token.split('.').collect();
    if parts.len() < 2 {
        return (String::new(), String::new());
    }
    use base64::Engine;
    let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let padded = match parts[1].len() % 4 {
        2 => format!("{}==", parts[1]),
        3 => format!("{}=", parts[1]),
        _ => parts[1].to_string(),
    };
    let payload_bytes = match engine.decode(padded.as_bytes()) {
        Ok(b) => b,
        Err(_) => match base64::engine::general_purpose::STANDARD.decode(padded.as_bytes()) {
            Ok(b) => b,
            Err(_) => return (String::new(), String::new()),
        },
    };
    let Ok(val) = serde_json::from_slice::<serde_json::Value>(&payload_bytes) else {
        return (String::new(), String::new());
    };
    let email = val
        .get("email")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    let sub = val
        .get("sub")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    (email, sub)
}

/// 根据 email 或 sub 生成文件名 (对齐 CPA CredentialFileName)
pub fn credential_file_name(email: &str, sub: &str) -> String {
    let seg_email = sanitize_file_segment(email);
    if !seg_email.is_empty() {
        return format!("xai-{seg_email}.json");
    }
    let seg_sub = sanitize_file_segment(sub);
    if !seg_sub.is_empty() {
        return format!("xai-{seg_sub}.json");
    }
    let ms = SystemTime::now()
        .duration_since(SystemTime::UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0);
    format!("xai-{ms}.json")
}

fn sanitize_file_segment(value: &str) -> String {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return String::new();
    }
    let mut out = String::with_capacity(trimmed.len());
    for c in trimmed.chars() {
        match c {
            'a'..='z' | 'A'..='Z' | '0'..='9' | '@' | '.' | '_' | '-' => out.push(c),
            _ => out.push('-'),
        }
    }
    out.trim_matches('-').to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_file_name_sanitization() {
        assert_eq!(credential_file_name("user@x.ai", ""), "xai-user@x.ai.json");
        assert_eq!(credential_file_name("", "sub_12345"), "xai-sub_12345.json");
    }

    #[test]
    fn test_parse_jwt_identity() {
        let payload = r#"{"email":"test@example.com","sub":"user-123"}"#;
        use base64::Engine;
        let encoded = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload.as_bytes());
        let token = format!("header.{}.sig", encoded);
        let (email, sub) = parse_jwt_identity(&token);
        assert_eq!(email, "test@example.com");
        assert_eq!(sub, "user-123");
    }
}
