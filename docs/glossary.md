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
| SSE | `text/event-stream` 响应格式；ccextra 将所有流式路径输出为 Anthropic SSE。 |
| 非流响应 | `stream` 为 `false` 或缺失时的单个 JSON 响应。 |

xAI Grok 不是 protocol。它使用 OAuth 凭证动态创建 `openai_responses` provider。

## 路由与配置

**provider**
一个上游连接定义，含 `name`、`protocol`、`base_url`、`key`、可选代理和 models。`base_url` 可按顺序列出多个地址。

**model alias**
客户端发送的模型名。路由先按 alias 匹配，再按真实上游模型名匹配。alias 全局唯一。

**upstream model**
实际写入上游请求 `model` 的名称，可与 alias 不同。

**route decision**
从入站模型得到 `(provider, protocol, upstream_model)` 的结果。未知模型或 alias 冲突会拒绝请求或配置。

**payload override**
`payload` 中按模型 glob 匹配的顶层 JSON 覆盖。规则可用 `protocol` 限定；Claude 直通必须显式限定才接受覆盖。

**reasoning 注册表**
用户 `models.json`（路径由 `models_file` 指定，默认同目录）。按上游模型 `id` 精确匹配，把 effort 钳到该模型支持的最近档。缺文件或未收录不钳。条目可设 `force_effort` 固定档：凡钳制介入处改写为该值（不钳制）。core 不读文件。

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
按最终代理地址缓存 `reqwest::Client` 的发送器。它选择协议端点、认证、User-Agent、Grok 会话头和流式头。

**重试预算**
网络错误、429、5xx 和 52x 可重试的总等待窗口。当前总预算为 3 秒，初始退避为 300ms，单次最多 1.5 秒。

**有界读取**
非流响应 body 的统一读取边界：成功 body 16 MiB（恰好上限可读，多 1 字节拒绝），错误 body 256 KiB（超出停止读取并标记截断，截断前缀不当作完整 JSON 解析）；读取停顿与流 chunk idle 共用 300s，每次非空数据后重置。成功 body 超限返回 502、停顿返回 504；错误 body 截断或读取失败保留上游状态码（429/401 不被改写）；OAuth/project/model 保留各自 30s 请求总超时。

**死连接**
复用池中已失效的连接（reset/broken pipe/提前关闭）。立刻重试一次，不计入 3 秒预算；普通建连失败/建连超时不属于死连接，交给 URL 回退与退避预算。

**relay 状态机**
将 OpenAI 或 Gemini 风格 SSE 转成 Anthropic `message_start`、content block、delta 和终态事件的状态机。终态后忽略后续上游事件。

**keepalive**
流式输出空闲 10 秒后发送的 `: keepalive\n\n` SSE 注释，防止中间网络层关闭空闲连接。上游流本身另有 300s chunk idle（对齐 grok）；超时发 Anthropic error，不是心跳。

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
将 Anthropic `input_schema` 转成 Gemini 或 Antigravity 可接受 JSON Schema 的递归过程。它内联本地引用、移除不支持关键字、归一化布尔 `true` 子 schema、处理 enum 和 required，并为不同目标采用不同规则；Gemini 直连保留 `additionalProperties` 与标准约束，Antigravity 搬入 description 提示。声明 `items` 但缺 `type` 的节点补 `type: array`；`type` 显式不是 `array` 时去掉 `items`。

**OAuth 动态 provider**
由保存的 Antigravity 或 xAI 凭证生成的运行时 provider。Antigravity 后台刷新模型，xAI 在启动和重载时扫描、刷新凭证。

**热重载**
`POST /reload` 串行加载、校验后原子替换统一不可变配置快照；失败不改变已生效版本。旧请求保留旧快照，新请求取得新快照。后台刷新绑定版本，过期结果丢弃；后续轮次使用最新已发布的静态配置、目录及代理。

**配置快照**
包含 providers、payload、运行时配置和后台刷新参数的不可变 `Arc`。请求入口一次获取，发布只在短暂写锁内替换。后台 Antigravity 空结果保留整个 provider 集合；显式 reload 优先应用删除、禁用及目录切换，不从旧快照恢复动态 provider。

**诊断落盘**
`logging.request_body: true` 时保存最终出站请求，便于检查缓存和转换问题；敏感入站认证头会脱敏。

**fail-open**
归一化不能安全处理某项数据时，优先保留可发送的原始语义，而非因缓存优化阻断请求。
