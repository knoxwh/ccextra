// 本地 token 估算
//
// Claude Code 的 /context 记账除 Skills 外所有类别都走 API 计数
// (POST /v1/messages/count_tokens),再回退到非流式 messages.create。
// 自定义 base URL(非 Anthropic 官方)没有原生 count_tokens 契约,一律
// 用本地估算:O200kBase tokenizer 把 system / messages / tools / tool_choice
// 的文本段收集起来 join 后计数,返回 {"input_tokens": N}。
//
// 本模块只做纯函数估算,不持有 IO。tokenizer 用 tiktoken-rs 的 o200k_base
// (include_str! 内嵌词表,离线可用)。

use serde::Serialize;
use serde_json::Value;
use std::sync::OnceLock;

/// O200kBase tokenizer 进程级缓存(惰性初始化)。
/// 词表 include_str! 内嵌,首次调用解析一次,后续请求复用。
static ENC: OnceLock<tiktoken_rs::CoreBPE> = OnceLock::new();

/// 求 input_tokens 计数的结果(Anthropic count_tokens 响应形状)
#[derive(Debug, Serialize)]
pub struct TokenCount {
    pub input_tokens: usize,
}

/// 对 Claude count_tokens 请求体做本地估算,返回 input_tokens。
pub fn count_claude_input_tokens(payload: &str) -> Result<TokenCount, String> {
    if payload.trim().is_empty() {
        return Err("count_tokens 请求体为空".into());
    }
    let root: Value = serde_json::from_str(payload)
        .map_err(|e| format!("count_tokens 请求体 JSON 解析失败: {e}"))?;

    let mut segments: Vec<String> = Vec::new();
    let mut overhead = 0usize; // 结构开销补偿(随词表偏差放大)
    let mut unscaled_tokens = 0usize; // 已按 Claude 口径的直接计费项(媒体/加密 blob),不放大

    collect_system(root.get("system"), &mut segments, &mut overhead);
    collect_messages(
        root.get("messages"),
        &mut segments,
        &mut overhead,
        &mut unscaled_tokens,
    );
    collect_tools(root.get("tools"), &mut segments);
    collect_tool_choice(root.get("tool_choice"), &mut segments);

    if segments.is_empty() && overhead == 0 && unscaled_tokens == 0 {
        return Ok(TokenCount { input_tokens: 0 });
    }

    let enc = if let Some(enc) = ENC.get() {
        enc
    } else {
        // 首次调用:解析内嵌词表。并发 set 失败无损(已有其他线程置入)。
        let enc = tiktoken_rs::o200k_base()
            .map_err(|e| format!("O200kBase tokenizer 初始化失败: {e}"))?;
        let _ = ENC.set(enc);
        ENC.get().expect("OnceLock 刚 set 必可读")
    };
    let joined = segments.join("\n");
    let base_count = enc.count_ordinary(&joined);

    // 补偿:文本段结构开销随词表偏差放大;直接计费项(媒体/加密 blob)原样累加
    let adjusted =
        (((base_count + overhead) as f64 * 1.12).ceil() as usize).saturating_add(unscaled_tokens);
    Ok(TokenCount {
        input_tokens: adjusted,
    })
}

/// system:字符串或 block 数组,取 text 字段
fn collect_system(system: Option<&Value>, segments: &mut Vec<String>, overhead: &mut usize) {
    let Some(system) = system else { return };
    match system {
        Value::String(s) => {
            push_trimmed(segments, s);
            *overhead += 3; // <system> 标签开销
        }
        Value::Array(blocks) => {
            for b in blocks {
                if let Some(t) = b.get("text").and_then(|v| v.as_str()) {
                    push_trimmed(segments, t);
                }
            }
            *overhead += 3 + blocks.len() * 2; // 标签 + 每块边界
        }
        _ => {}
    }
}

/// messages:role + content 递归
fn collect_messages(
    messages: Option<&Value>,
    segments: &mut Vec<String>,
    overhead: &mut usize,
    unscaled_tokens: &mut usize,
) {
    let Some(messages) = messages else { return };
    let Some(arr) = messages.as_array() else {
        return;
    };
    for msg in arr {
        if let Some(role) = msg.get("role").and_then(|v| v.as_str()) {
            push_trimmed(segments, role);
        }
        collect_content(msg.get("content"), segments, overhead, unscaled_tokens);
        *overhead += 4; // 每条消息 <message><role><content> 开销
    }
}

/// content:字符串 / 数组 / 对象块
fn collect_content(
    content: Option<&Value>,
    segments: &mut Vec<String>,
    overhead: &mut usize,
    unscaled_tokens: &mut usize,
) {
    let Some(content) = content else { return };
    match content {
        Value::String(s) => push_trimmed(segments, s),
        Value::Array(items) => {
            for item in items {
                collect_content(Some(item), segments, overhead, unscaled_tokens);
            }
        }
        Value::Object(_) => {
            let ty = content.get("type").and_then(|v| v.as_str()).unwrap_or("");
            match ty {
                "text" => {
                    if let Some(t) = content.get("text").and_then(|v| v.as_str()) {
                        push_trimmed(segments, t);
                    }
                    *overhead += 2; // <text> 块边界
                }
                "thinking" => {
                    if let Some(t) = content.get("thinking").and_then(|v| v.as_str()) {
                        push_trimmed(segments, t);
                    }
                    *overhead += 2;
                }
                "tool_use" | "server_tool_use" | "mcp_tool_use" => {
                    push_trimmed(
                        segments,
                        content.get("id").and_then(|v| v.as_str()).unwrap_or(""),
                    );
                    push_trimmed(
                        segments,
                        content.get("name").and_then(|v| v.as_str()).unwrap_or(""),
                    );
                    push_json(segments, content.get("input"));
                    *overhead += 5; // <tool_use><id><name><input> 开销
                }
                "tool_result" | "mcp_tool_result" => {
                    push_trimmed(
                        segments,
                        content
                            .get("tool_use_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or(""),
                    );
                    collect_content(content.get("content"), segments, overhead, unscaled_tokens);
                    *overhead += 3; // <tool_result> 边界
                }
                "image" => {
                    // 图片按中等分辨率估算 1600 token(官方 ~1000-2000,取 (w*h)/750 中位值)
                    *unscaled_tokens += 1600;
                    *overhead += 2; // <image> 块边界
                }
                "redacted_thinking" => {
                    // 加密 blob 不可解码,按 base64 密度粗估(~4 字符/token)
                    if let Some(d) = content.get("data").and_then(|v| v.as_str()) {
                        *unscaled_tokens += d.len() / 4;
                    }
                    *overhead += 2;
                }
                _ => {
                    // 未知块类型:兜底取 text
                    if let Some(t) = content.get("text").and_then(|v| v.as_str()) {
                        push_trimmed(segments, t);
                    }
                    *overhead += 2;
                }
            }
        }
        _ => {}
    }
}

/// tools:type/name/description/input_schema
fn collect_tools(tools: Option<&Value>, segments: &mut Vec<String>) {
    let Some(tools) = tools else { return };
    let Some(arr) = tools.as_array() else { return };
    for tool in arr {
        push_trimmed(
            segments,
            tool.get("type").and_then(|v| v.as_str()).unwrap_or(""),
        );
        push_trimmed(
            segments,
            tool.get("name").and_then(|v| v.as_str()).unwrap_or(""),
        );
        push_trimmed(
            segments,
            tool.get("description")
                .and_then(|v| v.as_str())
                .unwrap_or(""),
        );
        push_json(segments, tool.get("input_schema"));
    }
}

/// tool_choice:字符串或 {type,name}
fn collect_tool_choice(tool_choice: Option<&Value>, segments: &mut Vec<String>) {
    let Some(tool_choice) = tool_choice else {
        return;
    };
    match tool_choice {
        Value::String(s) => push_trimmed(segments, s),
        Value::Object(_) => {
            push_trimmed(
                segments,
                tool_choice
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or(""),
            );
            push_trimmed(
                segments,
                tool_choice
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or(""),
            );
        }
        _ => {}
    }
}

/// 收集后 trim,空段丢弃
fn push_trimmed(segments: &mut Vec<String>, value: &str) {
    let trimmed = value.trim();
    if !trimmed.is_empty() {
        segments.push(trimmed.to_string());
    }
}

/// 收集 JSON 值:字符串原样;对象/数组 compact 后收
fn push_json(segments: &mut Vec<String>, value: Option<&Value>) {
    let Some(value) = value else { return };
    match value {
        Value::String(s) => push_trimmed(segments, s),
        other => {
            let raw = other.to_string();
            push_trimmed(segments, &raw);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_count_string_content() {
        let body = json!({
            "model": "gpt-5.6-terra",
            "messages": [{"role": "user", "content": "你"},
                          {"role": "assistant", "content": [{"type": "text", "text": "hello world"}]}]
        });
        let r = count_claude_input_tokens(&body.to_string()).unwrap();
        assert!(r.input_tokens > 0);
    }

    #[test]
    fn test_count_empty_returns_zero() {
        let body = json!({"model": "x", "messages": []});
        let r = count_claude_input_tokens(&body.to_string()).unwrap();
        assert_eq!(r.input_tokens, 0);
    }

    #[test]
    fn test_count_null_content_still_counts_role() {
        // role 计入,null content 不贡献
        let body = json!({"model": "x", "messages": [{"role": "user", "content": null}]});
        let r = count_claude_input_tokens(&body.to_string()).unwrap();
        assert!(r.input_tokens > 0);
    }

    #[test]
    fn test_count_includes_tools() {
        let body = json!({
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [{"type": "function", "name": "get_weather", "description": "weather",
                       "input_schema": {"type": "object", "properties": {}}}]
        });
        let with_tools = count_claude_input_tokens(&body.to_string())
            .unwrap()
            .input_tokens;
        let without = count_claude_input_tokens(
            &json!({"messages": [{"role": "user", "content": "hi"}]}).to_string(),
        )
        .unwrap()
        .input_tokens;
        assert!(with_tools > without);
    }

    #[test]
    fn test_count_image_not_scaled_by_vocab_ratio() {
        // 纯图片 body:1600 按 Claude 口径原样计,不被 1.12 放大
        let body = json!({
            "model": "x",
            "messages": [{"role": "user", "content": [
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "..."}}
            ]}]
        });
        let r = count_claude_input_tokens(&body.to_string()).unwrap();
        // 1600 媒体 + 消息结构开销(~5),不含 1.12 放大
        assert!(
            r.input_tokens >= 1600 && r.input_tokens <= 1610,
            "得到 {}",
            r.input_tokens
        );
    }

    #[test]
    fn test_count_redacted_thinking_estimates_data_blob() {
        // redacted_thinking 按 data 字符数 /4 粗估,不按 2 token 计
        let data = "A".repeat(4000);
        let body = json!({
            "model": "x",
            "messages": [{"role": "assistant", "content": [
                {"type": "redacted_thinking", "data": data}
            ]}]
        });
        let r = count_claude_input_tokens(&body.to_string()).unwrap();
        assert!(
            r.input_tokens >= 1000,
            "4000 字符 blob 至少估 ~1000,得到 {}",
            r.input_tokens
        );
    }

    #[test]
    fn test_invalid_json_errors() {
        assert!(count_claude_input_tokens("{not json").is_err());
    }

    #[test]
    fn test_empty_body_errors() {
        assert!(count_claude_input_tokens("").is_err());
    }
}
