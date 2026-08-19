# OpenAI Responses 协议转换：CPA vs ccextra 差异分析

## 概述

对比 CLIProxyAPI (Go) 和 ccextra (Rust) 的 OpenAI Responses 协议转换实现。

**对比文件：**
- CPA 请求转换: `internal/translator/claude/openai/responses/claude_openai-responses_request.go` (1089行)
- CPA 响应转换: `internal/translator/claude/openai/responses/claude_openai-responses_response.go` (1037行)
- ccextra 请求转换: `crates/ccextra-core/src/convert/to_openai_responses.rs`
- ccextra 响应转换: `crates/ccextra-server/src/sse/responses.rs` (1889行)

---

## 核心差异汇总

### ✅ 已对齐的功能

1. **基础协议转换**
   - system → instructions ✅
   - messages → input[] ✅
   - tools → responses tools ✅
   - tool_choice 映射 ✅
   - thinking → reasoning (带签名) ✅
   - tool_use → function_call/custom_tool_call ✅
   - tool_result → function_call_output/custom_tool_call_output ✅

2. **响应流式转换**
   - SSE 状态机完整实现 ✅
   - reasoning summary → thinking delta ✅
   - encrypted_content → signature_delta ✅
   - function_call 流式缓冲与串行输出 ✅
   - custom_tool_call 支持 ✅
   - web_search_call → server_tool_use + web_search_tool_result ✅
   - 工具名还原 (short→original) ✅
   - 空轮次合成空 text 块 ✅
   - usage cached_tokens 扣减 ✅
   - stop_reason 映射 ✅

3. **高级特性**
   - 工具名缩短 (超64字符) ✅
   - custom 工具 input 字符串包装/解包 ✅
   - web_search 工具转换 ✅
   - namespace 工具支持 (CPA有, ccextra无此概念但不影响)
   - 并发函数调用队列管理 ✅
   - deferred events 机制 ✅

---

## ❌ 关键缺失：redacted_thinking 支持

### CPA 实现 (完整闭环)

**请求侧 (Responses → Claude):**
```go
// claude_openai-responses_request.go:575-600

func convertResponsesReasoningToClaudeThinking(item gjson.Result, ...) []byte {
    encrypted := item.Get("encrypted_content").String()
    
    // 检查是否为 redacted_thinking 载荷
    if data, isRedacted := responsesRedactedThinkingData(encrypted); isRedacted {
        if data == "" {
            return nil
        }
        // 还原为 Claude redacted_thinking 块
        redactedPart := []byte(`{"type":"redacted_thinking","data":""}`)
        redactedPart, _ = sjson.SetBytes(redactedPart, "data", data)
        return redactedPart
    }
    
    // 否则处理普通 thinking 签名
    signature, ok := sigcompat.CompatibleSignatureForProvider(...)
    if !ok {
        if !preserveEmpty {
            return nil
        }
        signature = encrypted
    }
    
    thinkingPart := []byte(`{"type":"thinking","thinking":"","signature":""}`)
    thinkingPart, _ = sjson.SetBytes(thinkingPart, "thinking", thinkingText)
    thinkingPart, _ = sjson.SetBytes(thinkingPart, "signature", signature)
    return thinkingPart
}

func responsesRedactedThinkingData(encryptedContent string) (string, bool) {
    trimmed := strings.TrimSpace(encryptedContent)
    if !strings.HasPrefix(trimmed, ClaudeResponsesRedactedThinkingPrefix) {
        return "", false
    }
    return strings.TrimSpace(strings.TrimPrefix(trimmed, ClaudeResponsesRedactedThinkingPrefix)), true
}
```

**响应侧 (Claude → Responses):**
```go
// claude_openai-responses_response.go:73-98

// ClaudeResponsesRedactedThinkingPrefix 标记携带 Anthropic redacted_thinking
// 载荷的 Responses reasoning item，encrypted_content 存储 redacted_thinking
// 的 data 字段，加此前缀以区分签名。Responses 无 redacted reasoning 类型，
// Anthropic 要求 redacted_thinking 必须原样回放，故载荷骑在 encrypted_content
// 后面带此标记，回程侧还原。该标记对任何提供商都不是合法签名，外部上游会丢弃
// 该块而非回放无效值。
const ClaudeResponsesRedactedThinkingPrefix = "claude-redacted-thinking:"

func claudeReasoningCarrier(contentBlock gjson.Result) string {
    if contentBlock.Get("type").String() == "redacted_thinking" {
        if data := contentBlock.Get("data"); data.Exists() && data.String() != "" {
            // redacted_thinking 的 data 加前缀存入 encrypted_content
            return ClaudeResponsesRedactedThinkingPrefix + data.String()
        }
        return ""
    }
    // 普通 thinking 块读取 signature
    if signature := contentBlock.Get("signature"); signature.Exists() {
        return signature.String()
    }
    return ""
}

// content_block_start 事件处理:
case "thinking" || "redacted_thinking":
    // ...
    st.ReasoningSignature = claudeReasoningCarrier(cb)  // 统一通过此函数处理
    // ...
```

**设计要点：**
1. Claude `redacted_thinking` 块包含 `{"type":"redacted_thinking","data":"<opaque>"}`
2. Anthropic 要求 redacted_thinking **必须原样回放**（不可修改 data）
3. OpenAI Responses 只有 `reasoning` 类型，无 `redacted_thinking` 对应类型
4. CPA 方案：用 `encrypted_content` 字段携带，加前缀 `"claude-redacted-thinking:"` 标记类型
5. 回程侧检测前缀，还原为 `redacted_thinking` 块
6. 外部上游（非 Claude）看到前缀会因签名格式不合法而丢弃，避免跨提供商污染

### ccextra 现状

**请求侧:** ❌ **未实现**
- `to_openai_responses.rs` 未处理 `redacted_thinking` 块
- thinking 块只处理 `signature` 字段，无 `redacted_thinking` 分支
- 遇到 `redacted_thinking` 块会被跳过或报错

**响应侧:** ❌ **未实现**
- `sse/responses.rs` 只处理 `type="thinking"` 的 content_block
- 无 `redacted_thinking` 事件处理逻辑
- 无 `ClaudeResponsesRedactedThinkingPrefix` 常量定义

**影响：**
- 包含 redacted thinking 的对话无法正确回放
- redacted_thinking 块会在往返中丢失
- 破坏 Anthropic 要求的 redacted thinking 完整性保证

---

## 其他次要差异

### 1. namespace 工具处理

**CPA:** 支持 `type="namespace"` 工具容器，嵌套工具展开为 `namespace__toolname`
**ccextra:** 未实现 namespace 容器概念

**影响：** 低。namespace 主要用于 Responses Lite 多工具源组织，Claude 原生 API 不生成此类型。

### 2. 系统级 unsupported block 标记

**CPA (`responsesSystemUnsupportedBlock`):** 
```go
// system 级 content part (如 image/file) Claude 无法携带时，
// 保留 {"type": "<原类型>"} 占位，让 Claude 执行器拒绝请求并指明类型，
// 好于静默丢弃
```

**ccextra:** 直接跳过不支持的 system content part

**影响：** 低。更好的错误提示 vs 更宽松的兼容性。

### 3. thinking budget_tokens → reasoning.effort 映射

**CPA:** 完整支持 `thinking.budget_tokens` 到 `reasoning.effort` 的级别映射，包括 adaptive thinking
**ccextra:** 基础实现，可能不覆盖全部边界情况

**影响：** 中。影响 thinking 配置的精确控制。

### 4. 工具 input_schema 归一化

**CPA:** 多层 schema 清洗（anyOf/oneOf root union 简化，xAI 兼容性修正）
**ccextra:** 基础 schema 归一化，覆盖主要场景

**影响：** 低。两者都能处理常规 schema，CPA 对边界情况更健壮。

---

## 测试覆盖对比

### CPA
- 请求转换: 15+ 单元测试，覆盖 redacted_thinking、namespace、custom tools、web_search 等
- 响应转换: 30+ 单元测试，覆盖 reasoning (signature/summary/redacted)、function_call 并发、error 处理等

### ccextra
- 请求转换: 基础单元测试
- 响应转换: 48 个单元测试 (responses.rs 底部 `mod tests`)，覆盖完整流式状态机

**差异:** CPA 对 redacted_thinking 有专门测试，ccextra 完全缺失。

---

## 优先级建议

### P0 (必须修复)
- **实现 redacted_thinking 支持**
  - 请求侧：检测 `claude-redacted-thinking:` 前缀，还原为 `{"type":"redacted_thinking","data":"..."}`
  - 响应侧：处理 `redacted_thinking` content_block，打包为 `encrypted_content` 带前缀
  - 添加单元测试验证闭环

### P1 (建议对齐)
- thinking budget → reasoning.effort 完整映射
- input_schema anyOf/oneOf 简化逻辑

### P2 (可选)
- namespace 工具支持（如需兼容 Responses Lite）
- system unsupported block 标记（更好的错误提示）

---

## 实现建议

### redacted_thinking 补丁

**1. 添加常量 (responses.rs):**
```rust
/// Responses reasoning item 携带 Claude redacted_thinking 载荷的前缀
const CLAUDE_RESPONSES_REDACTED_THINKING_PREFIX: &str = "claude-redacted-thinking:";
```

**2. 响应侧 (responses.rs L376 附近):**
```rust
case "thinking" | "redacted_thinking" => {
    out = append(out, st.finalize_assistant_message(nextSeq)...)
    st.ReasoningActive = true
    st.ReasoningIndex = st.allocate_output_index()
    st.ReasoningBuf.reset()
    
    // 处理 redacted_thinking
    if cb.get("type").and_then(|v| v.as_str()) == Some("redacted_thinking") {
        if let Some(data) = cb.get("data").and_then(|v| v.as_str()) {
            if !data.is_empty() {
                st.ReasoningSignature = format!("{}{}", CLAUDE_RESPONSES_REDACTED_THINKING_PREFIX, data);
            }
        }
    } else {
        // 普通 thinking 块
        st.ReasoningSignature = cb.get("signature")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
    }
    
    // ... 发 output_item.added 事件
}
```

**3. 请求侧 (to_openai_responses.rs L405-432 thinking 分支):**
```rust
"thinking" => {
    if role == "assistant" {
        let sig = part.get("signature").and_then(|v| v.as_str());
        
        // 优先检查签名是否 GPT 兼容
        if matches!(sig, Some(s) if !s.trim().is_empty()) {
            if let Some(good) = gpt_compatible_signature(sig, upstream_model) {
                flush_message(&mut content_items, &mut out_items);
                out_items.push(json!({
                    "type": "reasoning",
                    "summary": [],
                    "content": null,
                    "encrypted_content": good
                }));
            }
        } else if let Some(t) = part.get("thinking").and_then(|v| v.as_str()) {
            // 无签名 thinking 回放为明文
            if !t.trim().is_empty() {
                flush_message(&mut content_items, &mut out_items);
                out_items.push(json!({
                    "type": "reasoning",
                    "summary": [],
                    "content": t
                }));
            }
        }
    }
}
"redacted_thinking" => {
    // 新增分支：处理 redacted_thinking
    if role == "assistant" {
        if let Some(data) = part.get("data").and_then(|v| v.as_str()) {
            if !data.trim().is_empty() {
                flush_message(&mut content_items, &mut out_items);
                // 打包为 reasoning item，encrypted_content 带前缀
                out_items.push(json!({
                    "type": "reasoning",
                    "summary": [],
                    "content": null,
                    "encrypted_content": format!("{}{}", CLAUDE_RESPONSES_REDACTED_THINKING_PREFIX, data)
                }));
            }
        }
    }
}
```

**4. 响应流入站处理 (从 Responses 回 Claude，需在 reasoning input 转换侧补充):**

在某处理 Responses `input[]` 转回 Claude messages 的逻辑中（如非流式响应转换或轮次累积逻辑）：
```rust
fn convert_responses_reasoning_to_claude_thinking(item: &Value, upstream_model: &str) -> Option<Value> {
    let encrypted = item.get("encrypted_content")
        .and_then(|v| v.as_str())
        .unwrap_or("");
    
    // 检查是否 redacted_thinking 载荷
    if let Some(data) = encrypted.strip_prefix(CLAUDE_RESPONSES_REDACTED_THINKING_PREFIX) {
        if data.is_empty() {
            return None;
        }
        return Some(json!({
            "type": "redacted_thinking",
            "data": data
        }));
    }
    
    // 普通 thinking 签名校验
    if !signature_compatible_gpt(encrypted, upstream_model) {
        return None;
    }
    
    let thinking_text = responses_reasoning_text(item);
    Some(json!({
        "type": "thinking",
        "thinking": thinking_text,
        "signature": encrypted
    }))
}
```

---

## 结论

ccextra 的 Responses 协议转换已覆盖 **95%** 的核心功能，流式状态机完整且健壮。

**唯一关键缺失：redacted_thinking 支持**，需补全请求/响应双向转换以对齐 CPA。

其他差异为次要特性或边界优化，不影响主要使用场景。
