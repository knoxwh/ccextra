//! Live peet.ws probe for the Claude Bun fingerprint lane.
//!
//! ```text
//! cargo run -p ccextra-server --example peet_fp
//! ```
//!
//! Compares returned JA3/JA4 against `claude_tls_fp` baseline (wreq/BoringSSL Bun Hello).
//! Exit non-zero on mismatch.

use ccextra_server::claude_tls_fp::{
    build_fingerprint_client, PROFILE_ID, TARGET_JA3_HASH, TARGET_JA4,
};

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let client = build_fingerprint_client("direct", false)?;
    let url = "https://tls.peet.ws/api/all";
    eprintln!("profile={PROFILE_ID}");
    eprintln!("GET {url}");
    let resp = client.get(url).send().await?;
    let status = resp.status();
    let body = resp.text().await?;
    if !status.is_success() {
        anyhow::bail!("peet HTTP {status}: {body}");
    }
    let v: serde_json::Value = serde_json::from_str(&body)?;
    let ja3_hash = v["tls"]["ja3_hash"].as_str().unwrap_or("");
    let ja4 = v["tls"]["ja4"].as_str().unwrap_or("");
    let ja3 = v["tls"]["ja3"].as_str().unwrap_or("");
    let http_version = v["http_version"].as_str().unwrap_or("");
    println!("http_version={http_version}");
    println!("ja3={ja3}");
    println!("ja3_hash={ja3_hash}");
    println!("ja4={ja4}");
    println!("target_ja3_hash={TARGET_JA3_HASH}");
    println!("target_ja4={TARGET_JA4}");

    let mut ok = true;
    if http_version != "HTTP/1.1" {
        eprintln!("FAIL http_version want HTTP/1.1 got {http_version}");
        ok = false;
    } else {
        eprintln!("OK http_version=HTTP/1.1");
    }
    if ja3_hash == TARGET_JA3_HASH {
        eprintln!("OK ja3_hash match");
    } else {
        eprintln!("FAIL ja3_hash mismatch (see crates/ccextra-server/src/claude_tls_fp/STATUS.md)");
        ok = false;
    }
    if ja4 == TARGET_JA4 {
        eprintln!("OK ja4 match");
    } else {
        eprintln!("FAIL ja4 mismatch (got {ja4})");
        ok = false;
    }
    if !ok {
        std::process::exit(2);
    }
    Ok(())
}
