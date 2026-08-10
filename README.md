# ccextra

> 单进程 Rust 代理，把 Claude Code 接到任意上游：协议转换 + prompt 缓存优化 + 上游路由

[![Rust](https://img.shields.io/badge/rust-1.75%2B-orange)](https://www.rust-lang.org/)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
[![Workspace](https://img.shields.io/badge/workspace-3%20crates-lightgrey)]()
[![Docs: design](https://img.shields.io/badge/docs-architecture-informational)](docs/design.md)

单个二进制，监听一个端口，同时提供 Claude 原生协议、OpenAI Chat Completions、OpenAI Responses 三种上游接入。把 Claude Code 的请求按模型路由到不同 provider，并通过对请求体的确定性归一化，最大程度命中上游 prompt 缓存。

```
Claude Code          (ANTHROPIC_BASE_URL → http://127.0.0.1:8222)
     │  POST /v1/messages
     ▼
ccextra :8222  ──  路由 → 归一化 → 协议转换 → 上游请求
     │  响应经 SSE 流式沿原路返回
     ├── claude            → Claude 原生协议
     ├── openai_chat       → OpenAI Chat Completions
     └── openai_responses  → OpenAI Responses
```

## 特性

| 类别 | 说明 |
| ---- | ---- |
| **多协议接入** | 同时支持 Claude 原生 / OpenAI Chat Completions / OpenAI Responses 三种上游协议 |
| **模型路由** | 入站模型名按 alias 解析到唯一 provider，冲突启动即报错，不做隐式推导 |
| **字节级直通** | Claude → Claude 只改 model 字段，其余字节原样保留，保住归一化成果 |
| **prompt 缓存优化** | 九模块归一化消除序列化漂移，命中上游 prompt cache；drift 检测器跨轮观测盲区 |
| **流式状态机** | 手写 SSE 解析器 + 三条独立转发路径，断流发结构化 error 兜底，不裸断流 |
| **热重载** | `POST /reload` 无需重启更新 providers / payload / normalize / `logging.request_body` / secret / 全局代理，并清空 bcrypt 校验缓存；`logging.level` 仅启动生效 |
| **代理支持** | 全局 + 每 provider 覆盖，支持 SOCKS |
| **参数覆盖** | 按模型名通配符匹配（如 `*glm*`）覆盖请求参数，可限定协议生效 |
| **入口认证** | 可选 `secret_key`，明文自动转 bcrypt 落盘，校验结果缓存 |
| **模型列表** | `GET /v1/models` 返回 Anthropic 格式清单，Claude Code 启动自动拉取 |
| **诊断落盘** | 逐请求落盘上游 body，供缓存漂移定位 |

## 工作原理

```
Claude Code → ccextra:8222
    ↓
1. 解析入站 anthropic body + 入口认证
2. 路由决策 model → provider → protocol
3. 转换前归一化（claude 全量 / openai 精简子集）
4. 协议转换（三条 body-to-body，content 形态归一化）
5. post-transform 归一化（仅转换路径）+ drift 观测
6. payload 参数覆盖（通配匹配，可限定协议）
7. 剥离 prompt_cache_retention（仅 openai 路径）
8. prompt_cache_key 注入（provider 级开关）+ 诊断落盘（可选）
9. claude 直通：anthropic-beta 重建 + 身份头透传
10. 上游请求（reqwest + 按协议 UA + 代理）
11. 响应转发（流式 SSE 状态机 / 非流直通；上游错误转 anthropic 形状）
    ↓
上游 Provider
```

## 快速开始

### 编译

```bash
cargo build --release
```

需要 Rust 1.75+。

### 配置

```bash
cp config.example.yaml config.yaml
# 编辑 config.yaml，填入真实 key
```

最小配置示例：

```yaml
server:
  host: "127.0.0.1"
  port: 8222

providers:
  - name: claude
    protocol: claude
    base_url: https://xxxx
    key: sk-ant-xxx
    models:
      - name: claude-opus-5
        alias: claude-opus-5

  - name: saic
    protocol: openai_chat
    base_url: https://xxxx/compatible-mode/v1
    key: sk-xxx
    models:
      - name: glm-5.1
        alias: glm-5.1

  - name: ckff-codex
    protocol: openai_responses
    base_url: https://xxxx/v1
    key: sk-xxx
    prompt_cache_key: true
    models:
      - name: gpt-5.6-terra
        alias: gpt-5.6-terra
```

### 启动

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

### 接入 Claude Code

```bash
export ANTHROPIC_BASE_URL=http://127.0.0.1:8222
```

配置了 `secret_key` 时：

```bash
export ANTHROPIC_AUTH_TOKEN=sk-ccextra-xxx
```

### 验证

```bash
# 发送请求
curl http://127.0.0.1:8222/v1/messages \
  -H "Content-Type: application/json" \
  -d '{"model": "claude-opus-5", "messages": [{"role": "user", "content": "hi"}], "max_tokens": 1024}'

# 模型列表（Claude Code 启动时自动调用）
curl http://127.0.0.1:8222/v1/models -H "x-api-key: sk-ccextra-xxx"

# 热重载配置
curl -X POST http://127.0.0.1:8222/reload

# 健康检查
curl http://127.0.0.1:8222/health
```

`/v1/models` 返回 Anthropic 格式清单：

```json
{
  "data": [
    {"id": "claude-opus-5", "object": "model", "owned_by": "claude",
     "type": "model", "display_name": "claude-opus-5",
     "max_input_tokens": 200000, "max_tokens": 64000}
  ]
}
```

## 配置参考

完整配置项见 [`config.example.yaml`](config.example.yaml)。要点：

| 配置项 | 说明 |
| ------ | ---- |
| `server.host` / `server.port` | 监听地址，默认 `127.0.0.1:8222` |
| `server.proxy_url` | 全局代理兜底，可选；`"direct"` 表示不走代理 |
| `secret_key` | 入口认证，可选。配置后 `/v1/models` 与 `/v1/messages` 需携带匹配 key，否则 401。支持 `x-api-key` 或 `Authorization: Bearer`。明文自动转 bcrypt 落盘。`/reload` 热替换 secret，并清空 bcrypt 校验缓存 |
| `providers[].protocol` | 上游协议：`claude` / `openai_chat` / `openai_responses` |
| `providers[].base_url` / `key` | 上游地址与密钥 |
| `providers[].models[].alias` | 入站模型名 → 上游真实模型名 |
| `providers[].prompt_cache_key` | 缓存桶 key = 会话 ID（对齐 codex 0.147），仅 openai 协议生效 |
| `payload` | 按模型名通配（`*glm*`）覆盖请求参数，可 `protocol` 限定生效范围 |
| `normalize.enabled` | 归一化总开关；`drift_detector` 开启跨轮漂移观测 |
| `logging.request_body` | 逐请求落盘上游 body 到 `logs/`，供缓存漂移定位 |

## 项目结构

```
ccextra/
├── crates/
│   ├── ccextra-core/           # 纯逻辑，无 IO
│   │   ├── cache_stabilization/  # 九模块归一化
│   │   ├── convert/              # 三条协议转换路径
│   │   ├── thinking.rs           # 思考级别映射
│   │   ├── prompt_cache.rs       # prompt_cache_key 注入（key=会话 ID，对齐 codex）
│   │   ├── secret.rs             # 入口 key bcrypt 识别
│   │   ├── route.rs              # 路由决策
│   │   ├── session.rs            # 会话身份派生
│   │   └── normalize.rs          # 归一化编排
│   ├── ccextra-server/         # IO 层
│   │   ├── http.rs               # axum 入口 + 管线
│   │   ├── upstream.rs           # reqwest 客户端（按协议 UA）
│   │   └── sse/                  # SSE 解析 + 状态机
│   └── ccextra-cli/            # 入口 + 配置
├── config.example.yaml         # 配置示例
├── docs/
│   ├── design.md               # 架构设计
│   └── glossary.md             # 领域术语表
├── start.sh / stop.sh / restart.sh / build.sh
└── README.md
```

## 测试

```bash
cargo test --workspace
```

当前 421 个单元测试，分布在缓存归一化、协议转换、SSE 状态机、HTTP 管线与配置解析等模块。

## 文档

- **[docs/design.md](docs/design.md)** — 架构设计：转换路径、归一化、SSE 转发、性能优化
- **[docs/glossary.md](docs/glossary.md)** — 领域术语表

## 许可

[MIT](LICENSE)
