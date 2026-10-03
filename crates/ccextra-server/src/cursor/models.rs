// models.rs:原生 GetUsableModels 目录拉取(一元 RPC,空 body protobuf)
use super::constants::{CLIENT_TIMEOUT_SECS, MODELS_PATH};
use super::oauth;
use anyhow::{Context, Result};
use ccextra_core::convert::cursor::proto::decode_get_usable_models_response;
use std::time::Duration;

/// 目录条目:原始 model id(去空白、去重)
pub struct CursorModelEntry {
    pub id: String,
}

pub async fn fetch_models(
    base_url: &str,
    client_version: &str,
    access_token: &str,
    proxy_url: Option<&str>,
) -> Result<Vec<CursorModelEntry>> {
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
        .timeout(Duration::from_secs(CLIENT_TIMEOUT_SECS))
        .send()
        .await
        .context("Cursor GetUsableModels 请求失败")?;
    if !response.status().is_success() {
        return Err(anyhow::anyhow!(
            "Cursor GetUsableModels 返回 {}",
            response.status()
        ));
    }
    let body = crate::limits::read_success_body(response).await?;
    let catalog =
        decode_get_usable_models_response(&body).context("解析 Cursor GetUsableModels 响应失败")?;
    let mut entries: Vec<CursorModelEntry> = Vec::new();
    for model in catalog.models {
        let id = model.model_id.trim();
        if id.is_empty() || entries.iter().any(|e| e.id == id) {
            continue;
        }
        entries.push(CursorModelEntry { id: id.to_string() });
    }
    Ok(entries)
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
        assert_eq!(models[0].id, "composer-2");
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
    async fn empty_catalog_returns_empty_list() {
        // 空目录按设计发布空 model 集(对齐 antigravity 语义),由调用方决定保留旧目录
        let (server, _) = spawn_captured_server(MODELS_PATH, StatusCode::OK, b"".as_slice()).await;
        let models = fetch_models(&server.url, "cli-test", "token", None)
            .await
            .unwrap();
        assert!(models.is_empty());
    }

    #[tokio::test]
    async fn failed_catalog_errors() {
        let (server, _) =
            spawn_captured_server(MODELS_PATH, StatusCode::BAD_GATEWAY, b"failed".as_slice()).await;
        assert!(fetch_models(&server.url, "cli-test", "token", None)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn duplicate_ids_are_deduped() {
        let body = b"\x0a\x0c\x0a\x0acomposer-2\x0a\x0c\x0a\x0acomposer-2".as_slice();
        let (server, _) = spawn_captured_server(MODELS_PATH, StatusCode::OK, body).await;
        let models = fetch_models(&server.url, "cli-test", "token", None)
            .await
            .unwrap();
        assert_eq!(models.len(), 1);
    }
}
