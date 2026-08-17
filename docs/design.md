# 架构设计

> 本文档记录 ccextra 的设计决策与实现细节，含与参考实现对标的过程。术语定义见 [glossary.md](glossary.md)。

## 1. Body-to-Body 转换

四条独立转换路径，无中间类型，避免字段丢失：

```rust
// claude 直通：只改 model
convert_passthrough(&mut body, upstream_model);

// anthropic → openai chat
convert_to_openai_chat(&mut body, upstream_model);

// anthropic → openai responses
convert_to_openai_responses(&mut body, upstream_model);

// anthropic → Gemini 请求体（运输层再套信封）
convert_to_gemini(&mut body, upstream_model);
```

**thinking 回放(chat vs responses)**：chat 对齐 CPA 默认——签名只当门闩，过门回放正文到 `reasoning_content`，签名不进 Chat Completions。无/空签名过；有签名仅 `gAAAA` 过；Claude/未知整块扔。不抄 responses 的 grok 任意放行，也不抄 CPA compat。responses 仍走 `encrypted_content`（`gAAAA` 或 grok 模型名放行）。

**放弃星型枢纽理由**：入站单一，矩阵退化为 `1×N`，中间类型收益为零，且会丢失 image/cache_control/metadata。gemini 同样独立 body-to-body，不经 Standard 枢纽。

**GPT 上游适配块**：responses 转换在 `upstream_model` 以 `gpt` 前缀（不区分大小写）时，把固定英文指令（`GPT_ADAPTER_BLOCK`）拼到顶层 `instructions` 末尾。动机：Claude Code 系统词按 Claude 调教，缺少 codex prompt 式的输出压缩硬约束，GPT 收到后默认冗长、过度探索、并把 `apply_patch` 当可用工具（codex 训练分布太熟，Claude 编排层不认）。块字节固定、不配置化，追加不改 `input` 前缀，上游缓存主前缀仍命中；冲突时用户指令优先。无 system 时 `instructions` 仅含该块，不另建 developer / `input[0]`。触发条件与块内容见 `docs/glossary.md` 的 `gpt adapter block` / `gpt trigger`。

## 2. 字节级直通

claude → claude 只改 model 字段，其余字节原样保留：

```rust
body["model"] = Value::String(upstream_model.to_string());
```

**理由**：保住归一化的字节稳定性，key 顺序/空格/转义不变，缓存命中率不退步。

## 3. 两段式归一化（按协议分流）

```
入站 anthropic
    ↓
路由决策 model → provider → protocol
    ↓
转换前归一化 (按协议分流)
  ├─ claude 直通: normalize_anthropic_full (9 模块)
  │    - tool_def sort / smoosh split / bookkeeping strip
  │    - tool_input normalize / sort stabilize / reminder rstrip
  │    - volatile strip / cache_control inject / drift detect
  ├─ openai 转换路径: normalize_anthropic_pretransform (5 模块子集)
  │    - smoosh split / bookkeeping strip / tool_input normalize
  │    - sort stabilize / reminder rstrip
  │    (跳过 tool-def sort / volatile / cache_control —— 转换后处理)
  └─ gemini 路径: 空臂
       - 不跑九模块，不写 cachedContent，不注入 prompt_cache_key
       - 缓存优化入口留空，第一刀不接
    ↓
协议转换 (body-to-body，content 形态归一化)
    ↓
normalize_target_post (4 模块，仅 openai 转换路径；gemini 跳过)
  - tool_def normalize / sort stabilize / reminder rstrip / volatile strip
    ↓
drift 观测 (claude / openai 链路；gemini 跳过)
    ↓
上游请求
```

**理由**：`cache_control` 注入需 anthropic 结构且对 openai 上游无意义；转换引入新漂移需二次清理；drift 观测统一在转换后进行。gemini 第一刀不做前缀稳定，空臂占位，避免九模块误改 Gemini 请求体。

## 4. 关键修正（对标参考实现已知坑）

**坑1：system 位置错误**（responses 路径）

```rust
// ❌ 参考实现把 system 写入 input[0] as developer message
// ✅ ccextra: system 写入顶层 instructions（对齐 codex base_instructions）
openai_body["instructions"] = json!(instructions_str);
```

**坑2：工具名超长**（responses 路径）

超 64 字符缩短并建 short→original 还原表；流式 / 非流响应按表还原原名。测试：`test_tool_name_shortened_with_unique_suffix`。

## 5. 响应转发（SSE 状态机）

```
claude 直通:      字节级转发,不解析
openai chat:     relay_openai_chat_to_anthropic
                 (单 active block + reasoning 去重 + tool_calls index 映射 +
                  reasoning-only 空回合 thinking 转 text 兜底)
openai responses: relay_responses_to_anthropic
                 (reasoning 回放闭环: summary→thinking_delta +
                  encrypted_content→signature_delta)
gemini:          relay_gemini_to_anthropic (sse/gemini.rs)
                 (无 event: 分派; data: 行; part.text 当增量;
                  finishReason 在 = 终包; 帧复用 emit)
```

OpenAI 转换流在尚未输出首个 Anthropic SSE 帧时，若首帧为 `error`，重试上游一次；仅门控首帧，不全量缓冲。第二次仍失败、或已输出首帧后的传输错误/未满足终态 EOF，发 anthropic `error` 事件，不裸断流，也不补造 `message_start`、`message_delta` 或 `message_stop`。

- OpenAI Chat：`[DONE]` 是显式终态；已开始的消息即使没有 `finish_reason` 也正常收尾。未收到 `[DONE]` 的 EOF 仅在已有 `finish_reason` 时正常收尾。
- OpenAI Responses：`response.completed`、`response.incomplete` 正常收尾；`response.failed`、`error` 或缺少该终态的 EOF 发 `error`。
- Gemini：无首帧重试。`finishReason` 在即为终包。已发 `message_start` 且全程无 text/thinking/tool 时补空 text，再发 `message_delta` / `message_stop`；从未发过首帧则不发任何帧。出站是 anthropic SSE，不是 Chat 的 `[DONE]`。非流认信封里的 `response.*`。
- 状态机收尾后忽略后续上游事件，避免错误帧与正常收尾帧混合。

## 6. 管线流程

```
Claude Code → ccextra:8222
    ↓
1. 解析入站 anthropic body + 入口认证
2. 路由决策 model → provider → protocol
3. 转换前归一化 (claude 全量 / openai 精简子集 / gemini 空臂)
4. 协议转换 (四条 body-to-body；gemini 只产出 request 体)
5. normalize_target_post (仅 openai) + drift 观测 (gemini 跳过)
6. gemini 套信封 (运输字段)；claude/openai 跳过
7. Payload 参数覆盖 (通配匹配，可限定协议；claude / gemini 须显式 protocol，gemini 打信封顶层)
8. 剥离 prompt_cache_retention (仅 openai 路径)
9. prompt_cache_key 注入 (仅 openai)
10. claude 直通: anthropic-beta 重建 + 身份头透传
11. gemini 运输改写 (改名 parameters / schema 二洗 / 删 maxOutputTokens / 账本 / 补签) 后落盘最终信封；再独立发送。claude/openai 走 UpstreamClient.request
12. 响应转发 (流式 SSE 四臂 / 非流: claude 字节直通, openai/gemini 转回 anthropic; 上游错误转 anthropic 形状)
    ↓
上游 Provider
```

## 7. 性能优化

- `preserve_order`：JSON key 顺序稳定
- client 缓存：按代理 key 缓存 reqwest::Client
- bcrypt 验证缓存：避免每请求 ~100ms
- `lto = "fat"` + `codegen-units = 1` + `strip`：优先运行时性能和体积，牺牲 release 编译速度

## 8. 热重载并发模型

`AppState.runtime` 用 `Arc<RwLock<RuntimeConfig>>` 封装 `normalize`/`logging`/`secret`/`upstream`，`/reload` 整块写锁替换；`providers`、`payload_rules` 各自独立 `RwLock`。三把锁分别获取，**非全局原子** —— 窗口内并发请求可能见部分更新（新 providers 配旧 normalize）。热重载低频，可接受。

`handle_messages` 取运行时快照后立即释放读锁（`secret`/`logging`/`normalize` 字段 clone、`UpstreamClient` clone），再 `drop(providers)`/`drop(payload_rules)`，避免跨上游 `await` 持锁阻塞 `/reload`。

`/reload` 无条件重建 `UpstreamClient`（不比较新旧 `proxy_url`），丢弃内部 `reqwest::Client` 连接池缓存。低频操作，取舍可接受；若 proxy_url 未变可复用旧 client 优化。

`auth_cache().clear()` 同样无条件执行，旧 bcrypt 校验结果一律作废 —— secret 未变时代价是几次重算，比漏清风险小。

**不参与热重载**：`logging.level`。`EnvFilter` 仅在启动装载一次（`cli/main.rs`），改级别需重启，或用 `RUST_LOG` 覆盖。

## 9. 与现有栈关系

ccextra 设计为与主力网关并存，独立运行，验证时手动切换，出问题立刻回退。不以替换为目标。

- 日常主力走既有协议网关 + 字节稳定化 sidecar 组合
- ccextra 在 8222 独立运行，验证时切 `ANTHROPIC_BASE_URL` 到 ccextra
- 定位是长期并存的验证/实验入口，出了问题不影响主链路

## 10. Gemini 协议支持

### 10.1 架构概述

Gemini 协议实现完整的 Anthropic Claude ↔ Gemini 双向转换，分为两层：

1. **转换器**（core 层，纯函数）：
   - `convert/gemini.rs` - 请求转换（Anthropic → Gemini）
   - `convert/gemini_response.rs` - 响应转换（Gemini → Anthropic，状态机）
   - `convert/tool_sanitize.rs` - 工具名清洗（64 字符限制）
   - `convert/tool_id.rs` - 工具 ID 生成（SHA256 哈希）
   - `convert/carrier.rs` - 思维签名 carrier 协议
   - `convert/message_convert.rs` - 消息格式转换辅助

2. **执行器**（server 层，IO）：
   - `http.rs` Gemini 协议分支 - 请求管线集成
   - `sse/gemini.rs` - SSE 流式响应转换
   - OAuth 凭证管理 + 账本回放

### 10.2 核心转换模块

**请求转换** (`convert_to_gemini`):
- 工具定义：Anthropic tools → Gemini functionDeclarations
- 工具名清洗：64 字符限制，仅 alphanumeric + underscore，冲突时加确定性后缀
- 工具 ID：`cpa_gemini_{hex(sha256(name)[:16])}`
- Schema 清理：移除 `additionalProperties`、`$schema`，type 字段小写
- System 处理：支持字符串和数组，转为 `{"parts": [{"text": ...}]}`
- Thinking 配置：`body["thinking"]` → `generationConfig.thinkingConfig`
- 消息转换：role 映射（user/assistant→model），content parts 转换
- 生成配置：maxOutputTokens, topP, stopSequences
- Tool choice：转 functionCallingConfig（AUTO/ANY），allowedFunctionNames 使用短名

**响应转换** (状态机 `GeminiStreamState`):
- 响应类型状态：None / Content / Thinking / Function
- 状态转换时发送 `content_block_stop` + `content_block_start`
- Text parts → `text_delta`，thought parts → `thinking_delta`
- ThoughtSignature → `signature_delta`
- FunctionCall → `tool_use`（还原原始工具名，生成唯一 ID）
- 工具调用流式增量：名称为空时续传参数
- FinishReason 映射：STOP→end_turn, MAX_TOKENS→max_tokens，有工具→tool_use
- Usage：`output = candidatesTokenCount + thoughtsTokenCount`
- HasContent 标志：只在有内容时发送 message_stop

### 10.3 管线流程

gemini 不是第四种 UpstreamClient 协议臂。core 产出 Gemini 请求体；server 先套信封，再跑 payload，再做运输改写，再落盘，再独立发送。payload 必须打在信封顶层，不能等发送时才套。

```
入站 anthropic
    ↓
convert_to_gemini          // core：contents / systemInstruction / tools / generationConfig
    ↓
套信封                     // 见下方运输字段
    ↓
payload 覆盖               // 显式 protocol: gemini，打信封顶层
    ↓
运输改写                   // parametersJsonSchema 改名 parameters、
                           // schema 二洗、非 claude 删 maxOutputTokens、
                           // 账本回放、首个 functionCall 补签
    ↓
诊断落盘                   // 全部改写之后的最终信封
    ↓
独立发送                   // 不进 UpstreamClient.request
                           // 流 :streamGenerateContent?alt=sse
                           // 非流 :generateContent
                           // HTTP 头：Content-Type + Bearer + 现有 REQUEST_UA
    ↓
sse/gemini.rs / 非流臂
```

**运输字段**（套信封）：顶层 `model` / `userAgent=antigravity` / `requestType=agent`（第一刀不做 image_gen / web_search）/ `project` / `requestId=agent-<uuid>`。`request.sessionId`：信封已有则用；否则首条 user `parts.0.text` 的 sha256 高 63 位，前缀 `-`；再没有就随机负数。不是 Claude `session_id`。转换器可写默认 `safetySettings`（五类 HARM，CIVIC=`BLOCK_NONE`，其余 `OFF`）；执行器再删，网上游不带。顶层误放的 `toolConfig` 挪进 `request.toolConfig`。

**运输改写**（套信封之后、落盘之前）：`parametersJsonSchema` 改名 `parameters`（在 schema 二洗里做，不在套信封）。非 claude 删 `maxOutputTokens`。schema 只动 `tools` / `generationConfig`。账本回放后再补一次首个 `functionCall` 必须有合法签或哨兵。不抄 `VALIDATED`。

**分层**：core = 转换 / carrier / 工具 id / thinking / 工具名清洗。server = 信封 / schema 清洗 / 账本 / OAuth / daily-prod / `sse/gemini.rs`。账本跑完再补一次：每个 model 回合第一个 `functionCall` 必须有合法签或哨兵。

**工具 id**：Claude 面向 id = `cpa_gemini_` + `hex(sha256(callID \0 name \0 canonicalArgs)[:16])`。`callID` 或 `name` 空则不用此式，回退清洗后的兜底 id。canonicalArgs：能 parse 则再 marshal，否则 trim。

**工具名**：非法字符变 `_`；必须以字母或 `_` 开头（否则先截到 63 再前缀 `_`）；截断 64。不同原名洗成同一值时，按原名排序后加确定性后缀 `_` + `hex(sha256(原名 \0 attempt)[:6])`，总长仍 ≤ 64。响应按还原表改回原名。

**凭证**：`protocol: gemini` 时 `key` 不用。发前读 `auth_dir`（默认配置文件旁 `.cache/antigravity`），`list` 排序后第一份，空目录 401。只发前 `ensure_fresh`；流中 401 不重试。不切号。

**daily / prod**：无自定义 `base_url` 时按 `[daily, prod]` 试。网络错、读错、429、no-capacity 才切下一个 URL。别的 4xx/5xx 不切。不移植 cooldown / credits / 多账号。

**代理**：上游 provider URL > provider `direct`/`""` > 全局 `server.proxy_url` > 直连。代理解析复用现有逻辑。

**payload**：规则必须显式 `protocol: gemini` 才注入，打信封顶层。不发明点路径，不整段替换 `request` / `generationConfig`。

**诊断落盘**：落全部运输改写之后的最终信封（剥离 `Authorization` / `X-Goog-Api-Key` 等敏感头后），不是转换器产出的 request 体，也不是刚套完运输字段、尚未洗 schema 的中间态。

**thinking**：`enabled` + `budget_tokens` 写 `thinkingBudget`；`adaptive`/`auto` + `output_config.effort` 写 `thinkingLevel`，缺省 `high`。有注册表就钳档。第一刀不默认写 `includeThoughts`。

**签名**：带内 carrier + 缺签哨兵 + 账本回放。账本只进程内存：TTL 1h，10240 条，单会话 4096 项 / 16MB。不是九模块 bookkeeping，不落盘。账本键 `claude:{session}:agent:{agent}`（缺 agent = `main`；system 变了加 `:context:{hash}`，hash = sha256 前 16 字节，输入是 JSON marshal 后剥离 `cache_control` 的 system 值）。`prompt_cache_key` 继续裸 session，与账本键无关。

**名门 / 账本门**：名门只认子串 `claude` / `gemini-3-pro` / `gemini-3.1-pro`。账本门：名含 `gemini`/`flash`/`agent` 且不含 `claude`。非 claude 删 `maxOutputTokens`。schema 洗两遍、不合并：转换器对 `input_schema` 先洗一遍（占位 true）；执行器按名门再洗。flash / 3.7-pro 第一刀 `requirePlaceholder=false`。只动 `tools` / `generationConfig`，不动历史 `functionCall.args`。

**内容映射**：model parts 重排 thinking → 普通 → functionCall。入站 base64 `image` → `inlineData`。`redacted_thinking` / `document` / 非 base64 图 / `is_error` 丢掉。`cache_control` / `metadata` 不进信封。

**空 contents**：允许，不补假 user。`stop_sequences` 不映射。`systemInstruction.role` = `user`。对话中途 `role:system` 改成 `user`。剥 `x-anthropic-billing-header:` 归因条。`web_search_*` 声明丢掉。`/v1/models` 仍只汇总配置 alias，代理路径不调 `fetchAvailableModels`。无首帧重试。第一刀不接 `count_tokens`。

**stop_reason**：有工具 → `tool_use`；`MAX_TOKENS` → `max_tokens`；其余（含 `STOP` / `SAFETY` / `UNSPECIFIED` / `UNKNOWN`）→ `end_turn`。

**usage**：`input = promptTokenCount - cachedContentTokenCount`；`output = candidates + thoughts`；`cache_read` 有 cached 才写。`message_start` 用现有 `estimated_input_tokens` 占位，流尾覆盖。有则用 `responseId` / `modelVersion`，没有用占位；流尾不改 id。`functionCall.args` 整段一次 `input_json_delta`。

**不抄**：官方 Gemini 星型枢纽、进程内 tool_use_id 签名 Map、`includeThoughts: true` 默认、假 user 补位。那些是另一套网关的官方 `generativelanguage` 路径，不是 Cloud Code Assist。

## 11. 设计对齐目标

转换逻辑、认证、prompt cache key、thinking 映射、reasoning 回放对齐成熟协议网关实现;九模块归一化对齐字节稳定化 sidecar 的缓存稳定化管线。gemini 路径对齐 Cloud Code Assist 运输与签名回放，不经官方 generateContent 直连。