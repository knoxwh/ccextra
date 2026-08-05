# ADR-0002: Claude 直通路径绕过转换器

**状态**: 已采纳  
**日期**: 2026-08-04  
**决策者**: 用户 + Claude  

## 背景

ccextra 三条出站路径之一是 `claude → claude`(入站 `evol-opus-5` 等别名映射到上游 `claude-opus-5`)。作为"协议转换代理",自然的想法是让所有路径统一走转换管线,哪怕 claude → claude 只是 parse 后立刻 rebuild。

但你的实际情况:
- opus/sonnet 主力模型全走这条,是**最热路径**
- 已投入数月验证 tklite 归一化的缓存命中率
- `cache_control` 标记的字节级位置精度直接影响上游 5 分钟 TTL 命中

ADR-0001 已确认不用中间类型,但仍有两种实现方式:
1. **统一往返**:anthropic body → parse 成 `Value` AST → 按需修改 → serialize 回 JSON
2. **字节透传**:只改 `model` 字段,其余部分原样保留

问题:**serialize 重建的字节几乎不可能与入站逐字节一致**(key 顺序、空格、Unicode 转义、数字格式),tklite 归一化的前缀稳定成果会在最后一步被 `serde_json::to_string` 抹掉。

## 决策

**claude 直通路径只改 `model` 字段,其余字节原样保留,不经过 parse → rebuild 往返。**

转换管线分叉:
```rust
match route.protocol {
    Protocol::Claude => {
        // 只改 model,不 parse 完整 body
        passthrough_claude(&mut body, route.upstream_model)?;
    }
    Protocol::OpenAiChat | Protocol::OpenAiResponses => {
        // 完整 parse,转换,rebuild
        let mut value: Value = serde_json::from_slice(&body)?;
        convert_to_target(&mut value, route)?;
        body = serde_json::to_vec(&value)?;
    }
}
```

## 理由

### 1. 保住归一化的字节稳定成果

tklite 九模块归一化的目标是**字节级前缀稳定**,让上游 prompt cache 精确匹配。

入站 body 经过:
```
原始 JSON
  ↓ parse
Value AST
  ↓ normalize_anthropic_full (九模块)
Value AST (已排序/去重/占位符/cache_control 注入)
  ↓ serialize
稳定 JSON 字节
```

如果直通路径再 parse → rebuild 一遍:
```
稳定 JSON 字节
  ↓ parse
Value AST
  ↓ passthrough (只改 model)
Value AST
  ↓ serialize
不同字节 ← key 顺序/空格/转义可能变
```

即使逻辑等价,字节变了就是 cache miss。你已经在 CPA 配置里开了 `cache-regression` 检测,就是为了量化这类退步。

### 2. 最热路径零转换开销

opus/sonnet 占你日常流量大头,绕过 parse/rebuild 省掉:
- 一次 JSON 完整解析(递归遍历 messages 数组、tools 数组、system blocks)
- 一次完整序列化
- AST 内存分配/释放

虽然 `serde_json` 很快,但零开销更快,且直通路径理应如此。

### 3. 未来字段零维护

anthropic 加新字段时(如 `thinking.signature` / `redacted_thinking` 这些逐步演进的),直通路径自动透传,不需要:
- 更新 Rust 结构体定义
- 更新转换逻辑
- 写测试验证新字段

转换路径需要显式决定"转或丢",但直通路径的语义就是"原样",新字段天然涵盖。

### 4. cache_control 标记位置精度

`cache_control` 可以标在:
- system 的最后一个 block
- messages 的某条 message
- message.content 的某个 part
- tools 的某个 tool 定义

tklite `auto_place_anthropic_cache_control` 按启发式规则决定放哪,序列化后的字节位置固定。rebuild 时 `serde_json` 可能:
- 改 key 顺序(`{"type":"text","cache_control":{...}}` vs `{"cache_control":{...},"type":"text"}`)
- 改空格(`{"type": "text"}` vs `{"type":"text"}`)
- 改数字格式(`1.0` vs `1`)

虽然 JSON 语义等价,但前缀字节不匹配 = cache miss。

## 实现细节

### 字节级 model 替换

不能简单字符串替换(入站 body 可能在 messages 里引用 model 名),需解析到顶层:

```rust
pub fn passthrough_claude(body: &mut Vec<u8>, upstream_model: &str) -> Result<()> {
    // 只解析顶层 {"model": ...},不递归进 messages
    let mut value: Value = serde_json::from_slice(body)?;
    value["model"] = json!(upstream_model);
    *body = serde_json::to_vec(&value)?;
    Ok(())
}
```

**风险**:顶层 key 顺序仍可能变。更保险的做法是用 `serde_json::from_slice` + `preserve_order` feature + 只改 `model` 这一个 entry。

或者彻底避免 parse 顶层,用正则只替换顶层 `"model"` 字段:
```rust
// 匹配 "model"\s*:\s*"[^"]*" 但不在嵌套的 messages 里
// 复杂且脆弱,暂不采用,除非顶层 rebuild 也出现字节漂移
```

### 响应侧对称

请求侧字节透传,响应侧也对称:
```rust
async fn relay_claude_passthrough(
    upstream: impl Stream<Item = Bytes>,
    output: impl AsyncWrite,
) {
    // 逐块转发,不解析 SSE 事件
    while let Some(chunk) = upstream.next().await {
        output.write_all(&chunk).await?;
    }
}
```

### 归一化管线位置

直通路径**仍跑 `normalize_anthropic_full`**(pretransform 归一化),只是跳过 `normalize_target_post`(因为没有"目标 body"):

```rust
// 1. pretransform 归一化(所有路径)
normalize_anthropic_full(&mut body, session_key);

// 2. 路由决策
let route = resolve_route(model, providers)?;

// 3. 转换
match route.protocol {
    Protocol::Claude => {
        passthrough_claude(&mut body, route.upstream_model)?;
        // 跳过 target_post
    }
    _ => {
        let mut value = serde_json::from_slice(&body)?;
        convert(&mut value, route)?;
        body = serde_json::to_vec(&value)?;
        normalize_target_post(&mut body, session_key);
    }
}
```

## 代价

### 1. 两套代码路径

直通与转换路径分叉,各自测试:
- 直通路径测试:字节稳定性(归一化前后 diff)、model 替换正确性
- 转换路径测试:字段映射完整性、SSE 状态机正确性

缓解:代码量差异大(直通 <20 行,转换 200-400 行/路径),测试投入合理。

### 2. 架构一致性

"所有路径统一走转换器"的一致性被打破。但 ADR-0001 已经确认不走统一中间类型,所以"一致性"本身已经降级为"代码复用",而直通路径的复用收益为负(parse/rebuild 是纯开销)。

## 替代方案

### A. 统一往返,承认 cache 退步

让 opus/sonnet 也 parse → rebuild,接受字节变化。

**为何不选**:与立项动机("缓存优化")直接冲突。你已经用 `cache-regression` 监控命中率,主动引入退步说不通。

### B. 字节等价校验兜底

parse → rebuild 后与原始 body 做字节 diff,不等价则回退原始字节发送(fail-open)。

**为何不选**:
- 复杂度最高:要维护 diff 逻辑 + 回退分支
- diff 必然不等价(key 顺序几乎必变),兜底分支会 100% 触发,等于写了一套永远不走的转换代码
- 不如直接承认"直通就该透传"

### C. 自研 JSON 序列化器保字节顺序

fork `serde_json`,保证 serialize 输出与 parse 输入字节级一致。

**为何不选**:工程量巨大(serde_json 8 万行),且无法保证与 tklite 的 `serde_json` 行为一致(tklite 用的是官方版)。

## 影响

- `ccextra-core/convert/passthrough.rs` 独立实现,<20 行
- 转换管线 `match protocol` 分叉
- 直通路径单元测试:归一化前后字节 diff、model 替换
- 响应侧 `relay_claude_passthrough` 字节级转发,不解析 SSE

## 遵从原则

- **保护既有投资**:tklite 归一化已验证数月,不在最后一步破坏
- **最热路径优先**:opus/sonnet 是主力,架构迁就它而非强迫它迁就架构
- **语义驱动设计**:直通的语义就是"原样",parse → rebuild 违背这个语义
