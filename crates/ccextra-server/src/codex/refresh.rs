// Codex Token 刷新与运行时单飞锁
// (对齐 CPA RefreshTokensWithRetry 重试策略 + ensureAccessToken 单飞)

use super::constants::{REFRESH_MAX_RETRIES, REFRESH_SKEW_SECS};
use super::credential::CodexCredential;
use super::{oauth, store};
use anyhow::{anyhow, Result};
use std::path::Path;
use std::sync::OnceLock;
use tokio::sync::Mutex;

static REFRESH_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn refresh_lock() -> &'static Mutex<()> {
    REFRESH_LOCK.get_or_init(|| Mutex::new(()))
}

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

/// `refresh_token_reused` 属不可恢复错误,直接失败 (对齐 CPA isNonRetryableRefreshErr)
fn is_non_retryable_refresh_err(err: &anyhow::Error) -> bool {
    err.to_string().to_ascii_lowercase().contains("refresh_token_reused")
}

/// 运行时获取新鲜凭证 (单飞锁 + Double-Check + 自动落盘)
pub async fn ensure_credential_fresh(
    auth_dir: &Path,
    email_or_account: &str,
    proxy_url: Option<&str>,
) -> Result<CodexCredential> {
    let path = store::credential_path(auth_dir, email_or_account);
    if let Ok(cred) = store::load(&path) {
        if cred.is_fresh(std::time::SystemTime::now(), REFRESH_SKEW_SECS) {
            return Ok(cred);
        }
    }

    let _guard = refresh_lock().lock().await;

    // 获取锁后 double-check
    let mut cred = store::load(&path)?;
    if cred.is_fresh(std::time::SystemTime::now(), REFRESH_SKEW_SECS) {
        return Ok(cred);
    }

    if refresh_if_needed(&mut cred, proxy_url, REFRESH_SKEW_SECS).await? {
        store::save(auth_dir, &cred)?;
    }

    Ok(cred)
}
