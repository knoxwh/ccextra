// Antigravity 模型列表获取
use super::constants::{API_ENDPOINT, DAILY_API_ENDPOINT};
use super::oauth;
use anyhow::{anyhow, Result};
use ccextra_core::route::ModelConfig;
use serde_json::Value;

const ANTIGRAVITY_MODELS_PATH: &str = "/v1internal:fetchAvailableModels";

/// 从 Antigravity API 获取可用模型列表
pub async fn fetch_models(
    access_token: &str,
    project_id: Option<&str>,
    proxy_url: Option<&str>,
    user_agent: Option<&str>,
) -> Result<Vec<ModelConfig>> {
    let client = oauth::http_client(proxy_url)?;
    let ua = user_agent.unwrap_or(super::constants::REQUEST_UA);

    // 尝试生产和 daily 端点
    let base_urls = [API_ENDPOINT, DAILY_API_ENDPOINT];

    for base_url in base_urls {
        let url = format!("{}{}", base_url, ANTIGRAVITY_MODELS_PATH);

        // 构造请求体
        let body = if let Some(pid) = project_id {
            serde_json::json!({ "project": pid })
        } else {
            serde_json::json!({})
        };

        let resp = client
            .post(&url)
            .header("Content-Type", "application/json")
            .header("Authorization", format!("Bearer {}", access_token))
            .header("User-Agent", ua)
            .json(&body)
            .send()
            .await;

        let resp = match resp {
            Ok(r) => r,
            Err(e) => {
                tracing::debug!("请求 {} 失败: {}", base_url, e);
                continue;
            }
        };

        if !resp.status().is_success() {
            tracing::debug!("请求 {} 返回状态: {}", base_url, resp.status());
            continue;
        }

        let body: Value = match resp.json().await {
            Ok(v) => v,
            Err(e) => {
                tracing::debug!("解析 {} 响应失败: {}", base_url, e);
                continue;
            }
        };

        return parse_models(&body);
    }

    Err(anyhow!("无法从任何 Antigravity 端点获取模型列表"))
}

/// 解析 Antigravity API 返回的模型列表
fn parse_models(body: &Value) -> Result<Vec<ModelConfig>> {
    let models_obj = body
        .get("models")
        .and_then(|v| v.as_object())
        .ok_or_else(|| anyhow!("响应中缺少 models 字段"))?;

    let mut models = Vec::new();

    // 跳过的内部/实验性模型(对齐 CPA registry 移除的废弃模型)
    let skip_models = [
        "chat_20706",
        "chat_23310",
        "tab_flash_lite_preview",
        "tab_jump_flash_lite_preview",
        "gemini-2.5-flash-thinking",
        "gemini-2.5-pro",
        "gemini-3-flash-agent",
        "gemini-3.5-flash-low",
        "gemini-3.5-flash-extra-low",
    ];

    for (model_id, model_data) in models_obj {
        let model_id = model_id.trim();
        if model_id.is_empty() || skip_models.contains(&model_id) {
            continue;
        }

        let _display_name = model_data
            .get("displayName")
            .and_then(|v| v.as_str())
            .unwrap_or(model_id);

        // Antigravity 直接使用模型 ID（如 gemini-3.7-flash-medium）
        // 不使用 model 字段的内部占位符（如 MODEL_PLACEHOLDER_M299）
        let max_input_tokens = model_data
            .get("maxTokens")
            .and_then(|v| v.as_u64())
            .filter(|&n| n > 0);

        let max_tokens = model_data
            .get("maxOutputTokens")
            .and_then(|v| v.as_u64())
            .filter(|&n| n > 0);

        models.push(ModelConfig {
            name: model_id.to_string(),  // 上游真实名：直接使用模型 ID
            alias: model_id.to_string(), // 别名与真实名相同
            max_input_tokens,
            max_tokens,
        });
    }

    if models.is_empty() {
        return Err(anyhow!("没有找到可用模型"));
    }

    Ok(models)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_parse_models_filters_defunct_models() {
        let body = json!({
            "models": {
                "gemini-3.7-flash-high": {
                    "displayName": "Gemini 3.7 Flash High",
                    "maxTokens": 1000000,
                    "maxOutputTokens": 65536
                },
                "gemini-3-flash-agent": {
                    "displayName": "Defunct Agent",
                    "maxTokens": 500000
                },
                "gemini-3.5-flash-low": {
                    "displayName": "Defunct Low",
                    "maxTokens": 500000
                },
                "gemini-3.5-flash-extra-low": {
                    "displayName": "Defunct Extra Low",
                    "maxTokens": 500000
                },
                "claude-opus-4-6-thinking": {
                    "displayName": "Claude Opus",
                    "maxTokens": 200000,
                    "maxOutputTokens": 64000
                }
            }
        });

        let models = parse_models(&body).unwrap();
        assert_eq!(models.len(), 2);

        let names: Vec<&str> = models.iter().map(|m| m.name.as_str()).collect();
        assert!(names.contains(&"gemini-3.7-flash-high"));
        assert!(names.contains(&"claude-opus-4-6-thinking"));
        assert!(!names.contains(&"gemini-3-flash-agent"));
        assert!(!names.contains(&"gemini-3.5-flash-low"));
        assert!(!names.contains(&"gemini-3.5-flash-extra-low"));
    }
}
