# ADR-0003: 两段式归一化管线

**状态**: 已采纳  
**日期**: 2026-08-04  
**决策者**: 用户 + Claude  

## 背景

ccextra 照搬 tklite 九模块归一化,目标是保住上游 prompt cache 字节前缀命中。问题:协议转换会引入新的字节漂移(重排 tools、重建 JSON)——归一化在管线哪一环跑?

三种位置选择:
1. **只在入站归一化一次**:转换后的目标 body 字节漂移无人管
2. **只在出站归一化一次**:但 `cache_control` 注入是 anthropic 专属语义,转换后结构已变
3. **两段式**:转换前后各跑一遍,覆盖全管线

tklite 现状:被 CPA 调两次 —— `/v1/pretransform/messages` 稳定入站历史,`/v1/chat/completions` 优化转换后目标 body。ccextra 单进程,可以内部实现这个语义。

## 决策

**采用两段式归一化管线:**

```rust
// 1. pretransform 归一化 (所有路径,含直通)
normalize_anthropic_full(&mut body, session_key);
// 包含:tool_def sort + smoosh_split + bookkeeping strip
//      + tool_input normalize + sort stabilize + reminder rstrip
//      + volatile strip + cache_control inject + drift detect

// 2. 路由 + 转换
match protocol {
    Claude => passthrough_claude(&mut body, upstream_model)?,
    _ => convert_to_target(&mut body, route)?,
}

// 3. post-transform 归一化 (仅转换路径)
if protocol != Claude {
    normalize_target_post(&mut body, session_key);
    // 包含:tool_def normalize + volatile strip + drift
}
```

**核心分工:**
- `normalize_anthropic_full`:九模块全跑,含 anthropic 专属的 `cache_control` 注入
- `normalize_target_post`:子集三模块,对转换后的 openai body 做二次清理

## 理由

### 1. cache_control 注入必须在转换前

`auto_place_anthropic_cache_control` 按 anthropic 结构决定 marker 位置:
- system 的最后一个 block
- messages 的某条 message.content 最后一个 part
- tools 的最后一个 tool 定义

转换后:
- `system` 可能变成 openai chat 的 `messages[0]` 或 responses 的 `template.instructions`
- anthropic `content` 数组被打散成 openai 结构
- tools 定义格式完全不同

在目标 body 上反推"哪里该放 cache_control"不可行 —— openai 根本没这个字段,放了也无意义(虽然部分中转可能透传,但不是协议标准)。

### 2. 转换引入新漂移需要二次清理

即使入站 body 已归一化,转换器也会引入新的不确定性:

**工具定义重建**:
```rust
// CPA openai_claude_request.go:318
for tool in anthropic_tools {
    let openai_tool = json!({
        "type": "function",
        "function": {
            "name": tool.name,
            "parameters": tool.input_schema,  // key 顺序?
        }
    });
}
```
`serde_json::to_value` 的 key 顺序可能每次不同。

**volatile 新出现**:转换时可能生成新的 UUID/timestamp(如 tool_call_id),需要再跑一遍 `volatile strip`。

**drift 二次检测**:转换后的 tools 结构哈希与入站 anthropic tools 哈希不同,需要按目标协议重算。

### 3. 保留 tklite 的 pretransform 语义

tklite 设计了 `/v1/pretransform/messages` 端点,专门给 CPA 在翻译前调用,目标是"稳定翻译器的输入"。ccextra 单进程虽然没有 IPC,但语义等价:

```
tklite pretransform     →   ccextra normalize_anthropic_full
tklite chat/completions →   ccextra normalize_target_post
```

这让 ccextra 的归一化行为与当前生产栈(CPA + tklite)行为等价,切测时 cache 命中率可直接对比。

### 4. 直通路径只需第一段

claude 直通不经过转换,没有"转换引入的新漂移",所以跳过 `normalize_target_post`:

```rust
match protocol {
    Claude => {
        normalize_anthropic_full(&mut body, session_key);
        passthrough_claude(&mut body, upstream_model)?;
        // 完成,直接发上游
    }
    _ => {
        normalize_anthropic_full(&mut body, session_key);
        convert_to_target(&mut body, route)?;
        normalize_target_post(&mut body, session_key);  // 二次清理
    }
}
```

## 模块分配

### normalize_anthropic_full (九模块全跑)

1. **tool_def_sort** — tools 数组按 name 排序,schema key 递归排序
2. **smoosh_split** — 拆 tool_result 尾部折叠的 `</system-reminder>`
3. **bookkeeping_strip** — 删历史消息里的 token 账本 reminder
4. **tool_input_normalize** — 按 input_schema 顺序重排 tool_use.input key
5. **sort_stabilize** — system 里 skills/deferred 块内列表排序
6. **reminder_rstrip** — 抹平 `</system-reminder>` 尾空白
7. **volatile_strip** — 前缀时间戳/UUID 替换为定长占位符
8. **cache_control_inject** — 自动插 `{type: ephemeral}` marker
9. **drift_detect** — 对 system/tools/early_messages 做三轴哈希观测

### normalize_target_post (子集三模块)

1. **tool_def_normalize** — 对转换后的 openai tools 重排 schema key
2. **volatile_strip** — 清理转换生成的新 UUID(如 tool_call_id)
3. **drift_detect** — 按目标协议结构重算哈希

**不跑的六模块**(只在 anthropic 结构有意义):
- smoosh_split / bookkeeping_strip / tool_input_normalize / sort_stabilize / reminder_rstrip / cache_control_inject

## 实现细节

### 模块幂等性

两次调用 `volatile_strip` / `drift_detect` 要求模块幂等:
- `volatile_strip` 第二次遇到已替换的占位符(`___UUID___`)时 no-op
- `drift_detect` 第二次调用时更新同一 session_key 的哈希桶

tklite 已保证幂等(TECHNICAL_DETAILS.md 确认),ccextra 照搬时继承这个性质。

### session_key 稳定性

两段归一化用同一个 `session_key`(首条消息哈希),不因协议转换而变:

```rust
let session_key = derive_session_key(&body);  // 转换前派生

normalize_anthropic_full(&mut body, &session_key);
convert_to_target(&mut body, route)?;
normalize_target_post(&mut body, &session_key);  // 同一 key
```

这让 drift detector 能正确关联同一会话的前后两次观测。

### 性能开销

两遍归一化 = 两遍 JSON 遍历。但:
- 第二遍只跑三模块(九模块的 1/3)
- 转换路径本身就要 parse/rebuild,多一遍遍历边际成本低
- 直通路径只跑一遍,最热路径无额外开销

## 代价

### 1. 两个调用点

管线需维护两处归一化调用,忘记其中一个会导致:
- 只跑 pretransform:转换引入的漂移未清理,openai 上游 cache 命中率低
- 只跑 post-transform:`cache_control` 没注入,anthropic 上游 cache 失效

缓解:在类型系统里强制,转换函数签名要求传入 `NormalizeContext`:

```rust
pub struct NormalizeContext {
    pretransform_done: bool,
}

pub fn convert_to_target(
    body: &mut Value,
    ctx: &NormalizeContext,  // 编译期强制检查
) -> Result<()> {
    assert!(ctx.pretransform_done);  // 运行时兜底
    // ...
}
```

### 2. 模块职责划分

九模块中哪些放 pretransform、哪些放 post-transform,需要明确文档。错放会导致:
- anthropic 专属模块放 post-transform:在 openai body 上跑会崩溃或 no-op
- 通用模块只放 pretransform:转换引入的漂移未清理

缓解:CONTEXT.md 已明确列出两个子集,代码注释再强调。

## 替代方案

### A. 只在入站归一化一次

最简单,但转换引入的漂移(tools 重建、新 UUID)无人管,openai 上游 cache 命中率会低于 tklite 当前水平。

### B. 只在出站归一化一次

`cache_control` 注入无法实施(目标 body 结构已变),anthropic 上游直通路径 cache 失效。

### C. 三段式:pretransform + mid-transform + post-transform

在转换中间插第三个归一化点。过度设计,且转换是原子操作(body-to-body 函数),中间插点会破坏封装。

## 影响

- `ccextra-core/normalize/mod.rs` 导出两个函数:`normalize_anthropic_full` / `normalize_target_post`
- 管线两个调用点:转换前(所有路径)、转换后(仅转换路径)
- 直通路径只跑第一段,跳过第二段
- 单元测试分别验证两段的幂等性和模块子集正确性

## 遵从原则

- **行为等价**:与 tklite pretransform + post-transform 语义对齐,保证切测时 cache 命中率可比
- **职责分离**:anthropic 专属逻辑(cache_control)放 pretransform,通用清理(volatile/drift)两段都跑
- **最热路径优先**:直通路径只跑一遍,不为转换路径的需求拖累
