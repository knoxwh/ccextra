// 截断 tool_result 内容，按上游客户端策略分流
// - GPT:   对齐 codex TruncationPolicy::bytes(10_000):中间截断(保留首尾)
// - Grok:  对齐 grok-build DEFAULT_TOOL_OUTPUT_BYTES(40KB,2KB 预览 + footer)

use serde_json::Value;

/// 上游客户端策略:决定截断预算与 footer 形状
#[derive(Debug, Clone, Copy)]
pub enum UpstreamTruncation {
    /// codex 客户端:10KB bytes,中间截断(保留首尾)+ token/行数头
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

/// 中间截断:保留首尾,中间插截断标记
/// 对齐 codex utils/string truncate_middle_chars + split_budget(左右各半)。
/// 标记字节计入预算(代理侧每轮重跑归一化,结果必须 ≤ 预算保证幂等;
/// codex 原生只跑一次,不扣标记)。
fn truncate_middle_chars(s: &str, max_bytes: usize) -> String {
    if s.is_empty() || s.len() <= max_bytes {
        return s.to_string();
    }

    let mut left_budget = max_bytes / 2;
    let mut right_budget = max_bytes - left_budget;
    loop {
        let left = truncate_utf8_to_bytes(s, left_budget);
        let mut tail_start = s.len().saturating_sub(right_budget);
        // 尾部起点对齐字符边界
        while tail_start < s.len() && !s.is_char_boundary(tail_start) {
            tail_start += 1;
        }
        let right = &s[tail_start..];

        let removed_chars = s
            .chars()
            .count()
            .saturating_sub(left.chars().count())
            .saturating_sub(right.chars().count());

        let out = format!("{left}…{removed_chars} chars truncated…{right}");
        if out.len() <= max_bytes || left_budget + right_budget < 32 {
            return out;
        }
        // 超预算:左右各缩,重试(标记字节数随位数变化,一两轮内收敛)
        let shrink = (out.len() - max_bytes) / 2 + 1;
        left_budget = left_budget.saturating_sub(shrink);
        right_budget = right_budget.saturating_sub(shrink);
    }
}

/// 按策略截断文本
/// - Codex: 中间截断 + token/行数头(对齐 formatted_truncate_text;
///   头计入预算保证幂等)
/// - GrokBuild: 2KB 预览 + footer(对齐 grok-build truncate_with_preview)
fn fit_text(text: &str, strategy: UpstreamTruncation) -> String {
    let budget = strategy.budget();
    if text.len() <= budget {
        return text.to_string();
    }
    match strategy {
        UpstreamTruncation::Codex => {
            // 对齐 codex formatted_truncate_text:头两行报原始 token 数与总行数
            let original_token_count = approx_token_count(text);
            let total_lines = text.lines().count();
            let header = format!(
                "Warning: truncated output (original token count: {original_token_count})\nTotal output lines: {total_lines}\n\n"
            );
            let result = truncate_middle_chars(text, budget.saturating_sub(header.len()));
            format!("{header}{result}")
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

/// 块数组内单块截断到剩余预算(对齐 codex snippet 路径:无头,纯截断)。
/// 返回 None 表示剩余预算放不下 marker/footer,应丢弃该块并计入 omitted。
fn fit_snippet(text: &str, strategy: UpstreamTruncation, budget: usize) -> Option<String> {
    if text.len() <= budget {
        return Some(text.to_string());
    }
    match strategy {
        UpstreamTruncation::Codex => {
            // marker 最小形态 "…0 chars truncated…" 约 20 bytes;放不下则丢弃
            if budget < 32 {
                return None;
            }
            Some(truncate_middle_chars(text, budget))
        }
        UpstreamTruncation::GrokBuild => {
            // footer 计入预算:preview 压到 budget - footer 长度内;
            // 剩余放不下 footer 则丢弃
            let footer = format!("\n\n[Output truncated - {} bytes total]", text.len());
            if budget <= footer.len() {
                return None;
            }
            let preview_budget = strategy
                .preview_bytes()
                .unwrap_or(budget)
                .min(budget - footer.len());
            let preview = truncate_utf8_to_bytes(text, preview_budget.min(text.len()));
            Some(format!("{preview}{footer}"))
        }
    }
}

/// 近似 token 数(对齐 codex approx_token_count:字节/4 向上取整)
fn approx_token_count(text: &str) -> usize {
    text.len().div_ceil(4)
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
        // array 形式的 content blocks:共用总预算
        // 对齐 codex truncate_function_output_items_with_policy:
        // 逐块扣减剩余预算,超预算块截断到剩余量,预算耗尽后丢弃并计数
        Value::Array(blocks) => {
            truncate_block_array(blocks, strategy);
        }
        _ => {}
    }
}

/// 本模块生成的省略 footer 仅接受规范数字格式。
fn is_omitted_footer(block: &Value) -> bool {
    let Some(text) = block.get("text").and_then(|t| t.as_str()) else {
        return false;
    };
    let Some(count) = text
        .strip_prefix("[omitted ")
        .and_then(|text| text.strip_suffix(" text items ...]"))
    else {
        return false;
    };
    let Ok(count) = count.parse::<usize>() else {
        return false;
    };
    text == format!("[omitted {count} text items ...]")
}

/// 块数组共用总预算截断(对齐 codex truncate_function_output_items_with_policy)
fn truncate_block_array(blocks: &mut Vec<Value>, strategy: UpstreamTruncation) {
    let budget = strategy.budget();
    let mut remaining = budget;
    let mut omitted_text_items = 0usize;
    let mut kept: Vec<Value> = Vec::with_capacity(blocks.len() + 1);
    let last_index = blocks.len().saturating_sub(1);

    for (index, block) in blocks.iter_mut().enumerate() {
        // 仅末尾规范 footer 可视为本模块上轮生成,避免任意工具文本绕过预算。
        if index == last_index
            && block.get("type").and_then(|t| t.as_str()) == Some("input_text")
            && is_omitted_footer(block)
        {
            kept.push(block.clone());
            continue;
        }
        let is_text = matches!(
            block.get("type").and_then(|t| t.as_str()),
            Some("input_text")
        );
        if !is_text {
            // 图片等非文本块不占文本预算,原样保留
            kept.push(block.clone());
            continue;
        }
        let text_len = block
            .get("text")
            .and_then(|t| t.as_str())
            .map(str::len)
            .unwrap_or(0);
        if text_len == 0 {
            kept.push(block.clone());
            continue;
        }
        if remaining == 0 {
            // 预算耗尽:丢弃该块并计数(对齐 codex omitted 路径)
            omitted_text_items += 1;
            continue;
        }
        if text_len <= remaining {
            remaining -= text_len;
            kept.push(block.clone());
        } else {
            // 截断到剩余预算(对齐 codex snippet 路径);
            // 剩余放不下 marker/footer 时丢弃该块并计入 omitted
            match fit_snippet(
                block.get("text").and_then(|t| t.as_str()).unwrap_or(""),
                strategy,
                remaining,
            ) {
                Some(fitted) => {
                    let mut fitted_block = block.clone();
                    fitted_block["text"] = Value::String(fitted);
                    kept.push(fitted_block);
                    remaining = 0;
                }
                None => {
                    omitted_text_items += 1;
                    // 预算保持不变:后续块仍可尝试塞进剩余空间
                }
            }
        }
    }

    if omitted_text_items > 0 {
        // 对齐 codex lib.rs:165:footer 是 input_text 块,不是裸字符串
        // (output 数组元素必须是带 type 的内容块,裸串会被 Responses 上游 400)
        kept.push(serde_json::json!({
            "type": "input_text",
            "text": format!("[omitted {omitted_text_items} text items ...]")
        }));
    }
    *blocks = kept;
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
    fn test_truncate_middle_preserves_head_and_tail() {
        // codex bytes 策略:中间截断,首尾保留
        let text = format!("HEAD{}TAIL", "x".repeat(20_000));
        let out = truncate_middle_chars(&text, 1_000);

        assert!(out.starts_with("HEAD"));
        assert!(out.ends_with("TAIL"));
        assert!(out.contains("chars truncated"));
        assert!(out.len() < 2_000);
    }

    #[test]
    fn test_truncate_middle_under_budget_unchanged() {
        let text = "short";
        assert_eq!(truncate_middle_chars(text, 100), text);
    }

    #[test]
    fn test_truncate_string_output() {
        // Responses 形状:function_call_output.output 为 string
        let mut body = json!({
            "input": [
                {"type": "message", "role": "user", "content": "hi"},
                {"type": "function_call_output", "call_id": "1", "output": format!("HEAD{}TAIL", "x".repeat(20_000))}
            ]
        });

        truncate(&mut body, UpstreamTruncation::Codex).unwrap();

        let output = body["input"][1]["output"].as_str().unwrap();
        assert!(output.len() < 12_000);
        assert!(output.contains("Warning: truncated output"));
        assert!(
            output.ends_with("TAIL"),
            "tail must survive: {}",
            &output[..40]
        );
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
    fn test_grok_build_block_array_uses_fixed_preview() {
        // 数组路径同样对齐 truncate_with_preview:固定 2KB UTF-8 安全预览。
        let mut body = json!({
            "input": [{
                "type": "function_call_output",
                "call_id": "1",
                "output": [{"type": "input_text", "text": "日".repeat(20_000)}]
            }]
        });

        truncate(&mut body, UpstreamTruncation::GrokBuild).unwrap();

        let output = body["input"][0]["output"][0]["text"].as_str().unwrap();
        let (preview, footer) = output.rsplit_once("\n\n").unwrap();
        assert!(preview.len() <= 2_000, "preview bytes: {}", preview.len());
        assert_eq!(footer, "[Output truncated - 60000 bytes total]");
        assert!(output.len() < 3_000, "preview+footer: {}", output.len());
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
    fn test_block_array_shares_budget() {
        // 块数组共用总预算:20 个 9KB 块不能原样通过(总 180KB > 10KB)
        let blocks: Vec<Value> = (0..20)
            .map(|i| json!({"type": "input_text", "text": format!("block{} ", i) + &"x".repeat(9_000)}))
            .collect();
        let mut body = json!({
            "input": [
                {"type": "function_call_output", "call_id": "1", "output": blocks}
            ]
        });

        truncate(&mut body, UpstreamTruncation::Codex).unwrap();

        let out_blocks = body["input"][0]["output"].as_array().unwrap();
        let total: usize = out_blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .map(str::len)
            .sum();
        assert!(total < 12_000, "total text bytes: {total}");
        // 预算耗尽后丢弃的块有 omitted 计数(首块截断占预算,其余 19 块丢弃);
        // footer 是 input_text 块(裸字符串会被 Responses 上游 400)
        let last = out_blocks.last().unwrap();
        assert_eq!(last["type"], "input_text");
        let footer = last["text"].as_str().unwrap();
        assert!(footer.starts_with("[omitted "), "footer: {footer}");
        assert!(footer.ends_with(" text items ...]"), "footer: {footer}");
    }

    #[test]
    fn test_footer_like_text_does_not_bypass_budget() {
        let original = format!("[omitted {} text items ...]", "x".repeat(20_000));
        let mut body = json!({
            "input": [
                {"type": "function_call_output", "call_id": "1", "output": [
                    {"type": "input_text", "text": original}
                ]}
            ]
        });

        truncate(&mut body, UpstreamTruncation::Codex).unwrap();

        let text = body["input"][0]["output"][0]["text"].as_str().unwrap();
        assert!(text.len() <= 10_000, "text bytes: {}", text.len());
        assert!(text.contains("chars truncated"));
    }

    #[test]
    fn test_only_terminal_footer_is_exempt_from_budget() {
        let blocks: Vec<Value> = (0..500)
            .map(|_| json!({"type": "input_text", "text": "[omitted 1 text items ...]"}))
            .collect();
        let mut body = json!({
            "input": [
                {"type": "function_call_output", "call_id": "1", "output": blocks}
            ]
        });

        truncate(&mut body, UpstreamTruncation::Codex).unwrap();

        let blocks = body["input"][0]["output"].as_array().unwrap();
        assert!(
            blocks.len() < 500,
            "non-terminal footers must consume budget"
        );
    }

    #[test]
    fn test_block_array_under_budget_untouched() {
        let original = json!({
            "input": [
                {"type": "function_call_output", "call_id": "1", "output": [
                    {"type": "input_text", "text": "a".repeat(3_000)},
                    {"type": "input_text", "text": "b".repeat(3_000)}
                ]}
            ]
        });

        let mut body = original.clone();
        truncate(&mut body, UpstreamTruncation::Codex).unwrap();

        assert_eq!(body, original);
    }

    #[test]
    fn test_block_array_image_blocks_preserved() {
        // 图片块不占文本预算,原样保留
        let mut body = json!({
            "input": [
                {"type": "function_call_output", "call_id": "1", "output": [
                    {"type": "input_image", "image_url": "data:image/png;base64,AAAA"},
                    {"type": "input_text", "text": "x".repeat(20_000)}
                ]}
            ]
        });

        truncate(&mut body, UpstreamTruncation::Codex).unwrap();

        let blocks = body["input"][0]["output"].as_array().unwrap();
        assert_eq!(blocks[0]["type"], "input_image");
        assert_eq!(blocks[0]["image_url"], "data:image/png;base64,AAAA");
        assert!(blocks[1]["text"].as_str().unwrap().len() < 12_000);
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
                        {"type": "input_text", "text": format!("HEAD{}TAIL", "a".repeat(20_000))}
                    ]
                }
            ]
        });

        truncate(&mut body, UpstreamTruncation::Codex).unwrap();

        let text = body["input"][0]["output"][0]["text"].as_str().unwrap();
        assert!(text.len() < 12_000);
        // 块数组走 snippet 路径(对齐 codex):无 formatted 头,纯中间截断
        assert!(text.contains("chars truncated"));
        assert!(text.ends_with("TAIL"));
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
                {"type": "custom_tool_call_output", "call_id": "1", "output": format!("HEAD{}TAIL", "x".repeat(20_000))}
            ]
        });

        truncate(&mut body, UpstreamTruncation::Codex).unwrap();

        let output = body["input"][0]["output"].as_str().unwrap();
        assert!(output.len() < 12_000);
        assert!(output.contains("Warning: truncated output"));
        assert!(output.ends_with("TAIL"));
    }

    #[test]
    fn test_codex_truncation_idempotent() {
        // 幂等:截断后结果(含 marker)≤ 预算,二次 pass 零改动
        let mut body = json!({
            "input": [
                {"type": "function_call_output", "call_id": "1", "output": format!("HEAD{}TAIL", "x".repeat(20_000))}
            ]
        });

        truncate(&mut body, UpstreamTruncation::Codex).unwrap();
        let snapshot = body.clone();
        truncate(&mut body, UpstreamTruncation::Codex).unwrap();

        assert_eq!(body, snapshot, "second pass must be byte-identical");
    }

    #[test]
    fn test_grok_truncation_idempotent() {
        let mut body = json!({
            "input": [
                {"type": "function_call_output", "call_id": "1", "output": "x".repeat(50_000)}
            ]
        });

        truncate(&mut body, UpstreamTruncation::GrokBuild).unwrap();
        let snapshot = body.clone();
        truncate(&mut body, UpstreamTruncation::GrokBuild).unwrap();

        assert_eq!(body, snapshot, "second pass must be byte-identical");
    }

    #[test]
    fn test_codex_tiny_remaining_budget_drops_block() {
        // 首块 9,999 bytes 占预算,次块大文本只剩 1 byte:
        // 放不下 marker,应丢弃次块并计入 omitted,不越界
        let mut body = json!({
            "input": [
                {"type": "function_call_output", "call_id": "1", "output": [
                    {"type": "input_text", "text": "a".repeat(9_999)},
                    {"type": "input_text", "text": "b".repeat(50_000)}
                ]}
            ]
        });

        truncate(&mut body, UpstreamTruncation::Codex).unwrap();

        let blocks = body["input"][0]["output"].as_array().unwrap();
        let total: usize = blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .map(str::len)
            .sum();
        assert!(total <= 10_000 + 32, "total: {total}");
        assert_eq!(blocks.len(), 2, "first block + omitted footer");
        assert_eq!(blocks[0]["text"].as_str().unwrap().len(), 9_999);
        assert_eq!(blocks[1]["type"], "input_text");
        assert_eq!(
            blocks[1]["text"].as_str().unwrap(),
            "[omitted 1 text items ...]"
        );
    }

    #[test]
    fn test_grok_tiny_remaining_budget_drops_block() {
        // 首块 39,999 bytes 占预算,次块大文本只剩 1 byte:
        // 放不下 footer,应丢弃次块并计入 omitted,不越界
        let mut body = json!({
            "input": [
                {"type": "function_call_output", "call_id": "1", "output": [
                    {"type": "input_text", "text": "a".repeat(39_999)},
                    {"type": "input_text", "text": "b".repeat(50_000)}
                ]}
            ]
        });

        truncate(&mut body, UpstreamTruncation::GrokBuild).unwrap();

        let blocks = body["input"][0]["output"].as_array().unwrap();
        let total: usize = blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .map(str::len)
            .sum();
        assert!(total <= 40_000 + 32, "total: {total}");
        assert_eq!(blocks.len(), 2);
        assert_eq!(blocks[0]["text"].as_str().unwrap().len(), 39_999);
        assert_eq!(
            blocks[1]["text"].as_str().unwrap(),
            "[omitted 1 text items ...]"
        );
    }

    #[test]
    fn test_codex_array_idempotent() {
        // 数组路径幂等:截断后(含 omitted footer)二次 pass 零改动。
        // 首块 9,999 占预算,次块 50KB 截到剩余 1 byte 放不下 marker 被丢弃,
        // 三块 20KB 预算已耗尽也被丢弃 → footer 计 2;二次 pass 首块不变、
        // footer 块 26 bytes ≤ 预算原样保留,零改动。
        let mut body = json!({
            "input": [
                {"type": "function_call_output", "call_id": "1", "output": [
                    {"type": "input_text", "text": "a".repeat(9_999)},
                    {"type": "input_text", "text": "b".repeat(50_000)},
                    {"type": "input_text", "text": "c".repeat(20_000)}
                ]}
            ]
        });

        truncate(&mut body, UpstreamTruncation::Codex).unwrap();
        let snapshot = body.clone();
        truncate(&mut body, UpstreamTruncation::Codex).unwrap();

        assert_eq!(body, snapshot, "second pass must be byte-identical");
    }

    #[test]
    fn test_grok_array_idempotent() {
        let mut body = json!({
            "input": [
                {"type": "function_call_output", "call_id": "1", "output": [
                    {"type": "input_text", "text": "a".repeat(39_999)},
                    {"type": "input_text", "text": "b".repeat(50_000)}
                ]}
            ]
        });

        truncate(&mut body, UpstreamTruncation::GrokBuild).unwrap();
        let snapshot = body.clone();
        truncate(&mut body, UpstreamTruncation::GrokBuild).unwrap();

        assert_eq!(body, snapshot, "second pass must be byte-identical");
    }

    #[test]
    fn test_grok_array_partial_remaining_fits() {
        // 剩余预算 > footer 长度:次块截到剩余内,总文本 ≤ 40KB
        let mut body = json!({
            "input": [
                {"type": "function_call_output", "call_id": "1", "output": [
                    {"type": "input_text", "text": "a".repeat(39_000)},
                    {"type": "input_text", "text": "b".repeat(50_000)}
                ]}
            ]
        });

        truncate(&mut body, UpstreamTruncation::GrokBuild).unwrap();

        let blocks = body["input"][0]["output"].as_array().unwrap();
        let total: usize = blocks
            .iter()
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .map(str::len)
            .sum();
        assert!(total <= 40_000, "total: {total}");
        assert!(blocks[1]["text"]
            .as_str()
            .unwrap()
            .contains("[Output truncated - 50000 bytes total]"));
    }
}
