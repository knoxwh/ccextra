// xAI Token 刷新与运行时单飞锁 (按 refresh_token singleflight,对齐 Codex/AG)

use super::constants::{REFRESH_MAX_RETRIES, REFRESH_SKEW_SECS};
use super::credential::XAICredential;
use super::{oauth, store};
use anyhow::{anyhow, Result};
use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, OnceLock};
use tokio::sync::Mutex;

static REFRESH_LOCKS: OnceLock<Mutex<HashMap<String, Arc<Mutex<()>>>>> = OnceLock::new();

async fn lock_for_refresh_token(refresh_token: &str) -> Arc<Mutex<()>> {
    let key = refresh_token.trim().to_string();
    let mut map = REFRESH_LOCKS
        .get_or_init(|| Mutex::new(HashMap::new()))
        .lock()
        .await;
    map.entry(key)
        .or_insert_with(|| Arc::new(Mutex::new(())))
        .clone()
}

/// 检查并刷新凭证(如果需要);仅对明确可重试 HTTP 5xx 退避重试
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
    let mut last_err = None;
    for attempt in 0..REFRESH_MAX_RETRIES {
        if attempt > 0 {
            tokio::time::sleep(std::time::Duration::from_secs(attempt as u64)).await;
        }
        match oauth::refresh_token(&client, &cred.token_endpoint, &cred.refresh_token).await {
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
                tracing::info!("xAI token 刷新成功: {}", email_or_sub);
                return Ok(true);
            }
            Err(e) => {
                if !e.retryable {
                    return Err(anyhow!("{e}"));
                }
                tracing::warn!("xAI token 刷新第 {} 次失败(可重试): {}", attempt + 1, e);
                last_err = Some(e);
            }
        }
    }
    Err(anyhow!(
        "{}",
        last_err.unwrap_or_else(|| oauth::RefreshError {
            status: None,
            message: "xai token refresh failed".into(),
            retryable: false,
        })
    ))
}

fn usable(cred: &XAICredential, rejected_token: Option<&str>) -> bool {
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
) -> Result<XAICredential> {
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
