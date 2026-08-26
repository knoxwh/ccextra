// 截断 tool_result 内容，按上游客户端策略分流
// - GPT:   对齐 codex TruncationPolicy(10KB bytes,前缀截断)
// - Grok:  对齐 grok-build DEFAULT_TOOL_OUTPUT_BYTES(40KB bytes,2KB 预览 + footer)

use serde_json::Value;

/// 上游客户端策略:决定截断预算与 footer 形状
#[derive(Debug, Clone, Copy)]
pub enum UpstreamTruncation {
    /// codex 客户端:10KB bytes,前缀截断 + 简单后缀
    Codex,
    /// grok-build 客户端:40KB bytes,2KB 预览 + 指向完整输出的 footer
    GrokBuild,
}

impl UpstreamTruncation {
    /// 截断预算(字节)
    fn budget(&self) -> usize {
        match self {
            // codex 默认 TruncationPolicyConfig::bytes(10_000)
            UpstreamTruncation::Codex => 10_000,
            // grok-build DEFAULT_TOOL_OUTPUT_BYTES = 40_000
            UpstreamTruncation::GrokBuild => 40_000,
        }
    }

    /// 预览字节数(grok-build PREVIEW_SIZE;codex 无预览概念)
    fn preview_bytes(&self) -> Option<usize> {
        match self {
            UpstreamTruncation::Codex => None,
            UpstreamTruncation::GrokBuild => Some(2_000),
        }
    }
}

/// 截断 UTF-8 字符串到字节边界
/// 对齐 codex-rs/ext/skills/src/render.rs truncate_utf8_to_bytes
fn truncate_utf8_to_bytes(text: &str, max_bytes: usize) -> &str {
    if text.len() <= max_bytes {
        return text;
    }

    // 找到最后一个字符边界
    let mut boundary = max_bytes;
    while boundary > 0 && !text.is_char_boundary(boundary) {
        boundary -= 1;
    }

    &text[..boundary]
}

/// 截断单个 output 内容块(Responses 形状:input_text)
fn truncate_content_block(block: &mut Value, strategy: UpstreamTruncation) {
    match block.get("type").and_then(|t| t.as_str()) {
        Some("input_text") => {
            if let Some(text) = block.get_mut("text").and_then(|t| t.as_str()) {
                let fitted = fit_text(text, strategy);
                if fitted.len() != text.len() {
                    block["text"] = Value::String(fitted);
                }
            }
        }
        Some("input_image") => {
            // 图片块不截断，只截断文本
        }
        _ => {}
    }
}

/// 按策略截断文本
/// - Codex: 前缀截断 + 简单后缀(对齐 codex 工具输出截断)
/// - GrokBuild: 2KB 预览 + footer(对齐 grok-build truncate_with_preview)
fn fit_text(text: &str, strategy: UpstreamTruncation) -> String {
    let budget = strategy.budget();
    if text.len() <= budget {
        return text.to_string();
    }
    match strategy {
        UpstreamTruncation::Codex => {
            let truncated = truncate_utf8_to_bytes(text, budget);
            format!(
                "{truncated}...\n[truncated by ccextra, original {} bytes, kept {} bytes]",
                text.len(),
                truncated.len()
            )
        }
        UpstreamTruncation::GrokBuild => {
            // 对齐 grok-build truncate_with_preview:预览 + footer 报告总字节数
            let preview = strategy.preview_bytes().unwrap_or(budget);
            let preview = truncate_utf8_to_bytes(text, preview.min(text.len()));
            format!(
                "{preview}\n\n[Output truncated - {} bytes total]",
                text.len()
            )
        }
    }
}

/// 截断 tool_result 消息的 content 字段
fn truncate_tool_result_content(content: &mut Value, strategy: UpstreamTruncation) {
    match content {
        // string 形式的 content
        Value::String(text) => {
            let fitted = fit_text(text, strategy);
            if fitted.len() != text.len() {
                *text = fitted;
            }
        }
        // array 形式的 content blocks
        Value::Array(blocks) => {
            for block in blocks {
                truncate_content_block(block, strategy);
            }
        }
        _ => {}
    }
}

/// 截断所有 tool_result 输出(Responses 形状)
///
/// 处理转换后的 body:`input[]` 里的 `function_call_output` / `custom_tool_call_output`
/// 项,截断其 `output` 字段(string 或 input_text 块数组)。
///
/// 按上游客户端策略分流:
/// - GPT → Codex(10KB)
/// - Grok → GrokBuild(40KB + 2KB 预览)
pub fn truncate(body: &mut Value, strategy: UpstreamTruncation) -> Result<(), String> {
    let input = body
        .get_mut("input")
        .and_then(|i| i.as_array_mut())
        .ok_or("missing input array")?;

    for item in input {
        let is_tool_output = matches!(
            item.get("type").and_then(|t| t.as_str()),
            Some("function_call_output") | Some("custom_tool_call_output")
        );
        if !is_tool_output {
            continue;
        }
        if let Some(output) = item.get_mut("output") {
            truncate_tool_result_content(output, strategy);
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_budgets() {
        assert_eq!(UpstreamTruncation::Codex.budget(), 10_000);
        assert_eq!(UpstreamTruncation::GrokBuild.budget(), 40_000);
        assert_eq!(UpstreamTruncation::Codex.preview_bytes(), None);
        assert_eq!(UpstreamTruncation::GrokBuild.preview_bytes(), Some(2_000));
    }

    #[test]
    fn test_truncate_utf8_boundary() {
        let text = "hello 世界";
        assert_eq!(truncate_utf8_to_bytes(text, 100), "hello 世界");
        assert_eq!(truncate_utf8_to_bytes(text, 8), "hello "); // 避免切到 '世' 中间
    }

    #[test]
    fn test_truncate_string_output() {
        // Responses 形状:function_call_output.output 为 string
        let mut body = json!({
            "input": [
                {"type": "message", "role": "user", "content": "hi"},
                {"type": "function_call_output", "call_id": "1", "output": "x".repeat(20_000)}
            ]
        });

        truncate(&mut body, UpstreamTruncation::Codex).unwrap();

        let output = body["input"][1]["output"].as_str().unwrap();
        assert!(output.len() < 20_000);
        assert!(output.contains("[truncated by ccextra"));
    }

    #[test]
    fn test_grok_build_preview_footer() {
        // grok-build 策略:40KB 预算,超限保留 2KB 预览 + footer
        let mut body = json!({
            "input": [
                {"type": "function_call_output", "call_id": "1", "output": "x".repeat(50_000)}
            ]
        });

        truncate(&mut body, UpstreamTruncation::GrokBuild).unwrap();

        let output = body["input"][0]["output"].as_str().unwrap();
        // 2KB 预览 + footer,远小于原始 50KB
        assert!(output.len() < 3_000, "preview+footer: {}", output.len());
        assert!(output.contains("[Output truncated - 50000 bytes total]"));
    }

    #[test]
    fn test_grok_build_under_budget_preserved() {
        // grok-build 策略:40KB 内不截断
        let original = json!({
            "input": [
                {"type": "function_call_output", "call_id": "1", "output": "x".repeat(30_000)}
            ]
        });

        let mut body = original.clone();
        truncate(&mut body, UpstreamTruncation::GrokBuild).unwrap();

        assert_eq!(body, original);
    }

    #[test]
    fn test_truncate_input_text_block() {
        // Responses 形状:output 为 input_text 块数组
        let mut body = json!({
            "input": [
                {
                    "type": "function_call_output",
                    "call_id": "1",
                    "output": [
                        {"type": "input_text", "text": "a".repeat(20_000)}
                    ]
                }
            ]
        });

        truncate(&mut body, UpstreamTruncation::Codex).unwrap();

        let text = body["input"][0]["output"][0]["text"].as_str().unwrap();
        assert!(text.len() < 20_000);
        assert!(text.contains("[truncated by ccextra"));
    }

    #[test]
    fn test_preserve_small_content() {
        let original = json!({
            "input": [
                {"type": "function_call_output", "call_id": "1", "output": "small output"}
            ]
        });

        let mut body = original.clone();
        truncate(&mut body, UpstreamTruncation::Codex).unwrap();

        assert_eq!(body, original);
    }

    #[test]
    fn test_skip_non_tool_output_items() {
        // message / function_call 项不动
        let mut body = json!({
            "input": [
                {"type": "message", "role": "user", "content": "x".repeat(20_000)},
                {"type": "function_call", "call_id": "1", "name": "read", "arguments": "{}"}
            ]
        });

        let original = body.clone();
        truncate(&mut body, UpstreamTruncation::Codex).unwrap();

        assert_eq!(body, original);
    }

    #[test]
    fn test_truncate_custom_tool_call_output() {
        // custom_tool_call_output 同样截断
        let mut body = json!({
            "input": [
                {"type": "custom_tool_call_output", "call_id": "1", "output": "x".repeat(20_000)}
            ]
        });

        truncate(&mut body, UpstreamTruncation::Codex).unwrap();

        let output = body["input"][0]["output"].as_str().unwrap();
        assert!(output.len() < 20_000);
        assert!(output.contains("[truncated by ccextra"));
    }
}
