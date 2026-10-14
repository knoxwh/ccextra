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
use crate::http::claude_relay::{
    claude_inbound_user_agent, claude_passthrough_headers, ensure_oauth_beta,
};
use crate::http::error::AppError;
use crate::http::AppState;

fn find_provider<'a>(providers: &'a [ProviderConfig], name: &str) -> Option<&'a ProviderConfig> {
    providers.iter().find(|p| p.name == name)
}

/// POST /v1/messages/count_tokens(带 secret 认证)
///
/// Claude Code 的 /context 记账会调此端点。Claude 协议转发上游精确计数；
/// 非 Claude 协议返回该会话上轮记录的 input_tokens,未命中返回 0。
pub async fn handle_count_tokens(
    State(state): State<AppState>,
    uri: axum::http::Uri,
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
        let mut key = provider.key.clone();
        let proxy_url = provider.proxy_url.clone();
        let mut claude_oauth: Option<(String, String)> = None; // (auth_dir, id)

        // Claude 订阅:运行时刷新 access_token;失败硬失败
        if let Some(meta) = provider.metadata.as_ref() {
            if meta.get("provider_type").map(|s| s.as_str()) == Some("claude") {
                if let Some(auth_dir_str) = meta.get("auth_dir") {
                    let email = meta.get("email").map(|s| s.as_str()).unwrap_or("");
                    let account_uuid = meta.get("account_uuid").map(|s| s.as_str()).unwrap_or("");
                    let id = if !email.is_empty() {
                        email
                    } else {
                        account_uuid
                    };
                    if !id.is_empty() {
                        let auth_dir = std::path::Path::new(auth_dir_str);
                        match crate::claude::ensure_credential_fresh(
                            auth_dir,
                            id,
                            proxy_url.as_deref(),
                            None,
                        )
                        .await
                        {
                            Ok(fresh) => {
                                key = fresh.access_token;
                                claude_oauth = Some((auth_dir_str.clone(), id.to_string()));
                            }
                            Err(e) => {
                                return Err(AppError::unauthorized(format!(
                                    "Claude count_tokens 凭证刷新失败,拒绝使用旧 token: {e}"
                                )));
                            }
                        }
                    }
                }
            }
        }

        let endpoint =
            crate::upstream::append_inbound_query("/v1/messages/count_tokens", uri.query());
        let url = format!("{}{}", base_url.trim_end_matches('/'), endpoint);
        let inbound_user_agent = claude_inbound_user_agent(&headers);
        let mut extra_headers = claude_passthrough_headers(&headers);
        if crate::claude::is_claude_oauth_token(&key) {
            ensure_oauth_beta(&mut extra_headers);
        }
        let ua = inbound_user_agent.unwrap_or(user_agents.claude_cli.as_str());

        let mut attempt_key = key;
        for auth_attempt in 0..=1 {
            // protocol: claude 一律走 Bun 指纹车道(wreq);stale 时丢池再建一次
            let resp = upstream_client
                .send_fp_json_with_stale_retry(
                    proxy_url.as_deref(),
                    &url,
                    ua,
                    &attempt_key,
                    &extra_headers,
                    &body_json,
                )
                .await
                .map_err(|e| AppError::new(anyhow::anyhow!("上游请求失败: {e}")))?;
            let upstream = crate::upstream::UpstreamResponse {
                status: resp.status(),
                body: crate::upstream::UpstreamBody::Wreq(resp),
            };

            let status = upstream.status;
            if status.is_success() {
                // 成功 body 有界读取:16 MiB 上限 + 180s idle;
                // 超限 502、停顿 504(Anthropic api_error),传输错误维持 500
                let body_bytes =
                    crate::limits::read_success_body_or_anthropic(upstream.body).await?;
                return Response::builder()
                    .status(status)
                    .header(header::CONTENT_TYPE, "application/json")
                    .body(Body::from(body_bytes))
                    .map_err(|e| AppError::new(anyhow::anyhow!("构造响应失败: {e}")));
            }

            // 401:强制刷新后重试一次
            if status.as_u16() == 401 && auth_attempt == 0 {
                if let Some((ref auth_dir_str, ref id)) = claude_oauth {
                    let rejected = attempt_key.clone();
                    match crate::claude::ensure_credential_fresh(
                        std::path::Path::new(auth_dir_str),
                        id,
                        proxy_url.as_deref(),
                        Some(&rejected),
                    )
                    .await
                    {
                        Ok(fresh) => {
                            tracing::info!("Claude count_tokens 401,凭证已刷新,重试一次");
                            attempt_key = fresh.access_token;
                            continue;
                        }
                        Err(e) => {
                            return Err(AppError::unauthorized(format!(
                                "Claude count_tokens 401 后刷新失败,拒绝使用旧 token: {e}"
                            )));
                        }
                    }
                }
            }

            // 错误 body 有界读取(256 KiB + 180s idle):截断或读取失败保留已知
            // 上游状态(429/401 等不被改成 502/504/500),生成有界 Anthropic error;
            // 未截断的错误 body 按原语义原样转发
            let (body_bytes, truncated) =
                crate::limits::read_error_body_or_anthropic(upstream.body, status).await?;
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
