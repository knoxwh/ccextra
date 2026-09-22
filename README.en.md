<div align="center">

# ccextra

<p align="center">
  <strong>Use models from different providers in Claude Code.</strong><br>
  One local endpoint for Claude, OpenAI, Gemini, and OAuth providers.
</p>

<p align="center">
  <a href="LICENSE"><img src="https://img.shields.io/badge/License-MIT-black.svg?style=flat-square" alt="License"></a>
  <a href="Cargo.toml"><img src="https://img.shields.io/badge/Rust-1.75+-black.svg?style=flat-square&logo=rust" alt="Rust"></a>
  <img src="https://img.shields.io/badge/Version-0.2.0-black.svg?style=flat-square" alt="Version">
</p>

<p align="center">
  <a href="README.md">中文</a> • <b>English</b>
</p>

<p align="center">
  <a href="#key-features">Key Features</a> •
  <a href="#architecture">Architecture</a> •
  <a href="#upstream-matrix">Upstream Matrix</a> •
  <a href="#quick-start">Quick Start</a> •
  <a href="#configuration">Configuration</a> •
  <a href="docs/design.md">Design Spec</a>
</p>

---

</div>

A single-process Rust proxy. Claude Code sends Anthropic Messages requests; ccextra selects an upstream by model alias, translates the request, and returns Anthropic-format JSON or SSE responses.

## Key Features

- **Route by model**: Five protocols share one endpoint. Configure client-facing aliases separately from upstream model names.
- **Stable request content**: Normalize tools, schemas, and history to reduce incidental changes between turns. Actual cache hits depend on the upstream.
- **Model adaptation**: Translate messages, tool calls, and images; adjust supported reasoning levels through `models.json`.
- **OAuth providers**: Load and refresh Antigravity, xAI Grok, and Codex (OpenAI ChatGPT subscription) credentials with dynamic model routing.
- **Hot reload**: Publish configuration without restarting. In-flight requests keep their original snapshot.

## Architecture

```mermaid
flowchart LR
    CC["Claude Code"] --> P["ccextra"]
    P -->|claude| A["Anthropic Messages"]
    P -->|openai_chat| B["Chat Completions"]
    P -->|openai_responses| C["Responses"]
    P -->|gemini / antigravity| D["Gemini GenerateContent"]
```

One process listens on one port. Input is always Anthropic-shaped; every path returns Anthropic responses (including SSE). See [architecture](docs/design.md).

## Upstream Matrix

| Protocol (`protocol`) | Upstream API Target | Core Adaptation Strategy |
| :--- | :--- | :--- |
| `claude` | Anthropic Messages | Replace the target model; also sanitize system content and adjust effort for non-Claude models. Response bodies pass through. |
| `openai_chat` | Chat Completions | Translate messages, tools, and images; adapt Kimi K2.8 thinking parameters and temperature constraints. |
| `openai_responses` | Responses | Map `instructions` and `input`; support reasoning replay and search domain filtering. |
| `gemini` | Gemini GenerateContent | Translate content blocks, tool results, and schemas. |
| `antigravity` | Cloud Code Assist | Wrap Gemini requests, adapt tool names and output limits; use short connections by default. |

> **Note**: xAI Grok and Codex are automatically injected as `openai_responses` providers via OAuth without requiring a distinct protocol. Codex subscription requests carry the `Chatgpt-Account-Id` identity header automatically.

## Quick Start

### 1. Build

```bash
cargo build --release
```

### 2. Configure an upstream

Copy and edit the [configuration template](config.example.yaml), or save the minimal example below as `config.yaml`. Replace `base_url`, `key`, and `models[].name` with values supported by your upstream.

```yaml
server:
  host: "127.0.0.1"
  port: 8222

providers:
  - name: upstream
    protocol: openai_responses
    base_url: https://example.com/v1
    key: sk-xxx
    prompt_cache_key: true
    models:
      - name: your-upstream-model
        alias: coding

normalize:
  enabled: true
  drift_detector: true

logging:
  level: info
  request_body: false

secret_key: "replace-with-your-local-key"
```

For OpenAI protocols, include the version prefix (such as `/v1`) in `base_url`, but not `/responses` or `/chat/completions`. For the Claude protocol, omit `/v1`.

### 3. Start and connect

Start the proxy:

```bash
./target/release/ccextra --config config.yaml
```

In another terminal, use the local key from your configuration. This key is separate from the upstream `providers[].key`:

```bash
export ANTHROPIC_BASE_URL=http://127.0.0.1:8222
export ANTHROPIC_AUTH_TOKEN=replace-with-your-local-key
claude --model coding
```

On first load, `secret_key` is hashed with bcrypt and written back to the config. Clients continue to use the original plaintext key.

Check the service and available models:

```bash
curl http://127.0.0.1:8222/health
curl -H "Authorization: Bearer $ANTHROPIC_AUTH_TOKEN" \
  http://127.0.0.1:8222/v1/models
```

For background operation, use `build.sh` with `start.sh`, `stop.sh`, and `restart.sh`. These scripts use the root-level `./ccextra` binary, which `build.sh` updates.

## Configuration

See [config.example.yaml](config.example.yaml) for every field. Key points:

- `models[].alias` is the inbound model name and cannot duplicate across providers; `base_url` accepts an ordered fallback array.
- Provider `proxy_url` overrides `server.proxy_url`; `"direct"` disables proxy use.
- `secret_key` enables ingress authentication: plaintext keys become bcrypt hashes on load and are written back; requests accept `x-api-key` or `Authorization: Bearer`.
- `payload` applies model-glob top-level overrides, optionally scoped by `protocol`.
- `prompt_cache_key` applies only to OpenAI paths, uses Claude Code session ID, and never replaces a nonempty key.
- `models_file` points at the reasoning-level table (default `models.json` next to the config; not tracked by git — copy [models.json.example](models.json.example) and edit as needed). Exact `id` match clamps inbound effort to the nearest supported level; missing file or unknown models leave effort unchanged. An entry may set `force_effort`: wherever clamping would apply, effort is rewritten to this fixed value (unclamped); native `*claude*` models and requests with thinking explicitly disabled are unaffected.

<details>
<summary>Advanced options</summary>

- `user_agents` overrides Claude, Codex, Grok, and Antigravity identifiers.
- `logging.request_body` writes diagnostic requests under `logs/`.
- `POST /reload` validates and atomically publishes providers, payload, normalization, auth, proxy, User-Agent, the reasoning registry, and background refresh settings. Existing requests keep their snapshot; new requests use the new snapshot. Concurrent reloads serialize loading and publication; load or validation failures leave the snapshot and version unchanged. Restart for `logging.level` changes.
- Background refresh uses the last successfully published static configuration, credential directories, and proxy, without reading config file edits awaiting reload. Stale refresh results are discarded; an empty Antigravity result preserves the entire provider set. Explicit reload applies removals, disabled credentials, and directory changes without restoring old dynamic providers; dynamic loaders retain their existing behavior of skipping unavailable credentials.
- `antigravity.connection-pool` controls the Antigravity upstream connection pool: short connections by default; with `enabled: true`, idle connections are kept per `idle-conn-timeout` (default 30s, capped at 210s) and `max-idle-conns-per-host` (default 2, capped at 100).
- Edits to `models.json` take effect after `POST /reload`; no rebuild needed.

</details>

## API Endpoints

| Method & Endpoint | Auth Required* | Description |
| :--- | :--- | :--- |
| `POST /v1/messages` | Required* | Accept Anthropic Messages and return JSON or SSE. |
| `POST /v1/messages/count_tokens` | Required* | Forward Claude counting upstream; use session records for other protocols, returning 0 on a miss. |
| `GET /v1/models` | Required* | List available models in Anthropic format. |
| `GET /health` | Public | Return `ok`; does not check upstream availability. |
| `POST /reload` | Public | Load, validate, and publish configuration without restarting. |

> `*` Note: When `secret_key` is set, endpoints marked with `*` require `x-api-key` or `Authorization: Bearer`.

## Runtime Pipeline

<details>
<summary>Request processing, retries, and resource limits</summary>

Requests pass through authentication, model routing, normalization, protocol conversion, and parameter overrides before reaching the upstream. OpenAI paths can inject `prompt_cache_key`; Gemini and Antigravity skip OpenAI-specific processing.

- **Streaming**: Claude response bytes pass through; other protocols are converted to Anthropic SSE. All streaming paths use a 10-second `: keepalive`.
- **Retries**: Network errors, 429, and 5xx/52x share a 3-second backoff budget with multiple `base_url` fallbacks. This is not a total request timeout and does not truncate normal generation.
- **Read limits**: Non-stream success bodies are capped at 16 MiB, error bodies retain at most 256 KiB, and read idle is 300 seconds. Oversized success bodies return 502; stalls return 504. Error-body read failures retain the known status for normal error mapping. OAuth, project, and model reads retain their 30-second total timeout.
- **Headers**: Claude forwards permitted identity headers while excluding authentication and connection-management headers. `anthropic-beta` passes through unchanged and is not added when absent.

</details>

## OAuth and operations

Log in to an upstream or inspect saved credential status:

```bash
./ccextra antigravity-login
./ccextra antigravity-status
./ccextra xai-login
./ccextra xai-status
./ccextra codex-login
./ccextra codex-status
./scripts/check_antigravity_quota.sh
./scripts/check_grok_quota.sh
```

Antigravity credentials default to `.cache/antigravity` beside the config file; xAI defaults to `.cache/xai`; Codex defaults to `.cache/codex`. xAI and Codex load at startup. Antigravity loads in the background and refreshes models every three hours. Codex login uses PKCE browser authorization (local callback port defaults to 1455, override with `--callback-port`); tokens refresh 24 hours ahead of expiry.

After editing configuration, or to reload credentials immediately:

```bash
curl -X POST http://127.0.0.1:8222/reload
```

Configuration load or validation failures preserve the previous configuration. See “Advanced options” above for refresh and removal rules.

## Development & Architecture

```bash
# Run full test suite
cargo test --workspace

# Strict clippy lints
cargo clippy --workspace --all-targets -- -D warnings
```

Decoupled three-tier architecture:
- `ccextra-cli`: CLI interface, daemon lifecycle, and configuration loading.
- `ccextra-server`: HTTP layer, OAuth handlers, SSE state machines, and connection pools.
- `ccextra-core`: Pure IO-free core logic (routing algorithms, normalization, protocol converters, reasoning registry).

For detailed specifications, see [Architecture (design.md)](docs/design.md) and [Glossary (glossary.md)](docs/glossary.md).

## License

[MIT](LICENSE)
