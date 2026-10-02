# 架构设计

> 本文描述当前 ccextra 运行结构和行为。术语定义见 [glossary.md](glossary.md)。

## 目标

ccextra 将 Anthropic Messages 入口接到不同上游协议，同时尽量保持请求字节稳定、会话亲和和 Claude Code 客户端兼容性。它不是本地响应缓存；缓存优化目标是上游 prompt cache。

## 分层

- `ccextra-core`：纯逻辑。负责路由、归一化、会话派生、thinking 映射和协议转换；不依赖 `tokio`、`reqwest` 或文件系统。reasoning 注册表由调用方传入，core 只解析字符串。
  - `convert/`：协议请求体转换（含 `to_openai_responses/` 专用子模块：`instructions`、`messages`、`reasoning`、`schema`）。
- `ccextra-server`：HTTP、上游请求、SSE、OAuth provider 运行时注入和响应状态机。
  - `http/`：请求处理与路由装配（`handlers/`、`auth`、`retry`、`claude_relay`、`error`）。
  - `sse/`：响应流状态机（含 `responses/` 专用子模块：`state_machine`、`function_call`、`thinking`、`compensations`、`web_search`）。
- `ccextra-cli`：命令行、YAML 配置加载、日志初始化和进程启动。用户 `models.json` 在此读取。

这种分层使转换规则可用单元测试验证，网络和锁生命周期集中在 server。

## 服务端 API

| 端点 | 行为 |
| --- | --- |
| `POST /v1/messages` | 认证、路由、转换并返回 Anthropic 响应。 |
| `POST /v1/messages/count_tokens` | Claude 路径转发精确请求；其他路径返回该会话上轮响应记录的输入 token 数，未命中返回 0。Chat/Responses/Gemini/Antigravity 从原始 SSE 提取正数输入用量并写回会话缓存；不记录转换后的占位值，缺字段或零值保留旧值。 |
| `GET /v1/models` | 汇总 provider alias，返回 Anthropic 模型清单。 |
| `GET /health` | 固定返回 `ok`。 |
| `POST /reload` | 重新读取运行时配置。 |

`secret_key` 启用后，三个 Anthropic API 端点都要认证；`/health` 和 `/reload` 不认证。

## 请求管线

1. 读取请求体并校验 `x-api-key` 或 `Authorization: Bearer`。
2. 由入站 `model` 解析 `(provider, protocol, upstream_model)`。
3. 按协议执行转换前归一化。
4. 转换成目标请求体，或在 Claude 路径只替换 `model`，再清洗非 Claude 模型的 system 并钳制其越档 effort。
5. OpenAI 路径执行转换后归一化；Gemini 和 Antigravity 跳过该步骤。
6. 应用 `payload` 顶层参数覆盖，保留已有非空 `prompt_cache_key`。
7. 为符合条件的 OpenAI 请求注入会话 cache key，必要时写诊断日志。
8. 选择 URL、代理、认证头和 User-Agent，发送上游请求并处理重试。`cursor_sdk` 例外：不经通用 upstream，由 cursor relay 注入 apiKey 后直发本地 sidecar `/run`。
9. 直通或转换流式 SSE；非流响应也恢复为 Anthropic 形状。

## 路由与配置

`alias` 优先匹配；上游真实模型名是回退匹配，便于 `count_tokens` 等辅助请求。重复 alias 在启动或重载校验时失败。`base_url` 支持列表：单轮遇网络错误或 429 立即切换后续地址；5xx 则在指数退避后开始新一轮轮询。provider 代理优先于全局代理，`"direct"` 或空值不使用代理。

配置重载替换 providers、payload、normalize、认证、全局 HTTP client、User-Agent 和 reasoning 注册表。`logging.level` 在启动时创建 `EnvFilter`，因此不能热改。

## 缓存稳定化

归一化保持幂等。Claude 直通运行完整流程：工具和 schema 排序、历史 reminder 拆分、`tool_use.input` 键序、system 列表、尾部空白和客户端日期指纹。转换路径先运行适合 Anthropic 输入的确定性子集；OpenAI 输出随后排序工具/schema、system reminder 和日期字段。

`drift_detector` 对同一会话的结构哈希发出告警，不修改请求。Gemini 与 Antigravity 不做转换后归一化或 drift 观测，避免将 Anthropic 规则施加到 Gemini 形状。

## 协议转换

### Claude

`convert_passthrough` 只改 `model`，再对非 Claude 模型做两件事。一是 system 清洗（对齐 chat/responses/gemini）：顶层 `system` 与 `messages` 内 `role: system` 项剥掉计费归属指纹（逐请求变化，破坏上游缓存前缀；归属行与指令同块时只剥前导行、保留剩余指令，对齐 sub2api）、Claude 身份声明与 `<identity>`/`<rules>`/`# Output Style:` 等 Claude 触发段，保留白名单段与 `cache_control`，清空后移除该块或消息。二是越档 effort 钳制：命中 glob `*claude*` 的模型、`thinking.type: disabled` 或注册表未命中的请求跳过；写回优先顶层 `output_config.effort`，回退 `thinking.output_config.effort`，值不变不写回。条目设置 `force_effort` 时改写为该固定值（不钳制），跳过条件不变。`*claude*` 模型两项都跳过，保持逐字节直通。身份头按排除表透传，`anthropic-beta` 原样转发、缺失时不补。

### OpenAI Chat

Anthropic `system` 成为 system message；o 系列（`o1-mini`/`o1-preview` 除外）、GPT-5 族、`gpt-6-astra` 及其日期快照改用 `developer`。这些模型把 `max_tokens` 改发为 `max_completion_tokens`；o 系列删除 `temperature`，GPT-5 仅在 `gpt-5.1`/`gpt-5.2`/`gpt-5.4` 无 reasoning 时保留采样，Astra 永不发 `temperature`/`top_p`。全量 `openai_chat` 模型剥除 Claude system triggers 与 prompt reminder。Kimi K2.8 模型（`kimi-k2.8`/`kimi-k2.8-code`）将 reasoning 映射为 `thinking.type`/`effort`，显式 `none` 绕过 clamp，并守卫 temperature（disabled 限 0.6，enabled/default 限 1.0）。用户、助手、图片、工具调用和工具结果转换为 Chat Completions 形状。工具 `input_schema` 中的 `required: null` 仅在 schema 节点删除；原本缺省的 `required` 仍按现有规则补 `[]`，显式 null 不补。对齐 CPA `4b4a4448`，schema 位置的 `true`（根、properties/$defs、items 和联合分支）转为 `{}`；`false`、布尔 `additionalProperties`、`default` 和 `enum` 实例数据保持不变。`tool_choice.disable_parallel_tool_use` 映射为 `parallel_tool_calls: false`（未显式关闭不写，OpenAI 默认开）。`thinking` 映射为受模型能力限制的 `reasoning_effort` 或兼容的 reasoning 内容。无等价物的 Claude server-side web search 工具会删除。

### OpenAI Responses

普通上游把 system 放到 `instructions`。GPT/Grok 上游使用固定 developer 适配块，并清理不兼容的 Claude 系统段落。GPT-6 Astra 使用独立适配块，未指定 effort 时默认 `low`；其余 Responses 上游默认 `medium`。effort 再按用户 `models.json` 钳到该模型支持档（Astra 示例为 `low`/`medium`，更高档钳到 `medium`）；条目设置 `force_effort` 时固定使用该值（不钳制）。查不到或未配置文件则不钳。工具、tool choice、图片和自定义工具转换为 Responses 项；过长工具名使用请求侧缩写和响应侧反向映射，截断后清理前导 `_`/`-`（对齐 CPA `capResponsesChatToolName`；清理后为空则保留原截断值）。纯 const union（≥8 分支）简化为 enum 并清理 JSON Schema 方言关键字；工具 schema 节点的 `required: null` 删除而非替换为 `[]`，`default` 等实例数据不变。web_search 按家族映射：Grok 映射为 `filters.excluded_domains`，OpenAI 映射为 `filters.blocked_domains`，`allowed_domains` 优先。reasoning 清洗时将空 summary 的 `reasoning_text` 提升为 `summary_text`，强制 `reasoning.content: []`。严格 JSON schema 不满足 Responses 要求时自动降级 `strict`。

### Gemini 与 Antigravity

两条路径共享 Gemini `contents`、`parts`、`functionCall` 和 `functionResponse` 模型，剥离 Claude system triggers。Antigravity 另外将 system 块开头的 Claude Agent SDK/Claude Code 身份句改为 `You are an AI agent.`，保留句后指令；Gemini 直连不做此改写。工具 schema 会清理本地引用和不支持关键字（含 `additionalItems`/`unevaluated*`/`contentSchema`），布尔 `true` 子 schema 归一化为空对象；Gemini 直连的 `parametersJsonSchema` 保留 `additionalProperties` 与 `pattern`/`minLength` 等标准约束，Antigravity 仍搬入 description 提示。任一工具带 `strict: true` 且 tool_choice 为 auto/缺省时，Gemini 直连使用 `VALIDATED` 工具模式（Antigravity 的 `VALIDATED` 仅由 Claude 模型触发）。规范化 `responseJsonSchema` 为 `responseSchema`；工具结果强制字符串化为 `response.result`；user turn 尾部文本重排至 `functionResponse` 前。Antigravity 的 Claude 模型先拆分混合 user 回合并把 `functionResponse` 回合提升到紧跟含 `functionCall` 的 model 回合（system reminder 与普通文本顺延），再仅合并不含 `functionResponse` 的相邻 user 回合；非 Claude 模型维持合并加重排。Gemini 使用 API key 和 Google 端点。Antigravity 额外套 `model`、`request`、`project`、`requestId` 等信封；Claude 模型使用 `VALIDATED` 工具模式，冲突工具名加 `external_` 前缀；`gemini-3.5-flash-lite` 限制 `max_completion_tokens` 上限为 65535。

### Cursor

Cursor 走本地 Node sidecar 而非 HTTP 反代。凭证是 **User API Key**（cursor.com/settings → API Keys 手动生成）：SDK 内部经 `/auth/exchange_user_api_key` 换 access token，OAuth access token 不被该端点接受。凭证存 `cursor_auth_dir/api_key.txt`（单行裸 key，0600），`ensure_credential_fresh` 优先读取且不触发刷新；缺失时回落 `cursor.json`。`cursor-login` 的 PKCE 凭证写 `cursor.json`（DashboardService 配额查询用），与 `api_key.txt` 互不覆盖。`cursor_auth_dir`（默认配置文件旁 `.cache/cursor`）非空即启用。

**Rust/Node 边界**：sidecar 是 ccextra 的子进程（`sidecar/cursor/main.mjs`，嵌入 `@cursor/sdk`），固定监听 `127.0.0.1:8223`，端口占用时启动失败且不回退随机端口。**出站代理**：spawn 注入 `NODE_USE_ENV_PROXY=1`（覆盖 Node fetch 与 https.Agent 的 `https_proxy`/`http_proxy`/`no_proxy`）；SDK run 主流量走 connect-node 的 http2 会话（node:http2 无 Agent 概念，且 SDK bundle 以 ESM 命名空间导入 http2，属性快照使 `http2.connect` 补丁不可见），由 `proxy-tunnel.mjs` 在 `tls.connect` 层拦截（Node 内部 CJS 活引用，位置参数 + ALPN h2 识别）：预建 CONNECT 隧道池（默认后端预热 8 条）命中则 TLS 建于隧道，池空该次直连并异步补池；`proxyEnv`/`createConnection` 存在时放行避免与 Node 原生代理叠加。Rust 侧 `CursorSidecar` 负责 spawn、READY 握手（严格 JSON `{"event":"ready","port":8223}`，10 秒超时）、stderr 转 tracing、每 5 秒健康巡检与 1/2/4…秒退避重启（封顶 60 秒）。服务收到 `SIGTERM`/`SIGINT`、`/reload` 禁用 Cursor 或修改 `auth_dir` 时，先停止 sidecar 并释放端口。Bearer token 由 Rust 生成，只经 `CCEXTRA_CURSOR_TOKEN` 环境变量传递，不落盘；凭证目录经 `CCEXTRA_CURSOR_AUTH_DIR` 注入。协议为两个端点：`POST /run`（SSE）与 `POST /models`。Rust 不解析 SDK 内部结构，sidecar 不接触 ccextra 配置。

**请求形状**：`convert_to_cursor_sdk`（core，纯函数）将 Anthropic body 转为 `/run` 载荷——`model`（剥白名单钉参段后的裸 base）、`modelParams`、`systemPrompt`（system 块拍平为单字符串）、`workspaceDir`、Anthropic 形状 `messages`（递归剥 thinking 块并保留图片）与 `tools` 透传；sidecar 校验图片仅允许 `image/png`、`image/jpeg`、`image/webp`、`image/gif` 的合法 base64，远程 URL 返回 400，并将图片作为 SDK 附件传递。`modelParams` 由目录词表（`/models` 返回的每模型参数档位，存 `CursorRuntime.vocab`，目录刷新时整体替换）驱动：入站 `thinking.type: enabled` 映射 `thinking=true`（词表含该参数时），effort 取 models.json 注册表 `force_effort` 优先、回落 body 解析（budget_tokens → 顶层 `output_config.effort` → legacy），钳到词表最近档位（并列取低，`none` 不钳升，词表无对应参数则忽略）；白名单钉参（`id:param=value`）优先于映射值，固定参数 id 按当前模型词表归一 `effort`/`reasoning_effort` 与 `auto-smart` 的 `mode`/`optimize_for` 别名，参数按 id 排序保证会话哈希稳定。`apiKey` 由 Rust dispatch 时注入，不进转换层。分派链：凭证保鲜 → 注入 apiKey → `/run` → 401 时刷新一次重发 → 流式首业务帧预读（跳过心跳帧，首帧 error 返回 502 不提交 200）。system 提示词不传 SDK `systemPrompt` 选项（该选项账号级门控，无权限账号 `send` 即报 `invalid_argument unknown option '--system-prompt'`，官方 forum 确认无自助开通）；sidecar 在全量发送 transcript 时（new/replay/resume）将 system 文本以 `system:\n<prompt>` 前缀拼进 run 正文，suffix 续跑不拼（首轮已含）。

**SSE 事件与终态**：sidecar 事件已归一为 `text_delta`/`thinking_delta`/`tool_use`/`usage`/`turn_end`/`error` 六类。`relay_cursor_sdk_to_anthropic` 状态机自动开合 content block（text/thinking 增量、tool_use 单次 `input_json_delta`），`turn_end` 触发 `message_delta` + `message_stop` 后立即结束流；EOF 无 `turn_end`、`error` 事件或读取错误在已开始流中先闭合活动块并发 `message_delta(stop_reason: null)` + `message_stop`，再发结构化 Anthropic error；未开始流只发 error，不伪造成功终态。非流路径 `collect_cursor_sdk_response` 聚合同一事件集为 Anthropic JSON，缺终态或 error 返回 502。usage 写入会话 token cache，`count_tokens` 复用（缺失返回 0）。

**Run 状态机与 journal 恢复**：sidecar 内每个 `/run` 对应一个 SDK Agent Run。会话状态（消息前缀、pending callback）持久化在 `cursor_auth_dir` 的 `sessions.jsonl` 与 journal 文件：journal 记录已确认的消息前缀哈希，冷续接（sidecar 崩溃重启后同会话下一请求）只回放到已确认边界，不跨过未确认的副作用。入站与已记录 transcript 完全相同（零新回合，即客户端失败后重发同一 body）时降级 full replay 重跑，不报错。已知限制：崩溃窗口内未确认的工具副作用可能在续接后重复执行，由客户端 tool_result 幂等性兜底。模型目录经 `/models` 拉取（含每模型参数词表），按 `cursor_models` 白名单（`default` 归一为 `auto`，条目可钉 `"id:param=value"` 固定参数拼进模型 name）过滤后合成 name `cursor` 的 provider，词表存 `CursorRuntime.vocab` 供转换层映射；与现有 provider alias 冲突的模型跳过并告警（name 可跨 provider 重复）。启动、`/reload` 与每 3 小时后台刷新；失败保留最近成功目录与词表。

### 各协议 System 提示词清洗差异矩阵

| 目标协议 / 模型 | 计费归属指纹 (`x-anthropic-billing-header`) | Claude 身份声明与品牌块 (`<identity>`) | 行为策略与冗长触发块 (`<rules>` / `<response_style>` 等) | 简洁样式触发块 (`# Output Style:` / `# Concise Style Active`) | 白名单段落 (`# Memory` / `# Environment` / `# Language` / CLAUDE.md) | 目标承载位置与行为适配块 |
| :--- | :--- | :--- | :--- | :--- | :--- | :--- |
| **Claude 原生模型** (`*claude*`) | 保留（直通不改） | 保留（原生匹配） | 保留 | 保留 | 保留 | 原生 `system` 字段（保持入站逐字节不变） |
| **Claude 路由非 Claude 模型** | 剥离（防破坏上游前缀缓存） | 剥离 | 剥离 | 剥离 | 保留 | 原生 `system` 字段（保留 `cache_control`） |
| **OpenAI Chat** (全量模型) | 剥离 | 剥离 | 剥离 | 剥离 | 保留 | o 系列/GPT-5/Astra 入 `developer` message；其余入 `system` message |
| **OpenAI Responses** (GPT / Codex) | 剥离 | 剥离 | 剥离 | 剥离 | 保留 | `instructions` 留空；注入 `GPT_CODEX_ADAPTER_BLOCK` + 白名单入 `developer` message |
| **OpenAI Responses** (GPT-6 Astra) | 剥离 | 剥离 | 剥离 | 剥离 | 保留 | `instructions` 留空；注入 `GPT_6_ASTRA_ADAPTER_BLOCK` + 白名单入 `developer` message |
| **OpenAI Responses** (xAI Grok) | 剥离 | 剥离 | 剥离 | 剥离 | 保留 | `instructions` 留空；注入 `GROK_ADAPTER_BLOCK` + 白名单入 `developer` message |
| **OpenAI Responses** (其他上游，如 GLM/DeepSeek) | 剥离 | 剥离 | 剥离 | 剥离 | 保留 | 仅合并白名单段落直入 `instructions` 字段；无 developer 适配块注入 |
| **Gemini 直连** | 剥离 | 剥离 | 剥离 | 剥离 | 保留 | 转换为 `systemInstruction.parts` |
| **Antigravity** | 剥离 | system 前导 Claude 身份句中和为 `You are an AI agent.`，保留后文 | 剥离 | 剥离 | 保留 | 转换为 `request.systemInstruction.parts`；不改 user 内容 |
| **Cursor SDK** | 剥离 | 剥离 | 剥离 | 剥离 | 保留 | 全量发送时以 `system:\n<prompt>` 前缀拼进 run 正文（SDK `systemPrompt` 选项账号门控不可用）；suffix 续跑不拼 |

## 传输与可靠性

Responses 的最终 body 在 payload 覆盖后、zstd 压缩前序列化，对齐 codex `ed0cc1a4ab`，顶层按 `model`、`stream`、已有 `service_tier` 排在 instructions/input 前，避免增量解析网关先缓冲多 MiB prompt。缺失字段不补，显式值不改；其余顶层键与全部嵌套数据保持原顺序，不克隆大 input。Claude、Chat、Gemini 和 Antigravity 的序列化字节不受影响。

Grok 交互身份对齐 sub2api `ad05eda15`：Chat/Responses 的 identifier 为 `grok-pager`，mode 为 `interactive`，UA 为 `grok-pager/{version} grok-shell/{version} ({os}; {arch})`。默认版本 `1.0.46`，已有 `user_agents.grok_version` 覆盖原样保留；不新增版本钳制。`x-authenticateresponse: authenticate-response` 仅解析后 host 等于 `cli-chat-proxy.grok.com` 时添加，不匹配子域或路径字面量；既有模型/协议门控与 session/doom-loop 语义不变。

`UpstreamClient` 按最终代理地址缓存 `reqwest::Client`。请求使用协议对应 URL、认证和 User-Agent：Gemini 使用 `x-goog-api-key`，其他协议使用 Bearer；Responses 的 GPT 请求带 Codex 会话头，Grok 请求带 CLI 身份头和会话亲和 `x-grok-conv-id`。连接池空闲 90s（对齐 grok；流 chunk idle 为服务当前设置 180s，不是池寿命）。`send()` 等到响应头最多 60s。流式路径在转换前对上游 `stream.next()` 套 180s chunk idle，超时发 Anthropic error，不套 `Client::timeout` 掐整条 SSE。复用连接死亡（reset/broken pipe/提前关闭）立刻重试一次；普通建连失败/建连超时不属于死连接，交给 URL 回退，避免单 URL 建连超时被内部重试放大。`build()` 失败返回错误，不回落到默认 Client。

非流响应 body 有界读取：成功 body 上限 16 MiB（恰好上限可读，多 1 字节拒绝），错误 body 最多保留 256 KiB（超出停止读取并标记截断，截断前缀不当作完整 JSON 解析）；读取停顿与流 chunk idle 共用 180s，每次非空数据后重置，空 chunk 不续期。成功 body 超限返回 502、停顿返回 504（Anthropic `api_error`）；错误 body 截断或读取失败保留已知上游状态（429/401 不被改写），生成有界 Anthropic error，不再触发上游重试。OAuth/project/model 路径同样有界，保留各自 30s 请求总超时。

messages 路径不做本地退避重试：429、5xx（含 Cloudflare 52x）与网络错误在同轮内轮转多 `base_url`，耗尽后快速失败，把末次上游错误（Anthropic error 形状）返给客户端；退避重试交客户端（如 Claude Code 自带退避），`Retry-After` 头透传。429 不本地重试对齐 codex 传输层 `retry_429: false`。流式 OpenAI 请求声明 `Accept: text/event-stream` 和 `Cache-Control: no-cache`。

## 响应转发

Claude 响应字节直通。OpenAI Chat、OpenAI Responses、Gemini 和 Antigravity 分别由状态机转换为 Anthropic SSE。状态机维护 content block、thinking、工具调用、usage（含 `output_tokens_details.reasoning_tokens`）和终态，避免上游事件交错破坏 Anthropic 事件顺序。Chat 状态机在工具块未关闭前缓存交错文本与思考并在 finalize 时按序输出。Responses 上游未输出 `output_text.delta` 时从 `response.completed` 恢复 terminal 文本；0 token 的 `response.incomplete` 会直接抛出错误。Gemini 保持活跃思考块跨空文本片段不中断。

OpenAI 首帧预读跳过 `: keepalive` 心跳帧，只认业务帧（心跳帧会骗过错误检测、提前提交 200 丧失重试窗口）；首帧 error 内部重试一次，仍失败返回 502 Anthropic error，不提交 200。已输出首帧后的错误、未满足终态的 EOF、空 Gemini 风格流、读取错误和上游 180s chunk idle 产生结构化 Anthropic error，而不是裸断开。终态事件（含 `message_stop`）完整下发后立即结束流并关闭上游连接，不等上游 EOF（上游在 keep-alive/HTTP2 复用连接上可能拖延关流）。每条流式路径统一包裹 10 秒空闲心跳 `: keepalive\n\n`。非流 Claude 直通；其他路径转换为 Anthropic JSON，无法转换时保留上游原始 body。

## 会话、缓存与 reasoning

Claude Code 会话 ID 优先取 `x-claude-code-session-id`，其次从 `metadata.user_id` 派生。它用于 drift 分桶、`prompt_cache_key` 和协议会话头。OpenAI provider 开启 `prompt_cache_key` 后，Chat 和 Responses 使用该裸会话 ID；Chat+Grok 例外，使用 `x-grok-conv-id` 维持亲和。

Responses 流会收集可回放 reasoning。服务端以模型和会话为键保存完整回合，在下一请求中按 tool call 和输入锚点插回兼容的 reasoning 项，防止 `store=false` 上游丢失推理上下文。签名检查按目标提供方族执行；支持 CAQS Envelope v4 容器 field 5 signature 回退与 v4+ 豁免；无效或不兼容的 thinking 不会发到上游。

## OAuth provider

Antigravity、xAI、Codex、Cursor 登录命令均在凭证保存成功后调用共享的自动重载逻辑，按当前配置的监听地址发送 `POST /reload`；通配 IPv4/IPv6 地址转回环地址，不走代理、不跟随重定向，连接超时 2 秒、总超时 30 秒。仅 2xx 报告重载成功；配置读取、连接或 HTTP 错误只提示，不改变登录成功结果。服务未启动时下次启动加载；`--auth-dir` 不修改服务配置，需与服务扫描目录一致。

`antigravity-login` 使用浏览器回调登录并保存凭证。运行时后台读取有效凭证、刷新 token、拉取模型并注入 provider；首次加载不阻塞监听，之后每 3 小时刷新，失败时保留现有路由。

Antigravity 上游默认短连接：空闲连接在响应结束后立即关闭，防止凭证轮换下 socket 堆积与陈旧连接错误。`antigravity.connection-pool` 显式启用连接池（`idle-conn-timeout` 默认 30s、上限 210s，不超过 Google Frontend 240s keep-alive 截止；`max-idle-conns-per-host` 默认 2、上限 100），其余协议共享全局客户端不受影响。

`xai-login` 使用 OAuth device flow。启动和配置重载扫描 xAI 凭证，必要时提前刷新 token，并为每份有效凭证注入一个 Responses provider。相对 `auth_dir`、`xai_auth_dir` 和 `models_file` 始终相对配置文件目录解析。缺省 `models.json` 与配置同目录。缺文件或模型未收录时不钳 effort；条目可设 `force_effort` 固定档（生效范围与钳制一致，值不钳制）；解析失败则启动或 `/reload` 报错。

`codex-login` 使用 PKCE 浏览器授权（本地回调端口默认 1455）。凭证保存 `chatgpt_account_id` 与 `chatgpt_plan_type`（取自 ID token 的 `https://api.openai.com/auth` claim）。启动和配置重载扫描 Codex 凭证，token 提前 24 小时刷新（失败重试 3 次，`refresh_token_reused` 不重试），并为每份有效凭证注入一个 Responses provider，上游为 `https://chatgpt.com/backend-api/codex/responses`。请求时自动携带 `Chatgpt-Account-Id` 订阅身份头与 `X-Codex-Routing-Hint: model=<解析后模型>[;tier=<service_tier>]`（对齐 codex CLI `build_routing_hint_header`，hint 在压缩前从最终 body 构造，入站同名头可覆盖），并对请求体做 zstd 压缩（level 3，对齐 codex CLI `enable_request_compression` 默认行为；压缩在序列化后执行一次，退避重试共享压缩字节）；静态 API key provider 无该 metadata 不发头也不压缩。`codex_auth_dir` 相对配置文件目录解析。

Cursor sidecar 生命周期独立于配置快照：运行时状态（进程句柄、sidecar 客户端）挂在 `AppState.cursor` 的进程级 RwLock 中，不进 `ConfigSnapshot`。启动时若 `cursor_auth_dir` 启用则 spawn sidecar 并拉取目录合入 providers；`/reload` 发布新快照后按新旧配置对齐——禁用变启用启动、启用变禁用停止、`auth_dir` 变化受控重启，其余字段原子更新；catalog 重新合成后按版本保护发布（版本过期丢弃）。3 小时后台刷新失败或 sidecar 未运行时，从当前快照保留 name `cursor` 的 provider，不移除路由。

## 并发、安全与诊断

providers、payload、运行时配置及后台刷新参数保存在统一不可变 `Arc<ConfigSnapshot>` 中，通过一把 `RwLock` 整体发布。Messages、count_tokens 和 models 在请求入口只复制一次 Arc，认证、转换和上游访问使用同一版本；读取后立即释放锁，旧请求不随 reload 切换配置。

并发 `/reload` 按取得专用互斥锁的顺序串行执行完整加载、校验和发布；不按网络完成顺序覆盖。只有成功发布推进版本，加载或校验失败保留整个旧快照及版本。加载期间不持有配置写锁，请求读取和后台刷新不被配置 IO 阻塞。成功 reload 仍清空认证缓存，并沿用每次重建 UpstreamClient 的行为；本批不加入客户端复用优化，避免沿用旧代理或旧 Antigravity 连接策略。`logging.level` 仍仅启动生效。

后台每轮从当前快照复制版本、静态 providers、Antigravity/xAI/Codex 凭证目录和全局代理，再无锁执行凭证及模型读取。静态配置和刷新参数只在启动或成功 reload 时更新；后台不再重读未发布的磁盘配置。最终集合先校验，再在同一写锁内比较版本并发布；版本变化则丢弃旧轮次。集合未变化时不替换快照、不推进版本。后台只更新 providers，保留同版本的 payload、runtime 和刷新输入。

后台 Antigravity 返回空集合时，沿用整个 provider 集合保旧策略，本轮 xAI 结果也不发布。显式 reload 优先应用新配置：移除的静态 provider、切换目录或禁用凭证不从旧快照补回；动态加载器沿用既有跳过不可用凭证的语义，可能返回部分或空集合。该集合通过校验后随 reload 整体发布。之后的后台只扫描新目录，旧轮次不能恢复被移除的 provider。凭证刷新等磁盘副作用不属于配置快照事务。

`secret_key` 明文会写回 bcrypt；验证结果缓存以 (bcrypt hash, key) 为键，换 secret 后旧结果不可命中，最多 1024 项，满时淘汰最久未访问条目（不再整体清空），重载时清空。`logging.request_body` 保存最终出站请求的诊断数据，敏感入站头会脱敏。归一化只在能够安全处理时改变 body，避免优化本身阻断请求；转换失败由 HTTP 层返回 Anthropic error。

## 验证

测试与实现同文件的 `#[cfg(test)]` 模块以及 server 集成测试共同覆盖转换、归一化、SSE、HTTP 管线、热重载和 OAuth 辅助逻辑。运行 `cargo test --workspace`，再运行 `cargo clippy --workspace --all-targets -- -D warnings`。
