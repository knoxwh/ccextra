// Antigravity 动态 provider 注入
use crate::antigravity::{constants, models, refresh, store};
use ccextra_core::route::{Protocol, ProviderConfig};
use std::path::Path;

/// AG 加载结果:区分「没有可用凭证」与「有凭证但拉取失败」。
/// 前者空结果属正常(其它 provider 照常发布),后者应保留旧快照。
pub enum AntigravityLoad {
    /// 目录不存在/无凭证/凭证全禁用或全空 token
    NoCredentials,
    /// 扫描到可用凭证;列表为空表示刷新/发现/模型拉取全部失败
    Loaded(Vec<ProviderConfig>),
}

impl AntigravityLoad {
    /// 不区分失败语义的调用方(如 reload)直接取列表
    pub fn into_providers(self) -> Vec<ProviderConfig> {
        match self {
            Self::NoCredentials => Vec::new(),
            Self::Loaded(providers) => providers,
        }
    }
}

/// 从 auth_dir 扫描 antigravity 凭证，为每个有效凭证生成一个 provider；
/// filter 为模型白名单(空 = 全量)
pub async fn load_antigravity_providers(
    auth_dir: &Path,
    proxy_url: Option<&str>,
    filter: &[String],
    user_agent: Option<&str>,
) -> AntigravityLoad {
    let mut providers = Vec::new();

    let credentials = match store::list(auth_dir) {
        Ok(creds) => creds,
        Err(e) => {
            // 扫描失败按「拉取失败」处理,保留旧快照
            tracing::debug!("扫描 antigravity 凭证失败: {}", e);
            return AntigravityLoad::Loaded(providers);
        }
    };

    // 是否见到可用凭证(非禁用、非空 token):全没有则属「没有凭证」而非失败
    let mut usable_seen = false;
    for (path, cred) in credentials {
        if cred.disabled {
            tracing::debug!("跳过已禁用的 antigravity 凭证: {}", cred.email);
            continue;
        }

        // access_token 与 refresh_token 都为空才跳过;仅 access 空先走刷新
        if cred.access_token.trim().is_empty() && cred.refresh_token.trim().is_empty() {
            tracing::debug!("跳过空 token 的 antigravity 凭证: {}", cred.email);
            continue;
        }
        usable_seen = true;

        // 刷新与 project 发现走单飞 ensure(对齐 CLIProxyAPI refreshSkew):与请求路径
        // 共用 refresh_token 锁,避免轮换型 refresh token 被后台与请求双消费(invalid_grant)
        let cred = match refresh::ensure_credential_fresh_with_ua(
            &path, proxy_url, None, user_agent,
        )
        .await
        {
            Ok(cred) => cred,
            Err(e) => {
                tracing::warn!(
                    "刷新 Antigravity token 失败,跳过该凭证: {}: {}",
                    cred.email,
                    e
                );
                continue;
            }
        };

        // 对齐 CPA ensureAntigravityProjectID:补发现后仍缺 project 则跳过
        if cred.project_id.trim().is_empty() {
            tracing::warn!("跳过无 project_id 的 antigravity 凭证: {}", cred.email);
            continue;
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

    if usable_seen {
        AntigravityLoad::Loaded(providers)
    } else {
        AntigravityLoad::NoCredentials
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::antigravity::AntigravityCredential;

    /// 目录不存在、凭证全禁用或全空 token:NoCredentials(空结果属正常)
    #[tokio::test]
    async fn no_credentials_when_missing_disabled_or_empty() {
        let dir = tempfile::tempdir().unwrap();
        assert!(matches!(
            load_antigravity_providers(&dir.path().join("missing"), Some("direct"), &[], None)
                .await,
            AntigravityLoad::NoCredentials
        ));

        let mut disabled = AntigravityCredential::new(
            "tok".into(),
            "ref".into(),
            3600,
            "off@b.c".into(),
            "proj".into(),
        );
        disabled.disabled = true;
        let empty = AntigravityCredential::new(
            " ".into(),
            String::new(),
            3600,
            "empty@b.c".into(),
            "proj".into(),
        );
        store::save_to(&dir.path().join("antigravity-off.json"), &disabled).unwrap();
        store::save_to(&dir.path().join("antigravity-empty.json"), &empty).unwrap();
        assert!(matches!(
            load_antigravity_providers(dir.path(), Some("direct"), &[], None).await,
            AntigravityLoad::NoCredentials
        ));
    }

    /// 有可用凭证即 Loaded;模型拉取失败(非法代理)只清空模型,不改变分类
    #[tokio::test]
    async fn loaded_when_usable_credential_exists() {
        let dir = tempfile::tempdir().unwrap();
        let cred = AntigravityCredential::new(
            "tok".into(),
            "ref".into(),
            3600,
            "ok@b.c".into(),
            "proj".into(),
        );
        store::save_to(&dir.path().join("antigravity-ok.json"), &cred).unwrap();
        let load = load_antigravity_providers(dir.path(), Some("invalid-proxy"), &[], None).await;
        match load {
            AntigravityLoad::Loaded(providers) => {
                assert_eq!(providers.len(), 1);
                assert!(providers[0].models.is_empty());
            }
            AntigravityLoad::NoCredentials => panic!("可用凭证应归类 Loaded"),
        }
    }
}
