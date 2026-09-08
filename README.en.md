# ccextra

**[中文](README.md)** | **English**

> Single-process Rust proxy that routes, converts, and relays Claude Code Anthropic Messages requests to upstream providers.

## Overview

One process listens on one port. Anthropic-shaped input resolves to a provider by model alias. Deterministic normalization reduces cross-turn serialization drift and improves upstream prompt-cache reuse.

## Upstream protocols

| `protocol` | Upstream API | Behavior |
| --- | --- | --- |
| `claude` | Anthropic Messages | Replaces only `model`; remaining request content stays intact. |
| `openai_chat` | Chat Completions | Converts messages, tools, images, and reasoning. |
| `openai_responses` | Responses | Converts to `instructions` and `input`; supports reasoning replay. |
| `gemini` | Gemini GenerateContent | Uses Gemini content, tool, and schema shapes. |
| `antigravity` | Cloud Code Assist | Uses Gemini shapes inside an Antigravity transport envelope. |

xAI Grok OAuth injects an `openai_responses` provider; it is not a separate `protocol`.

## Quick start

```bash
cargo build --release
cp config.example.yaml config.yaml
./target/release/ccextra --config config.yaml
```

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
      - name: gpt-5.6-terra
        alias: gpt-5.6-terra
normalize:
  enabled: true
  drift_detector: true
```

```bash
export ANTHROPIC_BASE_URL=http://127.0.0.1:8222
export ANTHROPIC_AUTH_TOKEN=sk-ccextra-xxx # required when secret_key is set
```

`build.sh` updates root `./ccextra`; `start.sh`, `stop.sh`, and `restart.sh` manage a background process.

## Configuration

See [config.example.yaml](config.example.yaml) for every field. `models[].alias` is inbound model name and cannot duplicate across providers. `base_url` accepts an ordered fallback array. Provider `proxy_url` overrides `server.proxy_url`; `"direct"` disables proxy use.

`secret_key` enables ingress authentication. Plaintext keys become bcrypt hashes on load and are written back; requests accept `x-api-key` or `Authorization: Bearer`. `payload` applies model-glob top-level overrides, optionally scoped by `protocol`. `prompt_cache_key` applies only to OpenAI paths, uses Claude Code session ID, and never replaces a nonempty key.

`user_agents` overrides Claude, Codex, Grok, and Antigravity identifiers. `logging.request_body` writes diagnostic requests under `logs/`. `POST /reload` reloads providers, payload, normalization, auth, proxy, and User-Agent. Restart for `logging.level` changes. `antigravity.connection-pool` controls the Antigravity upstream connection pool: short connections by default; with `enabled: true`, idle connections are kept per `idle-conn-timeout` (default 30s, capped at 210s) and `max-idle-conns-per-host` (default 2, capped at 100).

## Endpoints

| Endpoint | Purpose |
| --- | --- |
| `POST /v1/messages` | Main request endpoint. |
| `POST /v1/messages/count_tokens` | Exact upstream count for Claude; previous-turn recorded count for the session otherwise, or 0 when absent. |
| `GET /v1/models` | Anthropic-shaped model list. |
| `GET /health` | Returns `ok`. |
| `POST /reload` | Reloads configuration. |

With `secret_key`, the first three Anthropic endpoints require authentication.

## Runtime behavior

Requests pass through authentication, routing, normalization, protocol conversion, payload overrides, cache-key injection, and upstream delivery. Claude passthrough preserves inbound identity headers and rebuilds required `anthropic-beta`. All streaming output returns as Anthropic SSE and emits `: keepalive` after 10 seconds idle.

Network errors, 429, and 5xx retry with exponential backoff within a 3-second total budget and constrained `Retry-After`; ordered `base_url` values are tried in sequence.

## OAuth and development

```bash
./ccextra antigravity-login
./ccextra antigravity-status
./ccextra xai-login
./ccextra xai-status
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

Antigravity credentials default to `.cache/antigravity` beside the config file; xAI defaults to `.cache/xai`. xAI loads at startup. Antigravity loads in background and refreshes models every three hours. See [architecture](docs/design.md), [glossary](docs/glossary.md), and [MIT license](LICENSE).
