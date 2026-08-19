# 架构设计

> 本文档记录 ccextra 的设计决策与实现细节，含与参考实现对标的过程。术语定义见 [glossary.md](glossary.md)。

## 1. Body-to-Body 转换

四条独立转换路径 + claude 直通，无中间类型，避免字段丢失：

```rust
// claude 直通：只改 model
convert_passthrough(&mut body, upstream_model);

// anthropic → openai chat
convert_to_openai_chat(&mut body, upstream_model);

// anthropic → openai responses
convert_to_openai_responses(&mut body, upstream_model);

// anthropic → Gemini 请求体(运输层直接发送)
convert_to_gemini(&mut body, upstream_model);

// anthropic → Antigravity 信封(先 Gemini 转换再套运输字段)
convert_to_antigravity(&mut body, upstream_model, project_id);
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
  └─ gemini/antigravity 路径: 同跑 pretransform 5 模块子集
       - 输入仍是 anthropic 形状,子集安全幂等
       - 不写 cachedContent,不注入 prompt_cache_key,跳过 post 归一化与 drift
    ↓
协议转换 (body-to-body，content 形态归一化)
    ↓
normalize_target_post (4 模块，仅 openai 转换路径；gemini/antigravity 跳过)
  - tool_def normalize / sort stabilize / reminder rstrip / volatile strip
    ↓
drift 观测 (claude / openai 链路；gemini/antigravity 跳过)
    ↓
上游请求
```

**理由**：`cache_control` 注入需 anthropic 结构且对 openai 上游无意义；转换引入新漂移需二次清理；drift 观测统一在转换后进行。gemini/antigravity 不做前缀稳定，空臂占位，避免九模块误改 Gemini 请求体。

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
                 (reasoning replay 闭环: output_item.done 收集 +
                  completed 补空 output + 缓存 replay 项 + 下轮注入;
                  summary→thinking_delta + encrypted_content→signature_delta)
gemini/antigravity: relay_gemini_to_anthropic (sse/gemini.rs,两协议共用;
                 antigravity 先解 {"response": {...}} 信封)
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
3. 转换前归一化 (claude 全量 / 其余协议精简子集；gemini/antigravity 跳过 drift)
4. 协议转换 (五条 body-to-body；antigravity 在 core 内完成信封)
   - responses: 转换后注入 reasoning replay (对齐 CPA applyCodexReasoningReplayCacheRequired,
     仅判断来源协议 FormatClaude 不限模型;ccextra 入站恒为 anthropic 故全部启用)
5. normalize_target_post (仅 openai) + drift 观测 (gemini/antigravity 跳过)
6. Payload 参数覆盖 (通配匹配，可限定协议；claude 直通须显式 protocol)
7. 剥离 prompt_cache_retention (非 claude 路径)
8. prompt_cache_key 注入 (provider 级开关,仅 openai 协议)
9. 诊断落盘 (可选) + claude 直通: anthropic-beta 重建 + 身份头透传
10. 统一走 UpstreamClient.request (按协议取 URL/UA;多 base_url 按序回退)
11. 响应转发 (流式 SSE 五臂, gemini/antigravity 共用状态机 / 非流: claude 字节直通, 其余转回 anthropic; 上游错误转 anthropic 形状)
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

## 10. Gemini / Antigravity 协议支持

两条协议共用 Gemini 内容模型（contents/parts/functionCall），差异在 schema 清洗语义与运输信封。转换对齐 CLIProxyAPI（CPA）的 gemini / antigravity 翻译器与 executor。

### 10.1 分层

- **core（纯函数）**：`convert/gemini.rs`（`SchemaFlavor` + `convert_to_gemini_with`）、`convert/gemini_schema.rs`（深度 schema 清洗，对齐 CPA `internal/util/gemini_schema.go`）、`convert/antigravity.rs`（信封 + VALIDATED + maxOutputTokens 封顶）、`convert/gemini_response.rs`（响应转换状态机）、`convert/message_convert.rs`、`tool_sanitize.rs`、`tool_id.rs`
- **server（IO）**：`http.rs` 协议分支、`upstream.rs`（URL/UA）、`sse/gemini.rs`（两条协议共用）、`antigravity/`（OAuth 登录、凭证存储、token 刷新、模型拉取、动态 provider 注入）

### 10.2 请求转换（对齐 CPA）

`convert_to_gemini` = `convert_to_gemini_with(.., SchemaFlavor::Gemini)`；Antigravity 走 `SchemaFlavor::Antigravity`，同一实现按口味分流。

- **工具**：`input_schema` 深度清洗后以 `parametersJsonSchema` 承载；`web_search_*` 服务端工具剥离（对齐 CPA `web_search.go` 的 strip；googleSearch 构造分支上游为死代码，不移植）
- **schema 清洗**（`gemini_schema.rs`，对齐 CPA 四阶段）：本地 `$ref` 内联/转提示、`const`→`enum`、enum 值转 string、约束与 additionalProperties 提示并入 description、条件/allOf/anyOf/oneOf 展平、type 数组取首+nullable、移除不支持关键字与 `x-*`、required 按 properties 修剪。Gemini 口味再去 nullable/title/占位字段并强制 enum `type=string`；Antigravity 口味内联全部本地 `$ref`、drop 全部 enum 转提示、空对象 schema 补 `reason` 占位属性（VALIDATED 模式要求）
- **thinking**：`enabled`+`budget_tokens`→`thinkingBudget`；`adaptive`/`auto`：显式 `output_config.effort`→`thinkingLevel`；否则 Gemini 按注册表 thinking.max 发 `thinkingBudget`（2.5-flash/lite 24576，其余 `gemini-*` 32768），Antigravity 兜底 `thinkingLevel=high`
- **system**→`systemInstruction`（role=user）；对话中途 `role:system` 消息→user 角色 `<system-reminder>` 文本；剥 `x-anthropic-billing-header:` 归因条
- **tool_choice**→`functionCallingConfig`（AUTO/NONE/ANY），命名选择写 `allowedFunctionNames`（用本轮声明短名）

### 10.3 消息与 thinking 签名

`message_convert.rs` 负责 messages→contents：role 映射（assistant→model）、空文本跳过、`tool_use`→`functionCall`（附 `skip_thought_signature_validator` 哨兵，对齐 CPA）、`tool_result`→`functionResponse`（名从 tool_use_id 反解）、base64 图→`inline_data`、尾部未应答 functionCall 的 model 回合剥离。

thinking 块按协议分流：Gemini 直连全丢（非 compat 路径）；Antigravity 保留带非空 `signature` 的块为 `{thought:true, text, thoughtSignature}`，空签名块跳过。CPA 的签名缓存/carrier/CAIS 校验子系统不移植——签名只做客户端透传。

### 10.4 Antigravity 信封与 executor 规则

`convert_to_antigravity` 先跑 Antigravity 口味的 Gemini 转换，再套信封（对齐 CPA `geminiToAntigravity`）：顶层 `model` / `userAgent=antigravity` / `requestType=agent` / `requestId=agent-<hex>` / `project`（空则不写），Gemini 字段移入 `request`；`request.sessionId` = 首条 user 文本 sha256 前 8 字节（掩符号位）前缀 `-`，无则随机。

executor 规则（对齐 CPA `antigravity_executor_request.go`）：claude 模型强制 `request.toolConfig.functionCallingConfig.mode=VALIDATED`；`generationConfig.maxOutputTokens` 按注册表 `max_completion_tokens` 封顶（静态表：claude-* 64000，gemini-3.x 系列 65536/65535，gpt-oss-120b-medium 32768）；非 claude 模型删 `maxOutputTokens`。`safetySettings` 删除。

### 10.5 凭证与发送

- **Gemini 直连**：`key` = API Key，经 `x-goog-api-key` 头发送（对齐 CPA gemini executor），`{base}/v1beta/models/{model}:generateContent | :streamGenerateContent`
- **Antigravity**：启动时 `antigravity/provider.rs` 扫描 `auth_dir`（默认 `.cache/antigravity`），为每份有效凭证注入一个 provider（过期自动刷新回写，模型列表拉取，project_id 进 metadata）。base_url 为 `[daily, prod]` 回退；URL `/v1internal:generateContent | :streamGenerateContent?alt=sse`；UA 固定 `antigravity/hub/...`（非 antigravity UA 上游直接 404）。登录走 CLI 子命令 `ccextra antigravity-login`

### 10.6 响应转换

流式：`sse/gemini.rs` 状态机两协议共用，Antigravity 先解 `{"response": {...}}` 信封。无 `event:` 分派；`data:` 行；part.text 当增量；thought parts→`thinking_delta`；thoughtSignature→`signature_delta`；functionCall→`tool_use`（按短名还原表改名）；`finishReason` 在即为终包（STOP→end_turn、MAX_TOKENS→max_tokens、有工具→tool_use）；已发 `message_start` 且全程无内容时补空 text 再收尾；无首帧重试。非流：`convert_gemini_response`，Antigravity 同样先解信封。usage：`output = candidatesTokenCount + thoughtsTokenCount`。

## 11. 设计对齐目标

转换逻辑、认证、prompt cache key、thinking 映射、reasoning replay 对齐成熟协议网关实现;九模块归一化对齐字节稳定化 sidecar 的缓存稳定化管线。gemini/antigravity 路径对齐 CLIProxyAPI 的翻译器与 executor(schema 清洗、VALIDATED、maxOutputTokens 封顶、信封与 UA)。reasoning replay 对齐 CPA 的 xAI/codex reasoning replay 实现(applyXAIReasoningReplayCacheRequired / applyCodexReasoningReplayCacheRequired + cacheXAIReasoningReplayFromCompleted / cacheCodexReasoningReplayFromCompleted):responses 协议 reasoning 是服务器端状态(store=false),上游不保留须回放 encrypted_content,否则模型丢失决策记忆重复发工具调用。CPA 启用条件为来源协议 FormatClaude(不判断模型),ccextra 入站恒为 anthropic 故 responses 全部启用。实现包含:流式 output_item.done 收集、completed 补空 output、归一化提取(reasoning/message/function_call/custom_tool_call 最小形状+锚点检测)、过滤(去重+对齐+匹配 output)、注入(call_id 双候选+插入位置计算)、TTL 1h 滑动续期、400 invalid signature 清缓存、缓存 key "{model}:{session}"。