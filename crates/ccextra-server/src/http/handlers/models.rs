use crate::http::AppState;
use crate::http::auth::check_secret;
use crate::http::error::AppError;
use axum::{
    extract::State,
    http::{HeaderMap, StatusCode, header},
    response::Response,
};
use ccextra_core::route::ProviderConfig;
use serde_json::Value;

/// 构建 Anthropic 格式模型列表(参考 GetAvailableModels claude 分支)
pub fn build_models_list(providers: &[ProviderConfig]) -> Value {
    let mut data = Vec::new();
    for provider in providers {
        for model in &provider.models {
            // 跳过通配条目(如官方 provider 的 claude-*),避免暴露假模型 id
            if model.alias.contains('*') {
                continue;
            }
            data.push(serde_json::json!({
                "id": model.alias,
                "object": "model",
                "owned_by": provider.name,
                "type": "model",
                "display_name": model.alias,
                "max_input_tokens": model.max_input_tokens.unwrap_or(200000),
                "max_tokens": model.max_tokens.unwrap_or(64000),
            }));
        }
    }
    serde_json::json!({ "data": data })
}

/// GET /v1/models:返回配置定义的模型列表(带 secret 认证)
pub async fn handle_models(
    State(state): State<AppState>,
    headers: HeaderMap,
) -> Result<Response, AppError> {
    let snapshot = std::sync::Arc::clone(&*state.config.read().await);
    check_secret(&headers, &snapshot.runtime.secret).await?;
    let body = build_models_list(&snapshot.providers);
    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(axum::body::Body::from(serde_json::to_vec(&body).unwrap()))
        .map_err(|e| AppError::new(anyhow::anyhow!("构造模型列表响应失败: {e}")))
}
