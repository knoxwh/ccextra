use super::constants::REFRESH_SKEW_SECS;
use super::credential::AntigravityCredential;
use super::{oauth, project, store};
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

/// 检查并刷新凭证（如果需要）
/// skew_secs: 提前刷新的时间窗口（秒），对齐 CPA antigravityRequestTokenSafetyWindow = 300s
pub async fn refresh_if_needed(
    cred: &mut AntigravityCredential,
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

    tracing::debug!("刷新 Antigravity token: {}", cred.email);

    let client = oauth::http_client(proxy_url)?;
    let token = oauth::refresh_token(&client, &cred.refresh_token).await?;
    let new_refresh = if token.refresh_token.trim().is_empty() {
        None
    } else {
        Some(token.refresh_token)
    };
    cred.apply_tokens(token.access_token, new_refresh, token.expires_in);

    tracing::info!("Antigravity token 刷新成功: {}", cred.email);

    Ok(true)
}

fn usable(cred: &AntigravityCredential, rejected_token: Option<&str>) -> bool {
    cred.is_fresh(std::time::SystemTime::now(), REFRESH_SKEW_SECS)
        && rejected_token != Some(cred.access_token.as_str())
}

/// 对齐 CPA ensureAntigravityProjectID:缺 project 时 loadCodeAssist/onboard 补发现。
async fn ensure_project_id(
    cred: &mut AntigravityCredential,
    proxy_url: Option<&str>,
    request_ua: Option<&str>,
) -> bool {
    if !cred.project_id.trim().is_empty() {
        return false;
    }
    if cred.access_token.trim().is_empty() {
        return false;
    }
    let client = match oauth::http_client(proxy_url) {
        Ok(c) => c,
        Err(e) => {
            tracing::warn!("antigravity: build client for project discover failed: {e}");
            return false;
        }
    };
    match project::fetch_project_id(&client, &cred.access_token, request_ua).await {
        Ok(id) if !id.trim().is_empty() => {
            cred.project_id = id;
            true
        }
        Ok(_) => false,
        Err(e) => {
            tracing::warn!(
                "antigravity: ensure project id failed ({}): {e}",
                cred.email
            );
            false
        }
    }
}

/// 运行时获取新鲜凭证(按 refresh_token 单飞 + Double-Check + 自动落盘)
///
/// `rejected_token`:上游 401 后传入被拒的 access_token,即使未过期也强制刷新。
/// 对齐 CPA:token 新鲜但缺 project_id 时仍补发现并落盘。
pub async fn ensure_credential_fresh(
    auth_dir: &Path,
    email: &str,
    proxy_url: Option<&str>,
    rejected_token: Option<&str>,
) -> Result<AntigravityCredential> {
    ensure_credential_fresh_with_ua(auth_dir, email, proxy_url, rejected_token, None).await
}

/// 同 [`ensure_credential_fresh`],可覆盖 project 发现用的 User-Agent。
pub async fn ensure_credential_fresh_with_ua(
    auth_dir: &Path,
    email: &str,
    proxy_url: Option<&str>,
    rejected_token: Option<&str>,
    request_ua: Option<&str>,
) -> Result<AntigravityCredential> {
    let path = store::credential_path(auth_dir, email);
    let prelim = store::load(&path)?;
    let needs_refresh = !usable(&prelim, rejected_token);
    let needs_project = prelim.project_id.trim().is_empty();
    if !needs_refresh && !needs_project {
        return Ok(prelim);
    }

    let lock_key = if prelim.refresh_token.trim().is_empty() {
        format!("path:{}", path.display())
    } else {
        prelim.refresh_token.clone()
    };
    let lock = lock_for_refresh_token(&lock_key).await;
    let _guard = lock.lock().await;

    let mut cred = store::load(&path)?;
    let mut dirty = false;

    if rejected_token == Some(cred.access_token.as_str()) {
        cred.expires_in = 0;
        cred.expired = "1970-01-01T00:00:00Z".into();
    }

    if !usable(&cred, rejected_token) {
        if refresh_if_needed(&mut cred, proxy_url, REFRESH_SKEW_SECS).await? {
            dirty = true;
        }
    }

    if ensure_project_id(&mut cred, proxy_url, request_ua).await {
        dirty = true;
    }

    if dirty {
        store::save(auth_dir, &cred)?;
    }

    Ok(cred)
}
