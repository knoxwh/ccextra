# ccextra

**针对 Claude Code 请求的 Rust 单进程代理：协议转换 + 缓存优化 + 上游路由**

[![Tests](https://img.shields.io/badge/tests-296%20passed-brightgreen)]()
[![Rust](https://img.shields.io/badge/rust-1.75%2B-orange)]()

## 特性

- ✅ **单进程全包**：HTTP + 转换 + 归一化 + 上游在一个 Rust 二进制
- ✅ **字节级直通**：claude → claude 只改 model，保住归一化成果
- ✅ **三协议支持**：Claude(直通) / OpenAI Chat / OpenAI Responses
- ✅ **缓存优化**：完整移植 tklite 九模块归一化（7547 行）
- ✅ **两段式归一化**：pretransform(9 模块) + post-transform(3 模块)
- ✅ **流式状态机**：手写 SSE 解析器 + 三条独立状态机
- ✅ **热重载**：`POST /reload` 无需重启更新配置
- ✅ **代理支持**：全局 + 每 provider 覆盖
- ✅ **Payload 参数覆盖**：按模型通配符匹配（如 `*glm*`）

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
  proxy_url: "http://127.0.0.1:7897"  # 可选

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
    models:
      - name: gpt-5.6-terra
        alias: ck-gpt-5.6-terra

payload:
  - models: ["*glm*", "*kimi*"]
    params:
      max_tokens: 32000
      temperature: 0.1

normalize:
  enabled: true
  drift_detector: true

logging:
  level: info
  request_body: false
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

热重载配置：

```bash
curl -X POST http://127.0.0.1:8222/reload
```

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

### 3. 两段式归一化

```
入站 anthropic
    ↓
normalize_anthropic_full (9 模块)
  - tool_def sort
  - smoosh split
  - bookkeeping strip
  - tool_input normalize
  - sort stabilize
  - reminder rstrip
  - volatile strip
  - cache_control inject
  - drift detect
    ↓
协议转换 (body-to-body)
    ↓
normalize_target_post (3 模块，仅转换路径)
  - tool_def normalize
  - volatile strip
  - drift detect
    ↓
上游请求
```

**理由**：`cache_control` 注入需 anthropic 结构；转换引入新漂移需二次清理。

### 4. 关键修正（CPA 已知坑）

**坑1：system 位置错误**

```rust
// ❌ CPA: system 写入 input[0] as developer message
// ✅ ccextra: system 写入 template.instructions
openai_body["template"]["instructions"] = json!(instructions_str);
```

**坑2：工具名截断**

```rust
// ❌ CPA: 工具名超 64 字符时截断 + 加 _1 后缀
// ✅ ccextra: 工具名保留原样
openai_body["template"]["tools"][0]["name"] = tool.get("name");
```

测试验证：

```rust
#[test]
fn test_tool_name_preserved() {
    let name = "very_long_tool_name_that_exceeds_sixty_four_characters";
    // 转换后工具名原样保留，无截断、无后缀
    assert_eq!(result["template"]["tools"][0]["name"], name);
}
```

## 管线流程

```
Claude Code → ccextra:8222
    ↓
1. 解析入站 anthropic body
2. normalize_anthropic_full (若启用)
3. 路由决策 model → provider → protocol
4. Payload 参数覆盖 (通配匹配)
5. 协议转换 (三条 body-to-body)
6. normalize_target_post (仅转换路径)
7. 上游请求 (reqwest + 代理)
8. 响应转发 (流式 SSE 状态机)
    ↓
上游 Provider
```

## 测试

296 个单元测试，覆盖率 100%：

```bash
cargo test --all
```

测试分布：
- cache_stabilization: 219 个（9 模块归一化）
- sse: 24 个（parser + 状态机）
- convert: 21 个（三条转换路径）
- http: 7 个（管线集成）
- upstream: 8 个（代理逻辑）
- config: 6 个（YAML 解析）
- 其他: 11 个

## 项目结构

```
ccextra/
├── crates/
│   ├── ccextra-core/           # 纯逻辑，无 IO
│   │   ├── cache_stabilization/  # 九模块归一化 (7547 行)
│   │   ├── convert/              # 三条转换 (773 行)
│   │   ├── route.rs              # 路由决策
│   │   ├── session.rs            # 会话身份派生
│   │   └── normalize.rs          # 归一化编排
│   ├── ccextra-server/         # IO 层
│   │   ├── http.rs               # axum 入口 + 管线
│   │   ├── upstream.rs           # reqwest 客户端
│   │   └── sse/                  # SSE 解析 + 状态机
│   └── ccextra-cli/            # 入口 + 配置
├── docs/adr/                   # 架构决策记录
├── config.example.yaml         # 配置示例
├── README.md
└── CONTEXT.md                  # 领域术语表
```

## 文档

- **[CONTEXT.md](CONTEXT.md)** — 领域术语表
- **[docs/adr/](docs/adr/)** — 架构决策记录
  - [ADR-0001](docs/adr/0001-body-to-body-over-intermediate-type.md) — Body-to-Body 转换
  - [ADR-0002](docs/adr/0002-claude-passthrough-preserves-bytes.md) — 字节级直通
  - [ADR-0003](docs/adr/0003-two-phase-normalization-pipeline.md) — 两段式归一化

## 性能优化

- `preserve_order`：JSON key 顺序稳定
- `Arc<RwLock<>>`：热重载并发安全
- client 缓存：按代理 key 缓存 reqwest::Client
- LTO + strip：release 二进制 4.8M

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
