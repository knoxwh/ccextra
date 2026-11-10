//! Claude Bun fingerprint-lane HTTP client (wreq + BoringSSL).
//!
//! Matches Claude Code 2.1.316 / Bun 1.4.4 local ClientHello sample (JA3/JA4 + HTTP/1.1);
//! the 2.1.318 sample is identical; 2.1.319 / 2.1.321 embed the same Bun 1.4.4 (not re-sampled).
//! Default (non-Claude) protocols keep reqwest+rustls; all `protocol: claude` use this lane.

use std::borrow::Cow;

use wreq::tls::{AlpnProtocol, ExtensionType, KeyShare, TlsOptions, TlsVersion};
use wreq::Client;

/// Build a wreq client for the Claude Bun fingerprint lane.
pub fn build_fingerprint_client(proxy_key: &str, no_redirect: bool) -> anyhow::Result<Client> {
    // gzip/br/deflate/zstd feature:自动解压 Content-Encoding,与 Claude AE 金样对齐
    let mut builder = Client::builder()
        .pool_max_idle_per_host(4)
        // Match UpstreamClient defaults (shared pool timeout consts).
        .pool_idle_timeout(crate::upstream::POOL_IDLE_TIMEOUT)
        .connect_timeout(crate::upstream::CONNECT_TIMEOUT)
        .tcp_keepalive(crate::upstream::TCP_KEEPALIVE)
        .tcp_nodelay(true)
        .http1_only()
        .tls_options(bun_tls_options());
    if no_redirect {
        builder = builder.redirect(wreq::redirect::Policy::none());
    }
    if proxy_key == "direct" {
        builder = builder.no_proxy();
    } else {
        let proxy = wreq::Proxy::all(proxy_key)
            .map_err(|e| anyhow::anyhow!("解析 fingerprint 代理失败: {e}"))?;
        builder = builder.proxy(proxy);
    }
    builder
        .build()
        .map_err(|e| anyhow::anyhow!("构建 Claude fingerprint HTTP client 失败: {e}"))
}

fn bun_cipher_list() -> &'static str {
    // Bun ClientHello cipher order (Bun 1.4.3 peet baseline; unchanged through Bun 1.4.4).
    "TLS_AES_128_GCM_SHA256:TLS_AES_256_GCM_SHA384:TLS_CHACHA20_POLY1305_SHA256:\
TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256:TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256:\
TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384:TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384:\
TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256:TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256:\
TLS_ECDHE_ECDSA_WITH_AES_128_CBC_SHA:TLS_ECDHE_RSA_WITH_AES_128_CBC_SHA:\
TLS_ECDHE_ECDSA_WITH_AES_256_CBC_SHA:TLS_ECDHE_RSA_WITH_AES_256_CBC_SHA:\
TLS_RSA_WITH_AES_128_GCM_SHA256:TLS_RSA_WITH_AES_256_GCM_SHA384:\
TLS_RSA_WITH_AES_128_CBC_SHA:TLS_RSA_WITH_AES_256_CBC_SHA"
}

/// signature_algorithms in ClientHello order (BoringSSL names; CC 2.1.316+ adds
/// ed25519 + ecdsa_secp521r1_sha512 before rsa_pkcs1_sha1).
const SIGALGS_LIST: &str = "ecdsa_secp256r1_sha256:rsa_pss_rsae_sha256:rsa_pkcs1_sha256:\
ecdsa_secp384r1_sha384:rsa_pss_rsae_sha384:rsa_pkcs1_sha384:\
rsa_pss_rsae_sha512:rsa_pkcs1_sha512:ed25519:ecdsa_secp521r1_sha512:rsa_pkcs1_sha1";

fn bun_tls_options() -> TlsOptions {
    // peet JA3 extensions: 0-23-65281-10-11-35-16-5-13-18-51-45-43
    let ext_order: Vec<ExtensionType> = [0u16, 23, 65281, 10, 11, 35, 16, 5, 13, 18, 51, 45, 43]
        .into_iter()
        .map(ExtensionType::from)
        .collect();
    let key_shares = vec![KeyShare::X25519_MLKEM768, KeyShare::X25519];
    TlsOptions::builder()
        .alpn_protocols([AlpnProtocol::HTTP1])
        .min_tls_version(TlsVersion::TLS_1_2)
        .max_tls_version(TlsVersion::TLS_1_3)
        .session_ticket(true)
        .renegotiation(true)
        .enable_ocsp_stapling(true)
        .enable_signed_cert_timestamps(true)
        .psk_dhe_ke(true)
        .grease_enabled(false)
        .permute_extensions(false)
        .enable_ech_grease(false)
        .curves_list("X25519MLKEM768:X25519:P-256:P-384")
        .sigalgs_list(SIGALGS_LIST)
        .cipher_list(bun_cipher_list())
        .preserve_tls13_cipher_list(true)
        .key_shares(Cow::Owned(key_shares))
        .extension_permutation(Cow::Owned(ext_order))
        .build()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sigalgs_match_vendored_profile() {
        let v: serde_json::Value = serde_json::from_str(super::super::PROFILE_JSON).unwrap();
        let names: Vec<&str> = v["signature_algorithms"]
            .as_array()
            .unwrap()
            .iter()
            .map(|n| n.as_str().unwrap())
            .collect();
        assert_eq!(SIGALGS_LIST, names.join(":"));
    }
}
