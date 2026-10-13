# Claude TLS fingerprint — current blocker

Status as of 2026-10-13 (Claude Code 2.1.296 / Bun 1.4.3 baseline).

## Goal

Match peet baseline:

- JA3 hash `1523504b38f0fae0d881d4b6554aac1b`
- JA4 `t13d1713h1_5b57614c22b0_6a3d802a7139`
- HTTP/1.1 only (ALPN `http/1.1`)

## What works today (scaffolding)

- Distilled profile + offline JA3 string/hash unit tests (`profile.rs`).
- Dual-lane wiring in `UpstreamClient`: Claude official OAuth → `api.anthropic.com` always selects `TlsLane::ClaudeBunFp`.
- Fingerprint lane forces **HTTP/1.1** (`http1_only`) and a best-effort rustls `CryptoProvider`
  (cipher / kx group order nudged toward Bun). This improves ALPN/h1 parity vs default H2-prefer
  client, but **does not** produce the Bun JA3.

## Why stock rustls cannot match JA3 yet

Upstream rustls **does not** expose ClientHello customization (extension set/order, GREASE-free
layout, session_ticket / renegotiation_info / status_request / SCT parity, etc.). See rustls
issues #1932 / #2498.

Even with a custom `CryptoProvider`:

1. **Cipher gap**: Bun advertises 17 suites including CBC + RSA (`0xc009`…`0x0035`). aws-lc-rs
   rustls only ships TLS1.3 AEAD + ECDHE-AEAD TLS1.2 — no CBC/RSA suites → JA3 cipher field
   cannot match (`t13d1713` needs 17).
2. **Extension order/set**: rustls emits its own extension list; Bun order is
   `0-23-65281-10-11-35-16-5-13-18-51-45-43` (and local CLI probe omits SNI when talking to IP).
3. **Patched forks are stale vs our lockfile**: `ja-tools` pins rustls `0.23.12`; `craftls` is
   rustls `0.22`. ccextra is on rustls `0.23.45` via reqwest 0.13 — a workspace-wide
   `[patch.crates-io]` would be high risk.

## Next options (plan Option B)

1. **BoringSSL-backed client** (`wreq` / similar) on the fingerprint lane only, hand-spec from
   this profile JSON (still no Bun preset — must craft cipher/extension lists).
2. Or revive/maintain a rustls fork at **0.23.45** with ClientHello override (ja-tools-style).

Fingerprint lane is **always on** for Claude official OAuth → `api.anthropic.com` (no config flag).
Exact JA3 still blocked; refresh profile from `/Users/wanghao/tools/claude-tls-fp-sampler` when CLI upgrades.

## Acceptance when unblocked

`cargo run -p ccextra-server --example peet_fp` must print matching `ja3_hash` and JA4 with `h1`.

## Live peet spike (2026-10-13)

`cargo run -p ccextra-server --example peet_fp` against `https://tls.peet.ws/api/all`:

| Field | Result |
|-------|--------|
| http_version | **HTTP/1.1** (match) |
| ja3_hash | `d11e8eb1142ab01b5f1344fe53d3c64b` (≠ baseline) |
| ja4 | `t13d1011h1_61a7ad8aa9b6_0d308c48d2a3` (`h1` ok; cipher/ext hash ≠) |
| observed ja3 | `771,4865-4866-4867-49195-49199-49196-49200-52393-52392-255,10-23-35-13-0-16-5-11-43-45-51,4588-29-23-24,0` |

Notes from the spike:

- KX groups `4588-29-23-24` already match Bun.
- rustls inserts GREASE cipher `255` and a different extension set/order.
- Cipher count is 10 (JA4 `t13d1011`) vs Bun 17 (`t13d1713`) — CBC/RSA suites still missing.
