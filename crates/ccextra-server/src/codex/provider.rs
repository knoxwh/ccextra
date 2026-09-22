// Codex 动态 provider 注入

use crate::codex::{constants, models, refresh, store};
use ccextra_core::route::{Protocol, ProviderConfig};
use std::path::Path;

/// 从 auth_dir 扫描 Codex 凭证，为每个有效凭证生成一个 provider
pub async fn load_codex_providers(auth_dir: &Path, proxy_url: Option<&str>) -> Vec<ProviderConfig> {
    let mut providers = Vec::new();

    let credentials = match store::list(auth_dir) {
        Ok(creds) => creds,
        Err(e) => {
            tracing::debug!("扫描 Codex 凭证失败: {}", e);
            return providers;
        }
    };

    for (_path, mut cred) in credentials {
        if cred.disabled {
            tracing::debug!("跳过已禁用的 Codex 凭证: email={}", cred.email);
            continue;
        }

        if cred.access_token.trim().is_empty() && cred.refresh_token.trim().is_empty() {
            tracing::debug!("跳过空 token 的 Codex 凭证: email={}", cred.email);
            continue;
        }

        // 检查并刷新 token (对齐 CPA RefreshLead 24h + 重试 3 次)
        match refresh::refresh_if_needed(&mut cred, proxy_url, constants::REFRESH_SKEW_SECS).await {
            Ok(true) => {
                if let Err(e) = store::save(auth_dir, &cred) {
                    tracing::warn!("刷新后保存 Codex 凭证失败: {}: {}", cred.email, e);
                } else {
                    tracing::info!("Codex token 已刷新并保存: {}", cred.email);
                }
            }
            Ok(false) => {
                tracing::debug!("Codex token 仍然有效: {}", cred.email);
            }
            Err(e) => {
                tracing::warn!("刷新 Codex token 失败: {}: {}", cred.email, e);
            }
        }

        let id = if !cred.email.is_empty() {
            cred.email.clone()
        } else if !cred.account_id.is_empty() {
            cred.account_id.clone()
        } else {
            "default".to_string()
        };

        let provider_name = format!("codex-{}", id.replace(['@', '.'], "-"));
        let codex_models = models::default_codex_models();

        let mut metadata = std::collections::HashMap::new();
        metadata.insert(
            "auth_dir".to_string(),
            auth_dir.to_string_lossy().to_string(),
        );
        metadata.insert("email".to_string(), cred.email.clone());
        metadata.insert("account_id".to_string(), cred.account_id.clone());
        metadata.insert("provider_type".to_string(), "codex".to_string());

        let provider = ProviderConfig::new(
            provider_name.clone(),
            Protocol::OpenAiResponses,
            vec![constants::DEFAULT_API_BASE_URL.to_string()],
            cred.access_token.clone(),
            proxy_url.map(|s| s.to_string()),
            false, // prompt_cache_key
            codex_models,
        )
        .with_metadata(metadata);

        tracing::info!(
            "加载 Codex provider: {} (email: {}, account: {})",
            provider_name,
            if cred.email.is_empty() {
                "(none)"
            } else {
                &cred.email
            },
            if cred.account_id.is_empty() {
                "(none)"
            } else {
                &cred.account_id
            },
        );

        providers.push(provider);
    }

    providers
}
