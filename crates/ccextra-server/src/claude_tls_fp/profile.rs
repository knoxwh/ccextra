//! Distilled Bun / Claude Code TLS ClientHello profile (offline JA3 target).
//!
//! Source of truth for wire capture:
//! `/Users/wanghao/tools/claude-tls-fp-sampler/baselines/claude-code-2.1.296-peet-api-all.json`
//!
//! This module does **not** forge a live Hello; it pins acceptance constants and builds the
//! canonical JA3 string from profile fields so upgrades fail tests loudly.

/// Profile id (bump when sampler SUMMARY changes).
pub const PROFILE_ID: &str = "bun-1.4.3-claude-2.1.296";

/// Claude Code version this profile was captured against.
pub const CLAUDE_CODE_VERSION: &str = "2.1.296";

/// Embedded runtime reported by Claude Code binary.
pub const RUNTIME: &str = "Bun 1.4.3";

/// Target JA3 full string (TLSVersion,Ciphers,Extensions,Curves,EcPointFormats).
pub const TARGET_JA3: &str = "771,4865-4866-4867-49195-49199-49196-49200-52393-52392-49161-49171-49162-49172-156-157-47-53,0-23-65281-10-11-35-16-5-13-18-51-45-43,4588-29-23-24,0";

/// Target JA3 MD5 hash (peet `tls.ja3_hash`).
pub const TARGET_JA3_HASH: &str = "1523504b38f0fae0d881d4b6554aac1b";

/// Target JA4 (peet `tls.ja4`); `h1` = HTTP/1.1 ALPN.
pub const TARGET_JA4: &str = "t13d1713h1_5b57614c22b0_6a3d802a7139";

/// Cipher suites in ClientHello order (IANA decimal, as in JA3).
pub const CIPHERS_IANA: &[u16] = &[
    4865, 4866, 4867, 49195, 49199, 49196, 49200, 52393, 52392, 49161, 49171, 49162, 49172, 156,
    157, 47, 53,
];

/// Extension types in ClientHello order (includes SNI=0 as seen by peet hostname probe).
pub const EXTENSIONS: &[u16] = &[0, 23, 65281, 10, 11, 35, 16, 5, 13, 18, 51, 45, 43];

/// supported_groups / elliptic_curves (JA3 curves field). 4588 = X25519MLKEM768.
pub const SUPPORTED_GROUPS: &[u16] = &[4588, 29, 23, 24];

/// ec_point_formats (JA3 last field).
pub const EC_POINT_FORMATS: &[u16] = &[0];

/// ALPN protocols Claude Code API path offers.
pub const ALPN: &[&str] = &["http/1.1"];

/// Vendored distilled JSON (relative to this module).
pub const PROFILE_JSON: &str = include_str!("profiles/bun-1.4.3-claude-2.1.296.json");

/// Build JA3 string from profile field lists (TLS 1.2 record version 771).
pub fn ja3_string(
    ciphers: &[u16],
    extensions: &[u16],
    groups: &[u16],
    ec_formats: &[u16],
) -> String {
    format!(
        "771,{},{},{},{}",
        join_dash(ciphers),
        join_dash(extensions),
        join_dash(groups),
        join_dash(ec_formats),
    )
}

fn join_dash(vals: &[u16]) -> String {
    vals.iter()
        .map(|v| v.to_string())
        .collect::<Vec<_>>()
        .join("-")
}

/// MD5 hex of a JA3 string (lowercase).
pub fn ja3_hash(ja3: &str) -> String {
    use md5::{Digest, Md5};
    let digest = Md5::digest(ja3.as_bytes());
    hex::encode(digest)
}

/// Official Anthropic API host(s) for fingerprint-lane gating.
pub fn is_official_anthropic_base_url(base_url: &str) -> bool {
    let Ok(url) = reqwest::Url::parse(base_url) else {
        return false;
    };
    url.host_str()
        .is_some_and(|h| h.eq_ignore_ascii_case("api.anthropic.com"))
}

/// Whether this outbound hop should use the Claude Bun fingerprint TLS lane.
///
/// Gate: feature/config enabled + Claude protocol + OAuth oat token + official Anthropic host.
/// Third-party Claude-compatible relays stay on the default rustls lane (scrub only).
pub fn should_use_fingerprint_lane(
    enabled: bool,
    protocol_is_claude: bool,
    api_key: &str,
    base_url: &str,
) -> bool {
    enabled
        && protocol_is_claude
        && crate::claude::is_claude_oauth_token(api_key)
        && is_official_anthropic_base_url(base_url)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn profile_ja3_matches_baseline() {
        let built = ja3_string(CIPHERS_IANA, EXTENSIONS, SUPPORTED_GROUPS, EC_POINT_FORMATS);
        assert_eq!(built, TARGET_JA3);
        assert_eq!(ja3_hash(&built), TARGET_JA3_HASH);
    }

    #[test]
    fn vendored_json_matches_constants() {
        let v: serde_json::Value = serde_json::from_str(PROFILE_JSON).unwrap();
        assert_eq!(v["profile_id"], PROFILE_ID);
        assert_eq!(v["ja3"], TARGET_JA3);
        assert_eq!(v["ja3_hash"], TARGET_JA3_HASH);
        assert_eq!(v["ja4"], TARGET_JA4);
        assert_eq!(v["alpn"][0], "http/1.1");
    }

    #[test]
    fn lane_gate_official_oauth_only() {
        assert!(should_use_fingerprint_lane(
            true,
            true,
            "sk-ant-oat01-abc",
            "https://api.anthropic.com"
        ));
        assert!(!should_use_fingerprint_lane(
            false,
            true,
            "sk-ant-oat01-abc",
            "https://api.anthropic.com"
        ));
        assert!(!should_use_fingerprint_lane(
            true,
            true,
            "sk-ant-api03-abc",
            "https://api.anthropic.com"
        ));
        assert!(!should_use_fingerprint_lane(
            true,
            true,
            "sk-ant-oat01-abc",
            "https://relay.example.com"
        ));
        assert!(!should_use_fingerprint_lane(
            true,
            false,
            "sk-ant-oat01-abc",
            "https://api.anthropic.com"
        ));
    }

    #[test]
    fn cipher_count_matches_ja4_t13d1713() {
        // JA4 `t13d1713…` → 17 cipher suites
        assert_eq!(CIPHERS_IANA.len(), 17);
    }
}
