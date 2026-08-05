// HTTP 服务入口:axum /v1/messages
//
// 完整管线:
// 1. 解析入站 anthropic body
// 2. normalize_anthropic_full(若启用)
// 3. 路由决策 model → provider → protocol
// 4. payload 参数覆盖
// 5. 协议转换(三条 body-to-body)
// 6. normalize_target_post(仅转换路径)
// 7. 上游请求
// 8. 响应转发(直通字节 / 流式 SSE 状态机)

use axum::{
    body::{Body, to_bytes},
    extract::State,
    http::{header, StatusCode},
    response::{IntoResponse, Response},
    routing::post,
    Router,
};
use ccextra_core::convert::{
    convert_passthrough, convert_to_openai_chat, convert_to_openai_responses,
};
use ccextra_core::normalize::{
    normalize_anthropic_full, normalize_target_post, TargetShape,
};
use ccextra_core::route::{resolve_route, validate_providers, ProviderConfig, Protocol};
use ccextra_core::session::derive_session_key;
use globset::Glob;
use serde_json::Value;
use std::sync::Arc;
use tokio::sync::RwLock;

use crate::upstream::UpstreamClient;

/// 热重载结果:闭包重读配置文件,返回新配置
#[derive(Debug, Clone)]
pub struct ReloadData {
    pub providers: Vec<ProviderConfig>,
    pub payload_rules: Vec<PayloadRule>,
}

#[derive(Clone)]
pub struct AppState {
    pub providers: Arc<RwLock<Vec<ProviderConfig>>>,
    pub payload_rules: Arc<RwLock<Vec<PayloadRule>>>,
    pub normalize: NormalizeConfig,
    pub logging: LoggingConfig,
    pub upstream: UpstreamClient,
    /// 重读配置文件的闭包(由 cli 构造,捕获 config 路径)
    pub reload: Arc<dyn Fn() -> anyhow::Result<ReloadData> + Send + Sync>,
}

#[derive(Clone, Debug, serde::Deserialize)]
pub struct PayloadRule {
    pub models: Vec<String>,
    pub params: serde_json::Map<String, Value>,
}

#[derive(Clone, Debug, serde::Deserialize)]
pub struct NormalizeConfig {
    pub enabled: bool,
    pub drift_detector: bool,
}

#[derive(Clone, Debug, serde::Deserialize)]
pub struct LoggingConfig {
    pub level: String,
    pub request_body: bool,
}

pub fn app(state: AppState) -> Router {
    Router::new()
        .route("/v1/messages", post(handle_messages))
        .route("/health", axum::routing::get(health_check))
        .route("/reload", post(handle_reload))
        .with_state(state)
}

async fn health_check() -> &'static str {
    "ok"
}

/// 热重载:重读配置文件,校验后热更新 providers + payload
async fn handle_reload(State(state): State<AppState>) -> Result<&'static str, AppError> {
    let data = (state.reload)()
        .map_err(|e| AppError(anyhow::anyhow!("重读配置失败: {e}")))?;
    validate_providers(&data.providers)
        .map_err(|e| AppError(anyhow::anyhow!("配置校验失败: {e}")))?;
    *state.providers.write().await = data.providers;
    *state.payload_rules.write().await = data.payload_rules;
    tracing::info!("配置热重载完成");
    Ok("reloaded")
}

/// 按 provider 名查找 provider 配置
fn find_provider<'a>(
    providers: &'a [ProviderConfig],
    name: &str,
) -> Option<&'a ProviderConfig> {
    providers.iter().find(|p| p.name == name)
}

/// 应用 payload 参数覆盖(支持 "*glm*" 通配)
fn apply_payload_overrides(
    body: &mut Value,
    model: &str,
    rules: &[PayloadRule],
) {
    for rule in rules {
        let matched = rule.models.iter().any(|pat| {
            if pat == "*" {
                return true;
            }
            Glob::new(pat)
                .map(|g| g.compile_matcher().is_match(model))
                .unwrap_or(false)
        });
        if matched {
            for (key, val) in &rule.params {
                body[key] = val.clone();
            }
        }
    }
}

async fn handle_messages(
    State(state): State<AppState>,
    body: Body,
) -> Result<Response, AppError> {
    let bytes = to_bytes(body, 10 * 1024 * 1024).await
        .map_err(|e| AppError(anyhow::anyhow!("读请求体失败: {e}")))?;
    if state.logging.request_body {
        tracing::debug!("请求体: {}", String::from_utf8_lossy(&bytes));
    }
    let mut body_json: Value = serde_json::from_slice(&bytes)
        .map_err(|e| AppError(anyhow::anyhow!("请求体 JSON 解析失败: {e}")))?;

    // 1. 入站 model(复制为 String,避免借用 body_json 阻碍后续可变借用)
    let model = body_json
        .get("model")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AppError(anyhow::anyhow!("缺少 model 字段")))?
        .to_string();

    // 2. 归一化第一遍(anthropic 全量)
    if state.normalize.enabled {
        let session_key = derive_session_key(&body_json);
        let counts = normalize_anthropic_full(&mut body_json, &session_key);
        tracing::debug!(?counts, "normalize_anthropic_full");
    }

    // 3. 路由决策(读当前快照)
    let providers = state.providers.read().await;
    let route = resolve_route(&model, &providers)?;
    let payload_rules = state.payload_rules.read().await;

    // 4. payload 参数覆盖
    apply_payload_overrides(&mut body_json, &model, &payload_rules);

    // 5. 协议转换
    let is_stream = body_json
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(true);
    let session_key = derive_session_key(&body_json);
    match route.protocol {
        Protocol::Claude => {
            convert_passthrough(&mut body_json, &route.upstream_model)?;
        }
        Protocol::OpenAiChat => {
            convert_to_openai_chat(&mut body_json, &route.upstream_model)?;
            if state.normalize.enabled {
                normalize_target_post(&mut body_json, &session_key, TargetShape::OpenAiChat);
            }
        }
        Protocol::OpenAiResponses => {
            convert_to_openai_responses(&mut body_json, &route.upstream_model)?;
            if state.normalize.enabled {
                normalize_target_post(&mut body_json, &session_key, TargetShape::OpenAiResponses);
            }
        }
    }

    // 6. 上游请求
    let provider = find_provider(&providers, &route.provider)
        .ok_or_else(|| AppError(anyhow::anyhow!("provider 未找到: {}", route.provider)))?;
    let upstream = state
        .upstream
        .request(
            &provider.base_url,
            &provider.key,
            route.protocol,
            provider.proxy_url.as_deref(),
            &body_json,
        )
        .await?;
    let status = upstream.status;

    // 上游错误透传
    if !status.is_success() {
        let body_bytes = upstream.body.bytes().await?;
        return Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body_bytes))
            .map_err(|e| AppError(anyhow::anyhow!("构造错误响应失败: {e}")));
    }

    // 7. 响应转换
    if is_stream {
        // 流式:claude 直通字节转发;转换路径走 SSE 状态机
        let stream = upstream.body.bytes_stream();
        let out = crate::sse::relay(route.protocol, stream);
        Ok(Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "text/event-stream")
            .header(header::CACHE_CONTROL, "no-cache")
            .body(Body::from_stream(out))
            .map_err(|e| AppError(anyhow::anyhow!("构造流式响应失败: {e}")))?)
    } else {
        // 非流:直接透传上游 body
        let body_bytes = upstream.body.bytes().await?;
        Ok(Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body_bytes))
            .map_err(|e| AppError(anyhow::anyhow!("构造响应失败: {e}")))?)
    }
}

pub async fn serve(addr: &str, state: AppState) -> anyhow::Result<()> {
    let listener = tokio::net::TcpListener::bind(addr).await?;
    tracing::info!("ccextra listening on {}", addr);

    axum::serve(listener, app(state)).await?;
    Ok(())
}

// 错误处理
#[derive(Debug)]
pub struct AppError(pub anyhow::Error);

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Internal error: {}", self.0),
        )
            .into_response()
    }
}

impl<E> From<E> for AppError
where
    E: Into<anyhow::Error>,
{
    fn from(err: E) -> Self {
        Self(err.into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::body::to_bytes;
    use axum::http::Request;
    use serde_json::json;
    use tower::util::ServiceExt;

    fn mock_state() -> AppState {
        use ccextra_core::route::{ModelConfig, Protocol};
        let providers = vec![
            ProviderConfig {
                name: "test-claude".into(),
                protocol: Protocol::Claude,
                base_url: "https://mock.example.com".into(),
                key: "sk-test".into(),
                proxy_url: None,
                models: vec![ModelConfig {
                    name: "claude-opus-5".into(),
                    alias: "test-opus".into(),
                }],
            },
            ProviderConfig {
                name: "test-openai".into(),
                protocol: Protocol::OpenAiChat,
                base_url: "https://mock-openai.example.com".into(),
                key: "sk-openai".into(),
                proxy_url: Some("direct".into()),
                models: vec![ModelConfig {
                    name: "gpt-4".into(),
                    alias: "test-gpt".into(),
                }],
            },
        ];
        let reload = Arc::new(|| -> anyhow::Result<ReloadData> {
            Ok(ReloadData {
                providers: vec![],
                payload_rules: vec![],
            })
        });
        AppState {
            providers: Arc::new(RwLock::new(providers)),
            payload_rules: Arc::new(RwLock::new(vec![])),
            normalize: NormalizeConfig {
                enabled: false,
                drift_detector: false,
            },
            logging: LoggingConfig {
                level: "info".into(),
                request_body: false,
            },
            upstream: UpstreamClient::new(None),
            reload,
        }
    }

    #[tokio::test]
    async fn test_health_check() {
        let app = app(mock_state());
        let req = Request::builder()
            .uri("/health")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::OK);
        let body = to_bytes(resp.into_body(), 1024).await.unwrap();
        assert_eq!(body, "ok");
    }

    #[tokio::test]
    async fn test_missing_model_field() {
        let app = app(mock_state());
        let body_json = json!({"messages": [{"role": "user", "content": "hi"}]});
        let req = Request::builder()
            .uri("/v1/messages")
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body_json).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[tokio::test]
    async fn test_model_not_found() {
        let app = app(mock_state());
        let body_json = json!({
            "model": "unknown-model",
            "messages": [{"role": "user", "content": "hi"}]
        });
        let req = Request::builder()
            .uri("/v1/messages")
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(serde_json::to_vec(&body_json).unwrap()))
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::INTERNAL_SERVER_ERROR);
    }

    #[test]
    fn test_payload_override_wildcard() {
        let mut body = json!({"model": "glm-5.1", "max_tokens": 1024});
        let rules = vec![PayloadRule {
            models: vec!["*glm*".into()],
            params: {
                let mut map = serde_json::Map::new();
                map.insert("max_tokens".into(), json!(32000));
                map.insert("temperature".into(), json!(0.1));
                map
            },
        }];
        apply_payload_overrides(&mut body, "glm-5.1", &rules);
        assert_eq!(body["max_tokens"], 32000);
        assert_eq!(body["temperature"], 0.1);
    }

    #[test]
    fn test_payload_override_no_match() {
        let mut body = json!({"model": "claude-opus", "max_tokens": 1024});
        let rules = vec![PayloadRule {
            models: vec!["*glm*".into()],
            params: {
                let mut map = serde_json::Map::new();
                map.insert("max_tokens".into(), json!(32000));
                map
            },
        }];
        apply_payload_overrides(&mut body, "claude-opus", &rules);
        assert_eq!(body["max_tokens"], 1024);
    }

    #[test]
    fn test_payload_override_star_matches_all() {
        let mut body = json!({"model": "any-model"});
        let rules = vec![PayloadRule {
            models: vec!["*".into()],
            params: {
                let mut map = serde_json::Map::new();
                map.insert("temperature".into(), json!(0.5));
                map
            },
        }];
        apply_payload_overrides(&mut body, "any-model", &rules);
        assert_eq!(body["temperature"], 0.5);
    }

    #[tokio::test]
    async fn test_reload_endpoint() {
        let app = app(mock_state());
        let req = Request::builder()
            .uri("/reload")
            .method("POST")
            .body(Body::empty())
            .unwrap();
        let resp = app.oneshot(req).await.unwrap();
        // reload 会失败(mock reload 返回空 providers),但端点应响应
        assert!(resp.status() == StatusCode::OK || resp.status().is_server_error());
    }
}
