# ccextra

**中文** | **[English](README.en.md)**

> 单进程 Rust 代理：将 Claude Code 的 Anthropic Messages 请求路由、转换并转发到多个上游。

## 概览

一个进程监听一个端口。入站始终使用 Anthropic 形状，按模型 alias 选择 provider；确定性归一化降低跨轮序列化漂移，提高上游 prompt cache 命中率。

## 支持的出站协议

| `protocol` | 上游接口 | 说明 |
| --- | --- | --- |
| `claude` | Anthropic Messages | 仅替换 `model`，其余请求内容保持原样。 |
| `openai_chat` | Chat Completions | 转换 messages、工具、图片和 reasoning。 |
| `openai_responses` | Responses | 转换为 `instructions` 和 `input`，支持 reasoning replay。 |
| `gemini` | Gemini GenerateContent | 使用 Gemini 内容、工具和 schema 形状。 |
| `antigravity` | Cloud Code Assist | 使用 Gemini 形状并封装运输信封。 |

xAI Grok 经 OAuth 自动注入 `openai_responses` provider，不是独立 `protocol`。

## 快速开始

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
export ANTHROPIC_AUTH_TOKEN=sk-ccextra-xxx # 配置 secret_key 时需要
```

`build.sh` 更新根目录 `./ccextra`；`start.sh`、`stop.sh`、`restart.sh` 管理后台进程。

## 配置

完整字段见 [config.example.yaml](config.example.yaml)。`models[].alias` 是入站模型名，不能跨 provider 重复。`base_url` 可为按顺序回退的数组；`server.proxy_url` 可被 provider `proxy_url` 覆盖，`"direct"` 表示直连。

`secret_key` 启用入口认证。明文 key 在加载时转为 bcrypt 并写回配置；请求接受 `x-api-key` 或 `Authorization: Bearer`。`payload` 按模型 glob 覆盖顶层参数，可用 `protocol` 限定。`prompt_cache_key` 只用于 OpenAI 路径，取 Claude Code 会话 ID，且不覆盖已有非空值。

`user_agents` 可覆盖 Claude、Codex、Grok、Antigravity 标识。`logging.request_body` 将诊断请求写入 `logs/`。`POST /reload` 重载 providers、payload、归一化、认证、代理和 User-Agent；`logging.level` 需重启。

## 端点

| 端点 | 用途 |
| --- | --- |
| `POST /v1/messages` | 主请求入口。 |
| `POST /v1/messages/count_tokens` | Claude 上游转发精确计数；其他协议返回该会话上轮响应记录的输入 token 数，未命中返回 0。 |
| `GET /v1/models` | Anthropic 形状模型列表。 |
| `GET /health` | 返回 `ok`。 |
| `POST /reload` | 热重载配置。 |

配置 `secret_key` 后，前三个 Anthropic 端点需要认证。

## 运行行为

请求依次经过认证、路由、归一化、协议转换、payload 覆盖、缓存 key 注入和上游发送。Claude 直通保留入站身份头并重建所需 `anthropic-beta`。所有流式出口以 Anthropic SSE 返回，空闲 10 秒发送 `: keepalive`。

网络错误、429 和 5xx 在 3 秒总预算内指数退避重试，并受限地尊重 `Retry-After`；多 `base_url` 按顺序尝试。

## OAuth 与开发

```bash
./ccextra antigravity-login
./ccextra antigravity-status
./ccextra xai-login
./ccextra xai-status
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
```

Antigravity 凭证默认在配置文件旁 `.cache/antigravity`，xAI 在 `.cache/xai`。xAI 启动时自动发现；Antigravity 后台加载并每 3 小时刷新模型。详见 [架构设计](docs/design.md)、[术语表](docs/glossary.md) 和 [MIT 许可证](LICENSE)。
