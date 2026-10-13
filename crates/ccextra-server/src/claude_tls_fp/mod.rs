//! Claude official OAuth TLS fingerprint lane (Bun ClientHello target).
//!
//! Profile + lane gating land first; exact JA3 match is blocked on stock rustls
//! (see [`BLOCKER.md`](BLOCKER.md)). Live peet probe: `cargo run -p ccextra-server --example peet_fp`.

mod client;
mod profile;

pub use client::build_fingerprint_client;
pub use profile::{
    is_official_anthropic_base_url, ja3_hash, ja3_string, should_use_fingerprint_lane, ALPN,
    CIPHERS_IANA, CLAUDE_CODE_VERSION, EC_POINT_FORMATS, EXTENSIONS, PROFILE_ID, PROFILE_JSON,
    RUNTIME, SUPPORTED_GROUPS, TARGET_JA3, TARGET_JA3_HASH, TARGET_JA4,
};
