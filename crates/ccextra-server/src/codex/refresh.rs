// Codex Token 刷新与运行时单飞锁
// (对齐 CPA RefreshTokensWithRetry + 按 refresh_token singleflight)

use super::constants::{REFRESH_MAX_RETRIES, REFRESH_SKEW_SECS};
use super::credential::CodexCredential;
use super::{oauth, store};
use crate::oauth_common::lock_for_refresh_token;
use anyhow::{anyhow, Result};
use std::path::Path;

/// 检查并刷新凭证(如果需要);失败按 CPA 策略重试
/// (3 次,退避 attempt 秒;`refresh_token_reused` 不可重试)
pub async fn refresh_if_needed(
    cred: &mut CodexCredential,
    proxy_url: Option<&str>,
    skew_secs: i64,
) -> Result<bool> {
    let now = std::time::SystemTime::now();

    if cred.is_fresh(now, skew_secs) {
        return Ok(false);
    }

    if cred.refresh_token.trim().is_empty() {
        return Err(anyhow!("refresh_token 为空，无法刷新"));
    }

    let email = cred.email.clone();
    tracing::debug!("刷新 Codex token: {}", email);

    let client = oauth::http_client(proxy_url)?;
    let mut last_err = None;
    for attempt in 0..REFRESH_MAX_RETRIES {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(attempt as u64)).await;
        }
        match oauth::refresh_token(&client, &cred.refresh_token).await {
            Ok(token) => {
                let new_refresh = if token.refresh_token.trim().is_empty() {
                    None
                } else {
                    Some(token.refresh_token)
                };
                let new_id = if token.id_token.trim().is_empty() {
                    None
                } else {
                    Some(token.id_token)
                };
                cred.apply_tokens(token.access_token, new_refresh, new_id, token.expires_in);
                tracing::info!("Codex token 刷新成功: {}", email);
                return Ok(true);
            }
            Err(e) => {
                if is_non_retryable_refresh_err(&e) {
                    return Err(e);
                }
                tracing::warn!("Codex token 刷新第 {} 次失败: {}", attempt + 1, e);
                last_err = Some(e);
            }
        }
    }
    Err(last_err.unwrap_or_else(|| anyhow!("codex token refresh failed")))
}

fn is_non_retryable_refresh_err(err: &anyhow::Error) -> bool {
    err.to_string()
        .to_ascii_lowercase()
        .contains("refresh_token_reused")
}

fn usable(cred: &CodexCredential, rejected_token: Option<&str>) -> bool {
    cred.is_fresh(std::time::SystemTime::now(), REFRESH_SKEW_SECS)
        && rejected_token != Some(cred.access_token.as_str())
}

/// 运行时获取新鲜凭证 (按 refresh_token 单飞 + Double-Check + 自动落盘)
///
/// `rejected_token`:上游 401 后传入被拒的 access_token,即使未过期也强制刷新。
pub async fn ensure_credential_fresh(
    path: &Path,
    proxy_url: Option<&str>,
    rejected_token: Option<&str>,
) -> Result<CodexCredential> {
    if let Ok(cred) = store::load(path) {
        if usable(&cred, rejected_token) {
            return Ok(cred);
        }
    }

    let prelim = store::load(path)?;
    let lock_key = if prelim.refresh_token.trim().is_empty() {
        format!("path:{}", path.display())
    } else {
        prelim.refresh_token.clone()
    };
    let lock = lock_for_refresh_token(&lock_key).await;
    let _guard = lock.lock().await;

    let mut cred = store::load(path)?;
    if usable(&cred, rejected_token) {
        return Ok(cred);
    }

    if rejected_token == Some(cred.access_token.as_str()) {
        cred.expired = "1970-01-01T00:00:00Z".into();
    }

    if refresh_if_needed(&mut cred, proxy_url, REFRESH_SKEW_SECS).await? {
        store::save_to(path, &cred)?;
    }

    Ok(cred)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cred(access_token: &str, expired: &str) -> CodexCredential {
        serde_json::from_value(serde_json::json!({
            "id_token": "", "access_token": access_token,
            "expired": expired,
        }))
        .expect("credential json")
    }

    #[test]
    fn usable_requires_fresh_and_not_rejected_token() {
        let live = cred("tok-live", "2099-01-01T00:00:00Z");
        assert!(usable(&live, None));
        assert!(!usable(&live, Some("tok-live")));
        assert!(usable(&live, Some("other-token")));
        assert!(!usable(&cred("", "2099-01-01T00:00:00Z"), None));
        assert!(!usable(&cred("tok", "2000-01-01T00:00:00Z"), None));
    }

    #[test]
    fn only_refresh_token_reused_is_non_retryable() {
        assert!(is_non_retryable_refresh_err(&anyhow!(
            "400: error=REFRESH_TOKEN_REUSED"
        )));
        assert!(!is_non_retryable_refresh_err(&anyhow!("500 server error")));
    }
}
