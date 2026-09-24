use super::constants::MODELS_PATH;
use super::oauth;
use anyhow::{anyhow, Context, Result};
use ccextra_core::convert::cursor::proto::decode_get_usable_models_response;
use ccextra_core::route::ModelConfig;
use std::time::Duration;

pub async fn fetch_models(
    base_url: &str,
    client_version: &str,
    access_token: &str,
    proxy_url: Option<&str>,
) -> Result<Vec<ModelConfig>> {
    let client = oauth::http_client(proxy_url)?;
    let url = format!("{}{MODELS_PATH}", base_url.trim_end_matches('/'));
    let response = client
        .post(url)
        .header("Content-Type", "application/proto")
        .header("Te", "trailers")
        .header("X-Ghost-Mode", "true")
        .header("X-Cursor-Client-Type", "cli")
        .header("X-Cursor-Client-Version", client_version)
        .bearer_auth(access_token)
        .body(Vec::new())
        .timeout(Duration::from_secs(5))
        .send()
        .await
        .context("Cursor GetUsableModels 请求失败")?;
    if !response.status().is_success() {
        return Err(anyhow!("Cursor GetUsableModels 返回 {}", response.status()));
    }
    let body = crate::limits::read_success_body(response).await?;
    let catalog =
        decode_get_usable_models_response(&body).context("解析 Cursor GetUsableModels 响应失败")?;
    let mut models: Vec<ModelConfig> = Vec::new();
    for model in catalog.models {
        let name = model.model_id.trim();
        if name.is_empty() || models.iter().any(|m| m.name == name) {
            continue;
        }
        models.push(ModelConfig {
            name: name.to_string(),
            alias: name.to_string(),
            max_input_tokens: None,
            max_tokens: None,
        });
    }
    if models.is_empty() {
        return Err(anyhow!("Cursor 未返回可用模型"));
    }
    Ok(models)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::spawn_captured_server;
    use axum::http::StatusCode;

    #[tokio::test]
    async fn models_request_is_empty_unframed_protobuf() {
        let (server, capture) = spawn_captured_server(
            MODELS_PATH,
            StatusCode::OK,
            b"\x0a\x0c\x0a\x0acomposer-2".as_slice(),
        )
        .await;
        let models = fetch_models(&server.url, "cli-test", "token", None)
            .await
            .unwrap();
        assert_eq!(models[0].name, "composer-2");
        assert_eq!(
            capture.raw_body.lock().unwrap().as_deref(),
            Some([].as_slice())
        );
        assert_eq!(
            capture.header("content-type").as_deref(),
            Some("application/proto")
        );
        assert_eq!(
            capture.header("authorization").as_deref(),
            Some("Bearer token")
        );
        assert_eq!(
            capture.header("x-cursor-client-version").as_deref(),
            Some("cli-test")
        );
    }

    #[tokio::test]
    async fn empty_or_failed_catalog_is_not_published() {
        let (server, _) = spawn_captured_server(MODELS_PATH, StatusCode::OK, b"".as_slice()).await;
        assert!(fetch_models(&server.url, "cli-test", "token", None)
            .await
            .is_err());
        let (server, _) =
            spawn_captured_server(MODELS_PATH, StatusCode::BAD_GATEWAY, b"failed".as_slice()).await;
        assert!(fetch_models(&server.url, "cli-test", "token", None)
            .await
            .is_err());
    }
}
