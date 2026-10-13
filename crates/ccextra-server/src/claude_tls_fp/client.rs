//! Best-effort fingerprint-lane HTTP client (HTTP/1.1 + nudged rustls).
//!
//! Exact Bun JA3 is **not** achievable with stock rustls — see `BLOCKER.md`.
//! This builder still isolates a dedicated pool with `http1_only` and prefers
//! Bun-like kx group order (X25519MLKEM768 first) so the lane is ready when a
//! Hello-override backend lands.

use std::sync::Arc;

use anyhow::Context;
use reqwest::Client;
use rustls::crypto::aws_lc_rs::{self, kx_group};
use rustls::ClientConfig;

/// Build a reqwest client for `TlsLane::ClaudeBunFp`.
///
/// - Forces HTTP/1.1 (Claude Code API ALPN).
/// - Uses a dedicated rustls `CryptoProvider` with kx groups ordered like the
///   Bun capture: X25519MLKEM768, X25519, P-256, P-384.
/// - Cipher suites reordered toward Bun's TLS1.3 preference (AES-128 first);
///   CBC/RSA suites from the baseline are unavailable in aws-lc-rs rustls.
pub fn build_fingerprint_client(proxy_key: &str, no_redirect: bool) -> anyhow::Result<Client> {
    let tls = bun_like_rustls_config().context("构建 Claude Bun fingerprint rustls 配置失败")?;
    let mut builder = Client::builder()
        .pool_max_idle_per_host(4)
        .pool_idle_timeout(std::time::Duration::from_secs(90))
        .connect_timeout(std::time::Duration::from_secs(10))
        .tcp_keepalive(std::time::Duration::from_secs(60))
        .tcp_nodelay(true)
        .http1_only()
        .use_preconfigured_tls(tls);
    if no_redirect {
        builder = builder.redirect(reqwest::redirect::Policy::none());
    }
    if proxy_key == "direct" {
        builder = builder.no_proxy();
    } else if let Ok(proxy) = reqwest::Proxy::all(proxy_key) {
        builder = builder.proxy(proxy);
    }
    builder
        .build()
        .map_err(|e| anyhow::anyhow!("构建 Claude fingerprint HTTP client 失败: {e}"))
}

fn bun_like_rustls_config() -> anyhow::Result<ClientConfig> {
    // Reorder TLS1.3 toward Bun (1301,1302,1303); keep available TLS1.2 ECDHE-AEAD only.
    let cipher_suites = vec![
        aws_lc_rs::cipher_suite::TLS13_AES_128_GCM_SHA256,
        aws_lc_rs::cipher_suite::TLS13_AES_256_GCM_SHA384,
        aws_lc_rs::cipher_suite::TLS13_CHACHA20_POLY1305_SHA256,
        aws_lc_rs::cipher_suite::TLS_ECDHE_ECDSA_WITH_AES_128_GCM_SHA256,
        aws_lc_rs::cipher_suite::TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256,
        aws_lc_rs::cipher_suite::TLS_ECDHE_ECDSA_WITH_AES_256_GCM_SHA384,
        aws_lc_rs::cipher_suite::TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384,
        aws_lc_rs::cipher_suite::TLS_ECDHE_ECDSA_WITH_CHACHA20_POLY1305_SHA256,
        aws_lc_rs::cipher_suite::TLS_ECDHE_RSA_WITH_CHACHA20_POLY1305_SHA256,
    ];
    let kx_groups: Vec<&'static dyn rustls::crypto::SupportedKxGroup> = vec![
        kx_group::X25519MLKEM768,
        kx_group::X25519,
        kx_group::SECP256R1,
        kx_group::SECP384R1,
    ];

    let mut provider = aws_lc_rs::default_provider();
    provider.cipher_suites = cipher_suites;
    provider.kx_groups = kx_groups;

    let mut roots = rustls::RootCertStore::empty();
    let native = rustls_native_certs::load_native_certs();
    for cert in native.certs {
        let _ = roots.add(cert);
    }
    if !native.errors.is_empty() {
        tracing::debug!(
            errors = native.errors.len(),
            "加载系统根证书时有部分错误(已尽量收录可用项)"
        );
    }
    if roots.is_empty() {
        anyhow::bail!("系统根证书为空,无法构建 fingerprint TLS");
    }

    let mut config = ClientConfig::builder_with_provider(Arc::new(provider))
        .with_safe_default_protocol_versions()
        .map_err(|e| anyhow::anyhow!("rustls 协议版本: {e}"))?
        .with_root_certificates(roots)
        .with_no_client_auth();
    // Mirror Claude Code API ALPN (http/1.1 only). reqwest http1_only also constrains h1.
    config.alpn_protocols = vec![b"http/1.1".to_vec()];
    Ok(config)
}
