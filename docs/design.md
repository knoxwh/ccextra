# 架构设计

> 本文档记录 ccextra 的设计决策与实现细节，含与参考实现对标的过程。术语定义见 [glossary.md](glossary.md)。

## 1. Body-to-Body 转换

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

## 4. 关键修正（对标参考实现已知坑）

**坑1：system 位置错误**（responses 路径）

```rust
// ❌ 参考实现把 system 写入 input[0] as developer message
// ✅ ccextra: system 写入 template.instructions
openai_body["template"]["instructions"] = json!(instructions_str);
```

**坑2：工具名截断**（responses 路径）

```rust
// ❌ 参考实现工具名超 64 字符时截断 + 加 _1 后缀
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

## 5. 响应转发（SSE 状态机）

```
claude 直通:      字节级转发,不解析
openai chat:     relay_openai_chat_to_anthropic
                 (单 active block + reasoning 去重 + tool_calls index 映射)
openai responses: relay_responses_to_anthropic
                 (reasoning 回放闭环: summary→thinking_delta +
                  encrypted_content→signature_delta)
```

上游流中断 → 发 anthropic `error` 事件收尾，不裸断流；EOF 兜底保证 `message_delta` + `message_stop`。

## 6. 管线流程

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

## 7. 性能优化

- `preserve_order`：JSON key 顺序稳定
- client 缓存：按代理 key 缓存 reqwest::Client
- bcrypt 验证缓存：避免每请求 ~100ms
- 多 codegen 单元 + strip：release 增量编译约 4s（弃 LTO 换速度）

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

## 10. 设计对齐目标

转换逻辑、认证、prompt cache key、thinking 映射、reasoning 回放对齐成熟协议网关实现;九模块归一化对齐字节稳定化 sidecar 的缓存稳定化管线。