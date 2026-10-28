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
| 保存 `config.yaml` / `models.json` 或 `POST /reload` | 重新读取运行时配置（文件监视与 HTTP 共用发布路径）。 |

`secret_key` 启用后，三个 Anthropic API 端点都要认证；`/health` 和 `/reload` 不认证。

## 请求管线

1. 读取请求体并校验 `x-api-key` 或 `Authorization: Bearer`。
2. 由入站 `model` 解析 `(provider, protocol, upstream_model)`。
3. 按协议执行转换前归一化。
4. 转换成目标请求体，或在 Claude 路径只替换 `model`，再清洗非 Claude 模型的 system 并钳制其越档 effort。
5. OpenAI 路径执行转换后归一化；Gemini 和 Antigravity 跳过该步骤。
6. 应用 `payload` 顶层参数覆盖，保留已有非空 `prompt_cache_key`。
7. 为符合条件的 OpenAI 请求注入会话 cache key，必要时写诊断日志。
8. 选择 URL、代理、认证头和 User-Agent，发送上游请求并处理重试。
9. 直通或转换流式 SSE；非流响应也恢复为 Anthropic 形状。

## 路由与配置

`alias` 优先匹配；上游真实模型名是回退匹配，便于 `count_tokens` 等辅助请求。重复 alias 在启动或重载校验时失败。`base_url` 支持列表：messages 路径遇网络错误、429 或 5xx（含 Cloudflare 52x）在同轮内立即切换后续地址，耗尽后快速失败，不做本地退避（退避交客户端，`Retry-After` 透传）；OAuth/token refresh 的 5xx 可重试退避（Claude / Antigravity / Codex / xAI）属于凭证层，不参与 messages URL 轮转。provider 代理优先于全局代理，`"direct"` 或空值不使用代理。

配置重载替换 providers、payload、normalize、认证、全局 HTTP client、User-Agent 和 reasoning 注册表。`logging.level` 在启动时创建 `EnvFilter`，因此不能热改。

## 缓存稳定化

归一化保持幂等。Claude 直通运行完整流程：工具和 schema 排序、历史 reminder 拆分、`tool_use.input` 键序、system 列表、尾部空白、客户端日期指纹，以及对前缀的 volatile 只读 WARN。转换路径先运行适合 Anthropic 输入的确定性子集；Gemini / Antigravity 在转换前补跑日期指纹与 volatile WARN。OpenAI 输出随后排序工具/schema、system reminder、日期字段，并做 volatile WARN（Chat 的 system/developer 消息计入前缀扫描）。

`drift_detector` 对同一会话的结构哈希发出告警，不修改请求；仅 Claude 与 OpenAI 路径接线。Gemini 与 Antigravity 不做转换后归一化或 drift 观测，避免将 Anthropic/OpenAI 规则施加到 Gemini 出站形状。**建议与 `normalize.enabled` 同开**：先归一化再观测，否则键序/reminder 抖动易产生假阳。父级 `system`/`tools` 变化打 WARN（`scope=parent`）；会话级（含 `early_messages`，以及可选的 `tools_description` 软维）打 DEBUG（`scope=conversation`）。`request-id` / `anonymous` 父级不稳定，跳过基线。可选 `normalize.drift_tools_description`（默认 false）仅会话 DEBUG，永不进父级 WARN。上游 `cache_read`/`cache_creation` 经 pending 与父级 WARN 限速关联（DEBUG `cache_drift_usage_correlated`）。

## 协议转换

### Claude

`convert_passthrough` 只改 `model`，再对非 Claude 模型做两件事。一是 system 清洗（对齐 chat/responses/gemini）：顶层 `system` 与 `messages` 内 `role: system` 项剥掉计费归属指纹（逐请求变化，破坏上游缓存前缀；归属行与指令同块时只剥前导行、保留剩余指令，对齐 sub2api）、Claude 身份声明与 `<identity>`/`<rules>`/`# Output Style:` 等 Claude 触发段，保留白名单段与 `cache_control`，清空后移除该块或消息。二是越档 effort 钳制：命中 glob `*claude*` 的模型、`thinking.type: disabled` 或注册表未命中的请求跳过；写回优先顶层 `output_config.effort`，回退 `thinking.output_config.effort`，值不变不写回。条目设置 `force_effort` 时改写为该固定值（不钳制），跳过条件不变。`*claude*` 模型两项都跳过，保持逐字节直通。身份头按排除表透传，`anthropic-beta` 默认原样转发；订阅 OAuth（`sk-ant-oat*`）缺 `oauth-2025-04-20` 时补上，已有则不动。

### OpenAI Chat

Anthropic `system` 成为 system message；o 系列（`o1-mini`/`o1-preview` 除外）、GPT-5 族、`gpt-6-astra` 及其日期快照改用 `developer`。这些模型把 `max_tokens` 改发为 `max_completion_tokens`；o 系列删除 `temperature`，GPT-5 仅在 `gpt-5.1`/`gpt-5.2`/`gpt-5.4` 无 reasoning 时保留采样，Astra 永不发 `temperature`/`top_p`。全量 `openai_chat` 模型剥除 Claude system triggers 与 prompt reminder。Kimi K2.8 模型（`kimi-k2.8`/`kimi-k2.8-code`）将 reasoning 映射为 `thinking.type`/`effort`，显式 `none` 绕过 clamp，并守卫 temperature（disabled 限 0.6，enabled/default 限 1.0）。用户、助手、图片、工具调用和工具结果转换为 Chat Completions 形状。工具 `input_schema` 中的 `required: null` 仅在 schema 节点删除；原本缺省的 `required` 仍按现有规则补 `[]`，显式 null 不补。对齐 CPA `4b4a4448`，schema 位置的 `true`（根、properties/$defs、items 和联合分支）转为 `{}`；`false`、布尔 `additionalProperties`、`default` 和 `enum` 实例数据保持不变。`tool_choice.disable_parallel_tool_use` 映射为 `parallel_tool_calls: false`（未显式关闭不写，OpenAI 默认开）。`thinking` 映射为受模型能力限制的 `reasoning_effort` 或兼容的 reasoning 内容。无等价物的 Claude server-side web search 工具会删除。

### OpenAI Responses

所有 Responses 上游先剥离 Claude 系统触发段（与 GPT/Grok 同一套 scrub），白名单段落保留。普通上游把清洗后的 system 放到 `instructions`；GPT/Grok 上游再使用固定 developer 适配块组装。GPT-6 Astra 使用独立适配块，未指定 effort 时默认 `low`；其余 Responses 上游默认 `medium`。effort 再按用户 `models.json` 钳到该模型支持档（Astra 示例为 `low`/`medium`，更高档钳到 `medium`）；条目设置 `force_effort` 时固定使用该值（不钳制）。查不到或未配置文件则不钳。工具、tool choice、图片和自定义工具转换为 Responses 项；过长工具名使用请求侧缩写和响应侧反向映射，截断后清理前导 `_`/`-`（对齐 CPA `capResponsesChatToolName`；清理后为空则保留原截断值）。纯 const union（≥8 分支）简化为 enum 并清理 JSON Schema 方言关键字；工具 schema 节点的 `required: null` 删除而非替换为 `[]`，`default` 等实例数据不变。web_search 按家族映射：Grok 映射为 `filters.excluded_domains`，OpenAI 映射为 `filters.blocked_domains`，`allowed_domains` 优先。reasoning 清洗时将空 summary 的 `reasoning_text` 提升为 `summary_text`，强制 `reasoning.content: []`。严格 JSON schema 不满足 Responses 要求时自动降级 `strict`。

### Gemini 与 Antigravity

两条路径共享 Gemini `contents`、`parts`、`functionCall` 和 `functionResponse` 模型，剥离 Claude system triggers。Antigravity（含 Claude 模型）一律将 system 块开头的 Claude Agent SDK/Claude Code 身份句改为 `You are an AI agent.`，保留句后指令；Gemini 直连不做此改写。工具 schema 会清理本地引用和不支持关键字（含 `additionalItems`/`unevaluated*`/`contentSchema`），布尔 `true` 子 schema 归一化为空对象；Gemini 直连的 `parametersJsonSchema` 保留 `additionalProperties` 与 `pattern`/`minLength` 等标准约束，Antigravity 仍搬入 description 提示。任一工具带 `strict: true` 且 tool_choice 为 auto/缺省时，Gemini 直连使用 `VALIDATED` 工具模式（Antigravity 的 `VALIDATED` 仅由 Claude 模型触发）。`tool_choice=none` 时两条路径都设 `mode=NONE` 且保留 tools 声明（对齐 CPA `80aa2b1d`，不再因 NONE 删 tools）。规范化 `responseJsonSchema` 为 `responseSchema`；工具结果强制字符串化为 `response.result`；user turn 尾部文本重排至 `functionResponse` 前。Antigravity 的 Claude 模型先拆分混合 user 回合并把 `functionResponse` 回合提升到紧跟含 `functionCall` 的 model 回合（system reminder 与普通文本顺延），再仅合并不含 `functionResponse` 的相邻 user 回合；非 Claude 模型维持合并加重排。Gemini 使用 API key 和 Google 端点。Antigravity 额外套 `model`、`request`、`project`、`requestId` 等信封；Claude 模型使用 `VALIDATED` 工具模式，冲突工具名加 `external_` 前缀；`gemini-3.5-flash-lite` 限制 `max_completion_tokens` 上限为 65535。

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
| **Antigravity**（含 Claude 模型） | 剥离 | system 前导 Claude 身份句中和为 `You are an AI agent.`，保留后文 | 剥离 | 剥离 | 保留 | 转换为 `request.systemInstruction.parts`；不改 user 内容 |

## 传输与可靠性

Responses 的最终 body 在 payload 覆盖后、zstd 压缩前序列化，对齐 codex `ed0cc1a4ab`，顶层按 `model`、`stream`、已有 `service_tier` 排在 instructions/input 前，避免增量解析网关先缓冲多 MiB prompt。缺失字段不补，显式值不改；其余顶层键与全部嵌套数据保持原顺序，不克隆大 input。Claude、Chat、Gemini 和 Antigravity 的序列化字节不受影响。

Grok 交互身份对齐 sub2api `ad05eda15`：Chat/Responses 的 identifier 为 `grok-pager`，mode 为 `interactive`，UA 为 `grok-pager/{version} grok-shell/{version} ({os}; {arch})`。默认版本 `1.0.46`，已有 `user_agents.grok_version` 覆盖原样保留；不新增版本钳制。`x-authenticateresponse: authenticate-response` 仅解析后 host 等于 `cli-chat-proxy.grok.com` 时添加，不匹配子域或路径字面量；既有模型/协议门控与 session/doom-loop 语义不变。

`UpstreamClient` 双栈：默认车道按 (proxy, antigravity, no_redirect, credential scope) 缓存 `reqwest::Client`；`protocol: claude` 走 Claude TLS fingerprint lane（wreq/BoringSSL），按 (proxy, no_redirect) 进 `fp_clients`。请求使用协议对应 URL、认证和 User-Agent：Gemini 使用 `x-goog-api-key`，其他协议使用 Bearer；Responses 的 GPT 请求带 Codex 会话头，Grok 请求带 CLI 身份头和会话亲和 `x-grok-conv-id`。连接池空闲 45s（TCP keepalive 30s；流 chunk idle 为服务当前设置 180s，不是池寿命）。`send()` 等到响应头最多 60s。流式路径在转换前对上游 `stream.next()` 套 180s chunk idle，超时发 Anthropic `timeout_error`，不套 `Client::timeout` 掐整条 SSE。复用连接死亡（reset/broken pipe/提前关闭）先丢掉缓存 Client 再建池，再立刻重试一次；等响应头 `SEND_TIMEOUT`「上游请求超时」映射为 408 `timeout_error`（对齐 CPA Claude），且不走死连接重试，避免真慢被放大成双倍空等；普通建连失败/建连超时（传输层 `is_timeout`）不属于死连接，交给 URL 回退，避免单 URL 建连超时被内部重试放大。`build()` 失败返回错误，不回落到默认 Client。

非流响应 body 有界读取：成功 body 上限 16 MiB（恰好上限可读，多 1 字节拒绝），错误 body 最多保留 256 KiB（超出停止读取并标记截断，截断前缀不当作完整 JSON 解析）；读取停顿与流 chunk idle 共用 180s，每次非空数据后重置，空 chunk 不续期。成功 body 超限返回 502（`api_error`）、停顿返回 504（`timeout_error`，对齐 CPA Claude）；错误 body 截断或读取失败保留已知上游状态（429/401 不被改写），生成有界 Anthropic error，不再触发上游重试。OAuth/project/model 路径同样有界，保留各自 30s 请求总超时。

messages 路径不做本地退避重试：429、5xx（含 Cloudflare 52x）与网络错误在同轮内轮转多 `base_url`，耗尽后快速失败，把末次上游错误（Anthropic error 形状）返给客户端；退避重试交客户端（如 Claude Code 自带退避），`Retry-After` 头透传。429 不本地重试对齐 codex 传输层 `retry_429: false`。流式 OpenAI 请求声明 `Accept: text/event-stream` 和 `Cache-Control: no-cache`。

上游 400/422 按错误文案识别特性不兼容（`response_format` / Responses `text.format`、reasoning `off`、`tool_choice`、tools+reasoning 冲突），按 `(provider, upstream_model)` 记入进程内 unfit 注册表并原地降级请求体重试一次：schema 注入 system/instructions 后剥 format——Chat/Claude 移除顶层 `response_format` / `output_config`，Responses 只剥 `text.format`（保留同级 `text.verbosity`，空则再删顶层 `text`）——effort `off`/`none` 提到 `low`，`tool_choice` 降为 `auto`，或移除 reasoning 配置；降级字段按目标协议形状写（Chat `messages`/`reasoning_effort`/字符串 `tool_choice`，Responses `instructions`/`reasoning.effort`，Claude `system`/`thinking`）。标记跨请求存活，已被标记的模型在首发前就按标记预降级，不再每请求先打一次 400；注入失败时不剥字段也不重试。`/reload` 清空注册表并增代，旧飞行请求的迟到标记丢弃。

## 响应转发

Claude 响应字节直通。OpenAI Chat、OpenAI Responses、Gemini 和 Antigravity 分别由状态机转换为 Anthropic SSE。状态机维护 content block、thinking、工具调用、usage（含 `output_tokens_details.reasoning_tokens`）和终态，避免上游事件交错破坏 Anthropic 事件顺序。Chat 状态机在工具块未关闭前缓存交错文本与思考并在 finalize 时按序输出。Responses 上游未输出 `output_text.delta` 时从 `response.completed` 恢复 terminal 文本；0 token 的 `response.incomplete` 会直接抛出错误。Gemini 保持活跃思考块跨空文本片段不中断。

OpenAI 首帧预读跳过 `: keepalive` 心跳帧，只认业务帧（心跳帧会骗过错误检测、提前提交 200 丧失重试窗口）；首帧 error 内部重试一次，仍失败返回 502 Anthropic error，不提交 200。已输出首帧后的错误、未满足终态的 EOF、空 Gemini 风格流、读取错误和上游 180s chunk idle 产生结构化 Anthropic error，而不是裸断开。终态事件（含 `message_stop`）完整下发后立即结束流并关闭上游连接，不等上游 EOF（上游在 keep-alive/HTTP2 复用连接上可能拖延关流）。每条流式路径统一包裹 10 秒空闲心跳 `: keepalive\n\n`。非流 Claude 直通；其他路径转换为 Anthropic JSON，无法转换时保留上游原始 body。

## 会话、缓存与 reasoning

Claude Code 会话 ID 优先取 `x-claude-code-session-id`，其次从 `metadata.user_id` 派生（与 `extract_claude_code_session` 一致）。它用于 drift 父级分桶、`prompt_cache_key` 和协议会话头；无稳定会话时 drift 还可回退 `x-tklite-session-key`，再退到不稳定的 `x-request-id`/`anonymous`（不播种基线）。OpenAI provider 开启 `prompt_cache_key` 后，Chat 和 Responses 使用该裸会话 ID；Chat+Grok 例外，使用 `x-grok-conv-id` 维持亲和。

Responses 流会收集可回放 reasoning。服务端以模型和会话为键保存完整回合，在下一请求中按 tool call 和输入锚点插回兼容的 reasoning 项，防止 `store=false` 上游丢失推理上下文。签名检查按目标提供方族执行；支持 CAQS Envelope v4 容器 field 5 signature 回退与 v4+ 豁免；无效或不兼容的 thinking 不会发到上游。

Antigravity（gemini/flash/agent，不含 claude）使用**独立的** thoughtSignature 账本（`AgReplayCache`），与 Responses 的 `input`/`encrypted_content` 缓存分离。

账本条目两种形状（对齐 CPA）：
- `thought_signature`：`thoughtSignature` + 可选 `contentIndex` / `partIndex` / `targetKind`（`text`|`thought`）/ `targetHash` / `targetOccurrence` / `contextHash`
- `function_call_part`：`name` + `args` + 可选 `call_id` / `thoughtSignature` + 同上索引字段

累积与提交：流式与非流式从上游 `candidates[].parts` 累积可回放项；`finishReason` **只标记 terminal**，不立即落库（后续帧仍可能带 signature / usage）。流式在 SSE 结束（`finish` / `Drop`）时若 terminal 才提交；非流在完整 body 观察后提交。terminal 但空或字节/条数溢出则清空该会话账本；非 terminal 不动缓存。

下一轮在已有 `request.contents` part 上补回 `thoughtSignature`，稳定 tool_use id（`ccx_gemini_…`）换回原生 `functionCall.id`，孤儿 `functionResponse` 按账本插回缺失的 functionCall（对齐 CPA）。上游 400 且 body 含 `signature` 时按读账本快照清空该会话账本（unfit 与 401 重试不清）。

配对校验对齐 CPA：每个 AG Gemini 族请求（与有无会话 / 账本无关）都校验 functionCall / functionResponse 配对；回放破坏配对则退回原始请求，原始历史本身无效则本地回 400。历史不变时该 400 每轮重现，直到用户回退（rewind）或 `/clear`。

## OAuth provider

Antigravity、xAI、Codex、Claude 登录命令均在凭证保存成功后调用共享的自动重载逻辑，按当前配置的监听地址发送 `POST /reload`；通配 IPv4/IPv6 地址转回环地址，不走代理、不跟随重定向，连接超时 2 秒、总超时 30 秒。仅 2xx 报告重载成功；配置读取、连接或 HTTP 错误只提示，不改变登录成功结果。服务未启动时下次启动加载；`--auth-dir` 不修改服务配置，需与服务扫描目录一致。

`antigravity-login` 使用浏览器回调登录并保存凭证。运行时后台读取有效凭证、提前 300 秒刷新 token（对齐 CPA 5 分钟安全窗口）、拉取模型并注入 provider；首次加载不阻塞监听，之后每 3 小时刷新，失败时保留现有路由。上游统一锁定单 daily 节点（`daily-cloudcode-pa.googleapis.com`），禁止跨集群回退以防破坏上游前缀缓存。

Antigravity 上游默认短连接：空闲连接在响应结束后立即关闭，防止凭证轮换下 socket 堆积与陈旧连接错误。`antigravity.connection-pool` 显式启用连接池（`idle-conn-timeout` 默认 30s、上限 210s，不超过 Google Frontend 240s keep-alive 截止；`max-idle-conns-per-host` 默认 2、上限 100），其余协议共享全局客户端不受影响。出站优先协商 HTTP/2（谈不上再走 HTTP/1.1）。Gemini 与 Antigravity 均不发送 `x-vscode-sessionid`（现 CPA / 官方路径已无此硬编码头）；其它自定义非标头同样清洗，保持同构规避 Google WAF。

`xai-login` 使用 OAuth device flow。启动和配置重载扫描 xAI 凭证，必要时提前刷新 token，并为每份有效凭证注入一个 Responses provider。相对 `auth_dir`、`xai_auth_dir` 和 `models_file` 始终相对配置文件目录解析。缺省 `models.json` 与配置同目录。缺文件或模型未收录时不钳 effort；条目可设 `force_effort` 固定档（生效范围与钳制一致，值不钳制）；解析失败则启动或 `/reload` 报错。

`codex-login` 使用 PKCE 浏览器授权（本地回调端口默认 1455）。凭证保存 `chatgpt_account_id` 与 `chatgpt_plan_type`（取自 ID token 的 `https://api.openai.com/auth` claim）。启动和配置重载扫描 Codex 凭证，token 提前 24 小时刷新（失败重试 3 次，`refresh_token_reused` 不重试），并为每份有效凭证注入一个 Responses provider，上游为 `https://chatgpt.com/backend-api/codex/responses`。请求时自动携带 `Chatgpt-Account-Id` 订阅身份头与 `X-Codex-Routing-Hint: model=<解析后模型>[;tier=<service_tier>]`（对齐 codex CLI `build_routing_hint_header`，hint 在压缩前从最终 body 构造，入站同名头可覆盖），并对请求体做 zstd 压缩（level 3，对齐 codex CLI `enable_request_compression` 默认行为；压缩在序列化后执行一次，退避重试共享压缩字节）；静态 API key provider 无该 metadata 不发头也不压缩。`codex_auth_dir` 相对配置文件目录解析。

`claude-login` 使用 PKCE 浏览器授权（本地回调端口默认 54545，RedirectURI 固定 `http://localhost:54545/callback`）。凭证保存 access/refresh、email、account/org UUID 与单个 device id。启动和配置重载扫描 `claude_auth_dir`（默认配置文件旁 `.cache/claude`），token 提前 4 小时刷新（仅 HTTP 5xx 可重试，传输/解码错误不盲重放 refresh），并为每份有效凭证注入一个 `protocol: claude` provider（`base_url: https://api.anthropic.com`，key=access_token）。出站走现有 Claude 直通与缓存归一化；OAuth token 缺 `oauth-2025-04-20` 时补 beta。不做 cloaking / 假 user_id / 改 UA。OAuth 订阅（Claude / Antigravity / Codex / xAI）预请求刷新失败时硬失败，拒绝继续打旧 token；上游 401 时强制刷新被拒 access_token 后重试一次，刷新仍失败则返回 `authentication_error`。


## 并发、安全与诊断

providers、payload、运行时配置及后台刷新参数保存在统一不可变 `Arc<ConfigSnapshot>` 中，通过一把 `RwLock` 整体发布。Messages、count_tokens 和 models 在请求入口只复制一次 Arc，认证、转换和上游访问使用同一版本；读取后立即释放锁，旧请求不随 reload 切换配置。

并发 `/reload` 按取得专用互斥锁的顺序串行执行完整加载、校验和发布；不按网络完成顺序覆盖。只有成功发布推进版本并在响应里置 `published=true`，加载或校验失败保留整个旧快照及版本（`published=false`）。加载期间不持有配置写锁，请求读取和后台刷新不被配置 IO 阻塞。成功 reload 仍清空认证缓存，并沿用每次重建 UpstreamClient 的行为；本批不加入客户端复用优化，避免沿用旧代理或旧 Antigravity 连接策略。`logging.level` 仍仅启动生效。

后台每轮从当前快照复制版本、静态 providers、Antigravity/xAI/Codex/Claude 凭证目录和全局代理，再无锁执行凭证及模型读取。静态配置和刷新参数只在启动或成功 reload 时更新；后台不再重读未发布的磁盘配置。最终集合先校验，再在同一写锁内比较版本并发布；版本变化则丢弃旧轮次。集合未变化时不替换快照、不推进版本。后台只更新 providers，保留同版本的 payload、runtime 和刷新输入。

后台 Antigravity 返回空集合时，沿用整个 provider 集合保旧策略，本轮 xAI 结果也不发布。显式 reload 优先应用新配置：移除的静态 provider、切换目录或禁用凭证不从旧快照补回；动态加载器沿用既有跳过不可用凭证的语义，可能返回部分或空集合。该集合通过校验后随 reload 整体发布。之后的后台只扫描新目录，旧轮次不能恢复被移除的 provider。凭证刷新等磁盘副作用不属于配置快照事务。

`secret_key` 明文会写回 bcrypt；验证结果缓存以 (bcrypt hash, key) 为键，换 secret 后旧结果不可命中，最多 1024 项，满时淘汰最久未访问条目（不再整体清空），重载时清空。`logging.request_body` 将最终出站请求的诊断数据写入**配置文件旁的 `logs/`**（启动与 `/reload` 时 pin，不跟进程 cwd）；敏感入站头会脱敏。归一化只在能够安全处理时改变 body，避免优化本身阻断请求；转换失败由 HTTP 层返回 Anthropic error。

## 有意保留（相对对照实现）

以下行为经审计确认保留，不强制对齐对照实现（CPA / sub2api / Codex 等）：

- Chat 的 `content_filter` 映射为 Anthropic `refusal`（非 `stop`）。
- Gemini / Antigravity：上游缺 `finishReason` 时合成终态，避免流无终态挂起。
- Grok Responses 推理账本：空轮仍 append（跟 Codex，不因空内容跳过）。
- `gemini_schema`：`const`→`enum` 求交与缺省 type 补齐比 CPA 更严。
- Chat tool 结果抽图 notice 跟 sub2api（不跟 CPA）。
- 已有表述见上文：Claude 直通仍可走 normalize；Antigravity 优先协商 HTTP/2；`logging.level` 仅启动生效；非 Claude `count_tokens` 读会话缓存；unfit 世代在 `/reload` 增代，与 `snapshot.version` 解耦。

## 验证

测试与实现同文件的 `#[cfg(test)]` 模块以及 server 集成测试共同覆盖转换、归一化、SSE、HTTP 管线、热重载和 OAuth 辅助逻辑。运行 `cargo test --workspace`，再运行 `cargo clippy --workspace --all-targets -- -D warnings`。
