// project_id:loadCodeAssist,空则 daily onboardUser 轮询

use super::constants::{
    API_ENDPOINT, API_VERSION, DAILY_API_ENDPOINT, GOOG_API_CLIENT, ONBOARD_UA, REQUEST_UA,
};
use anyhow::{anyhow, Context, Result};
use reqwest::Client;
use serde_json::{json, Value};
use std::time::Duration;

const ONBOARD_ATTEMPTS: u32 = 5;
const ONBOARD_INTERVAL: Duration = Duration::from_secs(2);

pub fn extract_project(data: &Value) -> String {
    for key in ["cloudaicompanionProject", "projectId", "project"] {
        match data.get(key) {
            Some(Value::String(s)) if !s.trim().is_empty() => return s.trim().to_string(),
            Some(Value::Object(obj)) => {
                if let Some(Value::String(id)) = obj.get("id") {
                    if !id.trim().is_empty() {
                        return id.trim().to_string();
                    }
                }
            }
            _ => {}
        }
    }
    String::new()
}

pub fn default_tier_id(load: &Value) -> String {
    if let Some(tiers) = load.get("allowedTiers").and_then(Value::as_array) {
        for tier in tiers {
            let is_default = tier.get("isDefault").and_then(Value::as_bool) == Some(true);
            if !is_default {
                continue;
            }
            if let Some(id) = tier.get("id").and_then(Value::as_str) {
                if !id.trim().is_empty() {
                    return id.trim().to_string();
                }
            }
        }
    }
    if let Some(id) = load
        .pointer("/currentTier/id")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return id.to_string();
    }
    "free-tier".into()
}

pub async fn fetch_project_id(client: &Client, access_token: &str) -> Result<String> {
    let url = format!("{API_ENDPOINT}/{API_VERSION}:loadCodeAssist");
    fetch_project_id_at(client, &url, access_token).await
}

/// 按给定端点获取 project id;URL 参数化便于接线测试
pub(crate) async fn fetch_project_id_at(
    client: &Client,
    url: &str,
    access_token: &str,
) -> Result<String> {
    let body = json!({"metadata": {"ideType": "ANTIGRAVITY"}});
    let resp = client
        .post(url)
        .header("Authorization", format!("Bearer {access_token}"))
        .header("Accept", "*/*")
        .header("Content-Type", "application/json")
        .header("User-Agent", REQUEST_UA)
        .json(&body)
        .send()
        .await
        .context("antigravity loadCodeAssist")?;
    let status = resp.status();
    // 有界读取:成功 16 MiB / 错误 256 KiB;30s 总超时由 client 覆盖
    let body = crate::limits::read_body_by_status(resp)
        .await
        .context("read loadCodeAssist")?;
    if !status.is_success() {
        return Err(anyhow!(
            "loadCodeAssist failed: status {}: {}{}",
            status.as_u16(),
            String::from_utf8_lossy(&body.bytes).trim(),
            if body.truncated { "…(已截断)" } else { "" }
        ));
    }
    let load: Value = serde_json::from_slice(&body.bytes).context("decode loadCodeAssist")?;
    let project = extract_project(&load);
    if !project.is_empty() {
        return Ok(project);
    }
    let project = onboard_user(client, access_token, &default_tier_id(&load)).await?;
    if project.is_empty() {
        return Err(anyhow!(
            "project id not found in loadCodeAssist or onboardUser response"
        ));
    }
    Ok(project)
}

pub async fn onboard_user(client: &Client, access_token: &str, tier_id: &str) -> Result<String> {
    let url = format!("{DAILY_API_ENDPOINT}/{API_VERSION}:onboardUser");
    onboard_user_at(client, &url, access_token, tier_id).await
}

/// 按给定端点执行 onboardUser 轮询;URL 参数化便于接线测试
pub(crate) async fn onboard_user_at(
    client: &Client,
    url: &str,
    access_token: &str,
    tier_id: &str,
) -> Result<String> {
    let body = json!({
        "tier_id": tier_id,
        "metadata": {
            "ide_type": "ANTIGRAVITY",
            "ide_version": "2.9.1",
            "ide_name": "antigravity",
        }
    });
    for _attempt in 1..=ONBOARD_ATTEMPTS {
        let resp = client
            .post(url)
            .header("Authorization", format!("Bearer {access_token}"))
            .header("Accept", "*/*")
            .header("Content-Type", "application/json")
            .header("User-Agent", ONBOARD_UA)
            .header("X-Goog-Api-Client", GOOG_API_CLIENT)
            .json(&body)
            .send()
            .await
            .context("antigravity onboardUser")?;
        let status = resp.status();
        // 有界读取:成功 16 MiB / 错误 256 KiB;30s 总超时由 client 覆盖
        let body = crate::limits::read_body_by_status(resp)
            .await
            .context("read onboardUser")?;
        if !status.is_success() {
            let preview = String::from_utf8_lossy(&body.bytes);
            let preview = preview.trim();
            let preview = if preview.len() > 200 {
                &preview[..200]
            } else {
                preview
            };
            let note = if body.truncated { "…(已截断)" } else { "" };
            return Err(anyhow!(
                "onboardUser http {}: {preview}{note}",
                status.as_u16()
            ));
        }
        let data: Value = serde_json::from_slice(&body.bytes).context("decode onboardUser")?;
        if data.get("done").and_then(Value::as_bool) == Some(true) {
            let project = data
                .get("response")
                .map(extract_project)
                .unwrap_or_default();
            if project.is_empty() {
                return Err(anyhow!("no project_id in onboardUser response"));
            }
            return Ok(project);
        }
        tokio::time::sleep(ONBOARD_INTERVAL).await;
    }
    Err(anyhow!(
        "onboard user did not complete after {ONBOARD_ATTEMPTS} attempts"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn extract_string_and_object_project() {
        assert_eq!(
            extract_project(&json!({"cloudaicompanionProject":"cogent-snow-4mnnp"})),
            "cogent-snow-4mnnp"
        );
        assert_eq!(
            extract_project(&json!({
                "cloudaicompanionProject": {
                    "id": "cogent-snow-4mnnp",
                    "name": "cogent-snow-4mnnp"
                }
            })),
            "cogent-snow-4mnnp"
        );
        assert_eq!(extract_project(&json!({"projectId":"p2"})), "p2");
        assert_eq!(extract_project(&json!({})), "");
    }

    #[test]
    fn tier_prefers_default_then_current() {
        assert_eq!(
            default_tier_id(&json!({
                "allowedTiers":[{"id":"paid","isDefault":false},{"id":"free-tier","isDefault":true}]
            })),
            "free-tier"
        );
        assert_eq!(default_tier_id(&json!({"currentTier":{"id":"pro"}})), "pro");
        assert_eq!(default_tier_id(&json!({})), "free-tier");
    }

    #[tokio::test]
    async fn project_and_onboard_body_limits() {
        use crate::{
            limits::SUCCESS_BODY_LIMIT,
            test_support::{padded_json, TestServer},
        };
        use axum::http::StatusCode;
        let client = crate::antigravity::oauth::http_client(Some("direct")).unwrap();
        for onboard in [false, true] {
            let json = if onboard {
                r#"{"done":true,"response":{"projectId":"proj-1"}}"#
            } else {
                r#"{"cloudaicompanionProject":"proj-1"}"#
            };
            for (status, len, expected) in [
                (StatusCode::OK, 0, ""),
                (StatusCode::OK, SUCCESS_BODY_LIMIT + 1, "上限"),
                (StatusCode::INTERNAL_SERVER_ERROR, 300 * 1024, "已截断"),
            ] {
                let server = TestServer::reply(status, padded_json(json, len)).await;
                let result = if onboard {
                    onboard_user_at(&client, &server.url, "tok", "free-tier").await
                } else {
                    fetch_project_id_at(&client, &server.url, "tok").await
                };
                if expected.is_empty() {
                    assert_eq!(result.unwrap(), "proj-1");
                } else {
                    let err = result.unwrap_err();
                    assert!(format!("{err:#}").contains(expected), "{err:#}");
                }
            }
        }
    }
}
