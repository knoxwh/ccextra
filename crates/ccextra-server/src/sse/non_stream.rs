// OpenAI 非流式响应 → Anthropic messages 非流式 JSON
//
// Claude Code 主对话恒走流式;非流式请求(标题生成 / /compact 摘要 /
// count_tokens 失败回退等)收到上游 JSON 时,须转回 Anthropic messages
// 形状,否则 SDK 无法解析。对齐 ConvertCodexResponseToClaudeNonStream
// (responses)与 openai chat message 结构。
//
// claude 直通协议上游返回的已是 Anthropic 形状,不经过本模块。

use serde_json::{json, Value};
use std::collections::HashMap;

use super::chat::map_finish_reason;
use super::responses::{
    codex_stop_reason, map_stop_reason, sanitize_tool_id, stop_sequence, web_search_result_content,
};
use ccextra_core::convert::fix_json_quotes;

/// OpenAI responses 非流式 body → Anthropic messages body
///
/// `tool_names`:short→original 工具名还原表(请求转换侧产出;无则原样)
pub fn responses_to_anthropic(
    body: &Value,
    tool_names: Option<&HashMap<String, String>>,
) -> Option<Value> {
    let ty = body.get("type").and_then(|v| v.as_str()).unwrap_or("");
    // SSE 完成信封 {"type":...,"response":{...}} 或官方 REST 顶层 Response
    // (object==response);错误 body 无 response,维持 None 不误转
    let is_wrapper = ty == "response.completed" || ty == "response.incomplete";
    let is_rest = body.get("object").and_then(|v| v.as_str()) == Some("response");
    if !is_wrapper && !is_rest {
        return None;
    }
    let response = if is_wrapper {
        body.get("response")
    } else {
        Some(body)
    }?;

    let mut content: Vec<Value> = Vec::new();
    let mut has_tool_use = false;

    if let Some(output) = response.get("output").and_then(|v| v.as_array()) {
        for item in output {
            match item.get("type").and_then(|v| v.as_str()).unwrap_or("") {
                "reasoning" => {
                    // summary > content 取文本;signature 保留
                    let mut text = String::new();
                    if let Some(summary) = item.get("summary") {
                        collect_response_parts(summary, &mut text);
                    }
                    if text.is_empty() {
                        if let Some(content_arr) = item.get("content") {
                            collect_response_parts(content_arr, &mut text);
                        }
                    }
                    let signature = item
                        .get("encrypted_content")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    if !text.is_empty() || !signature.is_empty() {
                        let mut block = json!({"type": "thinking", "thinking": text});
                        if !signature.is_empty() {
                            block["signature"] = json!(signature);
                        }
                        content.push(block);
                    }
                }
                "message" => {
                    if let Some(parts) = item.get("content").and_then(|v| v.as_array()) {
                        for part in parts {
                            if part.get("type").and_then(|v| v.as_str()) == Some("output_text") {
                                let t = part.get("text").and_then(|v| v.as_str()).unwrap_or("");
                                if !t.is_empty() {
                                    content.push(json!({"type": "text", "text": t}));
                                }
                            }
                        }
                    }
                }
                "function_call" | "custom_tool_call" => {
                    has_tool_use = true;
                    let is_custom =
                        item.get("type").and_then(|v| v.as_str()) == Some("custom_tool_call");
                    let id = sanitize_tool_id(
                        item.get("call_id").and_then(|v| v.as_str()).unwrap_or(""),
                    );
                    let raw_name = item.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    // 请求侧超长名缩短后,响应侧还原原名(对齐 buildReverseMap...)
                    let name = tool_names
                        .and_then(|rev| rev.get(raw_name))
                        .cloned()
                        .unwrap_or_else(|| raw_name.to_string());
                    let input = if is_custom {
                        // custom 工具 input 是字符串,包成 {"input": str}(对齐响应转换 custom 分支)
                        let raw = item.get("input").and_then(|v| v.as_str()).unwrap_or("");
                        json!({"input": raw})
                    } else {
                        item.get("arguments")
                            .and_then(|v| v.as_str())
                            .and_then(|s| serde_json::from_str::<Value>(s).ok())
                            .filter(|v| v.is_object())
                            .unwrap_or_else(|| json!({}))
                    };
                    content
                        .push(json!({"type": "tool_use", "id": id, "name": name, "input": input}));
                }
                "web_search_call" => {
                    // server_tool_use + web_search_tool_result(对齐 appendCodexWebSearchNonStreamContent)
                    let id = item.get("id").and_then(|v| v.as_str()).unwrap_or("");
                    if id.is_empty() {
                        continue;
                    }
                    let query = item
                        .pointer("/action/query")
                        .and_then(|v| v.as_str())
                        .or_else(|| item.get("query").and_then(|v| v.as_str()))
                        .unwrap_or("")
                        .trim()
                        .to_string();
                    let results = web_search_result_content(item, item);
                    if query.is_empty() && results.is_empty() {
                        continue;
                    }
                    let mut use_block = json!({
                        "type": "server_tool_use",
                        "id": id,
                        "name": "web_search",
                        "input": {}
                    });
                    if !query.is_empty() {
                        use_block["input"] = json!({"query": query});
                    }
                    content.push(use_block);
                    let mut result_block = json!({
                        "type": "web_search_tool_result",
                        "tool_use_id": id,
                        "content": []
                    });
                    if !results.is_empty() {
                        result_block["content"] = Value::Array(results);
                    }
                    content.push(result_block);
                }
                _ => {}
            }
        }
    }

    // usage:cached 从 input 扣(对齐 extractResponsesUsage)
    let (input_tokens, output_tokens, cached) = response
        .get("usage")
        .map(super::extract_usage_responses)
        .unwrap_or((0, 0, 0));

    let stop_reason = map_stop_reason(&codex_stop_reason(response), has_tool_use);
    let stop_seq = stop_sequence(response);
    build_message(
        response,
        content,
        &stop_reason,
        stop_seq,
        input_tokens,
        output_tokens,
        cached,
    )
}

/// OpenAI chat 非流式 body → Anthropic messages body
pub fn openai_chat_to_anthropic(body: &Value) -> Option<Value> {
    let choice = body.pointer("/choices/0")?;
    let message = choice.get("message")?;
    let finish_reason = choice
        .get("finish_reason")
        .and_then(|v| v.as_str())
        .unwrap_or("");

    let mut content: Vec<Value> = Vec::new();
    let mut has_tool_use = false;

    // reasoning → thinking(多供应商拼写,对齐 collectOpenAIReasoningTexts:
    // reasoning_content / reasoning_details[] / reasoning / thinking)。
    // 全部拼成一块,避免同义字段各推一块 thinking。
    let mut reasoning_texts = Vec::new();
    if let Some(r) = message.get("reasoning_content") {
        let text = collect_reasoning_value(r);
        if !text.is_empty() {
            reasoning_texts.push(text);
        }
    }
    // reasoning_details[](加密条目跳过,text/reasoning/thinking/summary 取首个)
    if let Some(Value::Array(details)) = message.get("reasoning_details") {
        for item in details {
            if item.get("encrypted_content").is_some() || item.get("data").is_some() {
                continue;
            }
            for field in ["text", "reasoning", "thinking", "summary"] {
                if let Some(v) = item.get(field) {
                    let text = collect_reasoning_value(v);
                    if !text.is_empty() {
                        reasoning_texts.push(text);
                    }
                    break;
                }
            }
        }
    }
    for field in ["reasoning", "thinking"] {
        if let Some(r) = message.get(field) {
            let text = collect_reasoning_value(r);
            if !text.is_empty() {
                reasoning_texts.push(text);
            }
        }
    }
    if !reasoning_texts.is_empty() {
        content.push(json!({
            "type": "thinking",
            "thinking": reasoning_texts.join("\n")
        }));
    }

    // content:text 字符串或 [{type:"text"}]
    match message.get("content") {
        Some(Value::String(s)) if !s.is_empty() => {
            content.push(json!({"type": "text", "text": s}));
        }
        Some(Value::Array(parts)) => {
            for part in parts {
                if part.get("type").and_then(|v| v.as_str()) == Some("text") {
                    let t = part.get("text").and_then(|v| v.as_str()).unwrap_or("");
                    if !t.is_empty() {
                        content.push(json!({"type": "text", "text": t}));
                    }
                }
            }
        }
        _ => {}
    }

    // tool_calls
    let mut args_buf = String::new();
    if let Some(calls) = message.get("tool_calls").and_then(|v| v.as_array()) {
        for call in calls {
            has_tool_use = true;
            let id = sanitize_tool_id(call.get("id").and_then(|v| v.as_str()).unwrap_or(""));
            let name = call
                .pointer("/function/name")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            args_buf.clear();
            collect_json_string(call.pointer("/function/arguments"), &mut args_buf);
            // 解析前先修单引号(对齐 util.FixJSON):部分上游输出非标准 JSON
            let fixed = fix_json_quotes(&args_buf);
            let input = if fixed.trim().is_empty() {
                json!({})
            } else {
                serde_json::from_str::<Value>(&fixed)
                    .ok()
                    .filter(|v| v.is_object())
                    .unwrap_or_else(|| json!({}))
            };
            content.push(json!({"type": "tool_use", "id": id, "name": name, "input": input}));
        }
    }

    // usage:prompt_tokens/completion_tokens(对齐 OpenAI Chat API)
    let (input_tokens, output_tokens, cached) = body
        .get("usage")
        .map(super::extract_usage_chat)
        .unwrap_or((0, 0, 0));

    let stop_reason = if has_tool_use {
        "tool_use"
    } else {
        map_finish_reason(finish_reason)
    };
    build_message(
        body,
        content,
        stop_reason,
        None,
        input_tokens,
        output_tokens,
        cached,
    )
}

/// 组装 Anthropic messages 非流式响应(基础形状)
fn build_message(
    source: &Value,
    content: Vec<Value>,
    stop_reason: &str,
    stop_sequence: Option<String>,
    input_tokens: i64,
    output_tokens: i64,
    cached: i64,
) -> Option<Value> {
    let mut out = json!({
        "id": source.get("id").and_then(|v| v.as_str()).unwrap_or(""),
        "type": "message",
        "role": "assistant",
        "model": source.get("model").and_then(|v| v.as_str()).unwrap_or(""),
        "content": content,
        "stop_reason": stop_reason,
        "stop_sequence": stop_sequence.map(Value::String).unwrap_or(Value::Null),
        "usage": {"input_tokens": input_tokens, "output_tokens": output_tokens}
    });
    if cached > 0 {
        out["usage"]["cache_read_input_tokens"] = json!(cached);
    }
    Some(out)
}

/// responses reasoning 的 summary/content 都是数组 forEach 取 text
fn collect_response_parts(v: &Value, out: &mut String) {
    match v {
        Value::Array(items) => {
            for item in items {
                if let Some(t) = item.get("text").and_then(|v| v.as_str()) {
                    push_text(out, t);
                } else if let Some(s) = item.as_str() {
                    push_text(out, s);
                }
            }
        }
        Value::String(s) => push_text(out, s),
        _ => {}
    }
}

/// reasoning_content:字符串 / 对象 {text} / 数组(对齐 collectOpenAIReasoningTexts)
fn collect_reasoning_value(v: &Value) -> String {
    let mut out = String::new();
    match v {
        Value::String(s) => push_text(&mut out, s),
        Value::Object(_) => {
            if let Some(t) = v.get("text").and_then(|x| x.as_str()) {
                push_text(&mut out, t);
            }
        }
        Value::Array(items) => {
            for item in items {
                if let Some(s) = item.as_str() {
                    push_text(&mut out, s);
                } else if let Some(t) = item.get("text").and_then(|x| x.as_str()) {
                    push_text(&mut out, t);
                }
            }
        }
        _ => {}
    }
    out
}

/// JSON 字符串段(字符串直接合并;对象/数组用 compact 文本)
fn collect_json_string(v: Option<&Value>, out: &mut String) {
    match v {
        Some(Value::String(s)) => out.push_str(s),
        Some(other) => out.push_str(&other.to_string()),
        None => {}
    }
}

fn push_text(out: &mut String, s: &str) {
    let t = s.trim();
    if !t.is_empty() {
        if !out.is_empty() {
            out.push('\n');
        }
        out.push_str(t);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_responses_nonstream_rest_top_level() {
        // 官方 REST 非流响应:顶层 object=response(无包裹信封)
        let body = json!({
            "id": "resp_9",
            "object": "response",
            "status": "completed",
            "model": "gpt-5.6-terra",
            "output": [{
                "type": "message",
                "content": [{"type": "output_text", "text": "rest hi"}]
            }],
            "usage": {"input_tokens": 50, "output_tokens": 3}
        });
        let out = responses_to_anthropic(&body, None).unwrap();
        assert_eq!(out["type"], "message");
        assert_eq!(out["content"][0]["text"], "rest hi");
        assert_eq!(out["stop_reason"], "end_turn");
        assert_eq!(out["usage"]["output_tokens"], 3);
    }

    #[test]
    fn test_responses_nonstream_text() {
        let body = json!({
            "type": "response.completed",
            "response": {
                "id": "r1",
                "model": "gpt-5",
                "output": [{
                    "type": "message",
                    "content": [{"type": "output_text", "text": "hello"}]
                }],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 100, "output_tokens": 5,
                          "input_tokens_details": {"cached_tokens": 80}}
            }
        });
        let out = responses_to_anthropic(&body, None).unwrap();
        assert_eq!(out["type"], "message");
        assert_eq!(out["content"][0]["type"], "text");
        assert_eq!(out["content"][0]["text"], "hello");
        assert_eq!(out["stop_reason"], "end_turn");
        assert_eq!(out["usage"]["input_tokens"], 20);
        assert_eq!(out["usage"]["cache_read_input_tokens"], 80);
    }

    #[test]
    fn test_responses_nonstream_tool_call() {
        let body = json!({
            "type": "response.completed",
            "response": {
                "id": "r1",
                "model": "gpt-5",
                "output": [{
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "get_weather",
                    "arguments": "{\"city\":\"beijing\"}"
                }],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 10, "output_tokens": 5}
            }
        });
        let out = responses_to_anthropic(&body, None).unwrap();
        assert_eq!(out["content"][0]["type"], "tool_use");
        assert_eq!(out["content"][0]["name"], "get_weather");
        assert_eq!(out["content"][0]["input"]["city"], "beijing");
        assert_eq!(out["stop_reason"], "tool_use");
    }

    #[test]
    fn test_responses_nonstream_reasoning() {
        let body = json!({
            "type": "response.completed",
            "response": {
                "id": "r1",
                "model": "gpt-5",
                "output": [{
                    "type": "reasoning",
                    "summary": [{"type": "summary_text", "text": "think step"},
                                {"type": "summary_text", "text": " step two"}],
                    "encrypted_content": "sig123"
                }],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 10, "output_tokens": 5}
            }
        });
        let out = responses_to_anthropic(&body, None).unwrap();
        assert_eq!(out["content"][0]["type"], "thinking");
        assert_eq!(out["content"][0]["thinking"], "think step\nstep two");
        assert_eq!(out["content"][0]["signature"], "sig123");
    }

    #[test]
    fn test_chat_nonstream_text_tool() {
        let body = json!({
            "id": "c1",
            "model": "gpt-5",
            "choices": [{
                "message": {
                    "role": "assistant",
                    "content": "hi",
                    "tool_calls": [{
                        "id": "call_1",
                        "type": "function",
                        "function": {"name": "get_weather", "arguments": "{}"}
                    }]
                },
                "finish_reason": "tool_calls"
            }],
            "usage": {"prompt_tokens": 30, "completion_tokens": 7,
                      "prompt_tokens_details": {"cached_tokens": 10}}
        });
        let out = openai_chat_to_anthropic(&body).unwrap();
        assert_eq!(out["content"][0]["type"], "text");
        assert_eq!(out["content"][1]["type"], "tool_use");
        assert_eq!(out["stop_reason"], "tool_use");
        assert_eq!(out["usage"]["input_tokens"], 20);
        assert_eq!(out["usage"]["cache_read_input_tokens"], 10);
    }

    #[test]
    fn test_chat_nonstream_reasoning_content() {
        let body = json!({
            "id": "c1",
            "model": "gpt-5",
            "choices": [{
                "message": {"role": "assistant", "reasoning_content": "think",
                           "content": ""},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        });
        let out = openai_chat_to_anthropic(&body).unwrap();
        assert_eq!(out["content"][0]["type"], "thinking");
        assert_eq!(out["content"][0]["thinking"], "think");
        assert_eq!(out["stop_reason"], "end_turn");
    }

    #[test]
    fn test_chat_nonstream_reasoning_alias_and_details() {
        // message.reasoning 顶层别名(实测 xAI 网关非流走此字段,对齐
        // collectOpenAIReasoningTexts 拼写集)+ reasoning_details[] 明文条目
        let body = json!({
            "id": "c1",
            "model": "grok-4.6",
            "choices": [{
                "message": {"role": "assistant", "reasoning": "step one",
                           "content": ""},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        });
        let out = openai_chat_to_anthropic(&body).unwrap();
        assert_eq!(out["content"][0]["type"], "thinking");
        assert_eq!(out["content"][0]["thinking"], "step one");

        // 加密条目跳过,明文 summary 取首
        let body2 = json!({
            "id": "c2",
            "model": "grok-4.6",
            "choices": [{
                "message": {"role": "assistant", "content": "",
                    "reasoning_details": [
                        {"type": "encrypted_content", "data": "ENCSIG"},
                        {"type": "summary", "summary": "think sum"}
                    ]},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        });
        let out2 = openai_chat_to_anthropic(&body2).unwrap();
        assert_eq!(out2["content"][0]["type"], "thinking");
        assert_eq!(out2["content"][0]["thinking"], "think sum");

        // 同义字段合并成一块,不拆多块 thinking
        let body3 = json!({
            "id": "c3",
            "model": "grok-4.6",
            "choices": [{
                "message": {
                    "role": "assistant",
                    "reasoning_content": "a",
                    "reasoning": "b",
                    "content": ""
                },
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        });
        let out3 = openai_chat_to_anthropic(&body3).unwrap();
        let thinking: Vec<_> = out3["content"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|b| b["type"] == "thinking")
            .collect();
        assert_eq!(thinking.len(), 1);
        assert_eq!(thinking[0]["thinking"], "a\nb");
    }

    #[test]
    fn test_not_recognized_shape_returns_none() {
        assert!(responses_to_anthropic(&json!({"type": "x"}), None).is_none());
        assert!(openai_chat_to_anthropic(&json!({"choices": []})).is_none());
    }

    #[test]
    fn test_responses_nonstream_tool_name_restored() {
        // 请求侧缩短的名,响应侧还原原名(对齐 buildReverseMap)
        let body = json!({
            "type": "response.completed",
            "response": {
                "id": "r1",
                "model": "gpt-5",
                "output": [{
                    "type": "function_call",
                    "call_id": "call_1",
                    "name": "mcp__short",
                    "arguments": "{}"
                }],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 10, "output_tokens": 5}
            }
        });
        let mut rev = HashMap::new();
        rev.insert(
            "mcp__short".to_string(),
            "mcp__long_original_name".to_string(),
        );
        let out = responses_to_anthropic(&body, Some(&rev)).unwrap();
        assert_eq!(out["content"][0]["name"], "mcp__long_original_name");
    }

    #[test]
    fn test_responses_nonstream_custom_tool_call() {
        // custom 工具:input 是字符串,包成 {"input": str} 对象
        let body = json!({
            "type": "response.completed",
            "response": {
                "id": "r1",
                "model": "gpt-5",
                "output": [{
                    "type": "custom_tool_call",
                    "call_id": "call_1",
                    "name": "apply_patch",
                    "input": "patch-data"
                }],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 10, "output_tokens": 5}
            }
        });
        let out = responses_to_anthropic(&body, None).unwrap();
        assert_eq!(out["content"][0]["type"], "tool_use");
        assert_eq!(out["content"][0]["name"], "apply_patch");
        assert_eq!(out["content"][0]["input"]["input"], "patch-data");
        assert_eq!(out["stop_reason"], "tool_use");
    }

    #[test]
    fn test_responses_nonstream_web_search_call() {
        let body = json!({
            "type": "response.completed",
            "response": {
                "id": "r1",
                "model": "gpt-5",
                "output": [{
                    "type": "web_search_call",
                    "id": "ws_1",
                    "action": {"query": "rust async"},
                    "results": [{"url": "https://example.com", "title": "Example"}]
                }],
                "stop_reason": "end_turn",
                "usage": {"input_tokens": 10, "output_tokens": 5}
            }
        });
        let out = responses_to_anthropic(&body, None).unwrap();
        assert_eq!(out["content"][0]["type"], "server_tool_use");
        assert_eq!(out["content"][0]["name"], "web_search");
        assert_eq!(out["content"][0]["input"]["query"], "rust async");
        assert_eq!(out["content"][1]["type"], "web_search_tool_result");
        assert_eq!(
            out["content"][1]["content"][0]["url"],
            "https://example.com"
        );
    }
}
