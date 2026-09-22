// Codex OAuth PKCE / 授权 URL / 换码 / 刷新
// (对齐 CLIProxyAPI internal/auth/codex/openai_auth.go + pkce.go)

use super::constants::{AUTH_SCOPE, AUTH_URL, CLIENT_ID, REFRESH_SCOPE, TOKEN_URL};
use anyhow::{anyhow, Context, Result};
use reqwest::Client;
use serde::Deserialize;
use sha2::{Digest, Sha256};
use std::time::Duration;

#[derive(Debug, Clone, Deserialize)]
pub struct TokenData {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: String,
    #[serde(default)]
    pub id_token: String,
    #[serde(default)]
    pub token_type: String,
    #[serde(default)]
    pub expires_in: i64,
}

/// PKCE 校验对 (对齐 CPA GeneratePKCECodes: 96 随机字节 base64url 无填充)
pub struct PkceCodes {
    pub code_verifier: String,
    pub code_challenge: String,
}

pub fn generate_pkce_codes() -> Result<PkceCodes> {
    let mut bytes = [0u8; 96];
    getrandom::getrandom(&mut bytes).context("generate pkce verifier")?;
    use base64::Engine;
    let engine = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    let code_verifier = engine.encode(bytes);
    let digest = Sha256::digest(code_verifier.as_bytes());
    let code_challenge = engine.encode(digest);
    Ok(PkceCodes {
        code_verifier,
        code_challenge,
    })
}

/// 构造授权 URL (对齐 CPA GenerateAuthURL 参数集)
pub fn build_auth_url(state: &str, redirect_uri: &str, pkce: &PkceCodes) -> String {
    let params = [
        ("client_id", CLIENT_ID),
        ("response_type", "code"),
        ("redirect_uri", redirect_uri),
        ("scope", AUTH_SCOPE),
        ("state", state),
        ("code_challenge", pkce.code_challenge.as_str()),
        ("code_challenge_method", "S256"),
        ("prompt", "login"),
        ("id_token_add_organizations", "true"),
        ("codex_cli_simplified_flow", "true"),
    ];
    let query: Vec<String> = params
        .iter()
        .map(|(k, v)| format!("{}={}", k, urlencode(v)))
        .collect();
    format!("{}?{}", AUTH_URL, query.join("&"))
}

/// form 编码(空格等保留字符转义)
fn urlencode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                out.push(b as char)
            }
            _ => out.push_str(&format!("%{b:02X}")),
        }
    }
    out
}

/// 与项目 xai/antigravity 相同的代理感知 HTTP 客户端
pub fn http_client(proxy_url: Option<&str>) -> Result<Client> {
    let trimmed = proxy_url.map(str::trim).unwrap_or("");
    let mut builder = Client::builder().timeout(Duration::from_secs(30));
    if trimmed.is_empty() {
        return builder.build().context("build codex http client");
    }
    if trimmed.eq_ignore_ascii_case("direct") || trimmed.eq_ignore_ascii_case("none") {
        builder = builder.no_proxy();
        return builder.build().context("build codex http client");
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
        .context("build codex http client")
}

/// 授权码换 token (对齐 CPA ExchangeCodeForTokens)
pub async fn exchange_code(
    client: &Client,
    code: &str,
    redirect_uri: &str,
    pkce: &PkceCodes,
) -> Result<TokenData> {
    let code = code.trim();
    if code.is_empty() {
        return Err(anyhow!("codex token exchange: missing authorization code"));
    }
    let resp = client
        .post(TOKEN_URL)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .form(&[
            ("grant_type", "authorization_code"),
            ("client_id", CLIENT_ID),
            ("code", code),
            ("redirect_uri", redirect_uri),
            ("code_verifier", pkce.code_verifier.as_str()),
        ])
        .send()
        .await
        .context("codex token exchange request")?;
    let status = resp.status();
    if !status.is_success() {
        let body = crate::limits::read_error_text(resp).await;
        return Err(anyhow!(
            "codex token exchange failed with status {status}: {body}"
        ));
    }
    let bytes = crate::limits::read_success_body(resp)
        .await
        .context("codex parse token response")?;
    let data: TokenData =
        serde_json::from_slice(&bytes).context("codex parse token response")?;
    if data.access_token.trim().is_empty() {
        return Err(anyhow!(
            "codex token exchange: empty access token in response"
        ));
    }
    Ok(data)
}

/// 刷新 token (对齐 CPA refreshTokensSingleFlight;重试由 refresh.rs 负责)
pub async fn refresh_token(client: &Client, refresh: &str) -> Result<TokenData> {
    let refresh = refresh.trim();
    if refresh.is_empty() {
        return Err(anyhow!("codex token refresh: missing refresh token"));
    }
    let resp = client
        .post(TOKEN_URL)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .form(&[
            ("client_id", CLIENT_ID),
            ("grant_type", "refresh_token"),
            ("refresh_token", refresh),
            ("scope", REFRESH_SCOPE),
        ])
        .send()
        .await
        .context("codex token refresh request")?;
    let status = resp.status();
    if !status.is_success() {
        let body = crate::limits::read_error_text(resp).await;
        return Err(anyhow!(
            "codex token refresh failed with status {status}: {body}"
        ));
    }
    let bytes = crate::limits::read_success_body(resp)
        .await
        .context("codex parse refresh response")?;
    let data: TokenData =
        serde_json::from_slice(&bytes).context("codex parse refresh response")?;
    if data.access_token.trim().is_empty() {
        return Err(anyhow!(
            "codex token refresh: empty access token in response"
        ));
    }
    Ok(data)
}
