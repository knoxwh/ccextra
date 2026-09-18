use bytes::Bytes;
use serde_json::Value;
use crate::sse::emit;

/// 工具参数 delta 非空才算产出;custom_tool_call_input 不计入
pub(crate) fn is_meaningful_output_delta(root: &Value, event_type: &str) -> bool {
    let meaningful = matches!(
        event_type,
        "response.output_text.delta"
            | "response.reasoning_text.delta"
            | "response.reasoning_summary_text.delta"
            | "response.function_call_arguments.delta"
    );
    if !meaningful {
        return false;
    }
    root.get("delta")
        .and_then(|v| v.as_str())
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false)
}

/// message item content 数组里的 output_text 拼接(文本兜底共用)
pub(crate) fn extract_output_text(content: Option<&Value>) -> String {
    let mut text = String::new();
    if let Some(parts) = content.and_then(|v| v.as_array()) {
        for part in parts {
            if part.get("type").and_then(|v| v.as_str()) == Some("output_text") {
                if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                    text.push_str(t);
                }
            }
        }
    }
    text
}

/// 空 incomplete 终态(对齐 CPA IsCodexTerminalEmptyIncomplete):上游静默中止
/// ——无任何输出 delta、无已完成 output item、response.output 空,且
/// output_tokens 为显式整数 0(缺失/null/浮点/非零一律不算)
pub fn is_terminal_empty_incomplete(
    root: &Value,
    saw_output_delta: bool,
    output_items_seen: usize,
) -> bool {
    if root.get("type").and_then(|v| v.as_str()) != Some("response.incomplete") {
        return false;
    }
    if saw_output_delta || output_items_seen > 0 {
        return false;
    }
    let has_output_items = root
        .pointer("/response/output")
        .and_then(|v| v.as_array())
        .is_some_and(|arr| !arr.is_empty());
    if has_output_items {
        return false;
    }
    let Some(tokens) = root.pointer("/response/usage/output_tokens") else {
        return false;
    };
    // 显式整数 0(缺失/null/浮点/非零一律不算;与 as_i64 联合排除 0.0)
    tokens.as_i64() == Some(0) && tokens == &serde_json::Value::from(0)
}

/// 流内 error 事件 → anthropic error(对齐 codexStreamErrorToClaudeError)
pub(crate) fn stream_error_frame(root: &Value) -> Bytes {
    let error = root.get("error");
    let mut err_type = error
        .and_then(|e| e.get("type"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if err_type.is_empty() {
        err_type = root
            .get("error_type")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
    }
    if err_type.is_empty() {
        err_type = "api_error".to_string();
    }
    let code = error
        .and_then(|e| e.get("code"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let mut message = error
        .and_then(|e| e.get("message"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if message.is_empty() {
        message = root
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
    }
    if message.is_empty() {
        message = code.clone();
    }
    if message.is_empty() {
        message = err_type.clone();
    }
    if code == "cyber_policy" || err_type == "invalid_request" {
        err_type = "invalid_request_error".to_string();
    }
    emit::error_event_typed(&err_type, &message)
}

/// stop_reason 提取(对齐 codexStopReason):
/// stop_reason > incomplete_details.reason > stop_sequence 推断
pub fn codex_stop_reason(r: &Value) -> String {
    if let Some(sr) = r.get("stop_reason").and_then(|v| v.as_str()) {
        if !sr.is_empty() {
            if sr == "stop" && stop_sequence(r).is_some() {
                return "stop_sequence".to_string();
            }
            return sr.to_string();
        }
    }
    if let Some(reason) = r
        .pointer("/incomplete_details/reason")
        .and_then(|v| v.as_str())
    {
        if !reason.is_empty() {
            return reason.to_string();
        }
    }
    if stop_sequence(r).is_some() {
        return "stop_sequence".to_string();
    }
    String::new()
}

pub fn stop_sequence(r: &Value) -> Option<String> {
    r.get("stop_sequence")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(String::from)
}

/// stop_reason → anthropic(对齐 mapCodexStopReasonToClaude)
pub fn map_stop_reason(stop_reason: &str, has_tool_use: bool) -> String {
    if has_tool_use {
        return "tool_use".to_string();
    }
    match stop_reason {
        "" | "stop" | "completed" => "end_turn".to_string(),
        "max_tokens" | "max_output_tokens" | "max_prompt_tokens" | "max_time_limit" => {
            "max_tokens".to_string()
        }
        // 无工具调用时参考实现把 tool 类原因映射为 end_turn
        "tool_use" | "tool_calls" | "function_call" => "end_turn".to_string(),
        "content_filter" => "refusal".to_string(),
        "end_turn"
        | "stop_sequence"
        | "pause_turn"
        | "refusal"
        | "model_context_window_exceeded" => stop_reason.to_string(),
        _ => "end_turn".to_string(),
    }
}

/// tool id 清洗:非法字符 → _,超 64 截断
/// (对齐 SanitizeClaudeToolID + shortenCodexCallIDIfNeeded)
pub fn sanitize_tool_id(id: &str) -> String {
    let mut out: String = id
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect();
    if out.len() > 64 {
        out.truncate(64);
    }
    out
}

