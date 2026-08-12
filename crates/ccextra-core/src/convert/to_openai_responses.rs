// Anthropic → OpenAI responses 转换(对齐转换路径字段映射)
//
// 主要映射(逐条一致):
// - system → instructions 字段(合并 text blocks,过滤计费归属块,对齐 codex base_instructions)
// - messages → input[]:text→input_text/output_text、thinking(带 GPT 兼容签名)→reasoning、
//   image→input_image、tool_use→function_call、tool_result→function_call_output
//   (遇 thinking/tool_use/tool_result 先 flush 文本 message,对齐 flushMessage 顺序)
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

/// GPT 上游追加的行为适配块(字节固定,缓存前缀稳定)。
///
/// 入站系统词构造自 Claude Code(CLAUDE.md 等),不是 GPT 原生运行环境;
/// 本块以 codex gpt prompt 的结构为骨架(gpt_5_2_prompt 的身份/环境/执行/输出),
/// 改造成 Claude Code agent loop 语义,明确工具边界与权限,抑制冗长与过度探索。
/// 英文固定文本,不配置化;冲突时用户指令(CLAUDE.md)优先,本块只补缺省。
const GPT_ADAPTER_BLOCK: &str = "\
You are the model operating inside Claude Code's agent loop, not a standalone Codex session. The user interacts with Claude Code through this loop.

## Operating environment
Treat the supplied system instructions, CLAUDE.md, user instructions, declared tools, permission decisions, and tool results as your complete operating environment. User and project instructions take precedence over this block.
Your capabilities are exactly the tools declared in the current request: the built-in Claude Code tools (Read, Edit, Write, Bash, Glob, Grep) plus any additional declared tools. Use only declared tools and follow their schemas. Do not assume any additional tool or capability is available. Do not claim an action succeeded until its tool result confirms it.
For a brand-new task with no prior context, be ambitious; when working in an existing codebase, do exactly what the user asks with surgical precision and keep changes minimal and consistent with the codebase style.

## Working style
You are a coding agent: keep going until the task is fully resolved end-to-end within the current turn, then yield. Solve problems at the root cause rather than surface-level patches. Do not fix unrelated bugs or add unrequested work; you may mention them in the final message.
You may be in a dirty git worktree. Never revert existing changes you did not make. Never use destructive git commands (e.g. git reset --hard, git checkout --) unless the user explicitly requests them. Prefer git log / git blame for history context.
Parallelize independent tool calls whenever possible, especially reads. When searching files from Bash, prefer rg over grep when available. Run tests or builds only to verify your own change, not to explore.

## Output
Be concise. Default final answers under 10 lines; small changes 2-5 sentences; multi-file work 1-2 bullets per file. Never dump file contents, before/after pairs, or entire methods unless explicitly asked; reference file paths instead.
Don't expose extended reasoning — show conclusions, not the thought process.
Stop when the task is done: report result plus one logical next step, then yield.";

/// 判定上游是否为 GPT 模型(仅按模型名前缀,对齐约定:responses 协议 + gpt*)
fn is_gpt_upstream(upstream_model: &str) -> bool {
    upstream_model.to_ascii_lowercase().starts_with("gpt")
}

/// signature 是否 GPT 兼容(对齐 CompatibleSignatureForProvider(GPT, sig) 的轻量版)
///
/// 参考实现 用完整 Fernet 容器校验;这里按 OpenAI reasoning 签名的确定前缀
/// ("gAAAA" 或 base64 首字符 'g')判定,足以区分 Claude/Gemini 签名(以 'C'/'E'/'R'
/// 开头)与 GPT 签名。Grok 特例按模型名放行(对齐 codexClaudeTargetAcceptsGrokSignature)。
fn signature_compatible_gpt(signature: &str, upstream_model: &str) -> bool {
    let sig = signature.trim();
    if sig.is_empty() {
        return false;
    }
    let first = sig.as_bytes()[0];
    // GPT Fernet reasoning 首字节 0x80 → base64 首字符 'g'(一致 selfDescribingSignatureFirstChars 判定)
    let looks_gpt = first == b'g' || sig.starts_with("gAAAA");
    if looks_gpt {
        return true;
    }
    // Grok encrypted_content 无信封,参考实现 对 grok 目标单独放行
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

/// tool_result content → responses output 数组或字符串(对齐 tool_result 分支)
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
                                .or_else(|| {
                                    item.pointer("/source/mime_type").and_then(|v| v.as_str())
                                })
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

/// image block → data URL(对齐 image 分支)
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

/// Claude custom 工具 tool_use.input → 字符串(对齐 unwrapCustomToolInput)
///
/// 响应侧把 custom 工具字符串 input 包成 {"input": str} 对象发回;
/// 请求侧此处解包还原。格式不符时回退到原始文本。
fn unwrap_custom_tool_input(input: Option<&Value>) -> String {
    match input {
        Some(Value::Object(map)) => {
            if let Some(inner) = map.get("input") {
                match inner {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                }
            } else {
                input.map(|v| v.to_string()).unwrap_or_default()
            }
        }
        Some(Value::String(s)) => s.clone(),
        _ => String::new(),
    }
}

/// input_schema → parameters(对齐 normalizeToolParameters)
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

/// Claude system 文本块 → 单字符串(合并所有 text blocks,对齐 codex base_instructions)
///
/// 注：每个 block 先 trim 再合并,理由：
/// 1. codex 存储 base_instructions 为单字符串,合并时自然规范化空白
/// 2. Claude Code 系统提示程序生成,不含前导/尾随空白
/// 3. 即使上游发送空白块,trim 后更利于缓存命中(减少无意义字节差异)
fn system_to_instructions_text(system: &Value) -> String {
    let mut texts: Vec<String> = Vec::new();
    match system {
        Value::String(s) => {
            let trimmed = s.trim();
            if !trimmed.is_empty() && !super::is_attribution_text(trimmed) {
                texts.push(trimmed.to_string());
            }
        }
        Value::Array(blocks) => {
            for b in blocks {
                if b.get("type").and_then(|v| v.as_str()) == Some("text") {
                    if let Some(t) = b.get("text").and_then(|v| v.as_str()) {
                        let trimmed = t.trim();
                        if !trimmed.is_empty() && !super::is_attribution_text(trimmed) {
                            texts.push(trimmed.to_string());
                        }
                    }
                }
            }
        }
        _ => {}
    }
    texts.join("\n\n")
}

/// Anthropic messages → OpenAI responses
pub fn convert_to_openai_responses(
    body: &mut Value,
    upstream_model: &str,
) -> Result<HashMap<String, String>> {
    // --- system → instructions(对齐 codex base_instructions) ---
    let mut instructions = String::new();
    if let Some(system) = body.get("system") {
        instructions = system_to_instructions_text(system);
    }
    // GPT 上游追加适配块到 instructions 末尾
    if is_gpt_upstream(upstream_model) {
        if !instructions.is_empty() {
            instructions.push_str("\n\n");
        }
        instructions.push_str(GPT_ADAPTER_BLOCK);
    }

    let mut openai = json!({
        "model": upstream_model,
        "instructions": instructions,
        "input": [],
    });

    // --- 工具名缩短映射(对齐 buildReverseMapFromClaudeOriginalToShort) ---
    let mut tool_name_map: HashMap<String, String> = HashMap::new();
    let mut web_search_names: HashSet<String> = HashSet::new();
    // custom 工具(freeform,无 input_schema)→ Responses type:"custom",input 是字符串
    let mut custom_tool_names: HashSet<String> = HashSet::new();
    if let Some(tools) = body.get("tools").and_then(|v| v.as_array()) {
        let mut names: Vec<String> = Vec::new();
        for tool in tools {
            let tool_type = tool.get("type").and_then(|v| v.as_str()).unwrap_or("");
            if super::is_web_search_tool_type(tool_type) {
                if let Some(n) = tool.get("name").and_then(|v| v.as_str()) {
                    if !n.is_empty() {
                        web_search_names.insert(n.to_string());
                    }
                }
                continue;
            }
            if tool_type == "custom" {
                if let Some(n) = tool.get("name").and_then(|v| v.as_str()) {
                    if !n.is_empty() {
                        custom_tool_names.insert(n.to_string());
                    }
                }
            }
            if let Some(n) = tool.get("name").and_then(|v| v.as_str()) {
                if !n.is_empty() {
                    names.push(n.to_string());
                }
            }
        }
        tool_name_map = build_short_name_map(&names);
    }

    // --- messages → input[] ---
    // custom 工具调用的 call_id 集合(tool_result 需转 custom_tool_call_output)
    let mut custom_call_ids: HashSet<String> = HashSet::new();
    // 先合并连续同角色消息,对齐上游 ClaudeMessageAccumulator
    let merged_messages = body
        .get("messages")
        .and_then(|v| v.as_array())
        .map(|msgs| super::merge_consecutive_messages(msgs));
    if let Some(messages) = merged_messages.as_deref() {
        for msg in messages {
            let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("user");

            // role=system 消息:reminder 文本 → user message(对齐 ClaudeMessageSystemReminderText)
            if role == "system" {
                if let Some(text) = claude_system_reminder_text(msg.get("content")) {
                    openai["input"].as_array_mut().unwrap().push(json!({
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

            // 字符串内容(对齐 extractStandardInputTextContent:空串直接跳过)
            if let Some(s) = content.as_str() {
                if s.is_empty() {
                    continue;
                }
                let item_type = if role == "assistant" {
                    "output_text"
                } else {
                    "input_text"
                };
                openai["input"].as_array_mut().unwrap().push(json!({
                    "type": "message", "role": role, "content": vec![json!({"type": item_type, "text": s})]
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
                            let item_type = if role == "assistant" {
                                "output_text"
                            } else {
                                "input_text"
                            };
                            content_items.push(json!({"type": item_type, "text": t}));
                        }
                    }
                    "thinking" => {
                        // 带 GPT 兼容签名的思考 → 独立 reasoning item(先 flush 保序);
                        // 无签名/不兼容签名丢弃,不退化为纯文本(对齐 appendReasoningContent)
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
                        let is_custom = custom_tool_names.contains(name);
                        let short_id = shorten_call_id(id);
                        if is_custom {
                            // custom 工具:Claude tool_use.input 是 {"input": str} 对象,
                            // 解包回字符串,发 custom_tool_call(对齐转换 custom 分支)
                            custom_call_ids.insert(short_id.clone());
                            let input_str = unwrap_custom_tool_input(part.get("input"));
                            out_items.push(json!({
                                "type": "custom_tool_call",
                                "call_id": short_id,
                                "name": short_name,
                                "input": input_str
                            }));
                        } else {
                            let args = part
                                .get("input")
                                .map(|i| i.to_string())
                                .unwrap_or_else(|| "{}".to_string());
                            out_items.push(json!({
                                "type": "function_call",
                                "call_id": short_id,
                                "name": short_name,
                                "arguments": args
                            }));
                        }
                    }
                    "tool_result" => {
                        flush_message(&mut content_items, &mut out_items);
                        let call_id = part
                            .get("tool_use_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let output = tool_result_output(part.get("content").unwrap_or(&json!("")));
                        let short_id = shorten_call_id(call_id);
                        if custom_call_ids.contains(&short_id) {
                            out_items.push(json!({
                                "type": "custom_tool_call_output",
                                "call_id": short_id,
                                "output": output
                            }));
                        } else {
                            out_items.push(json!({
                                "type": "function_call_output",
                                "call_id": short_id,
                                "output": output
                            }));
                        }
                    }
                    _ => {}
                }
            }
            flush_message(&mut content_items, &mut out_items);
            openai["input"].as_array_mut().unwrap().extend(out_items);
        }
    }

    // --- tools → codex tools(对齐 tools 分支) ---
    if let Some(tools) = body.get("tools").and_then(|v| v.as_array()) {
        let mut tool_items: Vec<Value> = Vec::new();
        for tool in tools {
            let tool_type = tool.get("type").and_then(|v| v.as_str()).unwrap_or("");
            // web search 工具特殊映射(对齐 convertClaudeWebSearchToolToCodex)
            if super::is_web_search_tool_type(tool_type) {
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
            if tool_type == "custom" {
                // custom 工具保留 type,不套 input_schema(对齐转换 custom 分支)
                if let Some(obj) = t.as_object_mut() {
                    obj.remove("input_schema");
                    obj.remove("cache_control");
                    obj.remove("defer_loading");
                }
                tool_items.push(t);
                continue;
            }
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
            // 剥 codex 不认的字段(一致)
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

    // --- tool_choice(对齐 convertClaudeToolChoiceToCodex) ---
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
                    } else if !tool_name_map.contains_key(name) {
                        // 命名工具未声明 → 降级 auto(对齐 chat 侧 declared 校验;
                        // sub2api responses 侧无该校验,这里补同一语义)
                        openai["tool_choice"] = json!("auto");
                    } else {
                        // 已校验声明:map 必有非空 short,不再判空
                        let short = tool_name_map.get(name).cloned().unwrap();
                        if custom_tool_names.contains(name) {
                            openai["tool_choice"] = json!({"type": "custom", "name": short});
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

    // --- reasoning.effort(对齐 thinking 分支,默认 medium) ---
    // 保留入站 reasoning.effort,钳制到模型支持级别(对齐 codex compact.rs:704 保留 turn_context.reasoning_effort)
    let effort = crate::thinking::resolve_effort_from_body(body).unwrap_or("medium");
    let effort = crate::thinking::clamp_effort(effort, upstream_model);
    openai["reasoning"] = json!({"effort": effort});

    // --- service_tier:speed=fast → priority(对齐 normalizeCodexServiceTier) ---
    if body.get("speed").and_then(|v| v.as_str()) == Some("fast") {
        openai["service_tier"] = json!("priority");
    }

    // --- codex 固定参数(一致) ---
    // stream 保留入站值(对齐 to_openai_chat)
    openai["stream"] = body.get("stream").unwrap_or(&json!(true)).clone();
    openai["store"] = json!(false);
    openai["include"] = json!(["reasoning.encrypted_content"]);

    // 反向映射 short→original,响应侧还原工具名
    let reverse = super::shorten::build_reverse_map(&tool_name_map);

    *body = openai;
    Ok(reverse)
}

/// role=system 消息的 reminder 文本(对齐 ClaudeMessageSystemReminderText)
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

/// call_id 超 64 字符确定性截短(对齐 shortenCodexCallIDIfNeeded)
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
    fn test_system_goes_to_instructions() {
        // 新行为：system → instructions 字段，而非 developer message
        let mut body = json!({
            "model": "test",
            "system": "You are helpful",
            "messages": [{"role": "user", "content": "hi"}]
        });
        let rev = convert_to_openai_responses(&mut body, "test-model").unwrap();
        assert_eq!(body["instructions"], "You are helpful");
        assert_eq!(body["input"][0]["role"], "user");
        assert_eq!(body["input"][0]["content"][0]["text"], "hi");
        assert!(rev.is_empty(), "无工具时反向映射为空");
    }

    #[test]
    fn test_gpt_adapter_block_appended_to_instructions() {
        // gpt 上游：GPT_ADAPTER_BLOCK 追加到 instructions 末尾
        let mut body = json!({
            "model": "test",
            "system": "You are helpful",
            "messages": []
        });
        convert_to_openai_responses(&mut body, "gpt-5.6-terra").unwrap();
        let instructions = body["instructions"].as_str().unwrap();
        // 精确验证结构：system + \n\n + adapter
        let expected = format!("You are helpful\n\n{}", GPT_ADAPTER_BLOCK);
        assert_eq!(instructions, expected);
        // input 应该为空（没有 messages）
        assert_eq!(body["input"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn test_gpt_adapter_creates_instructions_when_no_system() {
        // 空 system：instructions 只包含 GPT_ADAPTER_BLOCK
        let mut body = json!({
            "model": "test",
            "messages": [{"role": "user", "content": "hi"}]
        });
        convert_to_openai_responses(&mut body, "gpt-5.6-sol").unwrap();
        assert_eq!(body["instructions"], GPT_ADAPTER_BLOCK);
        let input = body["input"].as_array().unwrap();
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[0]["content"][0]["text"], "hi");
    }

    #[test]
    fn test_non_gpt_no_adapter_block() {
        // 非 gpt 上游不注入 adapter block
        let mut body = json!({
            "model": "test",
            "system": "You are helpful",
            "messages": []
        });
        convert_to_openai_responses(&mut body, "claude-opus-5").unwrap();
        assert_eq!(body["instructions"], "You are helpful");
        assert_eq!(body["input"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn test_adapter_block_describes_claude_code_environment() {
        assert!(GPT_ADAPTER_BLOCK
            .contains("inside Claude Code's agent loop, not a standalone Codex session."));
        assert!(
            GPT_ADAPTER_BLOCK.contains("The user interacts with Claude Code through this loop.")
        );
        assert!(GPT_ADAPTER_BLOCK.contains("Your capabilities are exactly the tools declared"));
        assert!(GPT_ADAPTER_BLOCK
            .contains("Do not assume any additional tool or capability is available."));
        assert!(GPT_ADAPTER_BLOCK.contains("Read, Edit, Write, Bash, Glob, Grep"));
        assert!(GPT_ADAPTER_BLOCK
            .contains("Do not claim an action succeeded until its tool result confirms it."));
        assert!(GPT_ADAPTER_BLOCK.contains("Never revert existing changes you did not make."));
        assert!(GPT_ADAPTER_BLOCK.contains("Never use destructive git commands"));
        assert!(GPT_ADAPTER_BLOCK
            .contains("Never dump file contents, before/after pairs, or entire methods"));
    }

    #[test]
    fn test_gpt_adapter_supports_declared_apply_patch() {
        let mut body = json!({
            "model": "test",
            "messages": [],
            "tool_choice": {"type": "tool", "name": "apply_patch"},
            "tools": [{"type": "custom", "name": "apply_patch", "description": "d"}]
        });

        convert_to_openai_responses(&mut body, "gpt-5.6-terra").unwrap();

        // adapter 现在在 instructions 里
        let instructions = body["instructions"].as_str().unwrap();
        assert!(instructions
            .contains("inside Claude Code's agent loop, not a standalone Codex session."));
        assert!(instructions.contains("built-in Claude Code tools (Read, Edit, Write, Bash"));
        assert!(!instructions.contains("Never use apply_patch"));
        assert!(!instructions.contains("Edit files only with the tools Claude Code provides"));
        assert_eq!(body["tools"][0]["type"], "custom");
        assert_eq!(body["tools"][0]["name"], "apply_patch");
        assert_eq!(body["tool_choice"]["type"], "custom");
        assert_eq!(body["tool_choice"]["name"], "apply_patch");
    }

    #[test]
    fn test_gpt_match_case_insensitive() {
        // 前缀判定不区分大小写
        assert!(is_gpt_upstream("GPT-5.6-terra"));
        assert!(is_gpt_upstream("gpt-5.6-terra"));
        assert!(!is_gpt_upstream("claude-opus-5"));
        assert!(!is_gpt_upstream("o3-mini"));
    }

    #[test]
    fn test_system_array_blocks_merged_to_instructions() {
        // system blocks 合并到 instructions 字段(对齐 codex base_instructions)
        let mut body = json!({
            "model": "test",
            "system": [
                {"type": "text", "text": "Block 1"},
                {"type": "text", "text": "Block 2"}
            ],
            "messages": []
        });
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        assert_eq!(body["instructions"], "Block 1\n\nBlock 2");
        // input 应该为空（没有 developer message）
        assert_eq!(body["input"].as_array().unwrap().len(), 0);
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
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        // attribution 被过滤，只保留 "Real"
        assert_eq!(body["instructions"], "Real");
    }

    #[test]
    fn test_system_whitespace_only_blocks_dropped() {
        let mut body = json!({
            "model": "test",
            "system": [
                {"type": "text", "text": "  \n  "},
                {"type": "text", "text": "Content"},
                {"type": "text", "text": ""},
            ],
            "messages": []
        });
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        // 纯空白块被 trim 后丢弃
        assert_eq!(body["instructions"], "Content");
    }

    #[test]
    fn test_system_blocks_trimmed_before_join() {
        let mut body = json!({
            "model": "test",
            "system": [
                {"type": "text", "text": "  Block 1  "},
                {"type": "text", "text": "\n\nBlock 2\n"},
            ],
            "messages": []
        });
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        // 每个块先 trim 再用 \n\n 连接
        assert_eq!(body["instructions"], "Block 1\n\nBlock 2");
    }

    #[test]
    fn test_tool_name_shortened_with_unique_suffix() {
        // 超长名截断;冲突加 _N(对齐 buildShortNameMap)
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
        let rev = convert_to_openai_responses(&mut body, "test-model").unwrap();
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
        let rev = convert_to_openai_responses(&mut body, "test-model").unwrap();
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
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        assert_eq!(
            body["tools"][0]["parameters"],
            json!({"type": "object", "properties": {}})
        );
        assert_eq!(body["tools"][1]["parameters"]["type"], "object");
        assert_eq!(body["tools"][1]["strict"], false);
        assert!(
            body["tools"][1].get("input_schema").is_none(),
            "input_schema 应剥除"
        );
        assert_eq!(body["tools"][2]["strict"], false);
    }

    #[test]
    fn test_custom_tool_round_trip() {
        // custom 工具声明保留 type;tool_use → custom_tool_call(input 解包);
        // tool_result → custom_tool_call_output
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "apply_patch",
                     "input": {"input": "patch-content"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": "ok"}
                ]}
            ],
            "tools": [
                {"type": "custom", "name": "apply_patch", "description": "d"}
            ]
        });
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        // 声明保持 custom,不套 input_schema
        assert_eq!(body["tools"][0]["type"], "custom");
        assert!(body["tools"][0].get("input_schema").is_none());
        // tool_use → custom_tool_call,字符串 input 解包
        assert_eq!(body["input"][0]["type"], "custom_tool_call");
        assert_eq!(body["input"][0]["name"], "apply_patch");
        assert_eq!(body["input"][0]["input"], "patch-content");
        // tool_result → custom_tool_call_output
        assert_eq!(body["input"][1]["type"], "custom_tool_call_output");
        assert_eq!(body["input"][1]["output"], "ok");
    }

    #[test]
    fn test_custom_tool_choice_mapping() {
        let mut body = json!({
            "model": "test",
            "messages": [],
            "tool_choice": {"type": "tool", "name": "apply_patch"},
            "tools": [{"type": "custom", "name": "apply_patch", "description": "d"}]
        });
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        assert_eq!(body["tool_choice"]["type"], "custom");
        assert_eq!(body["tool_choice"]["name"], "apply_patch");
    }

    #[test]
    fn test_regular_tool_unaffected_by_custom() {
        // 非 custom 工具仍走 function_call 路径
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "get_weather", "input": {"city": "bj"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": "sunny"}
                ]}
            ],
            "tools": [{"name": "get_weather", "input_schema": {"type": "object"}}]
        });
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        assert_eq!(body["input"][0]["type"], "function_call");
        assert_eq!(body["input"][1]["type"], "function_call_output");
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
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        assert_eq!(body["input"][0]["type"], "function_call");
        assert_eq!(body["input"][0]["call_id"], "t1");
        assert_eq!(body["input"][0]["name"], "get_weather");
        assert_eq!(body["input"][1]["type"], "function_call_output");
        assert_eq!(body["input"][1]["output"], "sunny");
    }

    #[test]
    fn test_consecutive_assistant_turns_merged() {
        // 连续 assistant 消息(thinking + text/tool)合并为一条输入序列,
        // 保序:message(output_text) → function_call。
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "assistant", "content": [{"type": "thinking", "thinking": "t1"}]},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "answer"},
                    {"type": "tool_use", "id": "c1", "name": "Read", "input": {"p": "a"}}
                ]}
            ]
        });
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 2);
        // 无签名 thinking 丢弃 —— 不产生 reasoning item
        assert!(input.iter().all(|i| i["type"] != "reasoning"));
        assert_eq!(input[0]["type"], "message");
        assert_eq!(input[0]["content"][0]["text"], "answer");
        assert_eq!(input[1]["type"], "function_call");
        assert_eq!(input[1]["call_id"], "c1");
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
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        assert_eq!(body["input"][0]["type"], "reasoning");
        assert_eq!(
            body["input"][0]["encrypted_content"],
            "gAAAA-claude-looking"
        );
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
        // 无 thinking → 默认 medium(对齐 reasoningEffort 初值)
        let mut body = json!({"model": "test", "messages": []});
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        assert_eq!(body["reasoning"]["effort"], "medium");
    }

    #[test]
    fn test_thinking_effort_mapping() {
        let mut body = json!({
            "model": "test",
            "thinking": {"type": "enabled", "budget_tokens": 8192},
            "messages": []
        });
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        assert_eq!(body["reasoning"]["effort"], "medium");
    }

    #[test]
    fn test_adaptive_effort_uses_output_config() {
        let mut body = json!({
            "model": "test",
            "thinking": {"type": "adaptive", "output_config": {"effort": "high"}},
            "messages": []
        });
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        assert_eq!(body["reasoning"]["effort"], "high");
    }

    #[test]
    fn test_max_effort_downgraded_to_xhigh() {
        // glm-5.1 注册表支持到 xhigh,max 自动降级
        let mut body = json!({
            "model": "test",
            "output_config": {"effort": "max"},
            "thinking": {"type": "adaptive"},
            "messages": []
        });
        convert_to_openai_responses(&mut body, "glm-5.1").unwrap();
        assert_eq!(body["reasoning"]["effort"], "xhigh");
    }

    #[test]
    fn test_service_tier_from_speed() {
        let mut body = json!({"model": "test", "messages": [], "speed": "fast"});
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        assert_eq!(body["service_tier"], "priority");
    }

    #[test]
    fn test_effort_preserved_from_body() {
        // 保留入站 effort(output_config.effort 显式 high),对齐 codex compact.rs:704
        // 保留 turn_context.reasoning_effort,不因 compact 或其他路径强制覆盖
        let mut body = json!({
            "model": "test",
            "messages": [],
            "output_config": {"effort": "high"}
        });
        convert_to_openai_responses(&mut body, "gpt-5.6-terra").unwrap();
        assert_eq!(body["reasoning"]["effort"], "high");
    }

    #[test]
    fn test_effort_clamped_to_model() {
        // 超出模型支持上限时钳制到最近级别(glm-5.1 最高 xhigh,max 降为 xhigh)
        let mut body = json!({
            "model": "test",
            "messages": [],
            "output_config": {"effort": "max"}
        });
        convert_to_openai_responses(&mut body, "glm-5.1").unwrap();
        assert_eq!(body["reasoning"]["effort"], "xhigh");
    }

    #[test]
    fn test_codex_fixed_fields() {
        let mut body = json!({"model": "test", "messages": []});
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        // 无 stream 字段默认流式(与历史行为一致)
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
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        assert_eq!(body["parallel_tool_calls"], false);
    }

    #[test]
    fn test_tool_choice_mappings() {
        let mut body = json!({"model": "test", "messages": [], "tool_choice": {"type": "any"}});
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        assert_eq!(body["tool_choice"], "required");

        let mut body2 = json!({
            "model": "test",
            "messages": [],
            "tool_choice": {"type": "tool", "name": "search"},
            "tools": [{"name": "search", "input_schema": {"type": "object"}}]
        });
        convert_to_openai_responses(&mut body2, "test-model").unwrap();
        assert_eq!(body2["tool_choice"]["type"], "function");
        assert_eq!(body2["tool_choice"]["name"], "search");

        // 命名 choice 指向未声明工具 → 降级 auto
        let mut body3 = json!({
            "model": "test",
            "messages": [],
            "tool_choice": {"type": "tool", "name": "ghost"},
            "tools": [{"name": "search", "input_schema": {"type": "object"}}]
        });
        convert_to_openai_responses(&mut body3, "test-model").unwrap();
        assert_eq!(body3["tool_choice"], "auto");
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
        convert_to_openai_responses(&mut body, "test-model").unwrap();
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
        convert_to_openai_responses(&mut body, "test-model").unwrap();
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
        convert_to_openai_responses(&mut body, "test-model").unwrap();
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
        convert_to_openai_responses(&mut body, "test-model").unwrap();
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
        convert_to_openai_responses(&mut body, "test-model").unwrap();
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
        convert_to_openai_responses(&mut body, "test-model").unwrap();
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
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["content"][0]["text"], "keep");
    }

    #[test]
    fn test_empty_string_content_dropped_in_responses() {
        // 对齐 extractStandardInputTextContent:空串不产出 message item
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "user", "content": ""},
                {"role": "assistant", "content": ""},
                {"role": "user", "content": "real"}
            ]
        });
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 1, "空串 message 应被跳过");
        assert_eq!(input[0]["content"][0]["text"], "real");
    }

    #[test]
    fn test_empty_array_content_no_output() {
        // 空 content 数组 → 经 flush_message 检查后无 item 产出
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "user", "content": []},
                {"role": "user", "content": "keep"}
            ]
        });
        convert_to_openai_responses(&mut body, "test-model").unwrap();
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
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        assert_eq!(body["input"][0]["type"], "message");
        assert_eq!(body["input"][0]["content"][0]["text"], "let me check");
        assert_eq!(body["input"][1]["type"], "function_call");
    }
}
