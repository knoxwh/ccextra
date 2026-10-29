// Claude OAuth PKCE / 授权 URL / 换码 / 刷新
// (对齐 CLIProxyAPI internal/auth/claude/anthropic_auth.go + pkce.go)

use super::constants::{
    AUTH_SCOPE, AUTH_URL, CLIENT_ID, PROFILE_URL, REDIRECT_URI, REFRESH_TOKEN_URL, TOKEN_URL,
};
use crate::oauth_common::urlencode;
use anyhow::{anyhow, Context, Result};
use reqwest::Client;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::time::Duration;

#[derive(Debug, Clone, Deserialize)]
pub struct TokenData {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: String,
    #[serde(default)]
    pub expires_in: i64,
    #[serde(default)]
    pub organization: TokenOrg,
    #[serde(default)]
    pub account: TokenAccount,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct TokenOrg {
    #[serde(default)]
    pub uuid: String,
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct TokenAccount {
    #[serde(default)]
    pub uuid: String,
    #[serde(default)]
    pub email_address: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct OAuthProfile {
    #[serde(default)]
    pub account: ProfileAccount,
    #[serde(default)]
    pub organization: ProfileOrg,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ProfileAccount {
    #[serde(default)]
    pub uuid: String,
    #[serde(default)]
    pub email: String,
}

#[derive(Debug, Clone, Default, Deserialize)]
pub struct ProfileOrg {
    #[serde(default)]
    pub uuid: String,
    #[serde(default)]
    pub name: String,
}

/// 换码请求体字段顺序对齐 Claude Code 2.1.220 线上流量
#[derive(Debug, Serialize)]
struct AuthorizationCodeExchangeRequest<'a> {
    grant_type: &'a str,
    code: &'a str,
    redirect_uri: &'a str,
    client_id: &'a str,
    code_verifier: &'a str,
    state: &'a str,
}

/// PKCE 校验对 (96 随机字节 base64url 无填充 → SHA256 challenge)
pub struct PkceCodes {
    pub code_verifier: String,
    pub code_challenge: String,
}

pub fn generate_pkce_codes() -> Result<PkceCodes> {
    let mut bytes = [0u8; 96];
    getrandom::fill(&mut bytes).context("generate pkce verifier")?;
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

/// 构造授权 URL (对齐 CPA GenerateAuthURL)
pub fn build_auth_url(state: &str, pkce: &PkceCodes) -> String {
    let params = [
        ("code", "true"),
        ("client_id", CLIENT_ID),
        ("response_type", "code"),
        ("redirect_uri", REDIRECT_URI),
        ("scope", AUTH_SCOPE),
        ("code_challenge", pkce.code_challenge.as_str()),
        ("code_challenge_method", "S256"),
        ("state", state),
    ];
    let query: Vec<String> = params
        .iter()
        .map(|(k, v)| format!("{}={}", k, urlencode(v)))
        .collect();
    format!("{}?{}", AUTH_URL, query.join("&"))
}

/// 与项目 xai/codex 相同的代理感知 HTTP 客户端
pub fn http_client(proxy_url: Option<&str>) -> Result<Client> {
    let trimmed = proxy_url.map(str::trim).unwrap_or("");
    let mut builder = Client::builder().timeout(Duration::from_secs(30));
    if trimmed.is_empty() {
        return builder.build().context("build claude http client");
    }
    if trimmed.eq_ignore_ascii_case("direct") || trimmed.eq_ignore_ascii_case("none") {
        builder = builder.no_proxy();
        return builder.build().context("build claude http client");
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
        .context("build claude http client")
}

/// 对齐 CPA applyClaudeOAuthAxiosHeaders(控制面;不含 Connection:close)
fn apply_oauth_headers(req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
    req.header("Accept", "application/json, text/plain, */*")
        .header("Content-Type", "application/json")
        .header("User-Agent", "axios/1.15.2")
        .header("Accept-Encoding", "gzip, compress, deflate, br")
}

/// 回调 code 可能带 `#state` 片段 (对齐 CPA parseCodeAndState)
pub fn parse_code_and_state(code: &str) -> (String, Option<String>) {
    let mut parts = code.splitn(2, '#');
    let parsed_code = parts.next().unwrap_or("").to_string();
    let parsed_state = parts
        .next()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    (parsed_code, parsed_state)
}

/// 授权码换 token (JSON body, platform.claude.com)
pub async fn exchange_code(
    client: &Client,
    code: &str,
    state: &str,
    pkce: &PkceCodes,
) -> Result<TokenData> {
    let (parsed_code, fragment_state) = parse_code_and_state(code.trim());
    if parsed_code.is_empty() {
        return Err(anyhow!("claude token exchange: missing authorization code"));
    }
    let effective_state = fragment_state.as_deref().unwrap_or(state);
    let body = AuthorizationCodeExchangeRequest {
        grant_type: "authorization_code",
        code: &parsed_code,
        redirect_uri: REDIRECT_URI,
        client_id: CLIENT_ID,
        code_verifier: pkce.code_verifier.as_str(),
        state: effective_state,
    };
    let resp = apply_oauth_headers(client.post(TOKEN_URL))
        .json(&body)
        .send()
        .await
        .context("claude token exchange request")?;
    let status = resp.status();
    if !status.is_success() {
        let body = crate::limits::read_error_text(resp).await;
        return Err(anyhow!(
            "claude token exchange failed with status {status}: {body}"
        ));
    }
    let bytes = crate::limits::read_success_body(resp)
        .await
        .context("claude parse token response")?;
    let data: TokenData = serde_json::from_slice(&bytes).context("claude parse token response")?;
    if data.access_token.trim().is_empty() {
        return Err(anyhow!(
            "claude token exchange: empty access token in response"
        ));
    }
    Ok(data)
}

/// 刷新结果错误分类:仅明确 HTTP 5xx 可重试;其余(含传输/解码)不盲重放
#[derive(Debug)]
pub struct RefreshError {
    pub status: Option<u16>,
    pub message: String,
    pub retryable: bool,
}

impl std::fmt::Display for RefreshError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.status {
            Some(s) => write!(f, "token refresh failed with status {s}: {}", self.message),
            None => write!(f, "token refresh failed: {}", self.message),
        }
    }
}

impl std::error::Error for RefreshError {}

/// 刷新 token (JSON body);失败不自动重试——由 refresh.rs 按 retryable 决定
pub async fn refresh_token(client: &Client, refresh: &str) -> Result<TokenData, RefreshError> {
    let refresh = refresh.trim();
    if refresh.is_empty() {
        return Err(RefreshError {
            status: None,
            message: "missing refresh token".into(),
            retryable: false,
        });
    }
    let body = serde_json::json!({
        "client_id": CLIENT_ID,
        "grant_type": "refresh_token",
        "refresh_token": refresh,
        "scope": AUTH_SCOPE,
    });
    let resp = match apply_oauth_headers(client.post(REFRESH_TOKEN_URL))
        .json(&body)
        .send()
        .await
    {
        Ok(r) => r,
        // 传输错误:上游可能已消费单次 refresh,不可重放
        Err(e) => {
            return Err(RefreshError {
                status: None,
                message: format!("request failed: {e}"),
                retryable: false,
            })
        }
    };
    let status = resp.status();
    if !status.is_success() {
        let text = crate::limits::read_error_text(resp).await;
        let code = status.as_u16();
        return Err(RefreshError {
            status: Some(code),
            message: text,
            // 仅 5xx 可重试;429 / 4xx 不重放
            retryable: code >= 500,
        });
    }
    let bytes = match crate::limits::read_success_body(resp).await {
        Ok(b) => b,
        Err(e) => {
            return Err(RefreshError {
                status: None,
                message: format!("read response: {e}"),
                retryable: false,
            })
        }
    };
    let mut data: TokenData = match serde_json::from_slice(&bytes) {
        Ok(d) => d,
        Err(e) => {
            return Err(RefreshError {
                status: None,
                message: format!("parse response: {e}"),
                retryable: false,
            })
        }
    };
    if data.access_token.trim().is_empty() {
        return Err(RefreshError {
            status: None,
            message: "empty access token in response".into(),
            retryable: false,
        });
    }
    // 响应未回 refresh_token 时保留原值
    if data.refresh_token.trim().is_empty() {
        data.refresh_token = refresh.to_string();
    }
    Ok(data)
}

/// 可选:登录后拉 profile 补全 email / org (失败不影响登录)
pub async fn fetch_profile(client: &Client, access_token: &str) -> Result<OAuthProfile> {
    let access_token = access_token.trim();
    if access_token.is_empty() {
        return Err(anyhow!("claude profile: empty access token"));
    }
    let resp = apply_oauth_headers(client.get(PROFILE_URL))
        .header("Authorization", format!("Bearer {access_token}"))
        .header("Cache-Control", "no-cache")
        .send()
        .await
        .context("claude profile request")?;
    let status = resp.status();
    if !status.is_success() {
        let body = crate::limits::read_error_text(resp).await;
        return Err(anyhow!(
            "claude profile failed with status {status}: {body}"
        ));
    }
    let bytes = crate::limits::read_success_body(resp)
        .await
        .context("claude parse profile")?;
    serde_json::from_slice(&bytes).context("claude parse profile")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_code_and_state() {
        let (code, state) = parse_code_and_state("abc#xyz");
        assert_eq!(code, "abc");
        assert_eq!(state.as_deref(), Some("xyz"));
        let (code2, state2) = parse_code_and_state("plain");
        assert_eq!(code2, "plain");
        assert!(state2.is_none());
    }

    #[test]
    fn test_build_auth_url_contains_required_params() {
        let pkce = PkceCodes {
            code_verifier: "v".into(),
            code_challenge: "c".into(),
        };
        let url = build_auth_url("st", &pkce);
        assert!(url.starts_with(AUTH_URL));
        assert!(url.contains("client_id="));
        assert!(url.contains("code_challenge=c"));
        assert!(url.contains("redirect_uri="));
        assert!(url.contains("state=st"));
        assert!(url.contains("code=true"));
    }
}
