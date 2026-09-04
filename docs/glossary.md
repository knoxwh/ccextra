# 领域术语表

本文档定义 ccextra 项目的统一语言。仅包含术语，不含实现细节。

## 协议与端点

**anthropic messages**  
Anthropic Claude API 的原生协议,端点 `/v1/messages`。入站请求体含 `system` / `messages` / `tools` / `thinking` / `cache_control` 等字段。

**openai chat / chat completions**  
OpenAI `/v1/chat/completions` 协议。请求体含 `messages` 数组,响应流式用 `data: {delta: {...}}` SSE 事件。

**openai responses**  
OpenAI `/v1/responses` 协议(Codex CLI 默认)。请求体用顶层 `instructions` + `input` 数组,响应流式用 `output_item` 事件。

**protocol**  
出站目标协议类型,五种:`claude` / `openai_chat` / `openai_responses` / `gemini` / `antigravity`。由 provider 配置决定,不由入站推导。

**gemini 协议**  
第四条独立 body-to-body 路径。入站仍是 anthropic messages；出站是 Gemini 请求体,再由运输层套信封。flash 与 pro 同一转换。不是官方 `generativelanguage` 直连,也不是星型枢纽。

**信封**  
发往 Cloud Code Assist 的整份 POST 体。顶层:`model`、`request`、`project`、`requestId`、`requestType`、`userAgent`。`request.sessionId` 也是运输字段,写在 `request` 里。

**Gemini 请求体**  
信封里的 `request`:`contents`、`systemInstruction`、`tools`、`generationConfig`、`toolConfig`。转换器只产出这一层,不写 `project` / `requestId` / `requestType` / `sessionId`。

**模型列表 / /v1/models**  
`GET /v1/models` 返回 Anthropic 格式模型清单(Claude Code 启动时自动调用)。由各 provider 的 alias 汇总,`max_input_tokens` / `max_tokens` 缺省 200000 / 64000,可逐模型覆盖。

**入口认证 / secret_key**  
可选顶层配置。配置后 `/v1/models` 与 `/v1/messages` 需 `x-api-key`(或 `Authorization: Bearer`)匹配,否则 401。明文 key 启动时自动转 bcrypt 并回写配置文件,校验用 bcrypt verify(bcrypt 结果缓存,避免每请求 ~100ms)。

## 路由与上游

**provider**  
上游服务提供商。每个 provider 声明 `protocol` / `base_url` / `key` / `models` 列表。入站 model 通过 alias 解析到唯一 provider。`protocol: antigravity` 时 `key` 不用,启动时扫 `auth_dir` OAuth 凭证自动注入 provider。

**model alias**  
入站请求里的模型名(如 `evol-opus-5`)映射到 provider 内真实上游模型名(如 `claude-opus-5`)的别名。

**route decision / 路由决策**  
从入站 model 解析到 `(provider, protocol, upstream_model)` 三元组的过程。冲突时启动报错,不做隐式推导。

**upstream model**  
发往上游的真实模型名,与入站 alias 可能不同。

**按协议 UA 分流**  
上游请求按协议+模型分流 User-Agent / Originator。可选顶层 `user_agents` 覆盖 `claude_cli` / `codex_tui` / `grok_version` / `antigravity`；字段缺失用内置默认值，`/reload` 生效。仅 responses + `*gpt*`(大小写不敏感)用 `codex_tui`（默认 `codex-tui/0.149.1 ...`）且带 `Originator: codex_cli_rs`;chat 或 responses + `*grok*` 用 `grok-shell/{grok_version} ({os}; {arch})`(默认 `1.0.5`，对齐 grok-build 默认 UA);antigravity 用 `antigravity`（默认 `antigravity/hub/2.10.0 darwin/arm64`，非 antigravity UA 上游直接 404）;其余(claude / 非 grok 的 chat / 非 gpt 非 grok 的 responses / gemini)用 `claude_cli`（默认 `claude-cli/2.1.246`）,不带 Originator。antigravity 信封里另有 `userAgent=antigravity`,与 HTTP 头不是同一字段。部分上游按 UA 识别客户端并分流缓存/特性,reqwest 默认 UA 会被判为非官方客户端。

**grok CLI 身份头**  
chat/responses + grok 发 `X-XAI-Token-Auth=xai-grok-cli`、`x-grok-client-version`（取 `user_agents.grok_version`）、`x-grok-client-identifier=grok-shell`、`x-grok-model-override`,以及有会话时的 `x-grok-conv-id`。`x-grok-doom-loop-check` 仅 responses。不发 `req-id` / `session-id` / `agent-id` / `turn-idx`(无官方会话计数源;conv-id 已承担粘性)。

**claude 直通头重建**  
claude 路径的 `anthropic-beta` 头按 body 条件重建(基础集 `claude-code-20250219` + thinking 无 display → `redact-thinking` / tools → `advanced-tool-use` / `effort-2025-11-24` / speed=fast → `fast-mode`),再追加 caller 自带 beta(去重)。`anthropic-version`/`x-app`/`x-stainless-*` 等身份头仅透传(有就转发,没有不补)。与直通头重建中转场景一致。

## 转换路径

**直通路径 / passthrough**  
claude 入站 → claude 出站,只改 `model` 字段,其余字节原样保留。绕过中间表示,保住归一化字节稳定性。

**转换路径 / conversion path**  
需要协议翻译的四条路径:  
- anthropic → openai chat  
- anthropic → openai responses  
- anthropic → Gemini 请求体
- anthropic → Antigravity(Gemini 请求体再套信封)

**body-to-body 转换**  
在 `serde_json::Value` 上原地读写,不经过中间类型。gjson/sjson 风格(Go)或 `value[path] = new_val`(Rust)。每条路径独立实现。顶层键序(`model,max_tokens,temperature/stop,stream,reasoning_effort,messages,tools,tool_choice,user,stream_options`)。

**stream_options.include_usage**  
仅 chat 路径、且 `stream` 缺省或为 true 时注入 `stream_options: {include_usage: true}`,对齐 `SetBoolIfDifferent` 语义。`stream: false` 不注入(部分上游拒该字段)。responses 路径无此字段。部分上游(kimi/moonshot)未开启时全程无 usage,客户端 statusline 无 context 显示;开启后流尾必发 usage chunk。

**响应错误转 anthropic 形状**  
上游非 2xx 时的 `{"error":{...}}` body 转 anthropic `{"type":"error","error":{...}}` 形状(对齐 `WriteErrorResponse` 语义)。`rate_limit`/`requests`/`tokens` 归为 `rate_limit_error`,已知类型透传,其余兜底 `api_error`。gemini 路径补读 Google `error.message` / `error.status`。无凭证 401 为 `authentication_error`。

**content 形态归一化**  
Claude Code 同一条消息当轮发 content 数组、历史重建发字符串;转换器统一输出 text 数组,消除跨轮字节漂移——这是周期性缓存 MISS(cache_read 掉 4096 后紧邻请求恢复)的根因。对齐 `JoinRawArray` 数组形态。空内容两侧语义不同:
- chat:空串 → `content:[]`;null/缺失 → 丢弃整条消息
- responses:空串 → `content:[]` 保留消息(assistant 空 content 是 thinking-only/tool 轮的正常信号,不能丢);null/缺失 → 丢弃

**gpt adapter block / GPT 适配块**  
responses 路径下,上游模型名以 `gpt` 开头(不区分大小写)时,拼到顶层 `instructions` 末尾的固定英文行为指令。修正 Claude 编排词对 GPT 的冗长/过度探索/误用 `apply_patch` 倾向。字节固定,不配置化,缓存主前缀稳定。无 system 时 `instructions` 仅含该块。

**gpt trigger**  
注入适配块的触发条件:`OpenAiResponses` 协议 + 上游模型名小写后以 `gpt` 为前缀。o 系列(o1/o3)不触发。

**strict schema 降级**  
OpenAI Responses 协议 strict 模式要求 schema 中所有 declared properties 必须存在于 sibling required 列表中。若存在未列入 required 的属性，自动降级 `strict: false`，避免上游报 400（对齐 CPA `aa365277`）。

## 缓存归一化

**归一化 / normalization**  
对请求体做确定性字节变换,消除序列化漂移,目标是命中**上游** prompt cache,不做本地响应缓存。

**pretransform 归一化 / 按协议分流**  
入站 anthropic body 在转换前跑的归一化,按目标协议分流:  
- claude 直通:`normalize_anthropic_full` 完整九模块,含 `cache_control` 注入  
- openai 转换路径:`normalize_anthropic_pretransform` 精简五模块子集(smoosh → bookkeeping → tool_input → sort → rstrip),跳过 tool-def sort / volatile / cache_control——这些在转换后处理或对 openai 上游无意义  
- gemini/antigravity 路径:同跑 `pretransform` 五模块子集(输入仍是 anthropic 形状),不写 `cachedContent`,不注入 `prompt_cache_key`,跳过 post 归一化与 drift

**post-transform 归一化 / target_post**  
转换后对目标协议 body 跑的二次归一化:tool_def normalize → sort stabilize → reminder rstrip → volatile strip(对齐 openai 转换后管线)。openai chat 随后观测 drift；responses 在 payload 覆盖后观测最终 body。claude 直通与 gemini 路径跳过此步。

**九模块**  
九个归一化单元:  
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
对齐 `helps.ExtractClaudeCodeSessionID` 的派生链:
1. 请求头 `X-Claude-Code-Session-Id`(Claude Code 原生发送,整会话稳定)
2. `metadata.user_id` 尾部 `_session_<uuid>`(或 JSON 形态 `session_id`)

用于 prompt_cache_key 注入与 drift detector 分桶(parent bucket)。drift detector 另有请求头侧回退链(`x-tklite-session-key` → `x-request-id` 哈希 → `anonymous`)。

注:早期版本的 `messages[0].content` SHA-256 兜底已删除——`messages[0]` 会被每请求注入的 system-reminder 与上下文压缩改变,不是稳定身份。

**prompt_cache_key**  
OpenAI chat/responses 的缓存桶标识。对齐 codex CLI 0.147 `prompt_cache_key()`:key = session_id 裸值(取代旧版 `UUIDv5(NameSpaceOID, "cli-proxy-api:codex:claude-code" \0 模型 \0 "claude:<会话>:agent:<agent>")` 派生)。provider 级开关(配置 `prompt_cache_key`,默认 false),仅 openai_chat / openai_responses 生效。chat + grok 跳过注入(官方 CLI 不把该字段映射上线,粘性走头 `x-grok-conv-id`);responses + grok 开关开时仍注入(对齐 grok-build `forwards_prompt_cache_key` 仅 Responses)。grok 判定看 payload 后出站 model。openai 转换丢未知顶层字段,管线转换后写回入站非空 key。payload / 入站已有非空 key 不覆盖、不硬剥。无 Claude Code 会话 ID 或 body 已有 key 时不注入。gemini/antigravity 路径不注入。

## 响应流式

**SSE / Server-Sent Events**  
`text/event-stream` 格式,`data:` 行承载 JSON 事件。anthropic 用 `message_start` / `content_block_delta` 等事件;openai chat 用 `data: {choices: [{delta: ...}]}`;responses 用 `output_item` 事件。

**relay 状态机**  
把上游 SSE 流逐 chunk 解析并转换为目标协议事件序列的状态维护。五条路径独立:
- claude 直通 = 字节级转发,不解析  
- openai chat → anthropic = `relay_openai_chat_to_anthropic` 状态机  
- responses → anthropic = `relay_responses_to_anthropic` 状态机  
- gemini → anthropic = `relay_gemini_to_anthropic`;直连 Gemini 只在看到有效 payload 后发 `message_stop`,不合成缺失 `finishReason` 的 terminal 事件
- antigravity → anthropic = `relay_antigravity_to_anthropic`;先解 `{"response": ...}` 信封。clean EOF 或 `[DONE]` 时,仅当已有有效 payload 且所有 candidate 都缺非空 `finishReason` 才合成 terminal 事件;已观察到 finish reason 时不重复合成。
两条 Gemini 风格路径均忽略空信封、空 `candidates`、空 usage;空流或 read error 发 anthropic `error`,不发正常终态。`cpaUsageMetadata` 会在转换前恢复为 usage 元数据。

**reasoning 回放闭环**  
responses 路径:有 `encrypted_content` 时流式 `summary` 转 `thinking_delta`(可见),`output_item.done` 的加密内容以 `signature_delta` 收尾。下一轮有签 thinking 仅当签名 GPT 兼容(`gAAAA` 前缀)或 grok 模型才回放为 `reasoning.encrypted_content`,否则丢弃。无签非空 thinking 回放为明文 `reasoning.content`。替代旧的 redacted_thinking 方案(形状不合规范且请求侧丢弃)。

chat 路径不同:Chat Completions 无 `encrypted_content`。对齐 CPA `shouldMapClaudeThinkingToGPTReasoning` 默认——无/空签名回放正文;有签名仅 `gAAAA` 过门后回放正文;Claude/Gemini/未知/grok 密文整块扔。签名本身不进 `reasoning_content`。无 grok 特例,无 CPA compat。

**断流兜底**  
OpenAI 转换流尚未输出首个 Anthropic SSE 帧时,若首帧为 `error`,重试上游一次;仅门控首帧,不缓冲完整响应。第二次仍失败、或已输出首帧后发生上游传输错误/EOF 未满足协议终态时,状态机只发 anthropic `error`,不伪造 `message_start` 或正常收尾帧。Chat 的 `[DONE]` 是显式终态;未收到 `[DONE]` 的 EOF 需已有 `finish_reason`。Responses 仅 `response.completed` / `response.incomplete` 正常收尾;`response.failed` 与 `error` 为错误终态。终态后忽略后续帧。responses 空轮次/纯思考轮次合成空 text 块(Claude 客户端遇零块消息报 "Content block not found")。Gemini 风格路径无首帧重试:直连 Gemini 在有效 payload 后仅发 `message_stop`,不合成缺失 finish terminal;Antigravity 在 clean EOF/`[DONE]` 且已有 payload、缺少有效 finish 时补空 text terminal 事件,已有 finish 时不重复。空流、空 envelope、空 `candidates`、空 usage、read error 均不报告成功完成。

**诊断落盘**  
`logging.request_body: true` 时逐请求把最终上游 body 与入站 HTTP 头落盘至单个文件 `logs/upstream_request_<session前8>_<毫秒>_<序号>.<protocol>.json`(密钥头脱敏)。另含入站 `request_body` 调试日志。须进程重启才生效(`/reload` 不够)。

**content_block index 对齐**  
openai chat 的 `delta.tool_calls[N]` 需映射到 anthropic 的 `content[M]`,首次出现时分配 index 并记录映射。

## 特殊字段

**cache_control**  
anthropic 的 prompt caching marker,可标记在 system block / message / tool 定义 / content part 上,形如 `{type: "ephemeral"}`。

**thinking**  
anthropic 的推理块,含 `type: "thinking"` 的 content,可带 `budget_tokens` / `signature` 字段。chat 转换把过门的正文写入 `reasoning_content`(见「reasoning 回放闭环」);responses 把兼容签名写入 `encrypted_content`。gemini:`enabled` + `budget_tokens` 写 `thinkingBudget`;`adaptive`/`auto` 显式 `output_config.effort="max"` 或缺失时走预算表发 `thinkingBudget`(查不到或 antigravity 兜底 `thinkingLevel=high`)，其余合法值(`low`/`medium`/`high`)写 `thinkingLevel`(非法值兜底 `high`)。gemini 直连丢全部 thinking 块;antigravity 保留带签名块(见「Gemini 签名与运输」)。

**system**  
anthropic 顶层字段,可以是 `string` 或 `block 数组`(含 text / tool_use / cache_control)两种形态。

**metadata**  
anthropic 请求体顶层 `metadata` 对象,含 `user_id` 等。openai 与 gemini 转换路径静默丢弃,不进信封。

**prompt_cache_retention**  
Claude Code 注入的缓存保留时长参数。openai 上游拒绝(HTTP 400 `Unsupported parameter`),转换路径剥离(对齐 `StripPromptCacheRetention`);claude 直通保留。gemini 不进信封。

**计费归属文本 / attribution text**  
Claude Code 每请求注入 system 的计费+prompt 指纹块,前缀 `x-anthropic-billing-header:`。内容逐请求变化,转换到 openai / gemini 侧必须剥离。

**tool_result 内容转换**  
claude `tool_result` 的 content 转 openai 工具消息:字符串原样;纯文本数组 `\n\n` 连接为字符串(tool role 兼容性最好);含 image 的数组保留 parts 数组(text/image_url)。responses 在 `normalize.enabled` 时，payload 后按最终 upstream_model 截断 `function_call_output` / `custom_tool_call_output` 的 output：grok 40KB + 2KB 预览，非 grok 10KB。gemini 路径:`tool_result` → `functionResponse`,id 用入站 `tool_use_id`,name 从本轮 `tool_use` 表查;**内容进 `response.result` 强制 JSON 字符串化(对齐 CPA 9d0a60bf),防止解析后对象/数组触发上游 400**;结果里的 base64 图进 `functionResponse.parts.inlineData`。不接 URL 图。

## Gemini 签名与运输

**签名透传**  
出站把 Gemini/Antigravity `thoughtSignature` 封进 Claude `thinking.signature`(signature_delta);下一轮入站 antigravity 路径把带非空签名的 thinking 块还原为 `{thought, text, thoughtSignature}` 贴回,gemini 直连丢全部 thinking 块。CPA 的签名缓存/carrier/账本回放子系统不移植——签名只做客户端透传。

**哨兵**  
合法但无语义的 `thoughtSignature` 占位:`skip_thought_signature_validator`。入站 `tool_use` 转 `functionCall` 时附上,满足上游签名校验门。不给 `functionResponse` 加签。

**VALIDATED 模式**  
antigravity 且上游模型名含 `claude` 时,强制 `request.toolConfig.functionCallingConfig.mode=VALIDATED`(对齐 CPA executor)。该模式下空对象 schema 必须带占位 `reason` 属性,由 schema 清洗补齐。

**schema 清洗**  
gemini/antigravity 工具 `input_schema` 的深度归一(对齐 CPA `internal/util/gemini_schema.go` 四阶段):$ref 内联/转提示、const→enum、约束并入 description、anyOf/oneOf/allOf 展平、不支持关键字移除、required 修剪。gemini 口味强制 enum `type=string` 并去 nullable/title;antigravity 口味 drop 全部 enum 转提示、补占位属性。

## 配置术语

**单 key 无重试**  
每个 provider 配置一个 `key` 字符串,上游失败直接透传错误,不做凭据级 fallback。MVP 最简模式。`protocol: antigravity` 不用 `key`,启动时扫 `auth_dir`,每份有效凭证注入一个 provider(多凭证各建 provider,alias 冲突先到者胜出)。

**fail-open**  
字节稳定化四原则之一:归一化出错时回退原始 body,不阻断请求。ccextra 继承此语义。

**providers 数组**  
配置文件顶层 `providers:` 列表(YAML),每项声明 name / protocol / base_url / key / models。

**payload 参数覆盖**  
按模型名模式(`*glm*` 通配)匹配,覆盖请求体参数(如 `max_tokens` / `temperature`)。单层规则数组。claude 直通与 gemini 须显式声明 `protocol` 才注入;gemini 打信封顶层,不发明点路径。

## 缩写约定

- **MVP** = Minimum Viable Product,第一版能跑的最小范围
