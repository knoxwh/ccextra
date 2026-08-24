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
    let body = json!({"metadata": {"ideType": "ANTIGRAVITY"}});
    let resp = client
        .post(&url)
        .header("Authorization", format!("Bearer {access_token}"))
        .header("Accept", "*/*")
        .header("Content-Type", "application/json")
        .header("User-Agent", REQUEST_UA)
        .json(&body)
        .send()
        .await
        .context("antigravity loadCodeAssist")?;
    let status = resp.status();
    let bytes = resp.bytes().await.context("read loadCodeAssist")?;
    if !status.is_success() {
        return Err(anyhow!(
            "loadCodeAssist failed: status {}: {}",
            status.as_u16(),
            String::from_utf8_lossy(&bytes).trim()
        ));
    }
    let load: Value = serde_json::from_slice(&bytes).context("decode loadCodeAssist")?;
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
            .post(&url)
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
        let bytes = resp.bytes().await.context("read onboardUser")?;
        if !status.is_success() {
            let preview = String::from_utf8_lossy(&bytes);
            let preview = preview.trim();
            let preview = if preview.len() > 200 {
                &preview[..200]
            } else {
                preview
            };
            return Err(anyhow!("onboardUser http {}: {preview}", status.as_u16()));
        }
        let data: Value = serde_json::from_slice(&bytes).context("decode onboardUser")?;
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
}
