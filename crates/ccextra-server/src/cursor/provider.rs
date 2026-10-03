// provider.rs:凭证指纹(目录合成在 catalog.rs)
use super::credential::CursorCredential;
use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine};
use sha2::{Digest, Sha256};

pub fn credential_fingerprint(cred: &CursorCredential) -> String {
    let identity = if cred.sub.is_empty() {
        &cred.refresh_token
    } else {
        &cred.sub
    };
    let digest = Sha256::digest(identity.as_bytes());
    URL_SAFE_NO_PAD.encode(&digest[..16])
}
