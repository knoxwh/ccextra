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

    for (path, mut cred) in credentials {
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

        // 检查并刷新 token (对齐 CLIProxyAPI refreshLead 300s)
        match refresh::refresh_if_needed(&mut cred, proxy_url, constants::REFRESH_SKEW_SECS).await {
            Ok(true) => {
                if let Err(e) = store::save_to(&path, &cred) {
                    tracing::warn!("刷新后保存 xAI 凭证失败: {}: {}", cred.email, e);
                } else {
                    tracing::info!("xAI token 已刷新并保存: {}", cred.email);
                }
            }
            Ok(false) => {
                tracing::debug!("xAI token 仍然有效: {}", cred.email);
            }
            Err(e) => {
                tracing::warn!("刷新 xAI token 失败,跳过该凭证: {}: {}", cred.email, e);
                continue;
            }
        }

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
