// Antigravity OAuth:换码 / userinfo / refresh

use super::constants::{
    AUTH_ENDPOINT, CALLBACK_PORT, CLIENT_ID, CLIENT_SECRET, SCOPES, TOKEN_ENDPOINT,
    TOKEN_REFRESH_UA, USERINFO_ENDPOINT,
};
use anyhow::{anyhow, Context, Result};
use reqwest::Client;
use serde::Deserialize;

/// 空=继承环境代理;`direct`/`none`=直连;否则走该 URL
pub fn http_client(proxy_url: Option<&str>) -> Result<Client> {
    let trimmed = proxy_url.map(str::trim).unwrap_or("");
    let mut builder = Client::builder();
    if trimmed.is_empty() {
        return builder.build().context("build antigravity http client");
    }
    if trimmed.eq_ignore_ascii_case("direct") || trimmed.eq_ignore_ascii_case("none") {
        builder = builder.no_proxy();
        return builder.build().context("build antigravity http client");
    }
    let parsed = reqwest::Url::parse(trimmed).context("parse proxy URL failed")?;
    if parsed.scheme().is_empty() || parsed.host_str().unwrap_or("").is_empty() {
        return Err(anyhow!("proxy URL missing scheme/host"));
    }
    match parsed.scheme() {
        "socks5" | "socks5h" | "http" | "https" => {}
        other => return Err(anyhow!("unsupported proxy scheme: {other}")),
    }
    let proxy =
        reqwest::Proxy::all(trimmed).with_context(|| format!("parse proxy URL {trimmed}"))?;
    builder
        .proxy(proxy)
        .build()
        .context("build antigravity http client")
}

#[derive(Debug, Clone, Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: String,
    #[serde(default)]
    pub expires_in: i64,
}

#[derive(Debug, Deserialize)]
struct UserInfo {
    #[serde(default)]
    email: String,
}

pub fn default_redirect_uri(port: u16) -> String {
    format!("http://localhost:{port}/oauth-callback")
}

/// `access_type=offline` + `prompt=consent`,保证拿到 refresh_token
pub fn build_auth_url(state: &str, redirect_uri: &str) -> String {
    let redirect = if redirect_uri.trim().is_empty() {
        default_redirect_uri(CALLBACK_PORT)
    } else {
        redirect_uri.to_string()
    };
    let mut url = reqwest::Url::parse(AUTH_ENDPOINT).expect("AUTH_ENDPOINT");
    url.query_pairs_mut()
        .append_pair("access_type", "offline")
        .append_pair("client_id", CLIENT_ID)
        .append_pair("prompt", "consent")
        .append_pair("redirect_uri", &redirect)
        .append_pair("response_type", "code")
        .append_pair("scope", &SCOPES.join(" "))
        .append_pair("state", state);
    url.to_string()
}

pub async fn exchange_code(
    client: &Client,
    code: &str,
    redirect_uri: &str,
) -> Result<TokenResponse> {
    let resp = client
        .post(TOKEN_ENDPOINT)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .form(&[
            ("code", code),
            ("client_id", CLIENT_ID),
            ("client_secret", CLIENT_SECRET),
            ("redirect_uri", redirect_uri),
            ("grant_type", "authorization_code"),
        ])
        .send()
        .await
        .context("antigravity token exchange")?;
    decode_token(resp, "token exchange").await
}

pub async fn fetch_user_email(client: &Client, access_token: &str) -> Result<String> {
    let token = access_token.trim();
    if token.is_empty() {
        return Err(anyhow!("antigravity userinfo: missing access token"));
    }
    let resp = client
        .get(USERINFO_ENDPOINT)
        .header("Authorization", format!("Bearer {token}"))
        .header("User-Agent", super::constants::REQUEST_UA)
        .send()
        .await
        .context("antigravity userinfo")?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(http_err("userinfo", status.as_u16(), &body));
    }
    let info: UserInfo = resp.json().await.context("decode userinfo")?;
    let email = info.email.trim().to_string();
    if email.is_empty() {
        return Err(anyhow!("antigravity userinfo: response missing email"));
    }
    Ok(email)
}

pub async fn refresh_token(client: &Client, refresh: &str) -> Result<TokenResponse> {
    let refresh = refresh.trim();
    if refresh.is_empty() {
        return Err(anyhow!("antigravity token refresh: missing refresh token"));
    }
    let resp = client
        .post(TOKEN_ENDPOINT)
        .header("Host", "oauth2.googleapis.com")
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("User-Agent", TOKEN_REFRESH_UA)
        .form(&[
            ("client_id", CLIENT_ID),
            ("client_secret", CLIENT_SECRET),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh),
        ])
        .send()
        .await
        .context("antigravity token refresh")?;
    decode_token(resp, "token refresh").await
}

async fn decode_token(resp: reqwest::Response, what: &str) -> Result<TokenResponse> {
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(http_err(what, status.as_u16(), &body));
    }
    let token: TokenResponse = resp
        .json()
        .await
        .with_context(|| format!("decode {what}"))?;
    if token.access_token.trim().is_empty() {
        return Err(anyhow!("antigravity {what}: empty access token"));
    }
    Ok(token)
}

fn http_err(what: &str, status: u16, body: &str) -> anyhow::Error {
    let body = body.trim();
    if body.is_empty() {
        anyhow!("antigravity {what}: request failed: status {status}")
    } else {
        anyhow!("antigravity {what}: request failed: status {status}: {body}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::antigravity::constants::CLIENT_ID;

    #[test]
    fn auth_url_has_offline_consent_and_scopes() {
        let url = build_auth_url("st8", "http://localhost:51121/oauth-callback");
        let parsed = reqwest::Url::parse(&url).unwrap();
        assert_eq!(
            parsed.origin().ascii_serialization(),
            "https://accounts.google.com"
        );
        let q: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
        assert_eq!(q["access_type"], "offline");
        assert_eq!(q["prompt"], "consent");
        assert_eq!(q["response_type"], "code");
        assert_eq!(q["client_id"], CLIENT_ID);
        assert_eq!(q["state"], "st8");
        assert_eq!(q["redirect_uri"], "http://localhost:51121/oauth-callback");
        assert!(q["scope"].contains("cloud-platform"));
        assert!(q["scope"].contains("userinfo.email"));
        assert!(!url.contains(CLIENT_SECRET));
    }

    #[test]
    fn http_client_accepts_direct_and_url() {
        http_client(None).unwrap();
        http_client(Some("")).unwrap();
        http_client(Some("direct")).unwrap();
        http_client(Some("http://127.0.0.1:7897")).unwrap();
        http_client(Some("socks5://127.0.0.1:1080")).unwrap();
        assert!(http_client(Some("not-a-url")).is_err());
    }
}
