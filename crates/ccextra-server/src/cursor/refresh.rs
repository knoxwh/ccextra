use super::constants::REFRESH_SKEW_SECS;
use super::credential::{jwt_sub, CursorCredential};
use super::{oauth, store};
use anyhow::{anyhow, Result};
use std::path::Path;
use std::sync::OnceLock;
use std::time::SystemTime;
use tokio::sync::Mutex;

static REFRESH_LOCK: OnceLock<Mutex<()>> = OnceLock::new();

pub async fn ensure_credential_fresh(
    auth_dir: &Path,
    proxy_url: Option<&str>,
    rejected_token: Option<&str>,
) -> Result<CursorCredential> {
    ensure_credential_fresh_at(auth_dir, proxy_url, rejected_token, None).await
}

async fn ensure_credential_fresh_at(
    auth_dir: &Path,
    proxy_url: Option<&str>,
    rejected_token: Option<&str>,
    refresh_url: Option<&str>,
) -> Result<CursorCredential> {
    let cred = store::load(auth_dir)?;
    if usable(&cred, rejected_token) {
        return Ok(cred);
    }

    let _guard = REFRESH_LOCK.get_or_init(|| Mutex::new(())).lock().await;
    let mut cred = store::load(auth_dir)?;
    if usable(&cred, rejected_token) {
        return Ok(cred);
    }
    if cred.refresh_token.trim().is_empty() {
        return Err(anyhow!("Cursor refresh_token 为空"));
    }
    let client = oauth::http_client(proxy_url)?;
    let pair = match refresh_url {
        Some(url) => oauth::refresh_token_at(&client, &cred.refresh_token, url).await?,
        None => oauth::refresh_token(&client, &cred.refresh_token).await?,
    };
    if let Some(new_sub) = jwt_sub(&pair.access_token) {
        if !cred.sub.is_empty() && cred.sub != new_sub {
            return Err(anyhow!("Cursor 刷新后的账户身份不一致"));
        }
    }
    cred.apply_tokens(pair.access_token, pair.refresh_token);
    store::save(auth_dir, &cred)?;
    Ok(cred)
}

fn usable(cred: &CursorCredential, rejected_token: Option<&str>) -> bool {
    cred.is_fresh(SystemTime::now(), REFRESH_SKEW_SECS)
        && rejected_token != Some(cred.access_token.as_str())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestServer;
    use axum::{body::Bytes, http::HeaderMap, routing::post, Router};
    use base64::Engine;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[tokio::test]
    async fn concurrent_refresh_is_single_flight_and_persisted() {
        let temp = tempfile::tempdir().unwrap();
        let expired = CursorCredential {
            access_token: "expired".into(),
            refresh_token: "refresh-secret".into(),
            sub: "account".into(),
            expires_at: Some(0),
        };
        store::save(temp.path(), &expired).unwrap();
        let expires = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs()
            + 3600;
        let payload = serde_json::json!({"sub":"account", "exp":expires});
        let token = format!(
            "a.{}.b",
            base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(payload.to_string())
        );
        let response = serde_json::json!({"accessToken": token}).to_string();
        let count = std::sync::Arc::new(AtomicUsize::new(0));
        let calls = count.clone();
        let server = TestServer::spawn(Router::new().route(
            super::super::constants::REFRESH_PATH,
            post(move |headers: HeaderMap, body: Bytes| {
                let calls = calls.clone();
                let response = response.clone();
                async move {
                    assert_eq!(
                        headers.get("authorization").unwrap(),
                        "Bearer refresh-secret"
                    );
                    assert_eq!(body.as_ref(), b"{}");
                    calls.fetch_add(1, Ordering::SeqCst);
                    response
                }
            }),
        ))
        .await;
        let url = format!("{}{}", server.url, super::super::constants::REFRESH_PATH);
        let mut tasks = Vec::new();
        for _ in 0..12 {
            let dir = temp.path().to_path_buf();
            let url = url.clone();
            tasks.push(tokio::spawn(async move {
                ensure_credential_fresh_at(&dir, None, None, Some(&url))
                    .await
                    .unwrap()
            }));
        }
        for task in tasks {
            let credential = task.await.unwrap();
            assert_eq!(credential.sub, "account");
            assert_eq!(credential.refresh_token, "refresh-secret");
        }
        assert_eq!(count.load(Ordering::SeqCst), 1);
        assert_eq!(
            store::load(temp.path()).unwrap().refresh_token,
            "refresh-secret"
        );
    }
}
