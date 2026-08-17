# ccextra

**[中文](README.md)** | **English**

> Single-process Rust proxy that connects Claude Code to any upstream: protocol translation, prompt-cache optimization, and model routing

[![Rust](https://img.shields.io/badge/rust-1.75%2B-orange)](https://www.rust-lang.org/)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Workspace](https://img.shields.io/badge/workspace-3%20crates-lightgrey)]()
[![Docs: design](https://img.shields.io/badge/docs-architecture-informational)](docs/design.md)

One binary, one port. Serves four upstream protocols at once: native Claude, OpenAI Chat Completions, OpenAI Responses, and Google Gemini. Routes Claude Code requests by model to the matching provider, and deterministically normalizes request bodies so upstream prompt caches hit as often as possible.

```
Claude Code          (ANTHROPIC_BASE_URL → http://127.0.0.1:8222)
     │  POST /v1/messages
     ▼
ccextra :8222  ──  route → normalize → convert → upstream
     │  Response: streaming SSE / non-stream JSON back the same path
     ├── claude            → native Claude protocol
     ├── openai_chat       → OpenAI Chat Completions
     ├── openai_responses  → OpenAI Responses
     └── gemini            → Google Gemini API
```

## Features

| Category | What it does |
| ---- | ---- |
| **Multi-protocol** | Native Claude / OpenAI Chat Completions / OpenAI Responses / Google Gemini |
| **Model routing** | Inbound model name resolves via alias to exactly one provider. Conflicts fail at startup; no implicit fallback |
| **Byte-level passthrough** | Claude → Claude changes only the `model` field. Remaining bytes stay intact so normalization is not undone |
| **Prompt-cache optimization** | Nine-module normalization kills serialization drift so upstream prompt cache can hit. A drift detector watches blind spots across turns |
| **Streaming state machines** | Hand-written SSE parser plus three independent relay paths. A dropped stream emits a structured error event instead of a bare hang-up |
| **Hot reload** | `POST /reload` updates providers / payload / normalize / `logging.request_body` / secret / global proxy without restart, and clears the bcrypt verify cache. `logging.level` applies at startup only |
| **Proxy** | Global default plus per-provider override. SOCKS supported |
| **Payload overrides** | Wildcard match on model name (e.g. `*glm*`) to override request params. Can be scoped to a protocol |
| **Ingress auth** | Optional `secret_key`. Plaintext is hashed to bcrypt and written back. Verify results are cached |
| **Model list** | `GET /v1/models` returns an Anthropic-shaped catalog. Claude Code fetches this on startup |
| **Diagnostics** | Optional per-request dump of the upstream body, for cache-drift debugging |

## How it works

```
Claude Code → ccextra:8222
    ↓
1. Parse inbound Anthropic body + ingress auth
2. Route: model → provider → protocol
3. Pre-transform normalize (full set for Claude / slim subset for OpenAI)
4. Protocol convert (three body-to-body paths; content shape normalized)
5. Post-transform normalize (convert paths only) + drift observe
6. Payload overrides (wildcard match, optional protocol scope)
7. Strip prompt_cache_retention (OpenAI paths only)
8. Inject prompt_cache_key (per-provider switch) + optional diagnostic dump
9. Claude passthrough: rebuild anthropic-beta + forward identity headers
10. Upstream request (reqwest + protocol-specific UA + proxy)
11. Relay response (streaming SSE state machine / non-stream: Claude byte-passthrough, OpenAI via non_stream back to Anthropic; convert failure returns upstream bytes as-is; upstream errors mapped to Anthropic shape)
    ↓
Upstream provider
```

## Quick start

### Build

```bash
cargo build --release
```

Requires Rust 1.75+.

### Configure

```bash
cp config.example.yaml config.yaml
# Edit config.yaml and fill in real keys
```

Minimal example:

```yaml
server:
  host: "127.0.0.1"
  port: 8222

providers:
  - name: claude
    protocol: claude
    base_url: https://xxxx
    key: sk-ant-xxx
    models:
      - name: claude-opus-5
        alias: claude-opus-5

  - name: saic
    protocol: openai_chat
    base_url: https://xxxx/compatible-mode/v1
    key: sk-xxx
    models:
      - name: glm-5.1
        alias: glm-5.1

  - name: ckff-codex
    protocol: openai_responses
    base_url: https://xxxx/v1
    key: sk-xxx
    prompt_cache_key: true
    models:
      - name: gpt-5.6-terra
        alias: gpt-5.6-terra

  - name: gemini
    protocol: gemini
    base_url: https://generativelanguage.googleapis.com
    key: YOUR_GEMINI_API_KEY
    models:
      - name: gemini-2.0-flash-exp
        alias: gemini-flash
```

### Run

```bash
./target/release/ccextra --config config.yaml
```

Or use the scripts:

```bash
./start.sh    # start in background
./stop.sh     # stop
./restart.sh  # restart
./build.sh    # build (restarts if already running)
```

### Point Claude Code at it

```bash
export ANTHROPIC_BASE_URL=http://127.0.0.1:8222
```

If `secret_key` is set:

```bash
export ANTHROPIC_AUTH_TOKEN=sk-ccextra-xxx
```

### Verify

```bash
# Send a request
curl http://127.0.0.1:8222/v1/messages \
  -H "Content-Type: application/json" \
  -d '{"model": "claude-opus-5", "messages": [{"role": "user", "content": "hi"}], "max_tokens": 1024}'

# Model list (Claude Code fetches this on startup)
curl http://127.0.0.1:8222/v1/models -H "x-api-key: sk-ccextra-xxx"

# Hot-reload config
curl -X POST http://127.0.0.1:8222/reload

# Health check
curl http://127.0.0.1:8222/health
```

`/v1/models` returns an Anthropic-shaped catalog:

```json
{
  "data": [
    {"id": "claude-opus-5", "object": "model", "owned_by": "claude",
     "type": "model", "display_name": "claude-opus-5",
     "max_input_tokens": 200000, "max_tokens": 64000}
  ]
}
```

## Configuration

Full knobs live in [`config.example.yaml`](config.example.yaml). Highlights:

| Key | Meaning |
| ------ | ---- |
| `server.host` / `server.port` | Listen address. Default `127.0.0.1:8222` |
| `server.proxy_url` | Global proxy fallback, optional. `"direct"` means no proxy |
| `secret_key` | Ingress auth, optional. When set, `/v1/models` and `/v1/messages` require a matching key or they 401. Accepts `x-api-key` or `Authorization: Bearer`. Plaintext is hashed to bcrypt and written back. `/reload` swaps the secret and clears the bcrypt verify cache |
| `providers[].protocol` | Upstream protocol: `claude` / `openai_chat` / `openai_responses` / `gemini` |
| `providers[].base_url` / `key` | Upstream URL and API key |
| `providers[].models[].alias` | Inbound model name → real upstream model name |
| `providers[].prompt_cache_key` | Cache-bucket key = session ID (aligned with Codex 0.147). OpenAI protocols only |
| `payload` | Wildcard match on model name (`*glm*`) to override request params. Can be scoped with `protocol` |
| `normalize.enabled` | Master switch for normalization. `drift_detector` turns on cross-turn drift observation |
| `logging.request_body` | Dump each upstream body under `logs/` for cache-drift debugging |

## Layout

```
ccextra/
├── crates/
│   ├── ccextra-core/           # Pure logic, no IO
│   │   ├── cache_stabilization/  # Nine-module normalization
│   │   ├── convert/              # Three protocol-convert paths
│   │   ├── thinking.rs           # Thinking-level mapping
│   │   ├── prompt_cache.rs       # prompt_cache_key inject (key = session ID, Codex-aligned)
│   │   ├── secret.rs             # Ingress key bcrypt recognition
│   │   ├── route.rs              # Route decision
│   │   ├── session.rs            # Session identity derivation
│   │   └── normalize.rs          # Normalization orchestration
│   ├── ccextra-server/         # IO layer
│   │   ├── http.rs               # axum entry + pipeline
│   │   ├── upstream.rs           # reqwest client (protocol-specific UA)
│   │   └── sse/                  # SSE parse + state machines
│   └── ccextra-cli/            # Entry + config
├── config.example.yaml         # Config example
├── docs/
│   ├── design.md               # Architecture (Chinese)
│   └── glossary.md             # Domain glossary (Chinese)
├── start.sh / stop.sh / restart.sh / build.sh
├── README.md                   # Chinese (default)
└── README.en.md                # English
```

## Tests

```bash
cargo test --workspace
```

Currently 567 tests (12 + 414 + 141), covering cache normalization, protocol conversion (Claude/OpenAI/Gemini), SSE state machines, the HTTP pipeline, and config parsing.

## Docs

- **[docs/design.md](docs/design.md)** — Architecture: convert paths, normalization, SSE relay, performance (Chinese)
- **[docs/glossary.md](docs/glossary.md)** — Domain glossary (Chinese)
- **[docs/gemini.md](docs/gemini.md)** — Gemini protocol support: tool calling, thinking, streaming (Chinese)

## License

[MIT](LICENSE)
