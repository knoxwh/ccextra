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
在 `serde_json::Value` 上原地读写,不经过中间类型。gjson/sjson 风格(Go)或 `value[path] = new_val`(Rust)。每条路径独立实现。

## 缓存归一化

**归一化 / normalization**  
对请求体做确定性字节变换,消除序列化漂移,目标是命中**上游** prompt cache,不做本地响应缓存。

**pretransform 归一化 / anthropic_full**  
入站 anthropic body 在转换前跑的完整九模块归一化,含 `cache_control` 注入。

**post-transform 归一化 / target_post**  
转换后对目标协议 body 跑的二次归一化,只含 tool_def normalize / volatile strip / drift。直通路径跳过此步。

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
从 `messages[0].content` 派生的 SHA-256 哈希,用于 drift detector 分桶。同一 Claude Code 会话首条消息固定,天然稳定。

## 响应流式

**SSE / Server-Sent Events**  
`text/event-stream` 格式,`data:` 行承载 JSON 事件。anthropic 用 `message_start` / `content_block_delta` 等事件;openai chat 用 `data: {choices: [{delta: ...}]}`;responses 用 `output_item` 事件。

**relay 状态机**  
把上游 SSE 流逐 chunk 解析并转换为目标协议事件序列的状态维护。三条路径独立:  
- claude 直通 = 字节级转发,不解析  
- openai chat → anthropic = `relay_openai_chat_to_anthropic` 状态机  
- responses → anthropic = `relay_responses_to_anthropic` 状态机

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
