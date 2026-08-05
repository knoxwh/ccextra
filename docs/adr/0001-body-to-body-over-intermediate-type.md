# ADR-0001: Body-to-Body 转换,放弃中间类型

**状态**: 已采纳  
**日期**: 2026-08-04  
**决策者**: 用户 + Claude  

## 背景

ccextra 入站锁定单一协议(anthropic messages),出站三条路径(claude 直通 / openai chat / openai responses)。初期调研推荐"抄 ai-gateway 星型枢纽",即所有协议先转 `StandardRequest` 中间类型,再转目标协议。

但深入调查 ai-gateway 源码后发现:**StarredRequest 对 anthropic 特性是有损的**:
- 无 `image` content 变体,图片在 source 解析时直接丢失
- 不捕获 `cache_control` 标记(仅 tool 定义字段白名单保留,message/system 级全丢)
- 不解析 `thinking.budget_tokens`
- `metadata` 不进中间类型
- target/anthropic-messages.ts 零 passthrough 字段,丢了补不回来

而你的需求:
- 要给 glm/kimi 粘图(image 必须保)
- `cache_control` 是整个缓存优化的命门(tklite 九模块最终落点)
- 主力模型 opus/sonnet 走 claude 直通,是最热路径,不能让中间层 rebuild 抹掉归一化的字节稳定成果

## 决策

**采用三条独立 body-to-body 转换,不引入中间类型。**

每条路径在 `serde_json::Value` 上原地读写,类似 CPA 的 gjson/sjson 风格:
- claude 直通:只改 `model` 字段,其余字节原样
- anthropic → openai chat:独立转换函数
- anthropic → openai responses:独立转换函数

公共逻辑(tool 定义转换、finish_reason 映射、usage 归一化)通过 helper 函数复用,不经过中间类型。

## 理由

### 1. 矩阵规模不支持星型枢纽收益

ai-gateway 星型枢纽的核心价值是**避免 N×N 组合爆炸**(6 协议双向 = 30 组转换器)。你的入站只有 1 种,矩阵退化为:

```
1 入站 × 3 出站 = 3 条转换路径
```

星型带来的"新协议只加一对 source/target"扩展性,在单入站场景下收益为零。

### 2. 无中间层 = 零字段丢失类 bug

body-to-body 转换天然无"中间层吞字段"问题:
- 每个字段都是显式一读一写,没写代码 = 没转换,但不会被中间层静默吞掉
- `cache_control` / `image` / `thinking.budget_tokens` 要加就加一行 `value["cache_control"] = src["cache_control"]`,不受中间类型建模约束
- ai-gateway 那套要补字段需同时改 types / source adapter / target adapter 三处,且已有 6 协议的存量负担

### 3. 直通路径保字节稳定

claude 直通绕过任何 parse → rebuild 往返:
```rust
// 只改 model,其余字节不动
body["model"] = upstream_model;
```

tklite 九模块归一化的字节稳定成果直达上游,不被 rebuild 抹掉。这对 opus/sonnet 主力流量至关重要 — 你已投入 tklite 数月验证缓存命中率,不能在最后一步退步。

### 4. CPA 已验证可行性

CPA 就是 body-to-body 架构,用 gjson 读 / sjson 写,支撑了你日常全部流量。`cache_control` 在 openai → claude 方向有 17 处搬运调用(虽然 claude → openai 方向都丢了,但证明技术可行)。500-900 行/路径的规模已知可控。

### 5. 响应侧对称

请求侧三条独立,响应侧也三条独立状态机:
- claude 直通 = 字节级 SSE 转发
- openai chat → anthropic SSE 状态机
- responses → anthropic SSE 状态机

没有统一的 `StandardResponse` 中间层,避免响应侧也遇到字段丢失问题。

## 代价

### 1. 公共逻辑需手动复用

tool 定义转换、finish_reason 映射、usage 归一化等逻辑会在三条路径中重复。缓解方式:提取 helper 函数到 `ccextra-core/convert/common.rs`。

### 2. 新出站协议扩展成本线性

加第四条出站路径(如 Gemini)需完整写一个新转换函数。但:
- ccextra 定位是实验项目,不以"支持 N 协议"为目标
- 当前三条已覆盖你全部上游,扩展需求不存在

### 3. 类型系统帮不上忙

在 `Value` 上 `value["messages"][0]["content"]` 这种路径操作,编译器不检查字段存在性,拼错字段名是运行时错误。缓解方式:
- 单元测试覆盖每个字段路径
- 从 CPA 真实请求体提取黄金语料做集成测试

## 替代方案

### A. 富中间类型 + raw passthrough 兜底

定义完整 Rust 结构体显式建模 anthropic 所有字段(image / cache_control / thinking),再给每个 content block 挂 `extra: Map<String, RawValue>` 存未建模字段。

**为何不选**:为一个不打算要的扩展性(多入站)付类型设计的税。`extra` 字段本身就是承认"类型建模跟不上协议演进",既然如此直接用 `Value` 更直白。

### B. 照抄 ai-gateway StandardRequest

移植 TS 定义到 Rust,自行补 image 变体和 cache_control 字段。

**为何不选**:补字段只解决一半问题。真正的坑在 rebuild:parse 后的 `Value` 重新序列化,key 顺序、空格、转义都可能变,cache 前缀字节稳定性无保证。直通路径必须绕过 rebuild,那"统一中间类型"的架构一致性已经破了。

## 影响

- `ccextra-core/convert/` 有三个独立转换函数,各 200-400 行(参考 CPA 规模)
- 响应侧 `ccextra-server/sse/` 有三个独立状态机
- 无 `types.rs` 定义 `StandardRequest` / `StandardResponse`
- DESIGN.md 第六节"核心类型"删除中间表示相关定义

## 遵从原则

- **YAGNI**(You Aren't Gonna Need It):不为假想的"未来支持多入站"做星型抽象
- **实证优先**:CPA 已验证 body-to-body 可行,ai-gateway 已暴露中间层有损
- **字节稳定优先**:cache 优化是立项动机,架构服从这个目标
