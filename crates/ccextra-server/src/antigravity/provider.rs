// Antigravity 动态 provider 注入
use crate::antigravity::{constants, models, oauth, project, refresh, store};
use ccextra_core::route::{Protocol, ProviderConfig};
use std::path::Path;

/// 从 auth_dir 扫描 antigravity 凭证，为每个有效凭证生成一个 provider；
/// filter 为模型白名单(空 = 全量)
pub async fn load_antigravity_providers(
    auth_dir: &Path,
    proxy_url: Option<&str>,
    filter: &[String],
    user_agent: Option<&str>,
) -> Vec<ProviderConfig> {
    let mut providers = Vec::new();

    let credentials = match store::list(auth_dir) {
        Ok(creds) => creds,
        Err(e) => {
            tracing::debug!("扫描 antigravity 凭证失败: {}", e);
            return providers;
        }
    };

    for (path, mut cred) in credentials {
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
                if let Err(e) = store::save_to(&path, &cred) {
                    tracing::warn!("刷新后保存凭证失败: {}: {}", cred.email, e);
                } else {
                    tracing::info!("Antigravity token 已刷新并保存: {}", cred.email);
                }
            }
            Ok(false) => {
                tracing::debug!("Antigravity token 仍然有效: {}", cred.email);
            }
            Err(e) => {
                tracing::warn!(
                    "刷新 Antigravity token 失败,跳过该凭证: {}: {}",
                    cred.email,
                    e
                );
                continue;
            }
        }

        // 对齐 CPA ensureAntigravityProjectID:缺 project 时补发现,仍空则跳过
        if cred.project_id.trim().is_empty() {
            match oauth::http_client(proxy_url) {
                Ok(client) => {
                    match project::fetch_project_id(&client, &cred.access_token, user_agent).await {
                        Ok(id) if !id.trim().is_empty() => {
                            cred.project_id = id;
                            if let Err(e) = store::save_to(&path, &cred) {
                                tracing::warn!("project 发现后保存失败: {}: {}", cred.email, e);
                            }
                        }
                        Ok(_) => {
                            tracing::warn!("跳过无 project_id 的 antigravity 凭证: {}", cred.email);
                            continue;
                        }
                        Err(e) => {
                            tracing::warn!(
                                "跳过无 project_id 的 antigravity 凭证: {}: {}",
                                cred.email,
                                e
                            );
                            continue;
                        }
                    }
                }
                Err(e) => {
                    tracing::warn!(
                        "跳过无 project_id 的 antigravity 凭证: {}: {}",
                        cred.email,
                        e
                    );
                    continue;
                }
            }
        }

        // 使用 email 作为 provider 名称（如果为空则使用 "antigravity"）
        let provider_name = if cred.email.is_empty() {
            "antigravity".to_string()
        } else {
            format!("antigravity-{}", cred.email.replace(['@', '.'], "-"))
        };

        let models = match models::fetch_models(
            &cred.access_token,
            Some(cred.project_id.as_str()),
            proxy_url,
            user_agent,
            filter,
        )
        .await
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

        // Antigravity 使用 cloudcode-pa.googleapis.com 的内部 API,不是公开的 generativelanguage API
        let mut metadata = std::collections::HashMap::new();
        // 运行时按扫描到的实际文件刷新/落盘(对齐 CPA Auth.Attributes["path"])
        metadata.insert(
            "credential_path".to_string(),
            path.to_string_lossy().to_string(),
        );
        metadata.insert("email".to_string(), cred.email.clone());
        metadata.insert("project_id".to_string(), cred.project_id.clone());
        // 连接池 scope 优先用 refresh_token（跨 access 刷新仍稳定，对齐 CPA）
        if !cred.refresh_token.trim().is_empty() {
            metadata.insert("refresh_token".to_string(), cred.refresh_token.clone());
        }

        let provider = ProviderConfig::new(
            provider_name.clone(),
            Protocol::Antigravity,
            // 对齐 CLIProxyAPI resolveAntigravityRequestBaseURL:
            // 纯直连主力 daily 端点,禁止跨层回退(cross-tier fallback 会切集群导致对话缓存归 0)
            vec![constants::DAILY_API_ENDPOINT.to_string()],
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
            cred.project_id
        );

        providers.push(provider);
    }

    providers
}
