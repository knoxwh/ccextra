// xAI Token 刷新与运行时单飞锁 (对齐 antigravity/refresh.rs 与 CPA singleflight)

use super::constants::REFRESH_SKEW_SECS;
use super::credential::XAICredential;
use super::{oauth, store};
use anyhow::{anyhow, Result};
use std::path::Path;
use std::sync::OnceLock;
use tokio::sync::Mutex;

static REFRESH_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

fn refresh_lock() -> &'static Mutex<()> {
    REFRESH_LOCK.get_or_init(|| Mutex::new(()))
}

/// 检查并刷新凭证（如果需要）
pub async fn refresh_if_needed(
    cred: &mut XAICredential,
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

    let email_or_sub = if !cred.email.is_empty() {
        cred.email.clone()
    } else {
        cred.sub.clone()
    };
    tracing::debug!("刷新 xAI token: {}", email_or_sub);

    let client = oauth::http_client(proxy_url)?;
    let token = oauth::refresh_token(&client, &cred.token_endpoint, &cred.refresh_token).await?;
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

    tracing::info!("xAI token 刷新成功: {}", email_or_sub);

    Ok(true)
}

/// 运行时获取新鲜凭证 (对齐 CLIProxyAPI ensureAccessToken: 单飞锁 + Double-Check + 自动落盘)
pub async fn ensure_credential_fresh(
    auth_dir: &Path,
    email_or_sub: &str,
    proxy_url: Option<&str>,
) -> Result<XAICredential> {
    let path = store::credential_path(auth_dir, email_or_sub);
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
