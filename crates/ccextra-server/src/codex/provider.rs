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

    for (path, mut cred) in credentials {
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
                if let Err(e) = store::save_to(&path, &cred) {
                    tracing::warn!("刷新后保存 Codex 凭证失败: {}: {}", cred.email, e);
                } else {
                    tracing::info!("Codex token 已刷新并保存: {}", cred.email);
                }
            }
            Ok(false) => {
                tracing::debug!("Codex token 仍然有效: {}", cred.email);
            }
            Err(e) => {
                tracing::warn!("刷新 Codex token 失败,跳过该凭证: {}: {}", cred.email, e);
                continue;
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
        // 运行时按扫描到的实际文件刷新/落盘(对齐 CPA Auth.Attributes["path"])
        metadata.insert(
            "credential_path".to_string(),
            path.to_string_lossy().to_string(),
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
            true, // prompt_cache_key: session 亲和(对齐 CPA)
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codex::CodexCredential;

    fn fresh_cred(email: &str, account_id: &str) -> CodexCredential {
        CodexCredential {
            r#type: "codex".to_string(),
            id_token: String::new(),
            access_token: "live-access".to_string(),
            refresh_token: "live-refresh".to_string(),
            account_id: account_id.to_string(),
            last_refresh: String::new(),
            email: email.to_string(),
            plan_type: "plus".to_string(),
            expired: "2999-01-01T00:00:00Z".to_string(),
            disabled: false,
        }
    }

    /// 非 canonical 文件名(如 CPA 的 codex-<email>-plus.json)与无 email/account_id 的凭证:
    /// provider 记录实际路径,运行时按该路径取凭证
    #[tokio::test]
    async fn runtime_uses_scanned_credential_path() {
        let dir = tempfile::tempdir().unwrap();
        for (file, cred) in [
            ("codex-a@b.c-plus.json", fresh_cred("a@b.c", "acct-1")),
            ("codex-anon.json", fresh_cred("", "")),
        ] {
            let path = dir.path().join(file);
            store::save_to(&path, &cred).unwrap();
        }

        let providers = load_codex_providers(dir.path(), Some("direct")).await;
        assert_eq!(providers.len(), 2);
        for provider in &providers {
            let path = provider.metadata.as_ref().unwrap()["credential_path"].clone();
            assert!(std::path::Path::new(&path).exists(), "{path}");
            let cred = refresh::ensure_credential_fresh(std::path::Path::new(&path), None, None)
                .await
                .unwrap();
            assert_eq!(cred.access_token, "live-access");
        }
        // 不会额外生成 canonical 文件
        assert_eq!(
            std::fs::read_dir(dir.path())
                .unwrap()
                .filter(|e| e
                    .as_ref()
                    .unwrap()
                    .path()
                    .extension()
                    .is_some_and(|x| x == "json"))
                .count(),
            2
        );
    }

    /// 扫描跳过规则:已禁用、access/refresh 皆空、坏 JSON、非 codex type 均不注入
    #[tokio::test]
    async fn skips_disabled_empty_and_invalid_credentials() {
        let dir = tempfile::tempdir().unwrap();
        let mut disabled = fresh_cred("off@b.c", "");
        disabled.disabled = true;
        let mut empty = fresh_cred("empty@b.c", "");
        empty.access_token = " ".to_string();
        empty.refresh_token = String::new();
        let mut other_type = fresh_cred("xai@b.c", "");
        other_type.r#type = "xai".to_string();
        for (file, cred) in [
            ("ok.json", fresh_cred("ok@b.c", "")),
            ("disabled.json", disabled),
            ("empty.json", empty),
            ("other.json", other_type),
        ] {
            store::save_to(&dir.path().join(file), &cred).unwrap();
        }
        std::fs::write(dir.path().join("bad.json"), "{not json").unwrap();

        let providers = load_codex_providers(dir.path(), Some("direct")).await;
        let names: Vec<_> = providers.iter().map(|p| p.name.as_str()).collect();
        assert_eq!(names, ["codex-ok-b-c"]);
    }
}
