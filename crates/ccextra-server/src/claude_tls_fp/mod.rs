//! Claude protocol TLS fingerprint lane (Bun ClientHello via wreq/BoringSSL).
//!
//! Live peet probe: `cargo run -p ccextra-server --example peet_fp`.

mod client;
mod profile;

pub use client::build_fingerprint_client;
pub use profile::{
    ja3_hash, ja3_string, should_use_fingerprint_lane, ALPN, CIPHERS_IANA, CLAUDE_CODE_VERSION,
    EC_POINT_FORMATS, EXTENSIONS, PROFILE_ID, PROFILE_JSON, RUNTIME, SUPPORTED_GROUPS, TARGET_JA3,
    TARGET_JA3_HASH, TARGET_JA4,
};
