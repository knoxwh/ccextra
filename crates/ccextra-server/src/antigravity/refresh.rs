// Antigravity OAuth token 刷新(按需)

use super::credential::AntigravityCredential;
use super::oauth;
use anyhow::{anyhow, Result};

/// 检查并刷新凭证（如果需要）
/// skew_secs: 提前刷新的时间窗口（秒），对齐 CLIProxyAPI refreshSkew 3000
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
