# Claude TLS fingerprint — status

Status as of 2026-10-13 (Claude Code 2.1.296 / Bun 1.4.3 baseline).

Verified 2026-10-14: Claude Code **2.1.299** (still Bun 1.4.3) re-sampled; JA3 / JA4 / peetprint match the 2.1.296 baseline — no TLS constant changes needed.

## Goal

Match peet baseline:

- JA3 hash `1523504b38f0fae0d881d4b6554aac1b`
- JA4 `t13d1713h1_5b57614c22b0_6a3d802a7139`
- HTTP/1.1 only (ALPN `http/1.1`)

## Current solution (wreq / BoringSSL)

Fingerprint lane uses **wreq 0.16 + BoringSSL (`btls`)** with a hand-tuned `TlsOptions`
spec from the sampler peet JSON (cipher list, curves, sigalgs, key shares, extension
order, GREASE off, OCSP + SCT on, ALPN `http/1.1` only).

Non-Claude protocols stay on **reqwest + rustls**. Lane is always on for every
config `protocol: claude` provider (official Anthropic and third-party Claude-compatible;
no config flag).

## Acceptance

`cargo run -p ccextra-server --example peet_fp` must print matching `ja3_hash` and JA4 with `h1`.

## Live peet spike (wreq lane, 2026-10-13)

| Field | Result |
|-------|--------|
| http_version | **HTTP/1.1** (match) |
| ja3_hash | `1523504b38f0fae0d881d4b6554aac1b` (**match**) |
| ja4 | `t13d1713h1_5b57614c22b0_6a3d802a7139` (**match**) |

Stock rustls CryptoProvider nudge could not advertise CBC/RSA suites or Bun extension
order; that path was removed in favor of wreq/BoringSSL.

Refresh the vendored profile from the local `claude-tls-fp-sampler` baselines when
Claude Code upgrades (see that tool's README / `sample.sh`).

## HTTP Accept-Encoding (L7)

Claude passthrough keeps inbound `Accept-Encoding`, or fills `gzip, deflate, br, zstd`
(CLI/CPA). Both the wreq fingerprint lane and default reqwest client enable
gzip/brotli/deflate/zstd auto-decompress so SSE/JSON stay readable.

