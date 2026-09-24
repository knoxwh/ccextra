use super::constants::{DEFAULT_BASE_URL, LOGIN_URL, POLL_PATH, REFRESH_PATH};
use anyhow::{anyhow, Context, Result};
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use reqwest::Client;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::time::Duration;

#[derive(Debug, Deserialize)]
pub struct TokenPair {
    #[serde(rename = "accessToken")]
    pub access_token: String,
    #[serde(default, rename = "refreshToken")]
    pub refresh_token: String,
}

pub struct AuthParams {
    pub verifier: String,
    pub uuid: String,
    pub login_url: String,
}

pub fn generate_auth_params() -> Result<AuthParams> {
    let mut random = [0u8; 112];
    getrandom::getrandom(&mut random).context("generate Cursor PKCE random bytes")?;
    let verifier = URL_SAFE_NO_PAD.encode(&random[..96]);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let uuid = format!(
        "{:08x}-{:04x}-{:04x}-{:04x}-{:012x}",
        u32::from_be_bytes(random[96..100].try_into().unwrap()),
        u16::from_be_bytes(random[100..102].try_into().unwrap()),
        u16::from_be_bytes(random[102..104].try_into().unwrap()),
        u16::from_be_bytes(random[104..106].try_into().unwrap()),
        u64::from_be_bytes([
            0,
            0,
            random[106],
            random[107],
            random[108],
            random[109],
            random[110],
            random[111]
        ])
    );
    let login_url =
        format!("{LOGIN_URL}?challenge={challenge}&uuid={uuid}&mode=login&redirectTarget=cli");
    Ok(AuthParams {
        verifier,
        uuid,
        login_url,
    })
}

pub fn http_client(proxy_url: Option<&str>) -> Result<Client> {
    super::super::codex::oauth::http_client(proxy_url)
}

pub async fn refresh_token(client: &Client, refresh_token: &str) -> Result<TokenPair> {
    refresh_token_at(
        client,
        refresh_token,
        &format!("{DEFAULT_BASE_URL}{REFRESH_PATH}"),
    )
    .await
}

pub(crate) async fn refresh_token_at(
    client: &Client,
    refresh_token: &str,
    url: &str,
) -> Result<TokenPair> {
    let response = client
        .post(url)
        .bearer_auth(refresh_token)
        .header("Content-Type", "application/json")
        .body("{}")
        .timeout(Duration::from_secs(10))
        .send()
        .await
        .context("Cursor token refresh request failed")?;
    let status = response.status();
    if !status.is_success() {
        return Err(anyhow!("Cursor token refresh failed ({status})"));
    }
    let body = crate::limits::read_success_body(response).await?;
    let pair: TokenPair =
        serde_json::from_slice(&body).context("parse Cursor token refresh response")?;
    if pair.access_token.trim().is_empty() {
        return Err(anyhow!("Cursor token refresh returned empty access token"));
    }
    Ok(pair)
}

pub fn poll_url(uuid: &str, verifier: &str) -> Result<reqwest::Url> {
    let mut url = reqwest::Url::parse(&format!("{DEFAULT_BASE_URL}{POLL_PATH}"))?;
    url.query_pairs_mut()
        .append_pair("uuid", uuid)
        .append_pair("verifier", verifier);
    Ok(url)
}
