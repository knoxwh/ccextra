# Grok doom loop 检测与恢复

> 状态:已实现(2026-08-19)。机制研究自 grok-build 官方源码
> (`/Users/wanghao/work/claude-data/grok-build`),实现针对 ccextra。

## 1. 背景:grok-4.6 死循环

### 1.1 现象

Claude Code 经 ccextra 走 grok-4.6(responses 协议)执行任务时,模型陷入
行为死循环:反复执行 `rm -rf` + `find` 等相同工具调用,16 轮不收敛,
token 持续燃烧。

### 1.2 日志分析结论

日志:`logs/upstream_body_fc8e789e_*.openairesponses.json`(16 轮请求体)+
`logs/upstream_meta_*.json`(状态码与 response_id)。

时间线:

| 轮次 | 状态 |
|------|------|
| round-0 | 正常,thinking + signature 闭环 |
| round-2/3 | 502,CC 自动重试 |
| round-4 | 正常,空 thinking(content=null 仅 signature)被 CC 保留 |
| round-5 起 | **上游响应不再含 reasoning item** |
| round-6~16 | 死循环(重复工具调用) |

根因链:

```
grok-4.6 从 round-5 起不返回 reasoning item
  → CC 无 thinking 块可存入对话历史
  → replay 缓存只剩 function_call 项,被 filter 去重
  → 下轮注入 0 项 reasoning
  → 模型丢失决策记忆,重复已做过的决策
```

反证确认:round-4 的空 thinking 块被 CC 正常保留,说明 CC 不丢空
thinking 块;round-5 起确实是上游响应里没有 reasoning item。

### 1.3 结论

ccextra 无 bug:reasoning replay 与 CPA 对齐(注入/提取/过滤/模型过滤),
SSE 状态机完备。死循环是 grok-4.6 模型行为——reasoning 缺失导致决策
记忆断裂,模型在无 thinking 状态下重复采样出相同行为。

ccextra 侧无法"修复"模型不返回 reasoning;能做的是**检测循环并触发
重采样**,即 grok-build 官方客户端的方案。

## 2. grok-build 官方方案

代码来源:`/Users/wanghao/work/claude-data/grok-build`(xAI 官方 grok CLI
源码)。核心思路:**服务端检测 + 客户端中断重采样**——不修 reasoning
缺失,直接掐掉燃烧的流,换新样本。

### 2.1 机制四层

**① 开启检测(请求头)**

来源:`crates/codegen/xai-grok-sampler/src/client.rs` L1420

```rust
if let Some(policy) = self.defaults.doom_loop_recovery {
    http_request =
        http_request.header(DOOM_LOOP_CHECK_HEADER, policy.window_tokens.to_string());
}
```

请求头 `x-grok-doom-loop-check: 1024`。值 = 检测窗口 token 数(默认
1024,范围 512-4096)。不发头 = 关闭。配置存在性即开关,无独立
enabled 标志。

**② 服务端报告(两处)**

来源:`crates/codegen/xai-grok-sampling-types/src/doom_loop.rs` L1-20
(wire 契约注释)

- mid-stream SSE 事件 `response.doom_loop_check`,携带**累计**触发器集,
  随新触发器出现增量下发:
  `{"type":"response.doom_loop_check","doom_loop_check":{"triggers":["…"]}}`
- 终态响应对象(`response.completed` / `response.incomplete`)带
  `doom_loop_check: {"triggers": ["…"]}` 字段

触发器语法(不透明标签):

```
tail_repetition:{threshold}@{channel}   # 尾部重复,阈值越低循环越紧
low_logprob@{channel}                   # 低对数概率
```

channel 取值如 `thinking` / `response`。

**③ 置信判定**

来源:`xai-grok-sampling-types/src/doom_loop.rs` `DoomLoopRecoveryPolicy::is_confident`

```rust
pub fn is_confident(&self, signal: &DoomLoopSignal) -> bool {
    let tight = |t: u32| t <= self.max_threshold;
    signal.channel == THINKING_CHANNEL
        && matches!(signal.kind, DoomLoopSignalKind::TailRepetition(t) if tight(t))
}
```

只认 **thinking channel 的 tail_repetition 且阈值 ≤ max_threshold(默认
64,范围 2-64)**。其他一切——response channel、low_logprob、未知类型、
更松阈值——warn-only 不行动。理由:可见输出的循环留给用户判断,
thinking 里打转才是真浪费。

**④ 恢复动作(中断 + 重采样)**

来源:
- `xai-grok-sampler/src/stream/responses.rs` L204-214(中断)
- `xai-grok-sampler/src/actor/request_task.rs` L125-260(重试循环)
- `xai-grok-sampler/src/retry.rs` L89-99(backoff)

置信信号出现 → **立即断 SSE 连接**(丢弃燃烧的尾部,不等终态)→
`SamplingError::DoomLoopDetected { triggers, aborted_at_chunk }`。

重试循环要点:

- **独立预算**:max_retries 默认 2(范围 0-5),不占传输层重试额度
- **近即时 backoff**:0-250ms 哈希抖动。源码注释原话:"Loops are
  stochastic at sampling temperature, so a fresh sample is the remedy —
  waiting buys nothing beyond de-syncing concurrent resamples."
- **预算耗尽 disarm**:`doom_check = doom_policy.filter(|_| doom_retry_count
  < doom_max_retries)`,预算用完后 abort 解除,最后一次尝试完整跑完
  照单接受(宁可要循环输出也不要空手)
- 每次尝试新建 `DoomLoopSignalCollector`,信号不跨尝试泄漏
- 中断检查在事件处理**之前**:终态帧携带信号时,不会先接受响应再中断

### 2.2 信号收集器

来源:`xai-grok-sampler/src/doom_loop.rs`(`DoomLoopSignalCollector`)

- SSE 解码层(Layer-1)对每帧调 `absorb(event_name, data)`:
  - 识别 `response.doom_loop_check` 事件(按 SSE `event:` 名或 payload
    `type`),**吞掉不转发**——非标准事件,typed 反序列化会失败
  - 终态响应对象上的 `doom_loop_check` 字段同样记录
  - 触发器按 raw label 去重(服务端重发累计集)
  - malformed payload 只 debug-log 一次,永不报错——检测是 best-effort,
    不能因解析失败弄挂流
- 流转换层(Layer-2)在 `response.completed` 时 `take()` 信号落到
  `ConversationResponse.doom_loop_signals`,供重试循环终态置信检查

### 2.3 默认策略值

来源:`xai-grok-sampling-types/src/doom_loop.rs`
`DoomLoopRecoveryPolicy` 常量

| 参数 | 默认 | 范围 | 说明 |
|------|------|------|------|
| max_threshold | 64 | 2-64 | 置信 tail_repetition 阈值上限 |
| max_retries | 2 | 0-5 | 重采样预算 |
| window_tokens | 1024 | 512-4096 | 检测窗口(请求头值) |

## 3. ccextra 实现

### 3.1 架构差异

ccextra 是代理不是客户端:重采样由 Claude Code 做(CC 收到 anthropic
`error` 事件自动重试),ccextra 无需自己管 retry 计数与 backoff——比
grok-build 简单一层。ccextra 只负责:**发头启用检测 + 解析信号 +
置信时中断流**。

### 3.2 改动点(已实现)

**① upstream.rs:发检测头**

与现有 `x-grok-conv-id` 会话路由头同位置,grok 模型请求加:

```
x-grok-doom-loop-check: 1024
```

**硬编码 1024**(对齐 grok-build 默认值),避免过早配置化。

**② sse/responses.rs 状态机:解析 + 吞掉 + 中断**

- 解析 `response.doom_loop_check` 事件(mid-stream),**吞掉不转发**:
  非标准事件,转发给 CC 无意义
- 解析终态响应对象上的 `doom_loop_check` 字段
- 触发器解析:raw label 按 `tail_repetition:{t}@{channel}` /
  `low_logprob@{channel}` 语法拆,提取 threshold 数值(如
  `tail_repetition:32@thinking` 提取 32),拆不动记 Unknown,不报错
- 置信判定(对齐 grok-build):`channel == "thinking"` 且
  `tail_repetition` 阈值 ≤ 64
- 置信触发 → 发 anthropic `error` 事件中断流(复用现有
  "流式中断发结构化 error 事件兜底"路径),CC 自动重试 = 免费获得
  重采样
- 非置信信号只记 warn 日志,流照常转发
- 触发器按 raw label 去重(服务端重发累计集,对齐
  DoomLoopSignalCollector)

**③ 纯逻辑下沉 core**

触发器解析 + 置信判定是纯函数,放 `ccextra-core/src/doom_loop.rs`
(`parse_trigger` + `is_confident`),单测覆盖。server 层只做
事件拦截与中断动作。

**④ 非流出口(non_stream.rs)**

终态响应对象同样带 `doom_loop_check` 字段,`responses_to_anthropic`
置信时返回 anthropic error body 而非正常响应。

### 3.3 与 grok-build 的取舍差异

| 点 | grok-build | ccextra 方案 | 理由 |
|----|-----------|--------------|------|
| 重采样执行 | 客户端自己重发请求 | 交给 CC 重试 | 代理无会话状态,CC 天然重试 |
| retry 预算 | 自管(默认 2 次) | CC 侧重试上限兜底 | 同上 |
| backoff | 0-250ms 抖动 | 无(CC 控制) | 同上 |
| disarm 机制 | 预算耗尽接受循环输出 | 无预算概念 | CC 重试次数有限,天然有界 |
| 检测头 | 配置门控 | provider 配置或硬编码 | 待定 |

### 3.4 风险与注意

- **error 事件触发 CC 重试的语义**:CC 重试会重发完整请求(含 replay
  注入),新样本即解药,与 grok-build 理念一致。但 CC 重试上限低于
  grok-build 的 2 次预算时,效果打折——可接受,总比烧 16 轮强。
- **误报**:置信判定只认 thinking channel tail_repetition,误报面窄;
  真误报代价是一次重试,可接受。
- **上游契约变更**:解析必须 best-effort,malformed 永不弄挂流(对齐
  grok-build "never fails" 原则)。
- **非流路径**:已覆盖(`responses_to_anthropic` 置信时返回错误)。
