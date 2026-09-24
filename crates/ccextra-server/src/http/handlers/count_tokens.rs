use axum::{
    body::{to_bytes, Body},
    extract::State,
    http::{header, HeaderMap, StatusCode},
    response::Response,
};
use ccextra_core::route::{resolve_route, Protocol, ProviderConfig};
use ccextra_core::session::extract_claude_code_session;
use serde_json::Value;

use crate::http::auth::check_secret;
use crate::http::claude_relay::{claude_inbound_user_agent, claude_relay_headers};
use crate::http::error::AppError;
use crate::http::AppState;

fn find_provider<'a>(providers: &'a [ProviderConfig], name: &str) -> Option<&'a ProviderConfig> {
    providers.iter().find(|p| p.name == name)
}

/// POST /v1/messages/count_tokens:本地 token 估算(带 secret 认证)
///
/// Claude Code 的 /context 记账会调此端点。Claude 协议上游转发到真实 API
/// 获取精确计数；非 Claude 协议一律本地估算(O200kBase)。
pub async fn handle_count_tokens(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, AppError> {
    let snapshot = std::sync::Arc::clone(&*state.config.read().await);
    let providers = &snapshot.providers;
    let upstream_client = &snapshot.runtime.upstream;
    let user_agents = &snapshot.runtime.user_agents;
    check_secret(&headers, &snapshot.runtime.secret)?;
    let bytes = to_bytes(body, crate::limits::INBOUND_BODY_LIMIT)
        .await
        .map_err(|e| AppError::new(anyhow::anyhow!("读请求体失败: {e}")))?;

    // 解析 model 字段
    let body_json: Value = serde_json::from_slice(&bytes)
        .map_err(|e| AppError::new(anyhow::anyhow!("解析请求体失败: {e}")))?;
    let model = body_json
        .get("model")
        .and_then(|v| v.as_str())
        .ok_or_else(|| AppError::new(anyhow::anyhow!("缺少 model 字段")))?;

    // 路由判定
    let route = resolve_route(model, providers)
        .map_err(|e| AppError::new(anyhow::anyhow!("路由失败: {e}")))?;

    // Claude 协议:转发上游
    if route.protocol == Protocol::Claude {
        let provider = find_provider(providers, &route.provider)
            .ok_or_else(|| AppError::new(anyhow::anyhow!("provider 未找到")))?;
        let base_url = provider.base_urls()[0].clone(); // 取首个 URL（count_tokens 无需回退）
        let key = provider.key.clone();
        let proxy_url = provider.proxy_url.clone();

        let url = format!(
            "{}/v1/messages/count_tokens",
            base_url.trim_end_matches('/')
        );
        let proxy_key = upstream_client.resolve_proxy(proxy_url.as_deref());
        let client = upstream_client.client_for(&proxy_key, Protocol::Claude, false)?;
        let inbound_user_agent = claude_inbound_user_agent(&headers);
        let extra_headers = claude_relay_headers(&headers);
        let mut request = client
            .post(&url)
            .header(
                header::USER_AGENT,
                inbound_user_agent.unwrap_or(user_agents.claude_cli.as_str()),
            )
            .bearer_auth(&key);
        for (name, value) in &extra_headers {
            request = request.header(name, value);
        }
        let resp = crate::upstream::send_with_timeout(request.json(&body_json))
            .await
            .map_err(|e| AppError::new(anyhow::anyhow!("上游请求失败: {e}")))?;

        let status = resp.status();
        if status.is_success() {
            // 成功 body 有界读取:16 MiB 上限 + 120s idle;
            // 超限 502、停顿 504(Anthropic api_error),传输错误维持 500
            let body_bytes = crate::limits::read_success_body_or_anthropic(resp).await?;
            return Response::builder()
                .status(status)
                .header(header::CONTENT_TYPE, "application/json")
                .body(Body::from(body_bytes))
                .map_err(|e| AppError::new(anyhow::anyhow!("构造响应失败: {e}")));
        }

        // 错误 body 有界读取(256 KiB + 120s idle):截断或读取失败保留已知
        // 上游状态(429/401 等不被改成 502/504/500),生成有界 Anthropic error;
        // 未截断的错误 body 按原语义原样转发
        let (body_bytes, truncated) =
            crate::limits::read_error_body_or_anthropic(resp, status).await?;
        if truncated {
            return Err(AppError::with_status(
                status,
                "上游错误 body 超过 256 KiB,已截断",
            ));
        }
        return Response::builder()
            .status(status)
            .header(header::CONTENT_TYPE, "application/json")
            .body(Body::from(body_bytes))
            .map_err(|e| AppError::new(anyhow::anyhow!("构造响应失败: {e}")));
    }

    // 非 Claude 协议:读缓存(有上轮真实值返回,无缓存返回 0)
    let session = extract_claude_code_session(&headers, &body_json);
    let tokens = session
        .as_deref()
        .and_then(|s| state.last_input_tokens.lock().ok()?.get(s))
        .unwrap_or(0);

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "application/json")
        .body(Body::from(format!(r#"{{"input_tokens":{}}}"#, tokens)))
        .map_err(|e| AppError::new(anyhow::anyhow!("构造响应失败: {e}")))
}
