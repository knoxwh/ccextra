# ccextra

**针对 Claude Code 请求的 Rust 单进程代理：协议转换 + 缓存优化 + 上游路由**

[![Tests](https://img.shields.io/badge/tests-378%20passed-brightgreen)]()
[![Rust](https://img.shields.io/badge/rust-1.75%2B-orange)]()

## 特性

- ✅ **单进程全包**：HTTP + 转换 + 归一化 + 上游在一个 Rust 二进制
- ✅ **字节级直通**：claude → claude 只改 model，保住归一化成果
- ✅ **三协议支持**：Claude(直通) / OpenAI Chat / OpenAI Responses
- ✅ **缓存优化**：完整移植 tklite 九模块归一化（约 7500 行）
- ✅ **按协议分流的归一化**：claude 全量(9 模块) / openai pretransform(5 模块) + post-transform(4 模块)
- ✅ **content 形态归一化**：空串→`[]`、null/缺失→丢弃，消除跨轮字节漂移（缓存 MISS 根因修复）
- ✅ **流式状态机**：手写 SSE 解析器 + 三条独立状态机，断流发结构化 error 兜底
- ✅ **claude 直通头重建**：anthropic-beta 按 body 条件重建 + 身份头透传
- ✅ **热重载**：`POST /reload` 无需重启更新配置
- ✅ **代理支持**：全局 + 每 provider 覆盖
- ✅ **Payload 参数覆盖**：按模型通配符匹配（如 `*glm*`），可限定协议生效
- ✅ **模型列表**：`GET /v1/models` 返回 Anthropic 格式清单
- ✅ **入口认证**：可选 `secret_key`，明文自动转 bcrypt 落盘
- ✅ **prompt_cache_key**：provider 级开关，按 会话+模型+agent UUIDv5 派生（对齐 CPA，同桶）
- ✅ **usage 对齐**：`stream_options.include_usage` 强制上游流尾发 usage，statusline 有 context 显示
- ✅ **诊断落盘**：`request_body` 开启时逐请求落盘上游 body，供缓存漂移定位

## 快速开始

### 1. 编译

```bash
cargo build --release
```

### 2. 配置

复制示例配置：

```bash
cp config.example.yaml config.yaml
# 编辑 config.yaml，填入真实 key
```

配置示例：

```yaml
server:
  host: "127.0.0.1"
  port: 8222
  proxy_url: "http://127.0.0.1:7897"  # 可选，全局代理兜底

secret_key: "sk-ccextra-xxx"  # 可选;配置后 /v1/models 与 /v1/messages 需 x-api-key 匹配

providers:
  - name: evol-claude
    protocol: claude
    base_url: https://mg-new.evolai.cn/claude-proxy
    key: sk-ant-xxx
    proxy_url: "direct"  # 覆盖全局代理
    models:
      - name: claude-opus-5
        alias: evol-opus-5

  - name: saic
    protocol: openai_chat
    base_url: https://dashscope.aliyuncs.com/compatible-mode/v1
    key: sk-xxx
    models:
      - name: glm-5.1-xxx
        alias: glm-5.1

  - name: ckff-codex
    protocol: openai_responses
    base_url: https://ckff.dev/v1
    key: sk-xxx
    prompt_cache_key: true  # 可选;注入派生缓存桶 key(对齐 CPA)
    models:
      - name: gpt-5.6-terra
        alias: ck-gpt-5.6-terra

payload:
  - models: ["*glm*", "*kimi*"]
    protocol: openai_chat   # 可选;限定生效协议,缺省 = 所有协议
    params:
      max_tokens: 32000
      temperature: 0.1

normalize:
  enabled: true
  drift_detector: true

logging:
  level: info
  request_body: false   # 开启后逐请求落盘 logs/upstream_body_*.json
```

### 3. 启动

```bash
./target/release/ccextra --config config.yaml
```

或使用脚本：

```bash
./start.sh    # 后台启动
./stop.sh     # 停止
./restart.sh  # 重启
./build.sh    # 构建（自动重启）
```

### 4. 使用

配置 Claude Code：

```bash
export ANTHROPIC_BASE_URL=http://127.0.0.1:8222
```

发送请求：

```bash
curl http://127.0.0.1:8222/v1/messages \
  -H "Content-Type: application/json" \
  -d '{
    "model": "evol-opus-5",
    "messages": [{"role": "user", "content": "hi"}],
    "max_tokens": 1024
  }'
```

拉取模型列表（Claude Code 启动时自动调用）：

```bash
curl http://127.0.0.1:8222/v1/models \
  -H "x-api-key: sk-ccextra-xxx"
```

返回 Anthropic 格式清单：

```json
{
  "data": [
    {"id": "evol-opus-5", "object": "model", "owned_by": "evol-claude",
     "type": "model", "display_name": "evol-opus-5",
     "max_input_tokens": 200000, "max_tokens": 64000}
  ]
}
```

热重载配置：

```bash
curl -X POST http://127.0.0.1:8222/reload
```

健康检查：

```bash
curl http://127.0.0.1:8222/health
```

## 入口认证

配置 `secret_key` 后，`GET /v1/models` 与 `POST /v1/messages` 均需携带匹配的 key，否则返回 401。支持两种头：`x-api-key` 或 `Authorization: Bearer <key>`（后者兼容 cc-switch 等工具）。Claude Code 通过 `ANTHROPIC_AUTH_TOKEN` 注入：

```bash
export ANTHROPIC_AUTH_TOKEN=sk-ccextra-xxx
```

**secret_key 自动哈希**（参考 CPA）：`secret_key` 填明文会在启动时自动转为 bcrypt 哈希，并回写配置文件，落盘不再含明文。`x-api-key` 校验用 bcrypt verify（结果缓存，避免每请求 ~100ms），已哈希的 key 不会重复转。

## 架构亮点

### 1. Body-to-Body 转换

三条独立转换路径，无中间类型，避免字段丢失：

```rust
// claude 直通：只改 model
convert_passthrough(&mut body, upstream_model);

// anthropic → openai chat
convert_to_openai_chat(&mut body, upstream_model);

// anthropic → openai responses
convert_to_openai_responses(&mut body, upstream_model);
```

**放弃星型枢纽理由**：入站单一，矩阵退化为 `1×3=3`，中间类型收益为零，且会丢失 image/cache_control/metadata。

### 2. 字节级直通

claude → claude 只改 model 字段，其余字节原样保留：

```rust
body["model"] = Value::String(upstream_model.to_string());
```

**理由**：保住 tklite 归一化的字节稳定性，key 顺序/空格/转义不变，缓存命中率不退步。

### 3. 两段式归一化（按协议分流，对齐 CPA）

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
  └─ openai 转换路径: normalize_anthropic_pretransform (5 模块子集)
       - smoosh split / bookkeeping strip / tool_input normalize
       - sort stabilize / reminder rstrip
       (跳过 tool-def sort / volatile / cache_control —— 转换后处理)
    ↓
协议转换 (body-to-body，content 形态归一化)
    ↓
normalize_target_post (4 模块，仅转换路径)
  - tool_def normalize / sort stabilize / reminder rstrip / volatile strip
    ↓
drift 观测 (三条链路，ancillary 请求跳过)
    ↓
上游请求
```

**理由**：`cache_control` 注入需 anthropic 结构且对 openai 上游无意义；转换引入新漂移需二次清理；drift 观测统一在转换后进行。

### 4. 关键修正（CPA 已知坑）

**坑1：system 位置错误**（responses 路径）

```rust
// ❌ CPA: system 写入 input[0] as developer message
// ✅ ccextra: system 写入 template.instructions
openai_body["template"]["instructions"] = json!(instructions_str);
```

**坑2：工具名截断**（responses 路径）

```rust
// ❌ CPA: 工具名超 64 字符时截断 + 加 _1 后缀
// ✅ ccextra: 工具名保留原样
openai_body["template"]["tools"][0]["name"] = tool.get("name");
```

测试验证（`to_openai_responses.rs`）：

```rust
#[test]
fn test_tool_name_preserved() {
    let name = "very_long_tool_name_that_exceeds_sixty_four_characters_in_total_length";
    // 转换后工具名原样保留，无截断、无后缀
    assert_eq!(name, body["template"]["tools"][0]["name"]);
}
```

### 5. 响应转发（SSE 状态机）

```
claude 直通:      字节级转发,不解析
openai chat:     relay_openai_chat_to_anthropic
                 (单 active block + reasoning 去重 + tool_calls index 映射)
openai responses: relay_responses_to_anthropic
                 (reasoning 回放闭环: summary→thinking_delta +
                  encrypted_content→signature_delta)
```

上游流中断 → 发 anthropic `error` 事件收尾，不裸断流；EOF 兜底保证 `message_delta` + `message_stop`。

## 管线流程

```
Claude Code → ccextra:8222
    ↓
1. 解析入站 anthropic body + 入口认证
2. 路由决策 model → provider → protocol
3. 转换前归一化 (claude 全量 / openai 精简子集)
4. 协议转换 (三条 body-to-body，content 形态归一化)
5. normalize_target_post (仅转换路径) + drift 观测
6. Payload 参数覆盖 (通配匹配，可限定协议)
7. 剥离 prompt_cache_retention (仅 openai 路径)
8. prompt_cache_key 注入 (provider 级开关) + 诊断落盘 (可选)
9. claude 直通: anthropic-beta 重建 + 身份头透传
10. 上游请求 (reqwest + 按协议 UA + 代理)
11. 响应转发 (流式 SSE 状态机 / 非流直通; 上游错误转 anthropic 形状)
    ↓
上游 Provider
```

## 测试

378 个单元测试：

```bash
cargo test --workspace
```

测试分布（按当前代码实测）：
- cache_stabilization: 220 个（drift 57 / volatile 47 / reminder_rstrip 27 / cache_control 25 / sort 23 / tool_def 16 / tool_input 10 / content_strip 8 / smoosh 7）
- convert: 45 个（chat 26 / responses 18 / passthrough 1，含空 content 语义与 CPA 对齐回归）
- sse: 33 个（parser 12 / responses 15 / chat 6，含 reasoning 回放/错误兜底）
- http: 22 个（管线 + 认证 + 模型列表 + payload 覆盖 + beta 重建）
- cli(config): 10 个（格式解析 + secret 自动哈希回写）
- normalize: 9 个（编排 + pretransform 子集 + post-transform 幂等）
- thinking: 9 个（CPA 移植的 effort 映射）
- prompt_cache: 11 个（CPA 移植的 prompt_cache_key 派生，含跨语言向量）
- upstream: 10 个（代理逻辑 + UA 分流）
- session: 5 个（会话派生优先级）
- 其他: 4 个（route 3 / secret 1）

## 项目结构

```
ccextra/
├── crates/
│   ├── ccextra-core/           # 纯逻辑，无 IO
│   │   ├── cache_stabilization/  # 九模块归一化 (约 7500 行)
│   │   ├── convert/              # 三条转换 (chat 855 / responses 594 / passthrough 39)
│   │   ├── thinking.rs           # 思考级别映射(移植 CPA)
│   │   ├── prompt_cache.rs       # prompt_cache_key 派生/注入(移植 CPA)
│   │   ├── secret.rs             # 入口 key bcrypt 识别
│   │   ├── route.rs              # 路由决策
│   │   ├── session.rs            # 会话身份派生
│   │   └── normalize.rs          # 归一化编排
│   ├── ccextra-server/         # IO 层
│   │   ├── http.rs               # axum 入口 + 管线
│   │   ├── upstream.rs           # reqwest 客户端(按协议 UA)
│   │   └── sse/                  # SSE 解析 + 状态机
│   └── ccextra-cli/            # 入口 + 配置
├── config.example.yaml         # 配置示例
├── README.md
└── CONTEXT.md                  # 领域术语表
```

## 文档

- **[CONTEXT.md](CONTEXT.md)** — 领域术语表（含 CPA 对齐会话派生 / prompt_cache_key / reasoning 回放闭环）

## 性能优化

- `preserve_order`：JSON key 顺序稳定
- `Arc<RwLock<>>`：热重载并发安全
- client 缓存：按代理 key 缓存 reqwest::Client
- bcrypt 验证缓存：避免每请求 ~100ms
- 多 codegen 单元 + strip：release 增量编译约 4s（弃 LTO 换速度）

## 与现有栈关系

```
日常主力: CPA(8317) + tklite(/tmp/tklite.sock)
              ↓
         Claude Code
              ↑
         (验证时手动切)
              ↓
          ccextra(8222)
```

ccextra 独立运行在 8222，验证时手动切换，坏了立刻回退。不以替换为目标，允许长期并存。

## 许可

MIT

## 作者

wanghao