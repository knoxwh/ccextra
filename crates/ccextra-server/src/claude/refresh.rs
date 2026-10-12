// Claude Token 刷新与运行时单飞锁
// (对齐 CPA RefreshTokensWithRetry:仅 5xx 可重试,传输/解码不盲重放)

use super::constants::{REFRESH_MAX_RETRIES, REFRESH_SKEW_SECS};
use super::credential::ClaudeCredential;
use super::{oauth, store};
use anyhow::{anyhow, Result};
use std::path::Path;
use std::sync::OnceLock;
use tokio::sync::Mutex;

static REFRESH_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn refresh_lock() -> &'static Mutex<()> {
    REFRESH_LOCK.get_or_init(|| Mutex::new(()))
}

/// 检查并刷新凭证(如果需要);仅对明确可重试 HTTP 错误退避重试
pub async fn refresh_if_needed(
    cred: &mut ClaudeCredential,
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
    tracing::debug!("刷新 Claude token: {}", email);

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
                // 刷新响应里的 email/org 可选补全
                if !token.account.email_address.is_empty() {
                    cred.email = token.account.email_address;
                }
                if !token.account.uuid.is_empty() {
                    cred.account_uuid = token.account.uuid;
                }
                if !token.organization.uuid.is_empty() {
                    cred.organization_uuid = token.organization.uuid;
                }
                if !token.organization.name.is_empty() {
                    cred.organization_name = token.organization.name;
                }
                cred.apply_tokens(token.access_token, new_refresh, token.expires_in);
                tracing::info!("Claude token 刷新成功: {}", email);
                return Ok(true);
            }
            Err(e) => {
                if !e.retryable {
                    return Err(anyhow!("{e}"));
                }
                tracing::warn!("Claude token 刷新第 {} 次失败(可重试): {}", attempt + 1, e);
                last_err = Some(e);
            }
        }
    }
    Err(anyhow!(
        "{}",
        last_err.unwrap_or_else(|| oauth::RefreshError {
            status: None,
            message: "claude token refresh failed".into(),
            retryable: false,
        })
    ))
}

/// 运行时获取新鲜凭证 (单飞锁 + Double-Check + 自动落盘)
pub async fn ensure_credential_fresh(
    auth_dir: &Path,
    email_or_account: &str,
    proxy_url: Option<&str>,
) -> Result<ClaudeCredential> {
    let path = store::credential_path(auth_dir, email_or_account);
    if let Ok(cred) = store::load(&path) {
        if cred.is_fresh(std::time::SystemTime::now(), REFRESH_SKEW_SECS) {
            return Ok(cred);
        }
    }

    let _guard = refresh_lock().lock().await;

    let mut cred = store::load(&path)?;
    if cred.is_fresh(std::time::SystemTime::now(), REFRESH_SKEW_SECS) {
        return Ok(cred);
    }

    if refresh_if_needed(&mut cred, proxy_url, REFRESH_SKEW_SECS).await? {
        store::save(auth_dir, &cred)?;
    }

    Ok(cred)
}
