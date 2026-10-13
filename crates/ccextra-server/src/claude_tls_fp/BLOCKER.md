# Claude TLS fingerprint — status

Status as of 2026-10-13 (Claude Code 2.1.296 / Bun 1.4.3 baseline).

## Goal

Match peet baseline:

- JA3 hash `1523504b38f0fae0d881d4b6554aac1b`
- JA4 `t13d1713h1_5b57614c22b0_6a3d802a7139`
- HTTP/1.1 only (ALPN `http/1.1`)

## Current solution (Option B — BoringSSL / wreq)

Fingerprint lane uses **wreq 0.16 + BoringSSL (`btls`)** with a hand-tuned `TlsOptions`
spec from the sampler peet JSON (cipher list, curves, sigalgs, key shares, extension
order, GREASE off, OCSP + SCT on, ALPN `http/1.1` only).

Default protocols stay on **reqwest + rustls**. Lane is always on for Claude official
OAuth (`sk-ant-oat*`) → `api.anthropic.com` (no config flag).

## Acceptance

`cargo run -p ccextra-server --example peet_fp` must print matching `ja3_hash` and JA4 with `h1`.

## Live peet spike (wreq lane, 2026-10-13)

| Field | Result |
|-------|--------|
| http_version | **HTTP/1.1** (match) |
| ja3_hash | `1523504b38f0fae0d881d4b6554aac1b` (**match**) |
| ja4 | `t13d1713h1_5b57614c22b0_6a3d802a7139` (**match**) |

Stock rustls CryptoProvider nudge path was removed; it could not advertise CBC/RSA suites
or Bun extension order (see git history / prior notes).

Refresh profile from `/Users/wanghao/tools/claude-tls-fp-sampler` when Claude upgrades.
