// xAI OAuth Discovery, Device Code 与 Token 交换/刷新 (对齐 CLIProxyAPI internal/auth/xai/xai.go)

use super::constants::{CLIENT_ID, DEVICE_CODE_GRANT_TYPE, DISCOVERY_URL, SCOPE};
use anyhow::{anyhow, Context, Result};
use reqwest::Client;
use serde::Deserialize;
use std::time::Duration;

#[derive(Debug, Clone, Deserialize)]
pub struct Discovery {
    pub device_authorization_endpoint: String,
    pub token_endpoint: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct DeviceCodeResponse {
    pub device_code: String,
    pub user_code: String,
    #[serde(default)]
    pub verification_uri: String,
    #[serde(default)]
    pub verification_uri_complete: String,
    #[serde(default)]
    pub expires_in: i64,
    #[serde(default)]
    pub interval: u64,
    #[serde(skip)]
    pub token_endpoint: String,
}

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

#[derive(Debug, Deserialize)]
struct TokenPollResponse {
    #[serde(default)]
    error: String,
    #[serde(default)]
    error_description: String,
    #[serde(default)]
    access_token: String,
    #[serde(default)]
    refresh_token: String,
    #[serde(default)]
    id_token: String,
    #[serde(default)]
    token_type: String,
    #[serde(default)]
    expires_in: i64,
}

pub fn http_client(proxy_url: Option<&str>) -> Result<Client> {
    let trimmed = proxy_url.map(str::trim).unwrap_or("");
    let mut builder = Client::builder().timeout(Duration::from_secs(30));
    if trimmed.is_empty() {
        return builder.build().context("build xai http client");
    }
    if trimmed.eq_ignore_ascii_case("direct") || trimmed.eq_ignore_ascii_case("none") {
        builder = builder.no_proxy();
        return builder.build().context("build xai http client");
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
        .context("build xai http client")
}

pub fn validate_oauth_endpoint(raw_url: &str, field: &str) -> Result<String> {
    let trimmed = raw_url.trim();
    if trimmed.is_empty() {
        return Err(anyhow!("xai discovery {field} is empty"));
    }
    let parsed = reqwest::Url::parse(trimmed)
        .with_context(|| format!("xai discovery {field} is invalid: {trimmed}"))?;
    if parsed.scheme() != "https" {
        return Err(anyhow!("xai discovery {field} must use https: {trimmed}"));
    }
    let host = parsed.host_str().unwrap_or("").to_ascii_lowercase();
    if host != "x.ai" && !host.ends_with(".x.ai") {
        return Err(anyhow!("xai discovery {field} host {host} is not on x.ai"));
    }
    Ok(trimmed.to_string())
}

pub async fn discover(client: &Client) -> Result<Discovery> {
    let resp = client
        .get(DISCOVERY_URL)
        .header("Accept", "application/json")
        .send()
        .await
        .context("xai discovery request")?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("xai discovery failed with status {status}: {body}"));
    }
    let disco: Discovery = resp.json().await.context("xai discovery parse json")?;
    let device_endpoint = validate_oauth_endpoint(
        &disco.device_authorization_endpoint,
        "device_authorization_endpoint",
    )?;
    let token_endpoint = validate_oauth_endpoint(&disco.token_endpoint, "token_endpoint")?;
    Ok(Discovery {
        device_authorization_endpoint: device_endpoint,
        token_endpoint,
    })
}

pub async fn start_device_flow(client: &Client) -> Result<DeviceCodeResponse> {
    let disco = discover(client).await?;
    let resp = client
        .post(&disco.device_authorization_endpoint)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .form(&[("client_id", CLIENT_ID), ("scope", SCOPE)])
        .send()
        .await
        .context("xai device code request")?;
    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!(
            "xai device code request failed with status {status}: {body}"
        ));
    }
    let mut dev: DeviceCodeResponse = resp
        .json()
        .await
        .context("xai parse device code response")?;
    if dev.device_code.trim().is_empty() {
        return Err(anyhow!("xai device code response missing device_code"));
    }
    if dev.user_code.trim().is_empty() {
        return Err(anyhow!("xai device code response missing user_code"));
    }
    dev.token_endpoint = disco.token_endpoint;
    Ok(dev)
}

pub async fn poll_for_token(client: &Client, device: &DeviceCodeResponse) -> Result<TokenData> {
    let mut interval = Duration::from_secs(device.interval.max(5));
    let deadline = std::time::Instant::now()
        + Duration::from_secs(if device.expires_in > 0 {
            device.expires_in as u64
        } else {
            1800
        });

    loop {
        tokio::time::sleep(interval).await;
        if std::time::Instant::now() > deadline {
            return Err(anyhow!("xai device code expired"));
        }

        let resp = client
            .post(&device.token_endpoint)
            .header("Content-Type", "application/x-www-form-urlencoded")
            .header("Accept", "application/json")
            .form(&[
                ("grant_type", DEVICE_CODE_GRANT_TYPE),
                ("device_code", device.device_code.as_str()),
                ("client_id", CLIENT_ID),
            ])
            .send()
            .await
            .context("xai poll device token request")?;

        let text = resp.text().await.unwrap_or_default();
        let payload: TokenPollResponse = match serde_json::from_str(&text) {
            Ok(p) => p,
            Err(e) => return Err(anyhow!("xai parse poll token response failed: {e}: {text}")),
        };

        if !payload.error.is_empty() {
            match payload.error.as_str() {
                "authorization_pending" => continue,
                "slow_down" => {
                    interval += Duration::from_secs(5);
                    continue;
                }
                "expired_token" => return Err(anyhow!("xai device code expired")),
                "access_denied" => return Err(anyhow!("xai device authorization denied")),
                other => {
                    let desc = payload.error_description.trim();
                    if !desc.is_empty() {
                        return Err(anyhow!("xai device token error: {other}: {desc}"));
                    }
                    return Err(anyhow!("xai device token error: {other}"));
                }
            }
        }

        if payload.access_token.trim().is_empty() {
            return Err(anyhow!("xai device token response missing access_token"));
        }

        return Ok(TokenData {
            access_token: payload.access_token,
            refresh_token: payload.refresh_token,
            id_token: payload.id_token,
            token_type: payload.token_type,
            expires_in: payload.expires_in,
        });
    }
}

pub async fn refresh_token(
    client: &Client,
    token_endpoint: &str,
    refresh: &str,
) -> Result<TokenData> {
    let refresh = refresh.trim();
    if refresh.is_empty() {
        return Err(anyhow!("xai token refresh: missing refresh token"));
    }
    let endpoint = if token_endpoint.trim().is_empty() {
        let disco = discover(client).await?;
        disco.token_endpoint
    } else {
        token_endpoint.trim().to_string()
    };

    let resp = client
        .post(&endpoint)
        .header("Content-Type", "application/x-www-form-urlencoded")
        .header("Accept", "application/json")
        .form(&[
            ("grant_type", "refresh_token"),
            ("client_id", CLIENT_ID),
            ("refresh_token", refresh),
        ])
        .send()
        .await
        .context("xai token refresh")?;

    let status = resp.status();
    if !status.is_success() {
        let body = resp.text().await.unwrap_or_default();
        return Err(anyhow!("xai token refresh failed: status {status}: {body}"));
    }
    let data: TokenData = resp
        .json()
        .await
        .context("xai decode refresh token response")?;
    if data.access_token.trim().is_empty() {
        return Err(anyhow!("xai token refresh: empty access token in response"));
    }
    Ok(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_validate_oauth_endpoint() {
        assert!(validate_oauth_endpoint("https://auth.x.ai/oauth2/token", "token").is_ok());
        assert!(validate_oauth_endpoint("https://api.x.ai/v1", "token").is_ok());
        assert!(validate_oauth_endpoint("http://auth.x.ai/oauth2/token", "token").is_err());
        assert!(validate_oauth_endpoint("https://google.com/token", "token").is_err());
    }
}
