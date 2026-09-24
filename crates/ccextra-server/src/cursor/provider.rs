use super::{constants, credential::CursorCredential, models, refresh, store};
use anyhow::Result;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use ccextra_core::route::{Protocol, ProviderConfig};
use sha2::{Digest, Sha256};
use std::path::Path;

pub fn credential_fingerprint(cred: &CursorCredential) -> String {
    let identity = if cred.sub.is_empty() {
        &cred.refresh_token
    } else {
        &cred.sub
    };
    let digest = Sha256::digest(identity.as_bytes());
    URL_SAFE_NO_PAD.encode(&digest[..16])
}

pub async fn load_cursor_provider(
    auth_dir: &Path,
    base_url: Option<&str>,
    client_version: Option<&str>,
    default_model: Option<&str>,
    proxy_url: Option<&str>,
) -> Result<Option<ProviderConfig>> {
    if !store::credential_path(auth_dir).exists() {
        return Ok(None);
    }
    let cred = refresh::ensure_credential_fresh(auth_dir, proxy_url, None).await?;
    let base_url = base_url.unwrap_or(constants::DEFAULT_BASE_URL);
    let client_version = client_version.unwrap_or(constants::DEFAULT_CLIENT_VERSION);
    let models =
        models::fetch_models(base_url, client_version, &cred.access_token, proxy_url).await?;

    let fingerprint = credential_fingerprint(&cred);
    let mut metadata = std::collections::HashMap::new();
    metadata.insert(
        "auth_dir".to_string(),
        auth_dir.to_string_lossy().to_string(),
    );
    metadata.insert("credential_id".to_string(), fingerprint);
    metadata.insert("client_version".to_string(), client_version.to_string());
    if let Some(model) = default_model {
        metadata.insert("default_model".to_string(), model.to_string());
    }
    Ok(Some(
        ProviderConfig::new(
            "cursor".to_string(),
            Protocol::Cursor,
            vec![base_url.to_string()],
            cred.access_token,
            proxy_url.map(str::to_owned),
            false,
            models,
        )
        .with_metadata(metadata),
    ))
}
