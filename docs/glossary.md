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

**全局代理与 provider 代理**
`server.proxy_url` 是默认值；provider `proxy_url` 优先。`"direct"` 或空值表示不走代理。

**入口认证**
`secret_key` 启用后，Anthropic API 端点要求 `x-api-key` 或 `Authorization: Bearer`。明文配置会转 bcrypt 并回写。

## 请求转换

**body-to-body 转换**
转换器直接读写 `serde_json::Value`，不经过通用中间协议类型，并为每个目标协议单独处理图片、工具和 content 语义。

**直通路径**
Anthropic 到 Claude 的路径。只改 `model`，并按规则转发身份头。

**转换路径**
Anthropic 到 Chat、Responses、Gemini 或 Antigravity 的独立转换路径。它们重建目标 body，再在需要时映射响应。

**工具调用**
Anthropic `tool_use` 和 `tool_result`。转换器保持 call ID 配对；空工具结果会补安全的非空输出。Responses 过长工具名在请求侧缩短、响应侧还原。

**服务端 web search 工具**
Claude `web_search_*` 工具。Chat 路径删除它；Responses 映射为 `web_search`；Gemini 风格路径删除它。

**thinking**
Anthropic 推理内容。目标协议会映射为 `reasoning_effort`、reasoning 项或 Gemini thinking 配置；不兼容签名会丢弃。

## 缓存稳定化

**归一化**
对请求进行确定性变换，目标是上游 prompt cache，不保存本地模型响应。完整流程会稳定工具顺序、schema、历史 reminder、工具参数键序、列表和尾部空白，并处理易变日期和 cache marker。

**pretransform / post-transform**
转换前在 Anthropic body 上执行的归一化，以及 OpenAI 转换后在目标 body 上执行的归一化。Gemini 和 Antigravity 不运行 post-transform。

**cache_control**
Anthropic prompt caching 标记，例如 `{type: "ephemeral"}`。完整 Claude 归一化可自动放置它。

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

**relay 状态机**
将 OpenAI 或 Gemini 风格 SSE 转成 Anthropic `message_start`、content block、delta 和终态事件的状态机。终态后忽略后续上游事件。

**keepalive**
流式输出空闲 10 秒后发送的 `: keepalive\n\n` SSE 注释，防止中间网络层关闭空闲连接。

**reasoning replay**
Responses 上游不保留完整会话时，服务端按模型和会话保存兼容 reasoning 回合，并在下一请求锚定插入。仅可安全回放的内容会进入请求。

**thinking signature**
推理块的不透明连续性令牌。系统按 Claude、Gemini、GPT、Kimi 和 Grok 形状验证兼容性；不能匹配目标提供方时不转发。

**Gemini contents/parts**
Gemini 请求的对话表示。`contents` 是回合数组，`parts` 可含 text、`functionCall`、`functionResponse` 或内联图片。

**Antigravity 信封**
Antigravity 请求的外层对象，含 `model`、`request`、`project`、`requestId`、`requestType` 和 `userAgent`。实际 Gemini body 位于 `request`。

**VALIDATED 工具模式**
Antigravity Claude 模型使用的函数调用模式。其工具 schema 需要可验证的 object properties，因此清洗器会补必要占位字段。

**schema 清洗**
将 Anthropic `input_schema` 转成 Gemini 或 Antigravity 可接受 JSON Schema 的递归过程。它内联本地引用、移除不支持关键字、处理 enum 和 required，并为不同目标采用不同规则。

**OAuth 动态 provider**
由保存的 Antigravity 或 xAI 凭证生成的运行时 provider。Antigravity 后台刷新模型，xAI 在启动和重载时扫描、刷新凭证。

**热重载**
`POST /reload` 重读配置并替换多份运行时状态。锁独立更新，因此并发请求可能短暂看到不同版本的 providers 和其他配置。

**诊断落盘**
`logging.request_body: true` 时保存最终出站请求，便于检查缓存和转换问题；敏感入站认证头会脱敏。

**fail-open**
归一化不能安全处理某项数据时，优先保留可发送的原始语义，而非因缓存优化阻断请求。
