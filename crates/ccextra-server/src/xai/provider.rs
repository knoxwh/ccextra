// xAI 动态 provider 注入

use crate::xai::{constants, models, refresh, store};
use ccextra_core::route::{Protocol, ProviderConfig};
use std::path::Path;

/// 从 auth_dir 扫描 xAI 凭证，为每个有效凭证生成一个 provider
pub async fn load_xai_providers(auth_dir: &Path, proxy_url: Option<&str>) -> Vec<ProviderConfig> {
    let mut providers = Vec::new();

    let credentials = match store::list(auth_dir) {
        Ok(creds) => creds,
        Err(e) => {
            tracing::debug!("扫描 xAI 凭证失败: {}", e);
            return providers;
        }
    };

    for (path, cred) in credentials {
        if cred.disabled {
            tracing::debug!(
                "跳过已禁用的 xAI 凭证: email={}, sub={}",
                cred.email,
                cred.sub
            );
            continue;
        }

        if cred.access_token.trim().is_empty() && cred.refresh_token.trim().is_empty() {
            tracing::debug!(
                "跳过空 token 的 xAI 凭证: email={}, sub={}",
                cred.email,
                cred.sub
            );
            continue;
        }

        // 刷新走单飞 ensure(对齐 CLIProxyAPI refreshLead 300s):与请求路径共用
        // refresh_token 锁,避免轮换型 refresh token 被后台与请求双消费(invalid_grant)
        let cred = match refresh::ensure_credential_fresh(&path, proxy_url, None).await {
            Ok(cred) => cred,
            Err(e) => {
                tracing::warn!("刷新 xAI token 失败,跳过该凭证: {}: {}", cred.email, e);
                continue;
            }
        };

        let id = if !cred.email.is_empty() {
            cred.email.clone()
        } else if !cred.sub.is_empty() {
            cred.sub.clone()
        } else {
            "default".to_string()
        };

        let provider_name = format!("xai-{}", id.replace(['@', '.'], "-"));
        let grok_models = models::default_grok_models();

        let mut metadata = std::collections::HashMap::new();
        // 运行时按扫描到的实际文件刷新/落盘(对齐 CPA Auth.Attributes["path"])
        metadata.insert(
            "credential_path".to_string(),
            path.to_string_lossy().to_string(),
        );
        metadata.insert("email".to_string(), cred.email.clone());
        metadata.insert("sub".to_string(), cred.sub.clone());
        metadata.insert("provider_type".to_string(), "xai".to_string());

        // OAuth 订阅钉 cli-chat-proxy(原生 grok-build SessionToken 路由;
        // 不 failover 到 api.x.ai,避免丢 Token-Auth 身份头)
        let provider = ProviderConfig::new(
            provider_name.clone(),
            Protocol::OpenAiResponses,
            vec![constants::CLI_CHAT_PROXY_BASE_URL.to_string()],
            cred.access_token.clone(),
            proxy_url.map(|s| s.to_string()),
            true, // prompt_cache_key: Responses 亲和(原生 forwards_prompt_cache_key)
            grok_models,
        )
        .with_metadata(metadata);

        tracing::info!(
            "加载 xAI provider: {} (email: {}, sub: {})",
            provider_name,
            if cred.email.is_empty() {
                "(none)"
            } else {
                &cred.email
            },
            if cred.sub.is_empty() {
                "(none)"
            } else {
                &cred.sub
            },
        );

        providers.push(provider);
    }

    providers
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestServer;
    use crate::xai::XAICredential;
    use axum::http::StatusCode;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// 后台(load)与请求(ensure)并发刷新:轮换型 refresh token 只被消费一次。
    /// mock 端点轮换 rt-0 → rt-1,重用 rt-0 返回 invalid_grant(与真实 OAuth 一致)。
    #[tokio::test]
    async fn concurrent_background_and_request_refresh_consumes_token_once() {
        let calls = std::sync::Arc::new(AtomicUsize::new(0));
        let router = axum::Router::new().route(
            "/token",
            axum::routing::post({
                let calls = calls.clone();
                move |body: String| {
                    let calls = calls.clone();
                    async move {
                        calls.fetch_add(1, Ordering::SeqCst);
                        let rt = body
                            .split('&')
                            .find_map(|kv| kv.strip_prefix("refresh_token="))
                            .unwrap_or("");
                        if rt != "rt-0" {
                            return (
                                StatusCode::BAD_REQUEST,
                                axum::Json(serde_json::json!({"error": "invalid_grant"})),
                            );
                        }
                        (
                            StatusCode::OK,
                            axum::Json(serde_json::json!({
                                "access_token": "new-access",
                                "refresh_token": "rt-1",
                                "expires_in": 3600,
                            })),
                        )
                    }
                }
            }),
        );
        let server = TestServer::spawn(router).await;

        let dir = tempfile::tempdir().unwrap();
        let cred: XAICredential = serde_json::from_value(serde_json::json!({
            "access_token": "old-access",
            "refresh_token": "rt-0",
            "expired": "2000-01-01T00:00:00Z",
            "token_endpoint": format!("{}/token", server.url),
            "email": "race@b.c",
        }))
        .unwrap();
        let path = dir.path().join("xai-race.json");
        store::save_to(&path, &cred).unwrap();

        let (providers, fresh) = tokio::join!(
            load_xai_providers(dir.path(), Some("direct")),
            refresh::ensure_credential_fresh(&path, Some("direct"), None),
        );
        let fresh = fresh.unwrap();
        assert_eq!(calls.load(Ordering::SeqCst), 1, "refresh token 只消费一次");
        assert_eq!(providers.len(), 1);
        assert_eq!(providers[0].key, "new-access");
        assert_eq!(fresh.access_token, "new-access");
        assert_eq!(store::load(&path).unwrap().refresh_token, "rt-1");
    }
}
