// Antigravity 动态 provider 注入
use crate::antigravity::{constants, models, refresh, store};
use ccextra_core::route::{Protocol, ProviderConfig};
use std::path::Path;

/// 从 auth_dir 扫描 antigravity 凭证，为每个有效凭证生成一个 provider
pub async fn load_antigravity_providers(
    auth_dir: &Path,
    proxy_url: Option<&str>,
) -> Vec<ProviderConfig> {
    let mut providers = Vec::new();

    let credentials = match store::list(auth_dir) {
        Ok(creds) => creds,
        Err(e) => {
            tracing::debug!("扫描 antigravity 凭证失败: {}", e);
            return providers;
        }
    };

    for (_path, mut cred) in credentials {
        if cred.disabled {
            tracing::debug!("跳过已禁用的 antigravity 凭证: {}", cred.email);
            continue;
        }

        // access_token 与 refresh_token 都为空才跳过;仅 access 空先走刷新
        if cred.access_token.trim().is_empty() && cred.refresh_token.trim().is_empty() {
            tracing::debug!("跳过空 token 的 antigravity 凭证: {}", cred.email);
            continue;
        }

        // 检查并刷新 token（对齐 CLIProxyAPI 的 refreshSkew）
        match refresh::refresh_if_needed(&mut cred, proxy_url, constants::REFRESH_SKEW_SECS).await {
            Ok(true) => {
                // token 已刷新，回写到文件
                if let Err(e) = store::save(auth_dir, &cred) {
                    tracing::warn!("刷新后保存凭证失败: {}: {}", cred.email, e);
                } else {
                    tracing::info!("Antigravity token 已刷新并保存: {}", cred.email);
                }
            }
            Ok(false) => {
                // token 仍然有效，无需刷新
                tracing::debug!("Antigravity token 仍然有效: {}", cred.email);
            }
            Err(e) => {
                tracing::warn!("刷新 Antigravity token 失败: {}: {}", cred.email, e);
                // 即使刷新失败，仍然尝试使用现有 token
            }
        }

        // 使用 email 作为 provider 名称（如果为空则使用 "antigravity"）
        let provider_name = if cred.email.is_empty() {
            "antigravity".to_string()
        } else {
            format!("antigravity-{}", cred.email.replace(['@', '.'], "-"))
        };

        // 获取模型列表
        let project_id_ref = if cred.project_id.is_empty() {
            None
        } else {
            Some(cred.project_id.as_str())
        };

        let models = match models::fetch_models(&cred.access_token, project_id_ref, proxy_url).await
        {
            Ok(m) => {
                tracing::info!("为 {} 获取了 {} 个模型", cred.email, m.len());
                m
            }
            Err(e) => {
                tracing::warn!("无法为 {} 获取模型列表: {}，使用空列表", cred.email, e);
                Vec::new()
            }
        };

        // 创建 ProviderConfig
        // Antigravity 使用 cloudcode-pa.googleapis.com 的内部 API,不是公开的 generativelanguage API
        let mut metadata = std::collections::HashMap::new();
        metadata.insert(
            "auth_dir".to_string(),
            auth_dir.to_string_lossy().to_string(),
        );
        metadata.insert("email".to_string(), cred.email.clone());
        if !cred.project_id.is_empty() {
            metadata.insert("project_id".to_string(), cred.project_id.clone());
        }

        let provider = ProviderConfig::new(
            provider_name.clone(),
            Protocol::Antigravity,
            // 对齐 CLIProxyAPI 回退顺序:daily 优先,prod 兜底
            vec![
                constants::DAILY_API_ENDPOINT.to_string(),
                constants::API_ENDPOINT.to_string(),
            ],
            cred.access_token.clone(),
            proxy_url.map(|s| s.to_string()),
            false, // prompt_cache_key
            models,
        )
        .with_metadata(metadata);

        tracing::info!(
            "加载 antigravity provider: {} (email: {}, project: {})",
            provider_name,
            cred.email,
            if cred.project_id.is_empty() {
                "(none)"
            } else {
                &cred.project_id
            }
        );

        providers.push(provider);
    }

    providers
}
