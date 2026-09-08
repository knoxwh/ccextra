# 架构设计

> 本文描述当前 ccextra 运行结构和行为。术语定义见 [glossary.md](glossary.md)。

## 目标

ccextra 将 Anthropic Messages 入口接到不同上游协议，同时尽量保持请求字节稳定、会话亲和和 Claude Code 客户端兼容性。它不是本地响应缓存；缓存优化目标是上游 prompt cache。

## 分层

- `ccextra-core`：纯逻辑。负责路由、归一化、会话派生、thinking 映射和协议转换；不依赖 `tokio`、`reqwest` 或文件系统。
- `ccextra-server`：HTTP、上游请求、SSE、OAuth provider 运行时注入和响应状态机。
- `ccextra-cli`：命令行、YAML 配置加载、日志初始化和进程启动。

这种分层使转换规则可用单元测试验证，网络和锁生命周期集中在 server。

## 服务端 API

| 端点 | 行为 |
| --- | --- |
| `POST /v1/messages` | 认证、路由、转换并返回 Anthropic 响应。 |
| `POST /v1/messages/count_tokens` | Claude 路径转发精确请求；其他路径返回该会话上轮响应记录的输入 token 数，未命中返回 0。 |
| `GET /v1/models` | 汇总 provider alias，返回 Anthropic 模型清单。 |
| `GET /health` | 固定返回 `ok`。 |
| `POST /reload` | 重新读取运行时配置。 |

`secret_key` 启用后，三个 Anthropic API 端点都要认证；`/health` 和 `/reload` 不认证。

## 请求管线

1. 读取请求体并校验 `x-api-key` 或 `Authorization: Bearer`。
2. 由入站 `model` 解析 `(provider, protocol, upstream_model)`。
3. 按协议执行转换前归一化。
4. 转换成目标请求体，或在 Claude 路径只替换 `model`。
5. OpenAI 路径执行转换后归一化；Gemini 和 Antigravity 跳过该步骤。
6. 应用 `payload` 顶层参数覆盖，保留已有非空 `prompt_cache_key`。
7. 为符合条件的 OpenAI 请求注入会话 cache key，必要时写诊断日志。
8. 选择 URL、代理、认证头和 User-Agent，发送上游请求并处理重试。
9. 直通或转换流式 SSE；非流响应也恢复为 Anthropic 形状。

## 路由与配置

`alias` 优先匹配；上游真实模型名是回退匹配，便于 `count_tokens` 等辅助请求。重复 alias 在启动或重载校验时失败。`base_url` 支持列表：单轮遇网络错误或 429 立即切换后续地址；5xx 则在指数退避后开始新一轮轮询。provider 代理优先于全局代理，`"direct"` 或空值不使用代理。

配置重载替换 providers、payload、normalize、认证、全局 HTTP client 和 User-Agent。`logging.level` 在启动时创建 `EnvFilter`，因此不能热改。

## 缓存稳定化

归一化保持幂等。Claude 直通运行完整流程：工具和 schema 排序、历史 reminder 拆分、账本内容剥离、`tool_use.input` 键序、system 列表、尾部空白、客户端日期指纹和自动 `cache_control` 放置。转换路径先运行适合 Anthropic 输入的确定性子集；OpenAI 输出随后排序工具/schema、system reminder 和日期字段。

`drift_detector` 对同一会话的结构哈希发出告警，不修改请求。Gemini 与 Antigravity 不做转换后归一化或 drift 观测，避免将 Anthropic 规则施加到 Gemini 形状。

## 协议转换

### Claude

`convert_passthrough` 只改 `model`。身份头按安全规则透传，并根据 body 重建所需 `anthropic-beta`。

### OpenAI Chat

Anthropic `system` 成为 system message；用户、助手、图片、工具调用和工具结果转换为 Chat Completions 形状。`thinking` 映射为受模型能力限制的 `reasoning_effort` 或兼容的 reasoning 内容。无等价物的 Claude server-side web search 工具会删除。

### OpenAI Responses

普通上游把 system 放到 `instructions`。GPT/Grok 上游使用固定 developer 适配块，并清理不兼容的 Claude 系统段落。工具、tool choice、图片和自定义工具转换为 Responses 项；过长工具名使用请求侧缩写和响应侧反向映射。严格 JSON schema 不满足 Responses 要求时自动降级 `strict`。

### Gemini 与 Antigravity

两条路径共享 Gemini `contents`、`parts`、`functionCall` 和 `functionResponse` 模型。工具 schema 会清理本地引用和不支持关键字；工具结果强制字符串化为 `response.result`。Gemini 使用 API key 和 Google 端点。Antigravity 额外套 `model`、`request`、`project`、`requestId` 等信封；Claude 模型使用 `VALIDATED` 工具模式，冲突工具名加 `external_` 前缀。

## 传输与可靠性

`UpstreamClient` 按最终代理地址缓存 `reqwest::Client`。请求使用协议对应 URL、认证和 User-Agent：Gemini 使用 `x-goog-api-key`，其他协议使用 Bearer；Responses 的 GPT 请求带 Codex 会话头，Grok 请求带 CLI 身份头和会话亲和 `x-grok-conv-id`。

网络错误、429、5xx 和 Cloudflare 52x 可重试。退避从 300ms 开始，单次最多 1.5 秒，所有重试共享 3 秒预算；`Retry-After` 只在该预算内生效。流式 OpenAI 请求声明 `Accept: text/event-stream` 和 `Cache-Control: no-cache`。

## 响应转发

Claude 响应字节直通。OpenAI Chat、OpenAI Responses、Gemini 和 Antigravity 分别由状态机转换为 Anthropic SSE。状态机维护 content block、thinking、工具调用、usage 和终态，避免上游事件交错破坏 Anthropic 事件顺序。

OpenAI 首帧错误允许重试一次；已输出首帧后的错误、未满足终态的 EOF、空 Gemini 风格流和读取错误产生结构化 Anthropic error，而不是裸断开。每条流式路径统一包裹 10 秒空闲心跳 `: keepalive\n\n`。非流 Claude 直通；其他路径转换为 Anthropic JSON，无法转换时保留上游原始 body。

## 会话、缓存与 reasoning

Claude Code 会话 ID 优先取 `x-claude-code-session-id`，其次从 `metadata.user_id` 派生。它用于 drift 分桶、`prompt_cache_key` 和协议会话头。OpenAI provider 开启 `prompt_cache_key` 后，Chat 和 Responses 使用该裸会话 ID；Chat+Grok 例外，使用 `x-grok-conv-id` 维持亲和。

Responses 流会收集可回放 reasoning。服务端以模型和会话为键保存完整回合，在下一请求中按 tool call 和输入锚点插回兼容的 reasoning 项，防止 `store=false` 上游丢失推理上下文。签名检查按目标提供方族执行；无效或不兼容的 thinking 不会发到上游。

## OAuth provider

`antigravity-login` 使用浏览器回调登录并保存凭证。运行时后台读取有效凭证、刷新 token、拉取模型并注入 provider；首次加载不阻塞监听，之后每 3 小时刷新，失败时保留现有路由。

Antigravity 上游默认短连接：空闲连接在响应结束后立即关闭，防止凭证轮换下 socket 堆积与陈旧连接错误。`antigravity.connection-pool` 显式启用连接池（`idle-conn-timeout` 默认 30s、上限 210s，不超过 Google Frontend 240s keep-alive 截止；`max-idle-conns-per-host` 默认 2、上限 100），其余协议共享全局客户端不受影响。

`xai-login` 使用 OAuth device flow。启动和配置重载扫描 xAI 凭证，必要时提前刷新 token，并为每份有效凭证注入一个 Responses provider。相对 `auth_dir` 和 `xai_auth_dir` 始终相对配置文件目录解析。

## 并发、安全与诊断

运行时配置、providers 和 payload 使用独立 `RwLock`。请求先复制快照并在网络 `await` 前释放读锁；热重载不是跨三把锁的原子事务，因此极短窗口内请求可能看到混合快照。

`secret_key` 明文会写回 bcrypt；验证结果最多缓存 1024 项，重载时清空。`logging.request_body` 保存最终出站请求的诊断数据，敏感入站头会脱敏。归一化只在能够安全处理时改变 body，避免优化本身阻断请求；转换失败由 HTTP 层返回 Anthropic error。

## 验证

测试与实现同文件的 `#[cfg(test)]` 模块以及 server 集成测试共同覆盖转换、归一化、SSE、HTTP 管线、热重载和 OAuth 辅助逻辑。运行 `cargo test --workspace`，再运行 `cargo clippy --workspace --all-targets -- -D warnings`。
