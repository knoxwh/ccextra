// Claude 订阅动态 provider 注入 (protocol: claude, 复用 API/中转形态)

use crate::claude::{constants, models, refresh, store};
use ccextra_core::route::{Protocol, ProviderConfig};
use std::path::Path;

/// 从 auth_dir 扫描 Claude 凭证，为每个有效凭证生成一个 protocol:claude provider
pub async fn load_claude_providers(
    auth_dir: &Path,
    proxy_url: Option<&str>,
) -> Vec<ProviderConfig> {
    let mut providers = Vec::new();

    let credentials = match store::list(auth_dir) {
        Ok(creds) => creds,
        Err(e) => {
            tracing::debug!("扫描 Claude 凭证失败: {}", e);
            return providers;
        }
    };

    for (_path, mut cred) in credentials {
        if cred.disabled {
            tracing::debug!("跳过已禁用的 Claude 凭证: email={}", cred.email);
            continue;
        }

        if cred.access_token.trim().is_empty() && cred.refresh_token.trim().is_empty() {
            tracing::debug!("跳过空 token 的 Claude 凭证: email={}", cred.email);
            continue;
        }

        match refresh::refresh_if_needed(&mut cred, proxy_url, constants::REFRESH_SKEW_SECS).await {
            Ok(true) => {
                if let Err(e) = store::save(auth_dir, &cred) {
                    tracing::warn!("刷新后保存 Claude 凭证失败: {}: {}", cred.email, e);
                } else {
                    tracing::info!("Claude token 已刷新并保存: {}", cred.email);
                }
            }
            Ok(false) => {
                tracing::debug!("Claude token 仍然有效: {}", cred.email);
            }
            Err(e) => {
                tracing::warn!("刷新 Claude token 失败: {}: {}", cred.email, e);
            }
        }

        if cred.access_token.trim().is_empty() {
            tracing::warn!("Claude 凭证无 access_token,跳过: email={}", cred.email);
            continue;
        }

        let id = if !cred.email.is_empty() {
            cred.email.clone()
        } else if !cred.account_uuid.is_empty() {
            cred.account_uuid.clone()
        } else {
            "default".to_string()
        };

        let provider_name = format!("claude-{}", id.replace(['@', '.'], "-"));
        let claude_models = models::default_claude_models();

        let mut metadata = std::collections::HashMap::new();
        metadata.insert(
            "auth_dir".to_string(),
            auth_dir.to_string_lossy().to_string(),
        );
        metadata.insert("email".to_string(), cred.email.clone());
        metadata.insert("account_uuid".to_string(), cred.account_uuid.clone());
        metadata.insert("provider_type".to_string(), "claude".to_string());

        let provider = ProviderConfig::new(
            provider_name.clone(),
            Protocol::Claude,
            vec![constants::DEFAULT_API_BASE_URL.to_string()],
            cred.access_token.clone(),
            proxy_url.map(|s| s.to_string()),
            false,
            claude_models,
        )
        .with_metadata(metadata);

        tracing::info!(
            "加载 Claude 订阅 provider: {} (email: {})",
            provider_name,
            if cred.email.is_empty() {
                "(none)"
            } else {
                &cred.email
            },
        );

        providers.push(provider);
    }

    providers
}
