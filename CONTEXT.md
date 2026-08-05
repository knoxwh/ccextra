# ccextra 领域术语表

本文档定义 ccextra 项目的统一语言。仅包含术语,不含实现细节。

## 协议与端点

**anthropic messages**  
Anthropic Claude API 的原生协议,端点 `/v1/messages`。入站请求体含 `system` / `messages` / `tools` / `thinking` / `cache_control` 等字段。

**openai chat / chat completions**  
OpenAI `/v1/chat/completions` 协议。请求体含 `messages` 数组,响应流式用 `data: {delta: {...}}` SSE 事件。

**openai responses**  
OpenAI `/v1/responses` 协议(Codex CLI 默认)。请求体用 `template.instructions` + `input` 数组,响应流式用 `output_item` 事件。

**protocol**  
出站目标协议类型,三种:`claude` / `openai_chat` / `openai_responses`。由 provider 配置决定,不由入站推导。

**模型列表 / /v1/models**  
`GET /v1/models` 返回 Anthropic 格式模型清单(Claude Code 启动时自动调用)。由各 provider 的 alias 汇总,`max_input_tokens` / `max_tokens` 缺省 200000 / 64000,可逐模型覆盖。

**入口认证 / secret_key**  
可选顶层配置。配置后 `/v1/models` 与 `/v1/messages` 需 `x-api-key`(或 `Authorization: Bearer`)匹配,否则 401。明文 key 启动时自动转 bcrypt 并回写配置文件,校验用 bcrypt verify(bcrypt 结果缓存,避免每请求 ~100ms)。

## 路由与上游

**provider**  
上游服务提供商。每个 provider 声明 `protocol` / `base_url` / `key` / `models` 列表。入站 model 通过 alias 解析到唯一 provider。

**model alias**  
入站请求里的模型名(如 `evol-opus-5`)映射到 provider 内真实上游模型名(如 `claude-opus-5`)的别名。

**route decision / 路由决策**  
从入站 model 解析到 `(provider, protocol, upstream_model)` 三元组的过程。冲突时启动报错,不做隐式推导。

**upstream model**  
发往上游的真实模型名,与入站 alias 可能不同。

## 转换路径

**直通路径 / passthrough**  
claude 入站 → claude 出站,只改 `model` 字段,其余字节原样保留。绕过中间表示,保住归一化字节稳定性。

**转换路径 / conversion path**  
需要协议翻译的两条路径:  
- anthropic → openai chat  
- anthropic → openai responses

**body-to-body 转换**  
在 `serde_json::Value` 上原地读写,不经过中间类型。gjson/sjson 风格(Go)或 `value[path] = new_val`(Rust)。每条路径独立实现。顶层键序对齐 CPA(`model,max_tokens,temperature/stop,stream,reasoning_effort,messages,tools,tool_choice,user`)。

**content 形态归一化**  
Claude Code 同一条消息当轮发 content 数组、历史重建发字符串;转换器统一输出 text 数组(空串 → `content:[]`,null/missing → 丢弃整条消息),消除跨轮字节漂移——这是周期性缓存 MISS(cache_read 掉 4096 后紧邻请求恢复)的根因。对齐 CPA 的 `JoinRawArray` 数组形态。

## 缓存归一化

**归一化 / normalization**  
对请求体做确定性字节变换,消除序列化漂移,目标是命中**上游** prompt cache,不做本地响应缓存。

**pretransform 归一化 / 按协议分流**  
入站 anthropic body 在转换前跑的归一化,按目标协议分流(对齐 CPA):  
- claude 直通:`normalize_anthropic_full` 完整九模块,含 `cache_control` 注入  
- openai 转换路径:`normalize_anthropic_pretransform` 精简五模块子集(smoosh → bookkeeping → tool_input → sort → rstrip),跳过 tool-def sort / volatile / cache_control——这些在转换后处理或对 openai 上游无意义

**post-transform 归一化 / target_post**  
转换后对目标协议 body 跑的二次归一化:tool_def normalize → sort stabilize → reminder rstrip → volatile strip(对齐 tklite openai 管线)。直通路径跳过此步。

**九模块**  
照搬 tklite 的九个归一化单元:  
1. tool_def sort — tools 数组按 name 排序,schema key 递归排序  
2. smoosh_split — 拆 tool_result 尾部折叠的 `</system-reminder>`  
3. bookkeeping strip — 删历史消息里的 token 账本 reminder  
4. tool_input normalize — 按 input_schema 顺序重排 tool_use.input key  
5. sort stabilize — system 里 skills/deferred 块内列表排序  
6. reminder rstrip — 抹平 `</system-reminder>` 尾空白  
7. volatile strip — 前缀时间戳/UUID 替换为定长占位符  
8. cache_control inject — 自动插 `{type: ephemeral}` marker  
9. drift detector — 对 system/tools/early_messages 做三轴哈希跨 turn 观测

**漂移 / drift**  
同一会话内,system / tools / messages 前缀的结构哈希变化。漂移 = 归一化未覆盖的盲区,WARN 级日志。

**会话身份 / session key**  
对齐 CPA(`helps.ExtractClaudeCodeSessionID`)的派生链:
1. 请求头 `X-Claude-Code-Session-Id`(Claude Code 原生发送,整会话稳定)
2. `metadata.user_id` 尾部 `_session_<uuid>`(或 JSON 形态 `session_id`)

用于 prompt_cache_key 注入与 drift detector 分桶(parent bucket)。drift detector 另有请求头侧回退链(`x-tklite-session-key` → `x-request-id` 哈希 → `anonymous`)。

注:早期版本的 `messages[0].content` SHA-256 兜底已删除——`messages[0]` 会被每请求注入的 system-reminder 与上下文压缩改变,不是稳定身份。

**prompt_cache_key**  
OpenAI chat/responses 的缓存桶标识。对齐 CPA `applyPromptCacheKey`:provider 级开关(配置 `prompt_cache_key`,默认 false),按 `UUIDv5(NameSpaceOID, "cli-proxy-api:codex:claude-code" \0 模型 \0 "claude:<会话>:agent:<agent>")` 派生。identity 前缀与 CPA 相同,两个代理命中同一缓存桶。无 Claude Code 会话 ID 或 body 已有 key 时不注入;仅 openai_chat / openai_responses 生效。

## 响应流式

**SSE / Server-Sent Events**  
`text/event-stream` 格式,`data:` 行承载 JSON 事件。anthropic 用 `message_start` / `content_block_delta` 等事件;openai chat 用 `data: {choices: [{delta: ...}]}`;responses 用 `output_item` 事件。

**relay 状态机**  
把上游 SSE 流逐 chunk 解析并转换为目标协议事件序列的状态维护。三条路径独立:  
- claude 直通 = 字节级转发,不解析  
- openai chat → anthropic = `relay_openai_chat_to_anthropic` 状态机  
- responses → anthropic = `relay_responses_to_anthropic` 状态机

**reasoning 回放闭环**  
responses 路径的加密思考跨轮闭环:上游 `reasoning_summary_text.delta` 流式转 `thinking_delta`(可见),`output_item.done` 的 `encrypted_content` 以 `signature_delta` 收尾;下一轮请求侧把 `thinking.signature` 转回 `reasoning.encrypted_content`。对齐 CPA,替代旧的 redacted_thinking 方案(形状不合规范且请求侧丢弃)。

**content_block index 对齐**  
openai chat 的 `delta.tool_calls[N]` 需映射到 anthropic 的 `content[M]`,首次出现时分配 index 并记录映射。

## 特殊字段

**cache_control**  
anthropic 的 prompt caching marker,可标记在 system block / message / tool 定义 / content part 上,形如 `{type: "ephemeral"}`。

**thinking**  
anthropic 的推理块,含 `type: "thinking"` 的 content,可带 `budget_tokens` / `signature` 字段。

**system**  
anthropic 顶层字段,可以是 `string` 或 `block 数组`(含 text / tool_use / cache_control)两种形态。

**metadata**  
anthropic 请求体顶层 `metadata` 对象,含 `user_id` 等。转换路径静默丢弃(openai 无对应字段)。

**prompt_cache_retention**  
Claude Code 注入的缓存保留时长参数。openai 上游拒绝(HTTP 400 `Unsupported parameter`),转换路径剥离(对齐 CPA `StripPromptCacheRetention`);claude 直通保留。

**计费归属文本 / attribution text**  
Claude Code 每请求注入 system 的计费+prompt 指纹块,前缀 `x-anthropic-billing-header:`。内容逐请求变化,转换到 openai 侧必须剥离,否则上游缓存前缀每次请求全 miss。

**tool_result 内容转换**  
claude `tool_result` 的 content 转 openai 工具消息:字符串原样;纯文本数组 `\n\n` 连接为字符串(tool role 兼容性最好);含 image 的数组保留 parts 数组(text/image_url)。

## 配置术语

**单 key 无重试**  
每个 provider 配置一个 `key` 字符串,上游失败直接透传错误,不做凭据级 fallback。MVP 最简模式。

**fail-open**  
tklite 四原则之一:归一化出错时回退原始 body,不阻断请求。ccextra 继承此语义。

**providers 数组**  
配置文件顶层 `providers:` 列表(YAML),每项声明 name / protocol / base_url / key / models。

**payload 参数覆盖**  
按模型名模式(`*glm*` 通配)匹配,覆盖请求体参数(如 `max_tokens` / `temperature`)。类似 CPA 的 `payload.default` / `payload.override` 功能,但简化为单层规则数组。

## 缩写约定

- **CPA** = CLIProxyAPI,Go 实现的多 provider 网关,ccextra 参考其转换逻辑
- **tklite** = Rust 实现的字节稳定化 sidecar,ccextra 照搬其九模块归一化
- **ccr** = claude-code-router,TypeScript 路由网关
- **MVP** = Minimum Viable Product,第一版能跑的最小范围
