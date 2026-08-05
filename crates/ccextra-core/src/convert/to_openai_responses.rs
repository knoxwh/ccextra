// Anthropic → OpenAI responses 转换(对齐 CPA codex/claude/codex_claude_request.go)
//
// 主要映射(与 CPA 逐条一致):
// - system → input[0] developer message(逐 text block 独立 part,过滤计费归属块)
// - messages → input[]:text→input_text/output_text、thinking(带 GPT 兼容签名)→reasoning、
//   image→input_image、tool_use→function_call、tool_result→function_call_output
//   (遇 thinking/tool_use/tool_result 先 flush 文本 message,对齐 CPA flushMessage 顺序)
// - tools → codex tools(原名超 64 缩短 + 唯一 _N 后缀;web_search_* → type:"web_search")、
//   input_schema→parameters(strict=false,剥 cache_control/defer_loading/$schema)
// - thinking.budget_tokens → reasoning.effort(直映射,不钳制)
// - service_tier: speed=fast → priority
// - store=false;include=["reasoning.encrypted_content"];parallel_tool_calls
//
// 返回 short→original 工具名映射,响应侧还原原名(对齐 buildReverseMap...)。

use std::collections::{HashMap, HashSet};

use serde_json::{json, Value};

use super::shorten::{build_short_name_map, shorten_name_if_needed};
use super::Result;

/// Claude web search 工具类型(对齐 CPA isClaudeWebSearchToolType)
fn is_web_search_tool_type(tool_type: &str) -> bool {
    matches!(tool_type, "web_search_20250305" | "web_search_20260209")
}

/// signature 是否 GPT 兼容(对齐 CPA CompatibleSignatureForProvider(GPT, sig) 的轻量版)
///
/// CPA 用完整 Fernet 容器校验;这里按 OpenAI reasoning 签名的确定前缀
/// ("gAAAA" 或 base64 首字符 'g')判定,足以区分 Claude/Gemini 签名(以 'C'/'E'/'R'
/// 开头)与 GPT 签名。Grok 特例按模型名放行(对齐 CPA codexClaudeTargetAcceptsGrokSignature)。
fn signature_compatible_gpt(signature: &str, upstream_model: &str) -> bool {
    let sig = signature.trim();
    if sig.is_empty() {
        return false;
    }
    let first = sig.as_bytes()[0];
    // GPT Fernet reasoning 首字节 0x80 → base64 首字符 'g'(对齐 CPA
    // selfDescribingSignatureFirstChars 中 'g' 的判定)
    let looks_gpt = first == b'g' || sig.starts_with("gAAAA");
    if looks_gpt {
        return true;
    }
    // Grok encrypted_content 无信封,CPA 对 grok 目标单独放行
    upstream_model.to_ascii_lowercase().contains("grok")
}

/// Claude thinking signature → 可回放给 GPT 上游的 reasoning.encrypted_content
fn gpt_compatible_signature(signature: Option<&str>, upstream_model: &str) -> Option<String> {
    let sig = signature.unwrap_or("").trim();
    if sig.is_empty() {
        return None;
    }
    if signature_compatible_gpt(sig, upstream_model) {
        Some(sig.to_string())
    } else {
        None
    }
}

/// tool_result content → responses output 数组或字符串(对齐 CPA tool_result 分支)
fn tool_result_output(content: &Value) -> Value {
    match content {
        Value::Array(items) => {
            let mut out: Vec<Value> = Vec::new();
            for item in items {
                match item.get("type").and_then(|v| v.as_str()) {
                    Some("image") => {
                        let data = item
                            .pointer("/source/data")
                            .and_then(|v| v.as_str())
                            .or_else(|| item.pointer("/source/base64").and_then(|v| v.as_str()))
                            .unwrap_or("");
                        if !data.is_empty() {
                            let media = item
                                .pointer("/source/media_type")
                                .and_then(|v| v.as_str())
                                .or_else(|| item.pointer("/source/mime_type").and_then(|v| v.as_str()))
                                .unwrap_or("application/octet-stream");
                            out.push(json!({
                                "type": "input_image",
                                "image_url": format!("data:{media};base64,{data}")
                            }));
                        }
                    }
                    Some("text") => {
                        if let Some(t) = item.get("text").and_then(|v| v.as_str()) {
                            out.push(json!({"type": "input_text", "text": t}));
                        }
                    }
                    _ => {}
                }
            }
            if out.is_empty() {
                Value::String(content.to_string())
            } else {
                Value::Array(out)
            }
        }
        Value::String(_) => content.clone(),
        other => other.clone(),
    }
}

/// image block → data URL(对齐 CPA image 分支)
fn image_to_data_url(part: &Value) -> Option<String> {
    let source = part.get("source")?;
    let data = source
        .get("data")
        .or_else(|| source.get("base64"))
        .and_then(|v| v.as_str())?;
    if data.is_empty() {
        return None;
    }
    let media_type = source
        .get("media_type")
        .and_then(|v| v.as_str())
        .or_else(|| source.get("mime_type").and_then(|v| v.as_str()))
        .unwrap_or("application/octet-stream");
    Some(format!("data:{media_type};base64,{data}"))
}

/// input_schema → parameters(对齐 CPA normalizeToolParameters)
fn normalize_tool_parameters(schema: &Value) -> Value {
    if schema.is_null() || !schema.is_object() {
        return json!({"type": "object", "properties": {}});
    }
    let mut s = schema.clone();
    let s_type = s.get("type").and_then(|v| v.as_str()).unwrap_or("");
    if s_type.is_empty() {
        s["type"] = json!("object");
    }
    if s.get("type").and_then(|v| v.as_str()) == Some("object")
        && s.get("properties").map_or(true, |v| !v.is_object())
    {
        s["properties"] = json!({});
    }
    s
}

/// Claude system 文本块 → 逐块独立 input_text part(对齐 CPA appendSystemText)
fn system_content_parts(system: &Value) -> Vec<Value> {
    let mut parts: Vec<Value> = Vec::new();
    let mut push = |text: &str| {
        if text.is_empty() || super::is_attribution_text(text) {
            return;
        }
        parts.push(json!({"type": "input_text", "text": text}));
    };
    match system {
        Value::String(s) => push(s),
        Value::Array(blocks) => {
            for b in blocks {
                if b.get("type").and_then(|v| v.as_str()) == Some("text") {
                    if let Some(t) = b.get("text").and_then(|v| v.as_str()) {
                        push(t);
                    }
                }
            }
        }
        _ => {}
    }
    parts
}

/// Anthropic messages → OpenAI responses
pub fn convert_to_openai_responses(
    body: &mut Value,
    upstream_model: &str,
) -> Result<HashMap<String, String>> {
    let mut openai = json!({
        "model": upstream_model,
        "instructions": "",
        "input": [],
    });

    // --- 工具名缩短映射(对齐 CPA buildReverseMapFromClaudeOriginalToShort) ---
    let mut tool_name_map: HashMap<String, String> = HashMap::new();
    let mut web_search_names: HashSet<String> = HashSet::new();
    if let Some(tools) = body.get("tools").and_then(|v| v.as_array()) {
        let mut names: Vec<String> = Vec::new();
        for tool in tools {
            let tool_type = tool.get("type").and_then(|v| v.as_str()).unwrap_or("");
            if is_web_search_tool_type(tool_type) {
                if let Some(n) = tool.get("name").and_then(|v| v.as_str()) {
                    if !n.is_empty() {
                        web_search_names.insert(n.to_string());
                    }
                }
                continue;
            }
            if let Some(n) = tool.get("name").and_then(|v| v.as_str()) {
                if !n.is_empty() {
                    names.push(n.to_string());
                }
            }
        }
        tool_name_map = build_short_name_map(&names);
    }

    // --- system → developer message(对齐 CPA system 分支) ---
    if let Some(system) = body.get("system") {
        let parts = system_content_parts(system);
        if !parts.is_empty() {
            openai["input"]
                .as_array_mut()
                .unwrap()
                .push(json!({"type": "message", "role": "developer", "content": parts}));
        }
    }

    // --- messages → input[] ---
    if let Some(messages) = body.get("messages").and_then(|v| v.as_array()) {
        for msg in messages {
            let role = msg
                .get("role")
                .and_then(|v| v.as_str())
                .unwrap_or("user");

            // role=system 消息:reminder 文本 → user message(对齐 CPA
            // ClaudeMessageSystemReminderText)
            if role == "system" {
                if let Some(text) = claude_system_reminder_text(msg.get("content")) {
                    openai["input"]
                        .as_array_mut()
                        .unwrap()
                        .push(json!({
                            "type": "message",
                            "role": "user",
                            "content": [{"type": "input_text", "text": text}]
                        }));
                }
                continue;
            }

            let Some(content) = msg.get("content") else {
                continue;
            };
            if content.is_null() {
                continue;
            }

            let mut content_items: Vec<Value> = Vec::new();
            let mut out_items: Vec<Value> = Vec::new();
            let flush_message = |content_items: &mut Vec<Value>, out_items: &mut Vec<Value>| {
                if !content_items.is_empty() {
                    out_items.push(json!({
                        "type": "message",
                        "role": role,
                        "content": std::mem::take(content_items)
                    }));
                }
            };

            // 字符串内容
            if let Some(s) = content.as_str() {
                let item_type = if role == "assistant" { "output_text" } else { "input_text" };
                let items = if s.is_empty() {
                    Vec::new()
                } else {
                    vec![json!({"type": item_type, "text": s})]
                };
                openai["input"].as_array_mut().unwrap().push(json!({
                    "type": "message", "role": role, "content": items
                }));
                continue;
            }

            let Some(parts) = content.as_array() else {
                continue;
            };

            for part in parts {
                let ptype = part.get("type").and_then(|v| v.as_str()).unwrap_or("");
                match ptype {
                    "text" => {
                        if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                            let item_type =
                                if role == "assistant" { "output_text" } else { "input_text" };
                            content_items.push(json!({"type": item_type, "text": t}));
                        }
                    }
                    "thinking" => {
                        // 带 GPT 兼容签名的思考 → 独立 reasoning item(先 flush 保序);
                        // 无签名/不兼容签名丢弃,不退化为纯文本(对齐 CPA appendReasoningContent)
                        if role == "assistant" {
                            if let Some(sig) = gpt_compatible_signature(
                                part.get("signature").and_then(|v| v.as_str()),
                                upstream_model,
                            ) {
                                flush_message(&mut content_items, &mut out_items);
                                out_items.push(json!({
                                    "type": "reasoning",
                                    "summary": [],
                                    "content": null,
                                    "encrypted_content": sig
                                }));
                            }
                        }
                    }
                    "image" => {
                        if let Some(url) = image_to_data_url(part) {
                            content_items.push(json!({"type": "input_image", "image_url": url}));
                        }
                    }
                    "tool_use" => {
                        flush_message(&mut content_items, &mut out_items);
                        let id = part.get("id").and_then(|v| v.as_str()).unwrap_or("");
                        let name = part.get("name").and_then(|v| v.as_str()).unwrap_or("");
                        let short_name = if let Some(s) = tool_name_map.get(name) {
                            s.clone()
                        } else {
                            shorten_name_if_needed(name)
                        };
                        let args = part
                            .get("input")
                            .map(|i| i.to_string())
                            .unwrap_or_else(|| "{}".to_string());
                        out_items.push(json!({
                            "type": "function_call",
                            "call_id": shorten_call_id(id),
                            "name": short_name,
                            "arguments": args
                        }));
                    }
                    "tool_result" => {
                        flush_message(&mut content_items, &mut out_items);
                        let call_id = part
                            .get("tool_use_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let output = tool_result_output(part.get("content").unwrap_or(&json!("")));
                        out_items.push(json!({
                            "type": "function_call_output",
                            "call_id": shorten_call_id(call_id),
                            "output": output
                        }));
                    }
                    _ => {}
                }
            }
            flush_message(&mut content_items, &mut out_items);
            openai["input"].as_array_mut().unwrap().extend(out_items);
        }
    }

    // --- tools → codex tools(对齐 CPA tools 分支) ---
    if let Some(tools) = body.get("tools").and_then(|v| v.as_array()) {
        let mut tool_items: Vec<Value> = Vec::new();
        for tool in tools {
            let tool_type = tool.get("type").and_then(|v| v.as_str()).unwrap_or("");
            // web search 工具特殊映射(对齐 CPA convertClaudeWebSearchToolToCodex)
            if is_web_search_tool_type(tool_type) {
                let mut ws = json!({"type": "web_search"});
                if let Some(domains) = tool.get("allowed_domains").and_then(|v| v.as_array()) {
                    ws["filters"] = json!({"allowed_domains": domains});
                }
                if let Some(loc) = tool.get("user_location") {
                    if loc.is_object() {
                        ws["user_location"] = loc.clone();
                    }
                }
                tool_items.push(ws);
                continue;
            }

            let mut t = tool.clone();
            if t.get("type").and_then(|v| v.as_str()) != Some("function") {
                t["type"] = json!("function");
            }
            // 名称缩短
            if let Some(orig) = t.get("name").and_then(|v| v.as_str()) {
                let short = if let Some(s) = tool_name_map.get(orig) {
                    s.clone()
                } else {
                    shorten_name_if_needed(orig)
                };
                if short != orig {
                    t["name"] = json!(short);
                }
            }
            // input_schema → parameters
            let schema = tool.get("input_schema").cloned().unwrap_or(json!(null));
            let params = normalize_tool_parameters(&schema);
            t["parameters"] = params;
            // 剥 codex 不认的字段(对齐 CPA)
            if let Some(obj) = t.as_object_mut() {
                obj.remove("input_schema");
                obj.remove("cache_control");
                obj.remove("defer_loading");
                if let Some(p) = obj.get_mut("parameters") {
                    if let Some(po) = p.as_object_mut() {
                        po.remove("$schema");
                    }
                }
            }
            if t.get("strict") != Some(&json!(false)) {
                t["strict"] = json!(false);
            }
            tool_items.push(t);
        }
        openai["tools"] = json!(tool_items);
    }

    // --- tool_choice(对齐 CPA convertClaudeToolChoiceToCodex) ---
    match body.get("tool_choice") {
        None | Some(Value::Null) => {
            openai["tool_choice"] = json!("auto");
        }
        Some(tc) => {
            let ty = tc
                .get("type")
                .and_then(|v| v.as_str())
                .or_else(|| tc.as_str())
                .unwrap_or("");
            match ty {
                "auto" | "" => openai["tool_choice"] = json!("auto"),
                "any" => openai["tool_choice"] = json!("required"),
                "none" => openai["tool_choice"] = json!("none"),
                "tool" => {
                    let name = tc.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    if web_search_names.contains(name) {
                        openai["tool_choice"] = json!({"type": "web_search"});
                    } else {
                        let short = if let Some(s) = tool_name_map.get(name) {
                            s.clone()
                        } else {
                            shorten_name_if_needed(name)
                        };
                        if short.is_empty() {
                            openai["tool_choice"] = json!("auto");
                        } else {
                            openai["tool_choice"] = json!({"type": "function", "name": short});
                        }
                    }
                }
                _ => openai["tool_choice"] = json!("auto"),
            }
        }
    }

    // --- parallel_tool_calls(默认开,disable_parallel_tool_use 关闭) ---
    let disable_parallel = body
        .get("tool_choice")
        .and_then(|tc| tc.get("disable_parallel_tool_use"))
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    openai["parallel_tool_calls"] = json!(!disable_parallel);

    // --- reasoning.effort(对齐 CPA thinking 分支,默认 medium) ---
    let effort = body
        .get("thinking")
        .and_then(crate::thinking::resolve_effort)
        .unwrap_or("medium");
    openai["reasoning"] = json!({"effort": effort});

    // --- service_tier:speed=fast → priority(对齐 CPA normalizeCodexServiceTier) ---
    if body.get("speed").and_then(|v| v.as_str()) == Some("fast") {
        openai["service_tier"] = json!("priority");
    }

    // --- codex 固定参数(对齐 CPA) ---
    openai["stream"] = json!(true);
    openai["store"] = json!(false);
    openai["include"] = json!(["reasoning.encrypted_content"]);

    // 反向映射 short→original,响应侧还原工具名
    let reverse = super::shorten::build_reverse_map(&tool_name_map);

    *body = openai;
    Ok(reverse)
}

/// role=system 消息的 reminder 文本(对齐 CPA ClaudeMessageSystemReminderText)
fn claude_system_reminder_text(content: Option<&Value>) -> Option<String> {
    let parts: Vec<String> = match content {
        Some(Value::String(s)) if !s.is_empty() && !super::is_attribution_text(s) => {
            vec![s.clone()]
        }
        Some(Value::Array(items)) => items
            .iter()
            .filter(|i| i.get("type").and_then(|v| v.as_str()) == Some("text"))
            .filter_map(|i| i.get("text").and_then(|v| v.as_str()))
            .filter(|t| !t.is_empty() && !super::is_attribution_text(t))
            .map(String::from)
            .collect(),
        _ => Vec::new(),
    };
    if parts.is_empty() {
        return None;
    }
    let text = parts.join("\n");
    if text.trim().is_empty() {
        return None;
    }
    Some(format!("<system-reminder>\n{text}\n</system-reminder>"))
}

/// call_id 超 64 字符确定性截短(对齐 CPA shortenCodexCallIDIfNeeded)
fn shorten_call_id(id: &str) -> String {
    const LIMIT: usize = 64;
    if id.len() <= LIMIT {
        return id.to_string();
    }
    use sha2::{Digest, Sha256};
    let digest = Sha256::digest(id.as_bytes());
    let suffix = format!("_{}", hex::encode(&digest[..8]));
    let mut prefix_len = LIMIT.saturating_sub(suffix.len());
    while prefix_len > 0 && !id.is_char_boundary(prefix_len) {
        prefix_len -= 1;
    }
    format!("{}{}", &id[..prefix_len], suffix)
}
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_system_goes_to_developer_message() {
        // 对齐 CPA:system → input[0] developer message,非 instructions
        let mut body = json!({
            "model": "test",
            "system": "You are helpful",
            "messages": [{"role": "user", "content": "hi"}]
        });
        let rev = convert_to_openai_responses(&mut body, "gpt-5").unwrap();
        assert_eq!(body["instructions"], "");
        assert_eq!(body["input"][0]["type"], "message");
        assert_eq!(body["input"][0]["role"], "developer");
        assert_eq!(body["input"][0]["content"][0]["type"], "input_text");
        assert_eq!(body["input"][0]["content"][0]["text"], "You are helpful");
        assert_eq!(body["input"][1]["content"][0]["text"], "hi");
        assert!(rev.is_empty(), "无工具时反向映射为空");
    }

    #[test]
    fn test_system_array_blocks_stay_separate_parts() {
        // 逐 block 独立 part,不 join(对齐 CPA appendSystemText)
        let mut body = json!({
            "model": "test",
            "system": [
                {"type": "text", "text": "Block 1"},
                {"type": "text", "text": "Block 2"}
            ],
            "messages": []
        });
        convert_to_openai_responses(&mut body, "gpt-5").unwrap();
        let content = body["input"][0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["text"], "Block 1");
        assert_eq!(content[1]["text"], "Block 2");
    }

    #[test]
    fn test_system_attribution_stripped() {
        let mut body = json!({
            "model": "test",
            "system": [
                {"type": "text", "text": "x-anthropic-billing-header: fp=abc"},
                {"type": "text", "text": "Real"}
            ],
            "messages": []
        });
        convert_to_openai_responses(&mut body, "gpt-5").unwrap();
        let content = body["input"][0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["text"], "Real");
    }

    #[test]
    fn test_tool_name_shortened_with_unique_suffix() {
        // 超长名截断;冲突加 _N(对齐 CPA buildShortNameMap)
        let a = "a".repeat(64) + "X";
        let b = "a".repeat(64) + "Y";
        let mut body = json!({
            "model": "test",
            "messages": [],
            "tools": [
                {"name": a.clone(), "description": "d", "input_schema": {"type": "object"}},
                {"name": b.clone(), "description": "d", "input_schema": {"type": "object"}},
                {"name": "short_tool", "description": "d", "input_schema": {"type": "object"}}
            ]
        });
        let rev = convert_to_openai_responses(&mut body, "gpt-5").unwrap();
        let sa = body["tools"][0]["name"].as_str().unwrap();
        let sb = body["tools"][1]["name"].as_str().unwrap();
        assert_eq!(sa.len(), 64);
        assert_eq!(sb.len(), 64);
        assert_eq!(body["tools"][2]["name"], "short_tool");
        // 反向映射还原原名
        assert_eq!(rev[sa], a);
        assert_eq!(rev[sb], b);
    }

    #[test]
    fn test_tool_use_name_uses_short_name() {
        let long = "mcp__".to_string() + &"x".repeat(80);
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": long.clone(), "input": {}}
                ]}
            ],
            "tools": [{"name": long.clone(), "input_schema": {"type": "object"}}]
        });
        let rev = convert_to_openai_responses(&mut body, "gpt-5").unwrap();
        let short = body["tools"][0]["name"].as_str().unwrap();
        assert_eq!(body["input"][0]["type"], "function_call");
        assert_eq!(body["input"][0]["name"], short);
        assert_eq!(rev[short], long);
    }

    #[test]
    fn test_tool_schema_normalized() {
        // 空 schema → 默认 object;type 缺失补 object;object 无 properties 补 {};strict=false
        let mut body = json!({
            "model": "test",
            "messages": [],
            "tools": [
                {"name": "a", "input_schema": null},
                {"name": "b", "input_schema": {"properties": {"x": {"type": "string"}}}},
                {"name": "c", "input_schema": {"type": "object", "properties": {}}}
            ]
        });
        convert_to_openai_responses(&mut body, "gpt-5").unwrap();
        assert_eq!(body["tools"][0]["parameters"], json!({"type": "object", "properties": {}}));
        assert_eq!(body["tools"][1]["parameters"]["type"], "object");
        assert_eq!(body["tools"][1]["strict"], false);
        assert!(body["tools"][1].get("input_schema").is_none(), "input_schema 应剥除");
        assert_eq!(body["tools"][2]["strict"], false);
    }

    #[test]
    fn test_tool_use_and_result() {
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "get_weather", "input": {"city": "beijing"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": "sunny"}
                ]}
            ]
        });
        convert_to_openai_responses(&mut body, "gpt-5").unwrap();
        assert_eq!(body["input"][0]["type"], "function_call");
        assert_eq!(body["input"][0]["call_id"], "t1");
        assert_eq!(body["input"][0]["name"], "get_weather");
        assert_eq!(body["input"][1]["type"], "function_call_output");
        assert_eq!(body["input"][1]["output"], "sunny");
    }

    #[test]
    fn test_thinking_signature_gpt_compat_kept_else_dropped() {
        // gAAAA 前缀 → reasoning 透传;'C' 开头(Claude 签名)→ 丢弃
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "t1", "signature": "gAAAA-claude-looking"},
                    {"type": "thinking", "thinking": "t2", "signature": "C4x2 weird"},
                    {"type": "text", "text": "answer"}
                ]}
            ]
        });
        convert_to_openai_responses(&mut body, "gpt-5").unwrap();
        assert_eq!(body["input"][0]["type"], "reasoning");
        assert_eq!(body["input"][0]["encrypted_content"], "gAAAA-claude-looking");
        assert_eq!(body["input"][1]["type"], "message");
        assert_eq!(body["input"][1]["content"][0]["text"], "answer");
    }

    #[test]
    fn test_thinking_grok_model_passes_any_signature() {
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "t", "signature": "C4x2 opaque"}
                ]}
            ]
        });
        convert_to_openai_responses(&mut body, "grok-3").unwrap();
        assert_eq!(body["input"][0]["type"], "reasoning");
        assert_eq!(body["input"][0]["encrypted_content"], "C4x2 opaque");
    }

    #[test]
    fn test_reasoning_effort_default_medium() {
        // 无 thinking → 默认 medium(对齐 CPA reasoningEffort 初值)
        let mut body = json!({"model": "test", "messages": []});
        convert_to_openai_responses(&mut body, "gpt-5").unwrap();
        assert_eq!(body["reasoning"]["effort"], "medium");
    }

    #[test]
    fn test_thinking_effort_mapping() {
        let mut body = json!({
            "model": "test",
            "thinking": {"type": "enabled", "budget_tokens": 8192},
            "messages": []
        });
        convert_to_openai_responses(&mut body, "gpt-5").unwrap();
        assert_eq!(body["reasoning"]["effort"], "medium");
    }

    #[test]
    fn test_adaptive_effort_uses_output_config() {
        let mut body = json!({
            "model": "test",
            "thinking": {"type": "adaptive", "output_config": {"effort": "high"}},
            "messages": []
        });
        convert_to_openai_responses(&mut body, "gpt-5").unwrap();
        assert_eq!(body["reasoning"]["effort"], "high");
    }

    #[test]
    fn test_service_tier_from_speed() {
        let mut body = json!({"model": "test", "messages": [], "speed": "fast"});
        convert_to_openai_responses(&mut body, "gpt-5").unwrap();
        assert_eq!(body["service_tier"], "priority");
    }

    #[test]
    fn test_codex_fixed_fields() {
        let mut body = json!({"model": "test", "messages": []});
        convert_to_openai_responses(&mut body, "gpt-5").unwrap();
        assert_eq!(body["stream"], true);
        assert_eq!(body["store"], false);
        assert_eq!(body["include"][0], "reasoning.encrypted_content");
        assert_eq!(body["parallel_tool_calls"], true);
    }

    #[test]
    fn test_parallel_tool_calls_disabled() {
        let mut body = json!({
            "model": "test",
            "messages": [],
            "tool_choice": {"type": "auto", "disable_parallel_tool_use": true}
        });
        convert_to_openai_responses(&mut body, "gpt-5").unwrap();
        assert_eq!(body["parallel_tool_calls"], false);
    }

    #[test]
    fn test_tool_choice_mappings() {
        let mut body = json!({"model": "test", "messages": [], "tool_choice": {"type": "any"}});
        convert_to_openai_responses(&mut body, "gpt-5").unwrap();
        assert_eq!(body["tool_choice"], "required");

        let mut body2 = json!({
            "model": "test",
            "messages": [],
            "tool_choice": {"type": "tool", "name": "search"},
            "tools": [{"name": "search", "input_schema": {"type": "object"}}]
        });
        convert_to_openai_responses(&mut body2, "gpt-5").unwrap();
        assert_eq!(body2["tool_choice"]["type"], "function");
        assert_eq!(body2["tool_choice"]["name"], "search");
    }

    #[test]
    fn test_web_search_tool_mapping() {
        let mut body = json!({
            "model": "test",
            "messages": [],
            "tools": [
                {"type": "web_search_20250305", "name": "web"},
                {"type": "function", "name": "f", "input_schema": {"type": "object"}}
            ]
        });
        convert_to_openai_responses(&mut body, "gpt-5").unwrap();
        assert_eq!(body["tools"][0]["type"], "web_search");
        assert_eq!(body["tools"][1]["type"], "function");
    }

    #[test]
    fn test_web_search_tool_choice() {
        let mut body = json!({
            "model": "test",
            "messages": [],
            "tool_choice": {"type": "tool", "name": "web"},
            "tools": [{"type": "web_search_20250305", "name": "web"}]
        });
        convert_to_openai_responses(&mut body, "gpt-5").unwrap();
        assert_eq!(body["tool_choice"]["type"], "web_search");
    }

    #[test]
    fn test_system_role_message_reminder() {
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "system", "content": "procedural note"},
                {"role": "user", "content": "hi"}
            ]
        });
        convert_to_openai_responses(&mut body, "gpt-5").unwrap();
        assert_eq!(body["input"][0]["type"], "message");
        assert_eq!(body["input"][0]["role"], "user");
        assert!(body["input"][0]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("<system-reminder>"));
    }

    #[test]
    fn test_image_block_to_data_url() {
        let mut body = json!({
            "model": "test",
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "image",
                    "source": {"type": "base64", "media_type": "image/png", "data": "aGVsbG8="}
                }]
            }]
        });
        convert_to_openai_responses(&mut body, "gpt-5").unwrap();
        assert_eq!(body["input"][0]["type"], "message");
        assert_eq!(
            body["input"][0]["content"][0]["image_url"],
            "data:image/png;base64,aGVsbG8="
        );
    }

    #[test]
    fn test_tool_result_array_output() {
        let mut body = json!({
            "model": "test",
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "t1",
                    "content": [
                        {"type": "text", "text": "result"},
                        {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "aGVsbG8="}}
                    ]
                }]
            }]
        });
        convert_to_openai_responses(&mut body, "gpt-5").unwrap();
        let output = body["input"][0]["output"].as_array().unwrap();
        assert_eq!(output[0]["type"], "input_text");
        assert_eq!(output[1]["type"], "input_image");
    }

    #[test]
    fn test_long_call_id_shortened() {
        let long_id = "toolu_".to_string() + &"x".repeat(80);
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": long_id.clone(), "name": "f", "input": {}}
                ]}
            ]
        });
        convert_to_openai_responses(&mut body, "gpt-5").unwrap();
        assert!(body["input"][0]["call_id"].as_str().unwrap().len() <= 64);
    }

    #[test]
    fn test_empty_and_null_messages_dropped() {
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "user", "content": null},
                {"role": "user"},
                {"role": "user", "content": "keep"}
            ]
        });
        convert_to_openai_responses(&mut body, "gpt-5").unwrap();
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["content"][0]["text"], "keep");
    }

    #[test]
    fn test_message_flushed_before_function_call() {
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "text", "text": "let me check"},
                    {"type": "tool_use", "id": "t1", "name": "search", "input": {}}
                ]}
            ]
        });
        convert_to_openai_responses(&mut body, "gpt-5").unwrap();
        assert_eq!(body["input"][0]["type"], "message");
        assert_eq!(body["input"][0]["content"][0]["text"], "let me check");
        assert_eq!(body["input"][1]["type"], "function_call");
    }
}
