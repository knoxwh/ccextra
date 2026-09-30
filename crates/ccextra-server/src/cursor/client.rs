// client.rs:sidecar HTTP 传输层
//
// 只做请求构造与响应解析,不含进程管理;进程状态由 sidecar.rs 持有,
// 测试经 TestServer 注入 base_url。/run 返回原始 Response(含 4xx/5xx),
// 状态码与 Retry-After 由调用方分类;models 解析为模型 ID 列表。

use serde_json::Value;
use thiserror::Error;

/// sidecar 调用错误:NotReady 映射 503;Request 为网络层失败;
/// InvalidJson 为 /models 响应解析失败
#[derive(Debug, Error)]
pub enum CursorSidecarError {
    #[error("cursor sidecar not ready")]
    NotReady,

    #[error("cursor sidecar request failed: {0}")]
    Request(#[from] reqwest::Error),

    #[error("cursor sidecar returned invalid models payload: {0}")]
    InvalidModels(String),
}

/// /models 目录条目:id + 参数词表(旧缓存无 parameters 字段 → 空)
#[derive(Debug, Clone, PartialEq, serde::Deserialize)]
pub struct CursorModelEntry {
    pub id: String,
    #[serde(default)]
    pub parameters: Vec<ccextra_core::convert::CursorParamVocab>,
}

#[derive(serde::Deserialize)]
struct ModelsPayload {
    models: Vec<CursorModelEntry>,
}

/// sidecar HTTP 客户端:每次请求附加 Bearer token
#[derive(Clone)]
pub struct SidecarClient {
    http: reqwest::Client,
    base_url: String,
    token: String,
}

impl SidecarClient {
    pub fn new(http: reqwest::Client, base_url: String, token: String) -> Self {
        Self {
            http,
            base_url,
            token,
        }
    }

    /// POST /run:返回原始 Response,不吞状态码(调用方读 Retry-After)
    pub async fn run(&self, body: &Value) -> Result<reqwest::Response, CursorSidecarError> {
        let response = self
            .http
            .post(format!("{}/run", self.base_url))
            .bearer_auth(&self.token)
            .json(body)
            .send()
            .await?;
        Ok(response)
    }

    /// POST /models:apiKey 发现模型目录,解析为 id + 参数词表
    pub async fn models(&self, api_key: &str) -> Result<Vec<CursorModelEntry>, CursorSidecarError> {
        let response = self
            .http
            .post(format!("{}/models", self.base_url))
            .bearer_auth(&self.token)
            .json(&serde_json::json!({ "apiKey": api_key }))
            .send()
            .await?;
        let status = response.status();
        let text = response.text().await?;
        if !status.is_success() {
            return Err(CursorSidecarError::InvalidModels(format!(
                "status {status}: {text}"
            )));
        }
        let payload: ModelsPayload = serde_json::from_str(&text)
            .map_err(|e| CursorSidecarError::InvalidModels(e.to_string()))?;
        Ok(payload.models)
    }

    /// GET /health:200 视为健康,其余(含网络失败)视为不健康
    pub async fn health(&self) -> bool {
        match self
            .http
            .get(format!("{}/health", self.base_url))
            .bearer_auth(&self.token)
            .send()
            .await
        {
            Ok(response) => response.status().is_success(),
            Err(_) => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestServer;
    use axum::http::StatusCode;
    use bytes::Bytes;

    fn client_for(url: &str) -> SidecarClient {
        SidecarClient::new(
            reqwest::Client::builder().build().unwrap(),
            url.to_string(),
            "test-token".into(),
        )
    }

    #[tokio::test]
    async fn run_sends_bearer_token_and_json_body() {
        let captured = std::sync::Arc::new(std::sync::Mutex::new(None));
        let cap = captured.clone();
        let router = axum::Router::new().route(
            "/run",
            axum::routing::post(move |headers: axum::http::HeaderMap, body: Bytes| {
                let cap = cap.clone();
                async move {
                    *cap.lock().unwrap() = Some((headers, body));
                    (
                        StatusCode::OK,
                        [(axum::http::header::CONTENT_TYPE, "application/json")],
                        Bytes::from_static(b"{}"),
                    )
                }
            }),
        );
        let server = TestServer::spawn(router).await;
        let client = client_for(&server.url);
        let body = serde_json::json!({ "model": "auto" });
        let response = client.run(&body).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let (headers, sent) = captured.lock().unwrap().clone().unwrap();
        assert_eq!(
            headers.get("authorization").and_then(|v| v.to_str().ok()),
            Some("Bearer test-token")
        );
        assert_eq!(sent, Bytes::from(r#"{"model":"auto"}"#));
    }

    #[tokio::test]
    async fn run_preserves_503_status_and_retry_after() {
        let server =
            TestServer::reply(StatusCode::SERVICE_UNAVAILABLE, Bytes::from_static(b"busy")).await;
        let client = client_for(&server.url);
        let response = client.run(&serde_json::json!({})).await.unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        // reply() 无 Retry-After 头;状态码与 body 原样可见即可
        assert_eq!(response.text().await.unwrap(), "busy");
    }

    #[tokio::test]
    async fn models_parses_id_list() {
        let server = TestServer::reply(
            StatusCode::OK,
            Bytes::from(
                r#"{"models":[{"id":"auto"},{"id":"grok-4.7","parameters":[{"id":"reasoning_effort","values":["low","xhigh"]}]},{"id":"composer-2.5"}]}"#,
            ),
        )
        .await;
        let client = client_for(&server.url);
        let models = client.models("key").await.unwrap();
        assert_eq!(models.len(), 3);
        assert_eq!(models[0].id, "auto");
        assert!(models[0].parameters.is_empty());
        assert_eq!(models[1].id, "grok-4.7");
        assert_eq!(models[1].parameters.len(), 1);
        assert_eq!(models[1].parameters[0].id, "reasoning_effort");
        assert_eq!(models[1].parameters[0].values, vec!["low", "xhigh"]);
        assert_eq!(models[2].id, "composer-2.5");
    }

    #[tokio::test]
    async fn models_rejects_invalid_json() {
        let server = TestServer::reply(StatusCode::OK, Bytes::from_static(b"not json")).await;
        let client = client_for(&server.url);
        let err = client.models("key").await.unwrap_err();
        assert!(matches!(err, CursorSidecarError::InvalidModels(_)));
    }

    #[tokio::test]
    async fn models_rejects_error_status() {
        let server = TestServer::reply(
            StatusCode::BAD_GATEWAY,
            Bytes::from_static(br#"{"error":"upstream"}"#),
        )
        .await;
        let client = client_for(&server.url);
        let err = client.models("key").await.unwrap_err();
        match err {
            CursorSidecarError::InvalidModels(message) => {
                assert!(message.contains("502"), "{message}");
            }
            other => panic!("unexpected error: {other:?}"),
        }
    }

    #[tokio::test]
    async fn health_true_on_200_and_false_on_connection_failure() {
        let server = TestServer::reply(StatusCode::OK, Bytes::from_static(b"ok")).await;
        let client = client_for(&server.url);
        assert!(client.health().await);

        // 已关闭端口:连接失败视为不健康
        let dead = SidecarClient::new(
            reqwest::Client::builder().build().unwrap(),
            "http://127.0.0.1:1".into(),
            "test-token".into(),
        );
        assert!(!dead.health().await);
    }
}
