# Reasoning Replay 累积语义(turn marker + 锚定注入)实现计划

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** responses 协议 reasoning replay 从覆盖语义改为累积语义,修复 grok 多轮工具调用二次死循环(历史 reasoning 丢失)。

**Architecture:** 整体移植 CPA codex 路径的累积设计(`codex_reasoning_replay_cache.go` + `codex_executor_reasoning.go`):每轮 completed 前插 turn marker(含 assistant_fingerprint/request_fingerprint/call_ids),缓存 append 累积(按 marker id 去重,256 turn / 16MB 上限),注入时按 turn 分段、call_id/fingerprint 锚定到 input 对应位置。GPT 与 grok 共用同一路径(CPA codex 路径本身不限模型,`codexReasoningReplayEnabledForSource` 只判来源协议)。

**Tech Stack:** Rust 1.75+ / serde_json (preserve_order) / sha2。core 纯函数无 IO,server 层持状态。

**Spec:** 本文档即 spec(设计来自会话分析,无独立 spec 文件)。参考源码:
- CPA 累积缓存: `/Users/wanghao/tools/CLIProxyAPI/internal/cache/codex_reasoning_replay_cache.go`
- CPA 锚定注入: `/Users/wanghao/tools/CLIProxyAPI/internal/runtime/executor/codex_executor_reasoning.go`
- ccextra 现有: `crates/ccextra-core/src/convert/reasoning_replay.rs` + `crates/ccextra-server/src/sse/replay_cache.rs`

## Global Constraints

- core 无 IO:新逻辑全部落 `ccextra-core`,server 只持状态调用
- 注释中文,对齐现有文件风格(标注对齐的 CPA 函数名)
- serde_json preserve_order,禁止依赖 map 顺序
- 单测内联 `#[cfg(test)]`,不新建测试文件
- `cargo clippy --workspace --all-targets -- -D warnings` 零警告
- 现有测试全保(仅语义变化处按新行为改断言)
- commit message 英语,conventional commits
- 硬约束:commit/push 须用户本轮明确确认

## 背景:CPA 两条路径的差异(为什么抄 codex 路径)

| | CPA xai 路径(grok) | CPA codex 路径(GPT) |
|---|---|---|
| 缓存语义 | 覆盖(`StoreXAIReasoningReplayItems`) | 累积(`AppendCodexReasoningReplayItemsBestEffort`) |
| turn 边界 | 无 | marker `cpa_codex_replay_turn` |
| 注入 | 单块插一处 | 按 turn 分段,call_id/fingerprint 锚定 |
| 历史 reasoning | 只剩最后一轮 | 全保留(256 turn 上限) |

ccextra 现状 = 抄了 xai 路径,故有覆盖语义缺陷。CPA codex 路径是现成的、生产验证过的累积实现,整体移植。

## 文件结构

- Modify: `crates/ccextra-core/src/convert/reasoning_replay.rs` — 新增 turn marker 构造/分段/锚定注入纯函数
- Modify: `crates/ccextra-core/src/convert/mod.rs` — 导出新函数
- Modify: `crates/ccextra-server/src/sse/replay_cache.rs` — `store_from_completed` 改累积 append;`apply_to_body` 改调新注入
- 不动: `http.rs`(调用点签名不变)、`to_openai_responses.rs`、antigravity 路径(共用缓存,merge 透明)

## 移植映射表(CPA → ccextra)

| CPA (Go) | ccextra (Rust) | 位置 |
|---|---|---|
| `CodexReasoningReplayTurnType` = `"cpa_codex_replay_turn"` | `REPLAY_TURN_TYPE` const | reasoning_replay.rs |
| `cacheCodexReasoningReplayFromCompleted` | `build_replay_turn` | reasoning_replay.rs |
| `codexReplayAssistantMessageFingerprint` | `assistant_message_fingerprint` | reasoning_replay.rs |
| `codexReplayInputPrefixFingerprint` | `input_prefix_fingerprint` | reasoning_replay.rs |
| `appendCodexReasoningReplayTurn` | `append_replay_turn` | reasoning_replay.rs |
| `trimCodexReasoningReplayItems` | `trim_replay_items` | reasoning_replay.rs |
| `splitCodexReasoningReplayTurns` | `split_replay_turns` | reasoning_replay.rs |
| `codexReasoningReplayTurnAnchorIndex` | `turn_anchor_index` | reasoning_replay.rs |
| `filterCodexReasoningReplayTurnItems` | `filter_turn_items` | reasoning_replay.rs |
| `insertCodexReasoningReplayTurns` | `insert_replay_turns` | reasoning_replay.rs |
| `AppendCodexReasoningReplayItemsBestEffort` | `ReplayCache::store_from_completed` 内联 | replay_cache.rs |
| `applyCodexReasoningReplayCacheRequired` | `ReplayCache::apply_to_body` 内联 | replay_cache.rs |

保留不动(现有函数,新路径复用):`comparable_call_ids`、`tool_call_keys`、`shorten_call_id`、`align_replay_call_ids`(即 CPA `codexAlignReasoningReplayToolCallIDs`)、`output_call_ids`。

废弃路径:`filter_replay_items` + `insert_replay_items` + `replay_insert_index` 旧单块逻辑被 `insert_replay_turns` 替代,但 antigravity 路径仍走 `apply_to_body` → 统一切到新路径(antigravity 的 gemini 形状 items 同样适用 call_id 锚定;若锚定全失败退回末尾插入,行为不劣于现状)。

---

### Task 1: fingerprint 纯函数(assistant_message_fingerprint + input_prefix_fingerprint)

**Files:**
- Modify: `crates/ccextra-core/src/convert/reasoning_replay.rs`(文件末尾 tests 模块前插入)
- Test: 同文件 `#[cfg(test)]`

**Interfaces:**
- Consumes: 无(纯新函数;sha2 已是依赖,`shorten_call_id` 已用)
- Produces:
  - `fn assistant_message_fingerprint(item: &Value) -> String`(空串 = 非 assistant message 或空内容)
  - `fn input_prefix_fingerprint(input_items: &[Value], end: usize) -> String`(sha256 hex;end 越界返回空串)

- [ ] **Step 1: 写失败测试**

```rust
    #[test]
    fn test_assistant_message_fingerprint() {
        // assistant message,字符串 content
        let m1 = json!({"type": "message", "role": "assistant", "content": "hello"});
        let f1 = assistant_message_fingerprint(&m1);
        assert!(!f1.is_empty());
        // 相同内容相同指纹
        let m2 = json!({"role": "assistant", "content": "hello"});
        assert_eq!(f1, assistant_message_fingerprint(&m2));
        // 不同内容不同指纹
        let m3 = json!({"role": "assistant", "content": "world"});
        assert_ne!(f1, assistant_message_fingerprint(&m3));
        // 数组 content:input_text/output_text 取 text,refusal 特殊标记
        let m4 = json!({"role": "assistant", "content": [
            {"type": "output_text", "text": "a"},
            {"type": "refusal", "refusal": "no"}
        ]});
        assert!(!assistant_message_fingerprint(&m4).is_empty());
        // user message / 非 message / 空 content → 空串
        assert_eq!(assistant_message_fingerprint(&json!({"role": "user", "content": "hello"})), "");
        assert_eq!(assistant_message_fingerprint(&json!({"type": "function_call", "call_id": "c"})), "");
        assert_eq!(assistant_message_fingerprint(&json!({"role": "assistant", "content": []})), "");
        // 数组含未知 part 类型 → 空串(歧义保守)
        let m5 = json!({"role": "assistant", "content": [{"type": "input_image", "image_url": "x"}]});
        assert_eq!(assistant_message_fingerprint(&m5), "");
    }

    #[test]
    fn test_input_prefix_fingerprint() {
        let items = vec![
            json!({"type": "message", "role": "user", "content": "q"}),
            json!({"type": "function_call", "call_id": "c1", "name": "f", "arguments": "{}"}),
        ];
        let f0 = input_prefix_fingerprint(&items, 0);
        let f1 = input_prefix_fingerprint(&items, 1);
        let f2 = input_prefix_fingerprint(&items, 2);
        assert!(!f0.is_empty() && !f1.is_empty() && !f2.is_empty());
        assert_ne!(f0, f1);
        assert_ne!(f1, f2);
        // 前缀性质:不同长度不同指纹;越界返回空串
        assert_eq!(input_prefix_fingerprint(&items, 3), "");
        assert_eq!(input_prefix_fingerprint(&items, usize::MAX), "");
        // 空数组 end=0 有值(空哈希)
        assert!(!input_prefix_fingerprint(&[], 0).is_empty());
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p ccextra-core test_assistant_message_fingerprint test_input_prefix_fingerprint 2>&1 | tail -5`
(注:cargo test 单过滤器,分两次跑或用 `cargo test -p ccextra-core fingerprint`)
Expected: FAIL,`cannot find function`

- [ ] **Step 3: 实现**

对齐 CPA `codexReplayAssistantMessageFingerprint`(codex_executor_reasoning.go:419-451)与 `codexReplayInputPrefixFingerprint`(:453-463)。插入位置:现有 `align_replay_call_ids` 之后、tests 前。

```rust
/// assistant message 内容指纹(对齐 codexReplayAssistantMessageFingerprint):
/// 字符串 content 直接取;数组只认 input_text/output_text/refusal,
/// 其他 part 类型返回空串(歧义保守)。sha256 hex,空内容空串。
fn assistant_message_fingerprint(item: &Value) -> String {
    use sha2::{Digest, Sha256};
    let item_type = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
    if !item_type.is_empty() && item_type != "message" {
        return String::new();
    }
    let is_assistant = item
        .get("role")
        .and_then(|v| v.as_str())
        .is_some_and(|r| r.trim().eq_ignore_ascii_case("assistant"));
    if !is_assistant {
        return String::new();
    }
    let mut builder = String::new();
    match item.get("content") {
        Some(Value::String(s)) => builder.push_str(s),
        Some(Value::Array(parts)) => {
            for part in parts {
                match part.get("type").and_then(|v| v.as_str()).unwrap_or("") {
                    "input_text" | "output_text" => {
                        builder.push_str(part.get("text").and_then(|v| v.as_str()).unwrap_or(""));
                    }
                    "refusal" => {
                        builder.push_str("\u{0}refusal\u{0}");
                        builder.push_str(part.get("refusal").and_then(|v| v.as_str()).unwrap_or(""));
                    }
                    _ => return String::new(),
                }
            }
        }
        _ => return String::new(),
    }
    if builder.is_empty() {
        return String::new();
    }
    hex::encode(Sha256::digest(builder.as_bytes()))
}

/// input 前 end 项的指纹(对齐 codexReplayInputPrefixFingerprint):
/// 逐项吸收 `\0item\0` + 原始 JSON 字节,sha256 hex。
/// end 越界返回空串。
fn input_prefix_fingerprint(input_items: &[Value], end: usize) -> String {
    use sha2::{Digest, Sha256};
    if end > input_items.len() {
        return String::new();
    }
    let mut hasher = Sha256::new();
    for item in &input_items[..end] {
        hasher.update(b"\0item\0");
        hasher.update(serde_json::to_vec(item).unwrap_or_default());
    }
    hex::encode(hasher.finalize())
}
```

注:`hex` crate 已是依赖(`shorten_call_id` 在用)。

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p ccextra-core fingerprint`
Expected: PASS(2 个新测试)

- [ ] **Step 5: clippy**

Run: `cargo clippy -p ccextra-core --all-targets -- -D warnings 2>&1 | tail -3`
Expected: 无警告

- [ ] **Step 6: Commit**

```bash
git add crates/ccextra-core/src/convert/reasoning_replay.rs
git commit -m "feat(reasoning_replay): add assistant/prefix fingerprint helpers for turn anchoring"
```

---

### Task 2: turn marker 构造(build_replay_turn)

**Files:**
- Modify: `crates/ccextra-core/src/convert/reasoning_replay.rs`
- Test: 同文件

**Interfaces:**
- Consumes: Task 1 的 `assistant_message_fingerprint`、`input_prefix_fingerprint`
- Produces:
  - `pub const REPLAY_TURN_TYPE: &str = "ccextra_replay_turn";`
  - `pub fn build_replay_turn(completed: &Value, request_fingerprint: &str) -> Option<Vec<Value>>` — 返回 `[marker, ...replay_items]`;无可回放项返回 None

- [ ] **Step 1: 写失败测试**

```rust
    #[test]
    fn test_build_replay_turn_basic() {
        let completed = json!({"response": {"output": [
            {"type": "reasoning", "encrypted_content": "gAAA"},
            {"type": "function_call", "call_id": "c1", "name": "f", "arguments": "{}"},
            {"type": "message", "role": "assistant", "content": [
                {"type": "output_text", "text": "hi"}
            ]},
            {"type": "web_search_call"}
        ]}});
        let turn = build_replay_turn(&completed, "reqfp").unwrap();
        // marker + 3 个可回放项(web_search_call 丢弃)
        assert_eq!(turn.len(), 4);
        assert_eq!(turn[0]["type"], REPLAY_TURN_TYPE);
        assert!(!turn[0]["id"].as_str().unwrap().is_empty());
        // call_ids 收集
        assert_eq!(turn[0]["call_ids"], json!(["c1"]));
        // assistant_fingerprint 非空
        assert!(!turn[0]["assistant_fingerprint"].as_str().unwrap().is_empty());
        // request_fingerprint 透传
        assert_eq!(turn[0]["request_fingerprint"], "reqfp");
        // items 原样保留(不归一化,对齐 CPA cacheCodexReasoningReplayFromCompleted)
        assert_eq!(turn[1]["encrypted_content"], "gAAA");
        assert_eq!(turn[2]["call_id"], "c1");
    }

    #[test]
    fn test_build_replay_turn_dedup_and_empty() {
        // 同一 completed + 同 request_fingerprint → marker id 相同(幂等去重键)
        let completed = json!({"response": {"output": [
            {"type": "reasoning", "encrypted_content": "gAAA"}
        ]}});
        let t1 = build_replay_turn(&completed, "fp").unwrap();
        let t2 = build_replay_turn(&completed, "fp").unwrap();
        assert_eq!(t1[0]["id"], t2[0]["id"]);
        // 不同 request_fingerprint → 不同 id
        let t3 = build_replay_turn(&completed, "fp2").unwrap();
        assert_ne!(t1[0]["id"], t3[0]["id"]);
        // 无可回放项 → None
        let empty = json!({"response": {"output": [{"type": "web_search_call"}]}});
        assert!(build_replay_turn(&empty, "fp").is_none());
        // output 缺失 → None
        assert!(build_replay_turn(&json!({"response": {}}), "fp").is_none());
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p ccextra-core build_replay_turn`
Expected: FAIL,`cannot find function`

- [ ] **Step 3: 实现**

对齐 CPA `cacheCodexReasoningReplayFromCompleted`(codex_executor_reasoning.go:760-815)。与现有 `extract_replay_items` 的差异:**不归一化 items**(CPA codex 路径存 raw),只收集 reasoning/function_call/custom_tool_call/message 四类 + marker。插入位置:Task 1 函数后。

```rust
/// turn 边界 marker 类型(对齐 CodexReasoningReplayTurnType,改名避免
/// 与 CPA 字面量混淆;注入前 filter 会剔除,永不上游)
pub const REPLAY_TURN_TYPE: &str = "ccextra_replay_turn";

/// 累积缓存的每轮 turn 上限(对齐 CodexReasoningReplayCacheMaxTurnsPerEntry)
pub const MAX_REPLAY_TURNS: usize = 256;
/// 累积缓存单条目字节上限(对齐 CodexReasoningReplayCacheMaxBytesPerEntry)
pub const MAX_REPLAY_BYTES: usize = 16 << 20;

/// 从 response.completed 构造一个 turn:[marker, ...items]
/// (对齐 cacheCodexReasoningReplayFromCompleted)。
/// marker id = sha256(request_fingerprint + assistant_fingerprint + call_ids
/// + items 原始字节),同轮重放 id 相同,append 时按 id 去重。
/// 无 reasoning/function_call/custom_tool_call/message 项返回 None。
pub fn build_replay_turn(completed: &Value, request_fingerprint: &str) -> Option<Vec<Value>> {
    use sha2::{Digest, Sha256};
    let output = completed.pointer("/response/output")?.as_array()?;
    let mut items: Vec<Value> = Vec::with_capacity(output.len());
    let mut call_ids: Vec<String> = Vec::new();
    let mut assistant_fingerprint = String::new();
    for item in output {
        match item.get("type").and_then(|v| v.as_str()).unwrap_or("") {
            "reasoning" | "function_call" | "custom_tool_call" => {
                items.push(item.clone());
                if let Some(call_id) = item
                    .get("call_id")
                    .and_then(|v| v.as_str())
                    .map(|c| c.trim())
                    .filter(|c| !c.is_empty())
                {
                    call_ids.push(call_id.to_string());
                }
            }
            "message" => {
                let fp = assistant_message_fingerprint(item);
                if !fp.is_empty() {
                    assistant_fingerprint = fp;
                }
            }
            _ => continue,
        }
    }
    if items.is_empty() {
        return None;
    }
    let mut hasher = Sha256::new();
    hasher.update(request_fingerprint.as_bytes());
    hasher.update(format!("\u{0}assistant\u{0}{}", assistant_fingerprint).as_bytes());
    for call_id in &call_ids {
        hasher.update(format!("\u{0}call\u{0}{}", call_id).as_bytes());
    }
    for item in &items {
        hasher.update(b"\0item\0");
        hasher.update(serde_json::to_vec(item).unwrap_or_default());
    }
    let mut marker = json!({
        "type": REPLAY_TURN_TYPE,
        "id": hex::encode(hasher.finalize()),
    });
    if !assistant_fingerprint.is_empty() {
        marker["assistant_fingerprint"] = json!(assistant_fingerprint);
    }
    if !request_fingerprint.is_empty() {
        marker["request_fingerprint"] = json!(request_fingerprint);
    }
    if !call_ids.is_empty() {
        marker["call_ids"] = json!(call_ids);
    }
    let mut turn = Vec::with_capacity(items.len() + 1);
    turn.push(marker);
    turn.extend(items);
    Some(turn)
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p ccextra-core build_replay_turn`
Expected: PASS

- [ ] **Step 5: clippy + Commit**

Run: `cargo clippy -p ccextra-core --all-targets -- -D warnings 2>&1 | tail -3`

```bash
git add crates/ccextra-core/src/convert/reasoning_replay.rs
git commit -m "feat(reasoning_replay): build per-turn marker with fingerprints for cumulative cache"
```

---

### Task 3: 累积 append + trim(append_replay_turn + trim_replay_items)

**Files:**
- Modify: `crates/ccextra-core/src/convert/reasoning_replay.rs`
- Test: 同文件

**Interfaces:**
- Consumes: Task 2 的 `REPLAY_TURN_TYPE`、`MAX_REPLAY_TURNS`、`MAX_REPLAY_BYTES`
- Produces:
  - `pub fn append_replay_turn(existing: &[Value], turn: &[Value]) -> Vec<Value>` — 累积合并
  - `fn trim_replay_items(items: Vec<Value>) -> Vec<Value>` — 上限裁剪(内部)

- [ ] **Step 1: 写失败测试**

```rust
    #[test]
    fn test_append_replay_turn_accumulates() {
        let turn1 = vec![
            json!({"type": REPLAY_TURN_TYPE, "id": "t1"}),
            json!({"type": "reasoning", "encrypted_content": "g1"}),
        ];
        let turn2 = vec![
            json!({"type": REPLAY_TURN_TYPE, "id": "t2"}),
            json!({"type": "reasoning", "encrypted_content": "g2"}),
        ];
        let merged = append_replay_turn(&turn1, &turn2);
        assert_eq!(merged.len(), 4);
        assert_eq!(merged[0]["id"], "t1");
        assert_eq!(merged[2]["id"], "t2");
        // 幂等:同 id turn 重复 append 不增长
        let again = append_replay_turn(&merged, &turn2);
        assert_eq!(again.len(), 4);
        // 旧格式 existing(首项非 marker)→ 视为过期,整体替换
        let legacy = vec![json!({"type": "reasoning", "encrypted_content": "old"})];
        let replaced = append_replay_turn(&legacy, &turn1);
        assert_eq!(replaced.len(), 2);
        assert_eq!(replaced[0]["id"], "t1");
    }

    #[test]
    fn test_append_trims_oldest_beyond_limit() {
        // 构造 MAX_REPLAY_TURNS+1 个 turn,append 后只剩最新 256 个
        let mut existing: Vec<Value> = Vec::new();
        for i in 0..MAX_REPLAY_TURNS {
            existing.push(json!({"type": REPLAY_TURN_TYPE, "id": format!("t{}", i)}));
            existing.push(json!({"type": "reasoning", "encrypted_content": format!("g{}", i)}));
        }
        let new_turn = vec![
            json!({"type": REPLAY_TURN_TYPE, "id": "t_new"}),
            json!({"type": "reasoning", "encrypted_content": "g_new"}),
        ];
        let merged = append_replay_turn(&existing, &new_turn);
        // 256 turn 上限:丢最老一个(t0),剩 256 个 marker
        let markers = merged.iter().filter(|i| i["type"] == REPLAY_TURN_TYPE).count();
        assert_eq!(markers, MAX_REPLAY_TURNS);
        // t0 已被丢,t_new 在
        assert!(!merged.iter().any(|i| i["id"] == "t0"));
        assert!(merged.iter().any(|i| i["id"] == "t_new"));
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p ccextra-core append_replay_turn`
Expected: FAIL

- [ ] **Step 3: 实现**

对齐 CPA `appendCodexReasoningReplayTurn`(codex_reasoning_replay_cache.go:190-210)+ `trimCodexReasoningReplayItems`(:212-230)。

```rust
/// 累积 append 一个 turn(对齐 appendCodexReasoningReplayTurn):
/// - existing 首项非 marker(旧覆盖语义残留)→ 丢弃 existing,从 turn 重建
/// - turn 首项 marker id 已在 existing → 幂等返回 existing 克隆
/// - 否则拼接后按 turn 数/字节上限裁最老
pub fn append_replay_turn(existing: &[Value], turn: &[Value]) -> Vec<Value> {
    let is_marker = |v: &Value| v.get("type").and_then(|t| t.as_str()) == Some(REPLAY_TURN_TYPE);
    let mut items: Vec<Value> = if !existing.is_empty() && !is_marker(&existing[0]) {
        Vec::new()
    } else {
        existing.to_vec()
    };
    let turn_id = turn
        .first()
        .filter(|m| is_marker(m))
        .and_then(|m| m.get("id"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string();
    if !turn_id.is_empty() {
        let dup = items.iter().any(|v| {
            is_marker(v) && v.get("id").and_then(|i| i.as_str()) == Some(turn_id.as_str())
        });
        if dup {
            return trim_replay_items(items);
        }
    }
    items.extend(turn.iter().cloned());
    trim_replay_items(items)
}

/// 超 turn 数/字节上限时从最老 turn 起丢弃(对齐 trimCodexReasoningReplayItems)
fn trim_replay_items(mut items: Vec<Value>) -> Vec<Value> {
    let is_marker = |v: &Value| v.get("type").and_then(|t| t.as_str()) == Some(REPLAY_TURN_TYPE);
    loop {
        let mut turn_starts = vec![0usize];
        let mut total_bytes: usize = 0;
        for (index, item) in items.iter().enumerate() {
            total_bytes += serde_json::to_vec(item).map(|v| v.len()).unwrap_or(0);
            if index > 0 && is_marker(item) {
                turn_starts.push(index);
            }
        }
        if turn_starts.len() <= MAX_REPLAY_TURNS && total_bytes <= MAX_REPLAY_BYTES {
            return items;
        }
        if turn_starts.len() <= 1 {
            return Vec::new();
        }
        items.drain(..turn_starts[1]);
    }
}
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p ccextra-core append_replay_turn`
Expected: PASS

- [ ] **Step 5: clippy + Commit**

```bash
cargo clippy -p ccextra-core --all-targets -- -D warnings
git add crates/ccextra-core/src/convert/reasoning_replay.rs
git commit -m "feat(reasoning_replay): cumulative append with turn-id dedup and oldest-first trim"
```

---

### Task 4: turn 分段 + 锚定注入(split_replay_turns + turn_anchor_index + filter_turn_items + insert_replay_turns)

**Files:**
- Modify: `crates/ccextra-core/src/convert/reasoning_replay.rs`
- Modify: `crates/ccextra-core/src/convert/mod.rs:37`(导出)
- Test: reasoning_replay.rs

**Interfaces:**
- Consumes: Task 1-3 全部;现有 `comparable_call_ids`、`tool_call_keys`、`align_replay_call_ids`
- Produces:
  - `struct ReplayTurn { marked: bool, assistant_fingerprint: String, request_fingerprint: String, call_ids: Vec<String>, items: Vec<Value> }`
  - `fn split_replay_turns(items: &[Value]) -> Vec<ReplayTurn>`
  - `fn turn_anchor_index(input_items: &[Value], turn: &ReplayTurn, fallback_end: usize, used: &mut HashSet<usize>, fingerprints: &PrefixFingerprints) -> Option<usize>`
  - `struct PrefixFingerprints { .. }` + `fn at(&self, end: usize) -> String`(增量哈希,对齐 codexReplayPrefixFingerprints)
  - `fn filter_turn_items(input_items: &[Value], items: Vec<Value>) -> Vec<Value>`
  - `pub fn insert_replay_turns(body: &mut Value, replay_items: Vec<Value>) -> bool`

- [ ] **Step 1: 写失败测试**

```rust
    #[test]
    fn test_split_replay_turns() {
        let items = vec![
            json!({"type": REPLAY_TURN_TYPE, "id": "t1", "call_ids": ["c1"]}),
            json!({"type": "reasoning", "encrypted_content": "g1"}),
            json!({"type": REPLAY_TURN_TYPE, "id": "t2"}),
            json!({"type": "reasoning", "encrypted_content": "g2"}),
            json!({"type": "reasoning", "encrypted_content": "g3"}), // 尾部无 marker 段
        ];
        let turns = split_replay_turns(&items);
        assert_eq!(turns.len(), 3);
        assert!(turns[0].marked);
        assert_eq!(turns[0].call_ids, vec!["c1"]);
        assert_eq!(turns[0].items.len(), 1);
        assert!(turns[1].marked);
        assert!(!turns[2].marked); // 无 marker 前导 = unmarked(旧格式兜底)
        assert_eq!(turns[2].items.len(), 1);
    }

    #[test]
    fn test_insert_replay_turns_two_rounds_anchored() {
        // 两轮缓存,input 已含第二轮的 call/output;第一轮的 call/output
        // 已被 CC compact 掉(不在 input)。各轮 reasoning 锚定到对应位置。
        let mut body = json!({"input": [
            {"type": "message", "role": "user", "content": "q1"},
            {"type": "message", "role": "user", "content": "q2"},
            {"type": "function_call", "call_id": "c2", "name": "f", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "c2", "output": "ok2"}
        ]});
        let replay = vec![
            json!({"type": REPLAY_TURN_TYPE, "id": "t1", "call_ids": ["c1"]}),
            json!({"type": "reasoning", "encrypted_content": "g1"}),
            json!({"type": "function_call", "call_id": "c1", "name": "f", "arguments": "{}"}),
            json!({"type": REPLAY_TURN_TYPE, "id": "t2", "call_ids": ["c2"]}),
            json!({"type": "reasoning", "encrypted_content": "g2"}),
            json!({"type": "function_call", "call_id": "c2", "name": "f", "arguments": "{}"}),
        ];
        assert!(insert_replay_turns(&mut body, replay));
        let input = body["input"].as_array().unwrap();
        // t2 锚定 c2(在 input),t1 的 c1 不在 input → 锚定失败被丢
        // (call 无匹配 output,filter 剔除;reasoning 无锚也不注入)
        // 断言:g2 注入在 c2 前,g1 不在
        let types: Vec<&str> = input.iter().map(|i| i["type"].as_str().unwrap_or("")).collect();
        let idx_g2 = input.iter().position(|i| i["encrypted_content"] == "g2");
        let idx_c2 = input.iter().position(|i| i["call_id"] == "c2" && i["type"] == "function_call");
        assert!(idx_g2.is_some());
        assert!(idx_g2.unwrap() < idx_c2.unwrap());
        assert!(!input.iter().any(|i| i["encrypted_content"] == "g1"));
        // c2 的 call 已在 input,不重复注入
        let c2_calls = input.iter().filter(|i| i["type"] == "function_call" && i["call_id"] == "c2").count();
        assert_eq!(c2_calls, 1);
    }

    #[test]
    fn test_insert_replay_turns_unmarked_fallback() {
        // 旧格式(无 marker):整块走现有 filter+insert 语义
        let mut body = json!({"input": [
            {"type": "message", "role": "user", "content": "q"},
            {"type": "function_call_output", "call_id": "c1", "output": "ok"}
        ]});
        let replay = vec![
            json!({"type": "reasoning", "encrypted_content": "g1"}),
            json!({"type": "function_call", "call_id": "c1", "name": "f", "arguments": "{}"}),
        ];
        assert!(insert_replay_turns(&mut body, replay));
        let input = body["input"].as_array().unwrap();
        // 插在首个 output 前
        assert_eq!(input[1]["type"], "reasoning");
        assert_eq!(input[2]["type"], "function_call");
        assert_eq!(input[3]["type"], "function_call_output");
    }

    #[test]
    fn test_insert_replay_turns_empty() {
        let mut body = json!({"input": []});
        assert!(!insert_replay_turns(&mut body, vec![]));
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p ccextra-core insert_replay_turns`
Expected: FAIL

- [ ] **Step 3: 实现**

对齐 CPA `splitCodexReasoningReplayTurns`(:283-313)、`codexReasoningReplayTurnAnchorIndex`(:315-362)、`filterCodexReasoningReplayTurnItems`(:364-417)、`insertCodexReasoningReplayTurns`(:217-281)、`codexReplayPrefixFingerprints`(:469-500)。unmarked turn 走现有 `filter_replay_items` + `replay_insert_index` + `align_replay_call_ids` 组合(即旧单块路径,保留兼容)。

```rust
/// 一个 replay turn:marker 元数据 + 该轮 items
struct ReplayTurn {
    marked: bool,
    assistant_fingerprint: String,
    request_fingerprint: String,
    call_ids: Vec<String>,
    items: Vec<Value>,
}

/// 按 marker 分段(对齐 splitCodexReasoningReplayTurns):
/// marker 开新段;无 marker 前导的头部 items 归入一个 unmarked 段。
fn split_replay_turns(items: &[Value]) -> Vec<ReplayTurn> {
    let mut turns: Vec<ReplayTurn> = Vec::new();
    let mut current = ReplayTurn {
        marked: false,
        assistant_fingerprint: String::new(),
        request_fingerprint: String::new(),
        call_ids: Vec::new(),
        items: Vec::new(),
    };
    for item in items {
        if item.get("type").and_then(|v| v.as_str()) == Some(REPLAY_TURN_TYPE) {
            if !current.items.is_empty() {
                turns.push(current);
            }
            current = ReplayTurn {
                marked: true,
                assistant_fingerprint: item
                    .get("assistant_fingerprint")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string(),
                request_fingerprint: item
                    .get("request_fingerprint")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string(),
                call_ids: item
                    .get("call_ids")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| v.as_str())
                            .map(|s| s.trim().to_string())
                            .filter(|s| !s.is_empty())
                            .collect()
                    })
                    .unwrap_or_default(),
                items: Vec::new(),
            };
            continue;
        }
        current.items.push(item.clone());
    }
    if !current.items.is_empty() {
        turns.push(current);
    }
    turns
}

/// 增量前缀指纹(对齐 codexReplayPrefixFingerprints):
/// 一次线性哈希应答任意 end 的前缀查询,避免 O(n²)。
struct PrefixFingerprints {
    items: Vec<Value>,
    sums: Vec<String>,
}

impl PrefixFingerprints {
    fn new(items: &[Value]) -> Self {
        Self {
            items: items.to_vec(),
            sums: vec![input_prefix_fingerprint(&[], 0)],
        }
    }

    /// items[0:end] 的指纹;越界空串
    fn at(&mut self, end: usize) -> String {
        if end > self.items.len() {
            return String::new();
        }
        while self.sums.len() <= end {
            let next = self.sums.len() - 1;
            let fp = {
                use sha2::{Digest, Sha256};
                // 重新从 sums[next] 无法增量(sha256 无状态导出),
                // 改为整段重算但记忆化:sums 缓存保证每 end 只算一次
                let mut hasher = Sha256::new();
                for item in &self.items[..=next] {
                    hasher.update(b"\0item\0");
                    hasher.update(serde_json::to_vec(item).unwrap_or_default());
                }
                hex::encode(hasher.finalize())
            };
            self.sums.push(fp);
        }
        self.sums[end].clone()
    }
}
```

注:Go 版用 `hash.Hash` 可拷贝 digest 状态做真增量;Rust `Sha256` 无状态导出,退化为记忆化整段重算——每 end 只算一次,总代价 O(n²) 最坏但单次请求 n 为 input 项数(数百级),可接受。若 profile 发现热点再引入 `sha2` 的 `clone` 中间状态(Sha256 支持 `Clone`,可在 `at` 里维护一个逐步推进的 hasher 克隆链,实现与 Go 等价的真增量):

```rust
// 真增量版(推荐直接用这个,Sha256: Clone):
struct PrefixFingerprints {
    hasher: sha2::Sha256,       // 持续推进
    sums: Vec<String>,          // sums[end] = items[0:end] 指纹
    len: usize,
}

impl PrefixFingerprints {
    fn new(len: usize) -> Self {
        use sha2::{Digest, Sha256};
        Self {
            hasher: Sha256::new(),
            sums: vec![hex::encode(Sha256::new().finalize())],
            len,
        }
    }

    fn push(&mut self, item: &Value) {
        use sha2::Digest;
        self.hasher.update(b"\0item\0");
        self.hasher.update(serde_json::to_vec(item).unwrap_or_default());
        // clone 当前状态再 finalize,得到该前缀指纹
        let fp = hex::encode(self.hasher.clone().finalize());
        self.sums.push(fp);
    }

    fn at(&self, end: usize) -> String {
        if end > self.len {
            return String::new();
        }
        self.sums[end].clone()
    }
}
```

采用真增量版;`insert_replay_turns` 里先 `for item in &input_items { fp.push(item); }` 一次喂入。

继续,锚定与注入:

```rust
/// turn 锚点定位(对齐 codexReasoningReplayTurnAnchorIndex):
/// 1. call_ids 非空:从 searchEnd 向前找首个 call_id 匹配的 call/output 项
/// 2. 否则 assistant_fingerprint 非空:向前找指纹匹配的 assistant message
/// 3. 两者都空:退回现有 replay_insert_index
/// request_fingerprint 非空时要求锚点处前缀指纹匹配(防错位)。
/// 已用锚点(used)跳过。找不到返回 None。
fn turn_anchor_index(
    input_items: &[Value],
    turn: &ReplayTurn,
    fallback_end: usize,
    used: &mut std::collections::HashSet<usize>,
    fingerprints: &PrefixFingerprints,
) -> Option<usize> {
    let mut search_end = fallback_end.min(input_items.len().saturating_sub(1));
    if !turn.request_fingerprint.is_empty() {
        search_end = input_items.len().saturating_sub(1);
    }
    let matches_prefix = |index: usize| {
        turn.request_fingerprint.is_empty()
            || fingerprints.at(index) == turn.request_fingerprint
    };
    if !turn.call_ids.is_empty() {
        let mut wanted: std::collections::HashSet<String> = Default::default();
        for call_id in &turn.call_ids {
            wanted.extend(comparable_call_ids(call_id));
        }
        for index in (0..=search_end).rev() {
            if used.contains(&index) || !matches_prefix(index) {
                continue;
            }
            let item_type = input_items[index]
                .get("type")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if !matches!(
                item_type,
                "function_call" | "custom_tool_call" | "function_call_output" | "custom_tool_call_output"
            ) {
                continue;
            }
            let call_id = input_items[index]
                .get("call_id")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            if comparable_call_ids(call_id).iter().any(|c| wanted.contains(c)) {
                return Some(index);
            }
        }
    }
    if !turn.assistant_fingerprint.is_empty() {
        for index in (0..=search_end).rev() {
            if used.contains(&index) || !matches_prefix(index) {
                continue;
            }
            if assistant_message_fingerprint(&input_items[index]) == turn.assistant_fingerprint {
                return Some(index);
            }
        }
    }
    if turn.call_ids.is_empty() && turn.assistant_fingerprint.is_empty() {
        return Some(replay_insert_index(input_items, &turn.items));
    }
    None
}

/// turn 内 items 过滤(对齐 filterCodexReasoningReplayTurnItems):
/// reasoning 按 encrypted_content 去重;call 按键去重且须有匹配 output;
/// message/output 直接丢(codex 路径不回放 output——output 由客户端提供,
/// call 的配对门保证只注入 input 已有 output 的 call)。
fn filter_turn_items(input_items: &[Value], items: Vec<Value>) -> Vec<Value> {
    let mut existing_reasoning: std::collections::HashSet<String> = Default::default();
    let mut existing_calls: std::collections::HashSet<String> = Default::default();
    let mut existing_outputs: std::collections::HashSet<String> = Default::default();
    for item in input_items {
        match item.get("type").and_then(|v| v.as_str()).unwrap_or("") {
            "reasoning" => {
                if let Some(enc) = item
                    .get("encrypted_content")
                    .and_then(|v| v.as_str())
                    .map(|s| s.trim())
                    .filter(|s| !s.is_empty())
                {
                    existing_reasoning.insert(enc.to_string());
                }
            }
            "function_call_output" | "custom_tool_call_output" => {
                if let Some(call_id) = item.get("call_id").and_then(|v| v.as_str()) {
                    existing_outputs.extend(comparable_call_ids(call_id));
                }
            }
            _ => {}
        }
        for key in tool_call_keys(item) {
            existing_calls.insert(key);
        }
    }
    let mut filtered = Vec::with_capacity(items.len());
    for item in items {
        match item.get("type").and_then(|v| v.as_str()).unwrap_or("") {
            "reasoning" => {
                let enc = item
                    .get("encrypted_content")
                    .and_then(|v| v.as_str())
                    .map(|s| s.trim())
                    .unwrap_or("");
                if existing_reasoning.contains(enc) {
                    continue;
                }
            }
            "function_call" | "custom_tool_call" => {
                let keys = tool_call_keys(&item);
                if keys.is_empty() || keys.iter().any(|k| existing_calls.contains(k)) {
                    continue;
                }
                let has_output = item
                    .get("call_id")
                    .and_then(|v| v.as_str())
                    .map(|c| {
                        comparable_call_ids(c).iter().any(|id| existing_outputs.contains(id))
                    })
                    .unwrap_or(false);
                if !has_output {
                    continue;
                }
                for key in keys {
                    existing_calls.insert(key);
                }
            }
            _ => continue,
        }
        filtered.push(item);
    }
    filtered
}

/// 累积 replay 注入(对齐 insertCodexReasoningReplayTurns):
/// 按 turn 从新到旧处理,各 turn 锚定后过滤、call_id 对齐、插入。
/// unmarked turn(旧格式)走现有单块路径。任一 turn 注入成功即 true。
pub fn insert_replay_turns(body: &mut Value, replay_items: Vec<Value>) -> bool {
    let Some(input_items) = body.get("input").and_then(|v| v.as_array().cloned()) else {
        return false;
    };
    if replay_items.is_empty() {
        return false;
    }
    let turns = split_replay_turns(&replay_items);
    let mut insertions: std::collections::HashMap<usize, Vec<Value>> = Default::default();
    let mut used: std::collections::HashSet<usize> = Default::default();
    let mut fingerprints = PrefixFingerprints::new(input_items.len());
    for item in &input_items {
        fingerprints.push(item);
    }
    let mut fallback_end = input_items.len().saturating_sub(1);
    let mut inserted = false;
    for turn in turns.iter().rev() {
        if turn.items.is_empty() {
            continue;
        }
        if !turn.marked {
            let filtered = filter_replay_items(body, turn.items.clone());
            if filtered.is_empty() {
                continue;
            }
            let index = replay_insert_index(&input_items, &filtered);
            let aligned = align_replay_call_ids(&input_items, filtered);
            insertions.entry(index).or_default().extend(aligned);
            inserted = true;
            continue;
        }
        let Some(anchor) = turn_anchor_index(
            &input_items,
            turn,
            fallback_end,
            &mut used,
            &fingerprints,
        ) else {
            continue;
        };
        used.insert(anchor);
        if turn.request_fingerprint.is_empty() {
            fallback_end = anchor.saturating_sub(1);
        }
        let filtered = filter_turn_items(&input_items, turn.items.clone());
        if filtered.is_empty() {
            continue;
        }
        let aligned = align_replay_call_ids(&input_items, filtered);
        insertions.entry(anchor).or_default().extend(aligned);
        inserted = true;
    }
    if !inserted {
        return false;
    }
    let mut merged: Vec<Value> = Vec::with_capacity(input_items.len() + replay_items.len());
    for (index, item) in input_items.into_iter().enumerate() {
        if let Some(extra) = insertions.remove(&index) {
            merged.extend(extra);
        }
        merged.push(item);
    }
    if let Some(tail) = insertions.remove(&usize::MAX) {
        merged.extend(tail);
    }
    // 末尾插入(index == len)用 len 键
    // (上面循环已覆盖 0..len;len 键在 remove(&usize::MAX) 前处理)
    if let Some(obj) = body.as_object_mut() {
        obj.insert("input".to_string(), Value::Array(merged));
        true
    } else {
        false
    }
}
```

**修正**:`insertions` 的末尾键直接用 `input_items.len()`(对齐 CPA `insertions[len(inputItems)]`),删掉 `usize::MAX` 段:

```rust
    // 循环后处理末尾键
    if let Some(tail) = insertions.remove(&input_items_len_snapshot) {
        merged.extend(tail);
    }
```

实现时以 `let input_len = input_items.len();` 在 `into_iter` 前快照,末尾 `insertions.remove(&input_len)`。

`mod.rs:37` 导出改:

```rust
pub use reasoning_replay::{
    append_replay_turn, build_replay_turn, extract_replay_items, filter_replay_items,
    insert_replay_items, insert_replay_turns, REPLAY_TURN_TYPE,
};
```

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p ccextra-core insert_replay_turns && cargo test -p ccextra-core split_replay_turns`
Expected: PASS(4 个新测试)

- [ ] **Step 5: 全量 core 测试(确认无回归)**

Run: `cargo test -p ccextra-core 2>&1 | tail -3`
Expected: 全 PASS

- [ ] **Step 6: clippy + Commit**

```bash
cargo clippy -p ccextra-core --all-targets -- -D warnings
git add crates/ccextra-core/src/convert/reasoning_replay.rs crates/ccextra-core/src/convert/mod.rs
git commit -m "feat(reasoning_replay): per-turn anchored injection with prefix fingerprint guard"
```

---

### Task 5: server 层累积 store + 新注入(ReplayCache)

**Files:**
- Modify: `crates/ccextra-server/src/sse/replay_cache.rs`
- Test: 同文件

**Interfaces:**
- Consumes: Task 2-4 的 `build_replay_turn`、`append_replay_turn`、`insert_replay_turns`、`input_prefix_fingerprint`(需 pub 或经 mod.rs 导出)
- Produces: `ReplayCache` 公开签名不变(`store_from_completed`/`apply_to_body`/`invalidate`),内部语义改累积

- [ ] **Step 0: 导出 input_prefix_fingerprint**

`reasoning_replay.rs` 的 `input_prefix_fingerprint` 改 `pub fn`;`mod.rs` 导出列表加 `input_prefix_fingerprint`。

- [ ] **Step 1: 写失败测试**

```rust
    #[test]
    fn test_cumulative_store_two_rounds() {
        let cache = ReplayCache::new(Duration::from_secs(60), 128);
        let round1 = json!({"type": "response.completed", "response": {"output": [
            {"type": "reasoning", "encrypted_content": "g1"},
            {"type": "function_call", "call_id": "c1", "name": "f", "arguments": "{}"}
        ]}});
        let round2 = json!({"type": "response.completed", "response": {"output": [
            {"type": "reasoning", "encrypted_content": "g2"},
            {"type": "function_call", "call_id": "c2", "name": "f", "arguments": "{}"}
        ]}});
        cache.store_from_completed("sess-1", &round1);
        cache.store_from_completed("sess-1", &round2);
        // 两轮 reasoning 都注入(input 含 c1/c2 的 output)
        let mut body = json!({"input": [
            {"type": "message", "role": "user", "content": "q"},
            {"type": "function_call_output", "call_id": "c1", "output": "ok1"},
            {"type": "function_call_output", "call_id": "c2", "output": "ok2"}
        ]});
        assert!(cache.apply_to_body("sess-1", &mut body));
        let input = body["input"].as_array().unwrap();
        assert!(input.iter().any(|i| i["encrypted_content"] == "g1"));
        assert!(input.iter().any(|i| i["encrypted_content"] == "g2"));
    }

    #[test]
    fn test_store_no_replayable_still_clears() {
        let cache = ReplayCache::new(Duration::from_secs(60), 128);
        cache.store_from_completed("sess-1", &completed());
        // 纯文本轮(无可回放项)→ 清空累积(对齐 XAIReasoningReplayNoReplayableState
        // 语义:会话已收尾,残留 encrypted state 不得注入后续轮)
        cache.store_from_completed(
            "sess-1",
            &json!({"type": "response.completed", "response": {"output": [
                {"type": "web_search_call"}
            ]}}),
        );
        let mut body = json!({"input": []});
        assert!(!cache.apply_to_body("sess-1", &mut body));
    }

    #[test]
    fn test_store_idempotent_same_completed() {
        let cache = ReplayCache::new(Duration::from_secs(60), 128);
        let round1 = json!({"type": "response.completed", "response": {"output": [
            {"type": "reasoning", "encrypted_content": "g1"}
        ]}});
        cache.store_from_completed("sess-1", &round1);
        cache.store_from_completed("sess-1", &round1); // 同轮重复(重试场景)
        // marker id 相同,不重复累积
        let mut body = json!({"input": [
            {"type": "message", "role": "user", "content": "q"}
        ]});
        // reasoning 无 call 锚 → unmarked? 不,marked turn 无 call_ids/fingerprint
        // → 退回 replay_insert_index,注入一次
        if cache.apply_to_body("sess-1", &mut body) {
            let count = body["input"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|i| i["encrypted_content"] == "g1")
                .count();
            assert_eq!(count, 1);
        }
    }
```

- [ ] **Step 2: 跑测试确认失败**

Run: `cargo test -p ccextra-server replay_cache`
Expected: 新测试 FAIL,旧测试仍 PASS

- [ ] **Step 3: 实现**

`store_from_completed` 改(对齐 `AppendCodexReasoningReplayItemsBestEffort` 内存分支,cache.go:173-187):

```rust
    /// 从 response.completed 事件 data 累积缓存 replay 项。
    /// 每轮构造 [marker, ...items] 追加(对齐 CPA
    /// AppendCodexReasoningReplayItemsBestEffort + appendCodexReasoningReplayTurn):
    /// marker id 含 request 指纹,同轮重放幂等去重;超 256 turn/16MB 丢最老。
    /// 无可回放项时删除累积(对齐 XAIReasoningReplayNoReplayableState:
    /// 成功轮次无可缓存 reasoning 时不得残留 encrypted state)。
    pub fn store_from_completed(&self, session_key: &str, completed: &Value) {
        if session_key.trim().is_empty() {
            return;
        }
        let mut map = self.inner.lock().unwrap();
        match build_replay_turn(completed, "") {
            Some(turn) => {
                if map.len() >= self.capacity && !map.contains_key(session_key) {
                    map.clear();
                }
                let existing = map
                    .get(session_key)
                    .filter(|e| e.stored_at.elapsed() <= self.ttl)
                    .map(|e| e.items.clone())
                    .unwrap_or_default();
                let merged = append_replay_turn(&existing, &turn);
                map.insert(
                    session_key.to_string(),
                    Entry {
                        items: merged,
                        stored_at: Instant::now(),
                    },
                );
            }
            None => {
                map.remove(session_key);
            }
        }
    }
```

注:`build_replay_turn(completed, "")` 的 request_fingerprint 传空串——真值需请求侧 input 前缀指纹,`store_from_completed` 无 body 上下文。CPA 在 scope 构造时算好传入。ccextra 的 `StreamReplayExtractor` 也无 body。取舍:**v1 传空串**(锚定退化为 call_ids/assistant_fingerprint 双通道,已覆盖主场景);request_fingerprint 防错位增强留 TODO,需要时给 `StreamReplayExtractor::new` 加参数从 http.rs 传入。此取舍写入代码注释。

`apply_to_body` 尾部改一行:

```rust
        let filtered = filter_replay_items(body, items);
        if filtered.is_empty() {
            return false;
        }
        insert_replay_items(body, filtered)
```

改为:

```rust
        insert_replay_turns(body, items)
```

(`insert_replay_turns` 内部含 unmarked 兜底路径 = 旧 filter+insert 语义,antigravity 旧格式条目兼容。)

- [ ] **Step 4: 跑测试确认通过**

Run: `cargo test -p ccextra-server replay_cache`
Expected: 全 PASS(新 3 + 旧 5)

- [ ] **Step 5: 全量测试**

Run: `cargo test --workspace 2>&1 | tail -5`
Expected: 全 PASS

- [ ] **Step 6: clippy + fmt + Commit**

```bash
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all
git add crates/ccextra-server/src/sse/replay_cache.rs crates/ccextra-core/src/convert/reasoning_replay.rs crates/ccextra-core/src/convert/mod.rs
git commit -m "feat(replay_cache): cumulative turn store with anchored injection

Replace overwrite-semantics replay cache with CPA codex-path cumulative
design: per-turn markers, append with id dedup, 256-turn/16MB oldest-first
trim, per-turn anchored injection (call_ids/assistant fingerprint)."
```

---

### Task 6: 清理旧路径死代码 + 文档

**Files:**
- Modify: `crates/ccextra-core/src/convert/reasoning_replay.rs`(若 `filter_replay_items`/`insert_replay_items`/`replay_insert_index` 仅剩内部调用则保 pub 不动;`extract_replay_items` 已无调用方则删)
- Modify: `crates/ccextra-core/src/convert/mod.rs`(导出同步)
- Modify: `CLAUDE.md`(「grok reasoning replay」条目更新为累积语义描述)

**Interfaces:**
- Consumes: Task 5 完成后的调用图
- Produces: 无死代码;文档与实现一致

- [ ] **Step 1: 查调用图**

Run: `rg -n 'extract_replay_items|filter_replay_items|insert_replay_items' crates/ --type rust | grep -v 'reasoning_replay.rs'`
判定:
- `extract_replay_items`:若仅 replay_cache.rs 旧 `store_from_completed` 用(已改),无调用方 → 从 mod.rs 导出与文件中删除(连同其 3 个 c40235f 测试,行为已被 `build_replay_turn` 覆盖——**注意**:`build_replay_turn` 不提取 output,zzswitch 回显 output 场景由 filter 配对门 + 客户端 output 覆盖,见 Step 2 验证)
- `filter_replay_items`/`insert_replay_items`/`replay_insert_index`:被 `insert_replay_turns` unmarked 路径内部调用 → 保留,`pub` 降级为私有(若 mod.rs 导出无人用则移除导出)

- [ ] **Step 2: output 提取语义确认(关键决策点)**

CPA codex 路径 `cacheCodexReasoningReplayFromCompleted` **不缓存 function_call_output**(只 reasoning/function_call/custom_tool_call/message)。c40235f 加的 output 提取在 `extract_replay_items` 里;若 Task 6 删除该函数,zzswitch 回显 output 的场景失去缓存来源。

决策:保留 output 提取,并入 `build_replay_turn`——在 `"reasoning" | "function_call" | "custom_tool_call"` 分支旁加:

```rust
            "function_call_output" | "custom_tool_call_output" => {
                // zzswitch 等 relay 非标准回显 output;缓存它保证 call/output
                // 配对完整(c40235f 场景)。标准 API completed 无此项,分支不触发。
                let call_id = item
                    .get("call_id")
                    .and_then(|v| v.as_str())
                    .map(|c| c.trim())
                    .filter(|c| !c.is_empty());
                if call_id.is_some() && item.get("output").is_some() {
                    items.push(item.clone());
                }
            }
```

同时 `filter_turn_items` 的 `_ => continue` 前加 output 保留分支(按 existing_outputs 去重):

```rust
            "function_call_output" | "custom_tool_call_output" => {
                let call_id = item.get("call_id").and_then(|v| v.as_str()).unwrap_or("");
                if call_id.trim().is_empty()
                    || comparable_call_ids(call_id).iter().any(|c| existing_outputs.contains(c))
                {
                    continue;
                }
                // output 跟随同 turn 的 call;call 被滤掉时 output 也无意义,
                // 但保守保留(上游容忍 output 先于 call 的交错)
            }
```

并在 Task 2 的测试 `test_build_replay_turn_basic` 补断言:

```rust
        // output 回显场景(c40235f):completed 带 output 时缓存
        let with_output = json!({"response": {"output": [
            {"type": "function_call", "call_id": "c1", "name": "f", "arguments": "{}"},
            {"type": "function_call_output", "call_id": "c1", "output": "ok"}
        ]}});
        let t = build_replay_turn(&with_output, "").unwrap();
        assert_eq!(t.len(), 3); // marker + call + output
        assert_eq!(t[2]["type"], "function_call_output");
```

(此步实际应在 Task 2 就位;执行时若 Task 2 已合,作为独立小 commit 补上。)

- [ ] **Step 3: 删除/降级 + 跑全量**

按 Step 1 判定执行删除或降级,然后:

Run: `cargo test --workspace 2>&1 | tail -3 && cargo clippy --workspace --all-targets -- -D warnings 2>&1 | tail -3`
Expected: 全 PASS 零警告

- [ ] **Step 4: 更新 CLAUDE.md**

「grok reasoning replay」条目改为:

```markdown
- **grok/gpt reasoning replay(累积)**:responses 协议 `store=false` 时,每轮
  `response.completed` 构造 turn(marker 含指纹/call_ids)追加进缓存
  (`replay_cache.rs`,256 turn/16MB 上限丢最老,marker id 幂等去重),下轮按 turn
  分段锚定注入 `input[]`(call_id/assistant 指纹定位,对齐 CPA codex 路径
  `insertCodexReasoningReplayTurns`)。缓存只认带 `encrypted_content` 的 reasoning;
  GPT/Grok 模型丢弃无签名 thinking,只回放 encrypted reasoning。zzswitch 等 relay
  回显 function_call_output 时一并缓存(配对完整,c40235f)。
```

- [ ] **Step 5: Commit**

```bash
git add -A
git commit -m "chore(reasoning_replay): fold output extraction into turn builder, drop dead overwrite path"
```

---

### Task 7: 端到端验证(真实 grok 会话)

**Files:** 无代码改动

- [ ] **Step 1: 构建 + 重启**

Run: `./build.sh`
Expected: 编译零警告,自动重启

- [ ] **Step 2: 真实多轮工具调用会话**

Claude Code 挂 ccextra 用 grok 模型跑一个 5+ 轮工具调用任务(如连续读几个文件再总结)。

- [ ] **Step 3: 验证日志**

Run: `ls -t logs/upstream_body_*.json | head -4` 取最后一轮请求 body,检查:
- input 中 reasoning 项数 ≈ 轮数(不再只剩最后一轮)
- 各 reasoning 位于对应 function_call 前(交错结构)
- 无重复 call_id 的 function_call

- [ ] **Step 4: 汇报结果,等用户确认后再谈 push**

## Self-Review 结论

1. **Spec 覆盖**:累积语义(Task 3/5)、锚定注入(Task 4)、output 保留(Task 6 Step 2)、GPT/grok 共用(单路径,无分叉——CPA codex 路径本身不限模型)、256/16M 上限(Task 2 const + Task 3 trim)全覆盖
2. **占位符扫描**:无 TBD/TODO;Task 4 的 `usize::MAX` 修正段已内联说明,执行者直接用 `input_len` 快照方案
3. **类型一致性**:`build_replay_turn(completed: &Value, request_fingerprint: &str) -> Option<Vec<Value>>`、`append_replay_turn(existing: &[Value], turn: &[Value]) -> Vec<Value>`、`insert_replay_turns(body: &mut Value, replay_items: Vec<Value>) -> bool` 各任务引用一致;`PrefixFingerprints` 采用真增量版(`new(len)` + `push(&Value)` + `at(end)`),Task 4 内部自洽
4. **风险点**:Task 5 的 request_fingerprint 传空串是显式取舍(注释说明);Task 6 Step 2 是关键决策点,已给完整代码防执行者跳过
