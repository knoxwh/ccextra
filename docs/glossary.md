# 领域术语表

本文定义 ccextra 文档和代码使用的术语。实现流程见 [design.md](design.md)。

## 协议与接口

| 术语 | 含义 |
| --- | --- |
| Anthropic Messages | ccextra 的入站与出口形状，主端点为 `/v1/messages`。 |
| `claude` | 上游 Anthropic Messages 协议。 |
| `openai_chat` | OpenAI Chat Completions 协议，端点为 `/chat/completions`。 |
| `openai_responses` | OpenAI Responses 协议，端点为 `/responses`。 |
| `gemini` | Google Gemini GenerateContent 协议。 |
| `antigravity` | Cloud Code Assist 运输协议，内部携带 Gemini 请求。 |
| `cursor_sdk` | Cursor 原生 connect-rpc 协议（`AgentService/Run` 双向流）；不经通用 upstream，由 cursor handler 直连 api2.cursor.sh。 |
| SSE | `text/event-stream` 响应格式；ccextra 将所有流式路径输出为 Anthropic SSE。 |
| 非流响应 | `stream` 为 `false` 或缺失时的单个 JSON 响应。 |
| 会话输入用量缓存 | 按 session 保存上游真实输入 token 数，供下一轮流首和 count_tokens 使用。Chat/Responses/Gemini/Antigravity 的原始 SSE 正数输入用量也会写回；缺失或零值不覆盖旧值，占位值不写回。记录写入后 30 分钟过期，最多 512 条。 |

xAI Grok 与 Codex 不是 protocol。它们使用 OAuth 凭证动态创建 `openai_responses` provider。Cursor 同样不是手写 protocol：`cursor_auth_dir` 凭证自动合成 name `cursor` 的 `cursor_sdk` provider。

## 路由与配置

**provider**
一个上游连接定义，含 `name`、`protocol`、`base_url`、`key`、可选代理和 models。`base_url` 可按顺序列出多个地址，至少一个；空列表在请求时被拒绝。

**model alias**
客户端发送的模型名。路由先按 alias 匹配，再按真实上游模型名匹配。alias 全局唯一。

**upstream model**
实际写入上游请求 `model` 的名称，可与 alias 不同。

**route decision**
从入站模型得到 `(provider, protocol, upstream_model)` 的结果。未知模型或 alias 冲突会拒绝请求或配置。

**payload override**
`payload` 中按模型 glob 匹配的顶层 JSON 覆盖。规则可用 `protocol` 限定；Claude 直通必须显式限定才接受覆盖。

**reasoning 注册表**
用户 `models.json`（路径由 `models_file` 指定，默认同目录）。按上游模型 `id` 精确匹配，把 effort 钳到该模型支持的最近档。缺文件或未收录不钳。条目可设 `force_effort` 固定档：凡钳制介入处改写为该值（不钳制）。Cursor 家族变体模型按 base id（如 `grok-4.7`）查此表，得到的档位拼进上游 model id（`grok-4.7-high`）。core 不读文件。

**全局代理与 provider 代理**
`server.proxy_url` 是默认值；provider `proxy_url` 优先。`"direct"` 或空值表示不走代理。

**入口认证**
`secret_key` 启用后，Anthropic API 端点要求 `x-api-key` 或 `Authorization: Bearer`。明文配置会转 bcrypt 并回写。

## 请求转换

**body-to-body 转换**
转换器直接读写 `serde_json::Value`，不经过通用中间协议类型，并为每个目标协议单独处理图片、工具和 content 语义。

**直通路径**
Anthropic 到 Claude 的路径。只改 `model`，并按规则转发身份头。非 Claude 模型额外清洗 system（剥计费归属指纹、Claude 身份声明与触发块）并钳制越档 effort；匹配 `*claude*` 的模型两项都跳过，保持逐字节直通。

**转换路径**
Anthropic 到 Chat、Responses、Gemini 或 Antigravity 的独立转换路径。它们重建目标 body，再在需要时映射响应。

**工具调用**
Anthropic `tool_use` 和 `tool_result`。转换器保持 call ID 配对；空工具结果会补安全的非空输出。Responses 过长工具名在请求侧缩短、响应侧还原，缩短结果去掉前导 `_`/`-`（全为分隔符时保留原截断值）。

**服务端 web search 工具**
Claude `web_search_*` 工具。Chat 路径删除它；Responses 映射为 `web_search`；Gemini 风格路径删除它。

**thinking**
Anthropic 推理内容。目标协议会映射为 `reasoning_effort`、reasoning 项或 Gemini thinking 配置；不兼容签名会丢弃。

## 缓存稳定化

**归一化**
对请求进行确定性变换，目标是上游 prompt cache，不保存本地模型响应。完整流程会稳定工具顺序、schema、历史 reminder、工具参数键序、列表和尾部空白，并处理易变日期。

**pretransform / post-transform**
转换前在 Anthropic body 上执行的归一化，以及 OpenAI 转换后在目标 body 上执行的归一化。Gemini 和 Antigravity 不运行 post-transform。

**prompt_cache_key**
OpenAI Chat 或 Responses 的 provider 级缓存桶标识。来自 Claude Code 会话 ID，不覆盖既有非空值；Chat+Grok 不注入。

**drift**
同一会话中 system、tools 或早期消息结构哈希的变化。检测器记录告警，不改变 body。

**会话 ID**
优先使用 `x-claude-code-session-id`，否则从 `metadata.user_id` 派生。用于缓存、drift 和上游会话亲和。

## 运行时术语

**UpstreamClient**
按最终代理地址缓存 `reqwest::Client` 的发送器。它选择协议端点、认证、User-Agent、Grok 会话头和流式头。Responses 序列化时先发顶层 `model`、`stream` 和已有 `service_tier`，供网关在大 input 到达前路由；其余键序与值不变。

**Grok CLI 身份**
Grok Chat/Responses 请求使用 `x-grok-client-identifier: grok-pager`、`x-grok-client-mode: interactive` 与 `grok-pager/{version} grok-shell/{version} ({os}; {arch})`。版本默认 `1.0.46`，可由 `user_agents.grok_version` 覆盖；`x-authenticateresponse: authenticate-response` 仅官方 `cli-chat-proxy.grok.com` 添加，不用于普通 API 或其他模型/协议。

**重试与回退**
messages 路径不做本地退避：429/5xx（含 52x）与网络错误在同轮内轮转多 `base_url`，耗尽后快速失败，把末次上游错误返给客户端，透传 `Retry-After`，退避重试交客户端（如 Claude Code）。流式首帧 error 内部重试一次，仍失败返回 502，不提交 200。

**有界读取**
非流响应 body 的统一读取边界：成功 body 16 MiB（恰好上限可读，多 1 字节拒绝），错误 body 256 KiB（超出停止读取并标记截断，截断前缀不当作完整 JSON 解析）；读取停顿与流 chunk idle 共用 180s，每次非空数据后重置。成功 body 超限返回 502、停顿返回 504；错误 body 截断或读取失败保留上游状态码（429/401 不被改写）；OAuth/project/model 保留各自 30s 请求总超时。

**死连接**
复用池中已失效的连接（reset/broken pipe/提前关闭）。立刻重试一次；普通建连失败/建连超时不属于死连接，交给 URL 回退，避免单 URL 建连超时被内部重试放大。

**relay 状态机**
将 OpenAI 或 Gemini 风格 SSE 转成 Anthropic `message_start`、content block、delta 和终态事件的状态机。终态后忽略后续上游事件。

**keepalive**
流式输出空闲 10 秒后发送的 `: keepalive\n\n` SSE 注释，防止中间网络层关闭空闲连接。上游流本身另有 180s chunk idle（当前服务设置）；超时发 Anthropic error，不是心跳。首帧预读阶段跳过心跳帧，不当作首帧成功。

**reasoning replay**
Responses 上游不保留完整会话时，服务端按模型和会话保存兼容 reasoning 回合，并在下一请求锚定插入。仅可安全回放的内容会进入请求。

**thinking signature**
推理块的不透明连续性令牌。系统按 Claude、Gemini、GPT、Kimi 和 Grok 形状验证兼容性；不能匹配目标提供方时不转发。

**Gemini contents/parts**
Gemini 请求的对话表示。`contents` 是回合数组，`parts` 可含 text、`functionCall`、`functionResponse` 或内联图片。

**Antigravity 信封**
Antigravity 请求的外层对象，含 `model`、`request`、`project`、`requestId`、`requestType` 和 `userAgent`。实际 Gemini body 位于 `request`。

**VALIDATED 工具模式**
Gemini 函数调用模式。Antigravity Claude 模型强制使用；Gemini 直连在任一工具带 `strict: true` 且 tool_choice 为 auto/缺省时使用。其工具 schema 需要可验证的 object properties，因此清洗器会补必要占位字段。

**schema 清洗**
将 Anthropic `input_schema` 转成 Gemini 或 Antigravity 可接受 JSON Schema 的递归过程。它内联本地引用、移除不支持关键字、归一化布尔 `true` 子 schema、处理 enum 和 required，并为不同目标采用不同规则；Gemini 直连保留 `additionalProperties` 与标准约束，Antigravity 搬入 description 提示。声明 `items` 但缺 `type` 的节点补 `type: array`；`type` 显式不是 `array` 时去掉 `items`。OpenAI Chat/Responses 转换另会删除 schema 节点中的 `required: null`，保留实例数据中的同名字段。Chat 将 schema 位置的 `true` 转为空对象，保留 `false`、布尔 `additionalProperties` 与 `default`/`enum` 实例数据。

**OAuth 动态 provider**
由保存的 Antigravity、xAI、Codex 或 Cursor 凭证生成的运行时 provider。Antigravity 后台刷新模型；xAI 与 Codex 在启动和重载时扫描、刷新凭证。Codex 请求携带 `Chatgpt-Account-Id` 订阅身份头；Cursor 经 GetUsableModels 拉取目录合成固定 name `cursor` 的 `cursor_sdk` provider。

**connect-rpc 双向流**
Cursor 原生传输：`agent.v1.AgentService/Run`（h2 + rustls 直连 api2.cursor.sh），Connect 帧 `[1B flags][4B len BE][payload]`。请求头含 `content-type: application/connect+proto`、`connect-protocol-version: 1`、`te: trailers`、`x-ghost-mode: true`、`x-cursor-client-version`、`x-cursor-client-type: cli`；无 checksum、无 x-client-key。出站代理复用全局 `proxy`（HTTP CONNECT 隧道）。

**Run**
一次 `AgentService/Run` 双向流调用，对应一个会话回合。服务端事件归一为 text/thinking 增量、tool 调用、TurnEnded 与 checkpoint。TurnEnded 只表示这一步生成结束，成功终态仍是 Connect end-stream；工具驻留期间忽略 TurnEnded，不取消会话。服务端 InteractionQuery（web_search/ask_question 等）必须回 InteractionResponse，否则挂死整流。用量读 TurnEnded field 1（本轮完整输入，≈ context 大小；上游 cache 字段疑似跨回合累计，不采用）：`message_delta.usage` 按真值 1%/99% 拆分（input 1% + cache_read 99% 假数据，相加即真实 context），output 为本响应 TokenDelta 累计。

**InteractionQuery**
服务端下发的交互查询（web_search/ask_question/switch_mode/exa_search/exa_fetch/create_plan/setup_vm）。ccextra 按种类回 reject/error（SetupVm 回空 success），不提供交互能力。

**pending callback**
Agent 回合中已发起但未收到结果的工具副作用。驻留会话丢失时冷续接 flatten 全量 transcript 重新起跑，崩溃窗口内的 pending callback 可能重复执行，由客户端 tool_result 幂等性兜底。

**journal 回放缓存**
按回合摘要内存缓存的已发布 SSE 帧。同摘要重复请求（断线重连/singleflight）直接回放缓存帧，不重跑上游；进程重启即失效。

**热重载**
四种 OAuth 登录命令保存凭证后自动调用配置地址的 `POST /reload`（直连，通配地址转回环地址，连接超时 2 秒、总超时 30 秒）；失败仅提示，凭证保留，服务未启动时下次启动加载。
`POST /reload` 串行加载、校验后原子替换统一不可变配置快照；失败不改变已生效版本。旧请求保留旧快照，新请求取得新快照。后台刷新绑定版本，过期结果丢弃；后续轮次使用最新已发布的静态配置、目录及代理。

**配置快照**
包含 providers、payload、运行时配置和后台刷新参数的不可变 `Arc`。请求入口一次获取，发布只在短暂写锁内替换。后台 Antigravity 空结果保留整个 provider 集合；显式 reload 优先应用删除、禁用及目录切换，不从旧快照恢复动态 provider。

**诊断落盘**
`logging.request_body: true` 时保存最终出站请求，便于检查缓存和转换问题；敏感入站认证头会脱敏。

**fail-open**
归一化不能安全处理某项数据时，优先保留可发送的原始语义，而非因缓存优化阻断请求。
