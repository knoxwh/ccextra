// Anthropic → OpenAI responses 转换(对齐转换路径字段映射)
//
// 主要映射(逐条一致):
// - system → instructions 字段(合并 text blocks,过滤计费归属块,对齐 codex base_instructions)
// - messages → input[]:text→input_text/output_text、thinking(带签名)→reasoning、
//   image→input_image、tool_use→function_call、tool_result→function_call_output
//   (遇 thinking/tool_use/tool_result 先 flush 文本 message,对齐 flushMessage 顺序;
//    tool_result 内嵌 image 抽出到随后 user message,output 不携带图片)
// - thinking 处理:GPT/Grok 仅回放带签名的 encrypted reasoning,无签名明文丢弃
//   (对齐 reasoning replay 缓存只认 encrypted_content;其余 responses 上游保留明文)
// - tools → codex tools(原名超 64 缩短 + 唯一 _N 后缀;web_search_* → type:"web_search")、
//   input_schema→parameters(strict=false,剥 cache_control/defer_loading/$schema)
// - thinking.budget_tokens → reasoning.effort(直映射,不钳制)
// - service_tier: speed=fast → priority
// - output_config.format(json_schema)→ text.format(name 缺省、strict 默认 true)
// - store=false;include=["reasoning.encrypted_content"];parallel_tool_calls
//
// 返回 short→original 工具名映射,响应侧还原原名(对齐 buildReverseMap...)。

use std::collections::{HashMap, HashSet};

use serde_json::{json, Value};

use super::gemini_schema::inline_local_refs;
use super::shorten::{build_short_name_map, shorten_name_if_needed};
use super::signature::{
    compatible_signature_for_provider_block, is_valid_gpt_reasoning_signature,
    is_valid_grok_encrypted_content, SignatureBlockKind, SignatureProvider,
};
use super::Result;

/// Grok 上游追加的行为适配块(字节固定,缓存前缀稳定)。
///
/// 直接对齐官方 grok-build prompt.md 核心约束,仅替换环境声明为 Claude Code。
/// 保留官方 prompt 的工作策略(L4-11)、工具调用(L14-16)、沟通风格(L34-48)精髓。
/// 英文固定文本,不配置化;冲突时用户指令(CLAUDE.md)优先,本块只补缺省。
const GROK_ADAPTER_BLOCK: &str = "\
You are operating inside Claude Code's agent loop. Your main goal is to complete the user's request. The supplied system instructions, CLAUDE.md, declared tools, and tool results are your complete operating environment. Your capabilities are exactly the tools declared in the current request: the built-in Claude Code tools (Read, Edit, Write, Bash, LSP, Agent, WebFetch, WebSearch, TaskCreate/Update/List, and others) plus any additional declared tools.

Always respond in Simplified Chinese (简体中文). Use Simplified Chinese for all explanations, communications, and user-facing messages. Technical terms, code identifiers, file paths, command names, and error strings should remain in their original form.

## Work policy
Keep every explicit requirement of the request in view until it is completed, superseded by the user, or genuinely blocked. If something is blocked, say so plainly rather than quietly dropping it.
Match your response to the user's intent. Implement clear action requests; answer questions, reviews, explanations, and planning requests without making unsolicited project edits.
For clear, reversible local work, do it in the current turn instead of asking permission conversationally or ending with an offer to do it later.
Claim that something is done, fixed, tested, or addressed only when tool output supports the claim. Otherwise state what you did not verify and why.
Keep changes scoped to what was asked. Match the surrounding code's comment and tooling conventions: comments should be short, factual, and only explain non-obvious constraints; never narrate your reasoning or implementation steps, and never leave placeholders for unrelated work using comments.

## Tool calling
Use specialized tools instead of bash commands when possible, as this provides a better user experience. For file operations, prefer dedicated file tools (Read for reading files instead of cat/head/tail, Edit for editing and creating files instead of sed/awk). Reserve bash tools exclusively for actual system commands and terminal operations that require shell execution.
NEVER use bash echo or other command-line tools to communicate thoughts, explanations, or instructions to the user. Output all communication directly in your response text instead.

## Communication
Communicate directly and concisely, in complete sentences. Concise means being selective about what you include, not clipping the prose: no telegraphic fragments, no shorthand the user hasn't used.

Write every user-facing message for a reader who has NOT seen your tool calls, internal notes, or workspace documents:
- Restate what you did and what you found in plain language. Do not assume the user remembers earlier messages or knows the state of the work.
- Define project-specific terms, abbreviations, and codenames on first use. Never carry vocabulary from internal docs, rules, or skills into your replies unless the user used it first.
- State facts literally. Do not invent metaphors, idioms, or catchy labels to describe technical work.

Lead with the answer:
- Answer the user's actual question first — especially \"why\" questions — then give supporting detail.
- Open with what is true or what to do. Do not open answers or sections with negations (\"It's not X\") or \"Do not...\" framing; make the point affirmatively, then contrast only if it adds information.
- If the question is answerable from context, answer it. Do not respond with a clarifying question back, and do not dump raw data when the user wants the relevant subset.

Keep intermediate progress updates short and infrequent. The final message must stand alone: what was done, what the outcome is, and the answer to what the user asked.

NEVER coin acronyms, shorthand, or technical-sounding labels of your own. ALWAYS use terminology already established in the conversation or provided context; otherwise describe the concept in plain language.";

/// 判定上游是否为 GPT 模型(仅按模型名前缀,对齐约定:responses 协议 + gpt*)
fn is_gpt_upstream(upstream_model: &str) -> bool {
    upstream_model.to_ascii_lowercase().starts_with("gpt")
}

/// 判定上游是否为 Grok 模型(按模型名包含 grok)
fn is_grok_upstream(upstream_model: &str) -> bool {
    upstream_model.to_ascii_lowercase().contains("grok")
}

/// 判定上游是否需要注入 developer message + adapter block(仅 Grok 保留适配块)
fn needs_adapter_block(upstream_model: &str) -> bool {
    is_grok_upstream(upstream_model)
}

/// thinking signature → 可回放给目标上游的 reasoning.encrypted_content
/// (对齐 CPA codex_claude_request.appendReasoningContent):
/// GPT 目标走兼容性解析(剥 provider 前缀 + Fernet 形状校验);
/// grok 目标无信封,按来源确认后做形状校验;其余一律丢弃。
fn gpt_compatible_signature(signature: Option<&str>, upstream_model: &str) -> Option<String> {
    let raw = signature.unwrap_or("").trim();
    if let Some(normalized) = compatible_signature_for_provider_block(
        SignatureProvider::Gpt,
        raw,
        SignatureBlockKind::Unknown,
    ) {
        return Some(normalized);
    }
    // 空签名:CPA 仅在 preserveEmptyThinkingBlocks 时保留空 encrypted_content,
    // 该开关未移植,ccextra 一律丢弃无签名 thinking
    if raw.is_empty() {
        return None;
    }
    if is_grok_upstream(upstream_model) && is_valid_grok_encrypted_content(raw) {
        return Some(raw.to_string());
    }
    None
}

/// 移除 reasoning 项的孤儿 id(encrypted_content 已无且 store 非 true 时)
fn remove_reasoning_id_if_orphan(item: &mut Value, store_true: bool) -> bool {
    if !store_true && item.get("encrypted_content").is_none() && item.get("id").is_some() {
        if let Some(obj) = item.as_object_mut() {
            obj.remove("id");
            return true;
        }
    }
    false
}

/// GPT/Codex 请求发送前仅剥离格式无效的 encrypted_content。
/// `store` 非 true 时顺带丢掉无法回放的 reasoning id；空 reasoning 项整项丢弃。
pub fn sanitize_gpt_reasoning_items(body: &mut Value) -> bool {
    let store_true = body.get("store").and_then(|v| v.as_bool()) == Some(true);
    let Some(input) = body.get_mut("input").and_then(|v| v.as_array_mut()) else {
        return false;
    };
    let mut changed = false;
    let mut kept = Vec::with_capacity(input.len());
    for mut item in input.drain(..) {
        if item.get("type").and_then(|v| v.as_str()) != Some("reasoning") {
            kept.push(item);
            continue;
        }
        let invalid = match item.get("encrypted_content") {
            Some(Value::String(content)) => !is_valid_gpt_reasoning_signature(content),
            Some(_) => true,
            None => false,
        };
        if invalid {
            if let Some(obj) = item.as_object_mut() {
                obj.remove("encrypted_content");
            }
            changed = true;
        }
        if remove_reasoning_id_if_orphan(&mut item, store_true) {
            changed = true;
        }
        if reasoning_item_empty(&item) {
            changed = true;
            continue;
        }
        kept.push(item);
    }
    *input = kept;
    changed
}

/// 上游 400 `invalid_encrypted_content` / thinking signature invalid 时,
/// 剥离 Responses `input[]` reasoning 项的 `encrypted_content` 再重试。
/// 对齐 CPA sanitizeOpenAIResponsesReasoningEncryptedContent 的剥离动作
/// (重试路径全剥,不做形状校验)。剥后无 content/summary 的空项整项丢掉。
/// `store` 非 true 时顺带丢掉孤儿 `id`(防 store=false 的 lookup 400)。
pub fn trim_encrypted_reasoning_items(body: &mut Value) -> bool {
    let store_true = body.get("store").and_then(|v| v.as_bool()) == Some(true);
    let Some(input) = body.get_mut("input").and_then(|v| v.as_array_mut()) else {
        return false;
    };
    let mut changed = false;
    let mut kept: Vec<Value> = Vec::with_capacity(input.len());
    for mut item in input.drain(..) {
        if item.get("type").and_then(|v| v.as_str()) != Some("reasoning") {
            kept.push(item);
            continue;
        }
        if item.get("encrypted_content").is_some() {
            if let Some(obj) = item.as_object_mut() {
                obj.remove("encrypted_content");
            }
            changed = true;
        }
        if remove_reasoning_id_if_orphan(&mut item, store_true) {
            changed = true;
        }
        if reasoning_item_empty(&item) {
            changed = true;
            continue;
        }
        kept.push(item);
    }
    *input = kept;
    changed
}

/// 上游错误 body 是否 thinking 签名无效。
/// Codex: `invalid_encrypted_content` / `invalid signature in thinking block`
/// (CPA thinking_signature_invalid)。xAI/grok: `Could not decrypt`。
pub fn is_thinking_signature_invalid(body: &[u8]) -> bool {
    let lower = String::from_utf8_lossy(body).to_ascii_lowercase();
    lower.contains("invalid_encrypted_content")
        || lower.contains("invalid signature in thinking block")
        || lower.contains("could not decrypt")
}

fn reasoning_item_empty(item: &Value) -> bool {
    let content_empty = match item.get("content") {
        None | Some(Value::Null) => true,
        Some(Value::String(s)) => s.is_empty(),
        Some(Value::Array(a)) => a.is_empty(),
        Some(_) => false,
    };
    let summary_empty = match item.get("summary") {
        None | Some(Value::Null) => true,
        Some(Value::String(s)) => s.is_empty(),
        Some(Value::Array(a)) => a.is_empty(),
        Some(_) => false,
    };
    content_empty && summary_empty && item.get("encrypted_content").is_none()
}

/// tool_result content → responses output 与图片 parts 元组(对齐 convertToolResultOutput)。
/// function_call_output.output 只接受文本,内嵌 image 抽出为元组第二项,
/// 由调用方追加成随后的 user message(input_image parts)。
fn tool_result_output(content: &Value) -> (Value, Vec<Value>) {
    match content {
        Value::Array(items) => {
            let mut out: Vec<Value> = Vec::new();
            let mut images: Vec<Value> = Vec::new();
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
                            images.push(json!({
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
                // 无文本可留:抽取过图片用占位(对齐 sub2api "(empty)"),
                // 否则维持原样序列化,未识别结构不丢信息
                if images.is_empty() {
                    (Value::String(content.to_string()), images)
                } else {
                    (Value::String("(no output)".to_string()), images)
                }
            } else {
                (Value::Array(out), images)
            }
        }
        Value::String(_) => (content.clone(), Vec::new()),
        other => (other.clone(), Vec::new()),
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

/// document block → input_file(对齐 CPA appendDocumentContent:
/// 仅支持 base64 + application/pdf,输出 type=input_file,filename=document.pdf)
fn document_to_input_file(part: &Value) -> Option<Value> {
    let source = part.get("source")?;
    if source.get("type").and_then(|v| v.as_str()) != Some("base64") {
        return None;
    }
    let media_type = source
        .get("media_type")
        .or_else(|| source.get("mime_type"))
        .and_then(|v| v.as_str())
        .map(str::trim)?;
    if !media_type.eq_ignore_ascii_case("application/pdf") {
        return None;
    }
    let data = source
        .get("data")
        .or_else(|| source.get("base64"))
        .and_then(|v| v.as_str())?;
    if data.is_empty() {
        return None;
    }
    Some(json!({
        "type": "input_file",
        "file_data": format!("data:{media_type};base64,{data}"),
        "filename": "document.pdf"
    }))
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
    // 本地 $ref 内联;内联发生后删除 $defs/definitions 容器
    // (对齐 CPA normalizeXAITool InlineLocalRefs)
    let mut inlined_owned;
    let schema: &Value = {
        let inlined = inline_local_refs(schema);
        if &inlined == schema {
            schema
        } else {
            inlined_owned = inlined;
            if let Some(obj) = inlined_owned.as_object_mut() {
                obj.remove("$defs");
                obj.remove("definitions");
            }
            &inlined_owned
        }
    };
    // xAI 系上游(对齐 CPA normalizeXAIObjectRootUnionBranchTypes +
    // xaiFunctionParametersNeedSimplification):root 为 object 且带 root union 时,
    // 先补缺失 type,仍非 object-only → 整体简化,宁可工具参数不可用也不让请求被拒。
    // root 非 object 的 schema 不处理直透(对齐 CPA root 检查)。
    if schema.get("type").and_then(|v| v.as_str()) == Some("object")
        && ["anyOf", "oneOf"]
            .iter()
            .any(|k| schema.get(*k).is_some_and(|v| v.is_array()))
    {
        let mut s = schema.clone();
        // 先补缺失 type(基于补后数据判定,对齐 CPA 先 normalize 后 needSimplification)
        for union_key in ["anyOf", "oneOf"] {
            let Some(Value::Array(arr)) = s.get_mut(union_key) else {
                continue;
            };
            for branch_schema in arr.iter_mut() {
                // 含 $ref 的分支不补 type(对齐 CPA:$ref 分支跳过补 type)
                if branch_schema.get("type").is_none() && branch_schema.get("$ref").is_none() {
                    branch_schema["type"] = json!("object");
                }
            }
        }
        let object_only = ["anyOf", "oneOf"].iter().all(|k| {
            let Some(Value::Array(arr)) = s.get(*k) else {
                return true;
            };
            arr.iter().all(|b| {
                b.get("type")
                    .map(branch_schema_type_is_object_only)
                    .unwrap_or(false)
            })
        });
        if !object_only {
            return json!({"type": "object", "properties": {}, "additionalProperties": true});
        }
        return s;
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

/// branch type 是否只允许 object(字符串 "object" 或数组全 "object")
fn branch_schema_type_is_object_only(t: &Value) -> bool {
    match t {
        Value::String(s) => s.eq_ignore_ascii_case("object"),
        Value::Array(arr) => {
            !arr.is_empty()
                && arr.iter().all(|item| {
                    item.as_str()
                        .map(|s| s.eq_ignore_ascii_case("object"))
                        .unwrap_or(false)
                })
        }
        _ => false,
    }
}

/// Claude system 文本块 → 单字符串(合并所有 text blocks,对齐 codex base_instructions)
///
/// 注：每个 block 先 trim 再合并,理由：
/// 1. codex 存储 base_instructions 为单字符串,合并时自然规范化空白
/// 2. Claude Code 系统提示程序生成,不含前导/尾随空白
/// 3. 即使上游发送空白块,trim 后更利于缓存命中(减少无意义字节差异)
fn system_to_instructions_text(system: &Value, upstream_model: &str) -> String {
    let mut texts: Vec<String> = Vec::new();
    match system {
        Value::String(s) => {
            let trimmed = s.trim();
            if !super::is_ignorable_system_text(trimmed, upstream_model) {
                texts.push(trimmed.to_string());
            }
        }
        Value::Array(blocks) => {
            for b in blocks {
                if b.get("type").and_then(|v| v.as_str()) == Some("text") {
                    if let Some(t) = b.get("text").and_then(|v| v.as_str()) {
                        let trimmed = t.trim();
                        if !super::is_ignorable_system_text(trimmed, upstream_model) {
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
    // --- system → instructions / developer message ---
    // 对齐 CPA convertClaudeRequestToCodex:
    // GPT/Responses 上游将 system 作为 developer message 放入 input[] (instructions 留空)
    // Grok 上游将 system 配合 GROK_ADAPTER_BLOCK 作为 developer message
    // 其余 responses 上游保持 system → instructions
    let system = body
        .get("system")
        .map(|system| system_to_instructions_text(system, upstream_model))
        .unwrap_or_default();
    let gpt_upstream = is_gpt_upstream(upstream_model);
    let needs_adapter = needs_adapter_block(upstream_model);
    let instructions = if gpt_upstream || needs_adapter {
        String::new()
    } else {
        system.clone()
    };

    let mut openai = json!({
        "model": upstream_model,
        "instructions": instructions,
        "input": [],
    });
    if gpt_upstream {
        if !system.is_empty() {
            openai["input"].as_array_mut().unwrap().push(json!({
                "type": "message",
                "role": "developer",
                "content": [{"type": "input_text", "text": system}]
            }));
        }
    } else if needs_adapter {
        let mut developer = system;
        if !developer.is_empty() {
            developer.push_str("\n\n");
        }
        developer.push_str(GROK_ADAPTER_BLOCK);
        openai["input"].as_array_mut().unwrap().push(json!({
            "type": "message",
            "role": "developer",
            "content": [{"type": "input_text", "text": developer}]
        }));
    }

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
    let mut pending_tool_use_ids: Vec<String> = Vec::new();
    let mut pending_system_reminders: Vec<Value> = Vec::new();

    if let Some(messages) = body.get("messages").and_then(|v| v.as_array()) {
        for msg in messages {
            let role = msg.get("role").and_then(|v| v.as_str()).unwrap_or("user");

            // role=system 消息:reminder 文本 → user message(对齐 ClaudeMessageSystemReminderText)
            // 当存在未应答 tool_use 时暂存 reminder,保持 tool 调用紧邻配对
            if role == "system" {
                if let Some(text) = claude_system_reminder_text(msg.get("content"), upstream_model)
                {
                    let reminder_msg = json!({
                        "type": "message",
                        "role": "user",
                        "content": [{"type": "input_text", "text": text}]
                    });
                    if !pending_tool_use_ids.is_empty() {
                        pending_system_reminders.push(reminder_msg);
                    } else {
                        openai["input"].as_array_mut().unwrap().push(reminder_msg);
                    }
                }
                continue;
            }

            let Some(raw_content) = msg.get("content") else {
                continue;
            };
            if raw_content.is_null() {
                continue;
            }

            // user 消息按 preceding tool_use_ids 对齐 tool_result
            let content = if role == "user" && !pending_tool_use_ids.is_empty() {
                super::message_convert::align_tool_results(raw_content, &pending_tool_use_ids)
            } else {
                raw_content.clone()
            };
            pending_tool_use_ids.clear();

            let mut content_items: Vec<Value> = Vec::new();
            let mut out_items: Vec<Value> = Vec::new();
            // tool_result 抽出的图片 parts(对齐 sub2api toolResultImageParts,
            // 追加成该消息末尾的独立 user message)
            let mut result_image_parts: Vec<Value> = Vec::new();
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
                if !pending_system_reminders.is_empty() {
                    openai["input"]
                        .as_array_mut()
                        .unwrap()
                        .extend(std::mem::take(&mut pending_system_reminders));
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

            let mut tool_result_items: Vec<Value> = Vec::new();

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
                        // GPT/Codex/Grok 仅回放可识别的加密信封；无签名明文不进入请求
                        // (对齐 reasoning replay 缓存只认 encrypted_content,明文 reasoning
                        // 无法回放会导致 grok 多轮丢失决策记忆陷入工具调用死循环)。
                        // 其余 Responses 上游保留既有明文 reasoning 回放。
                        if role == "assistant" {
                            let sig = part.get("signature").and_then(|v| v.as_str());
                            if matches!(sig, Some(s) if !s.trim().is_empty()) {
                                if let Some(good) = gpt_compatible_signature(sig, upstream_model) {
                                    flush_message(&mut content_items, &mut out_items);
                                    let mut reasoning = json!({
                                        "type": "reasoning",
                                        "content": null,
                                        "encrypted_content": good
                                    });
                                    if gpt_upstream {
                                        reasoning["summary"] = json!([]);
                                    }
                                    out_items.push(reasoning);
                                }
                            } else if !gpt_upstream
                                && !upstream_model.to_ascii_lowercase().contains("grok")
                            {
                                if let Some(t) = part.get("thinking").and_then(|v| v.as_str()) {
                                    if !t.trim().is_empty() {
                                        flush_message(&mut content_items, &mut out_items);
                                        out_items.push(json!({
                                            "type": "reasoning",
                                            "content": t
                                        }));
                                    }
                                }
                            }
                        }
                    }
                    "redacted_thinking" => {
                        // redacted_thinking 块不回放(对齐 grok-build parse-only 丢弃,
                        // xai-grok-sampler/src/stream/messages.rs:288)。CPA 虽打包成
                        // "claude-redacted-thinking:" 前缀,但有签名兼容性检查层剥离不兼容的;
                        // ccextra 无该层,且 grok-build 官方参考实现也不回放,故跳过。
                    }
                    "image" => {
                        if let Some(url) = image_to_data_url(part) {
                            content_items.push(json!({"type": "input_image", "image_url": url}));
                        }
                    }
                    "document" => {
                        if let Some(doc) = document_to_input_file(part) {
                            content_items.push(doc);
                        }
                    }
                    "tool_use" => {
                        flush_message(&mut content_items, &mut out_items);
                        let id = part.get("id").and_then(|v| v.as_str()).unwrap_or("");
                        if !id.is_empty() {
                            pending_tool_use_ids.push(id.to_string());
                        }
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
                        let call_id = part
                            .get("tool_use_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or("");
                        let (output, images) =
                            tool_result_output(part.get("content").unwrap_or(&json!("")));
                        result_image_parts.extend(images);
                        let short_id = shorten_call_id(call_id);
                        if custom_call_ids.contains(&short_id) {
                            tool_result_items.push(json!({
                                "type": "custom_tool_call_output",
                                "call_id": short_id,
                                "output": output
                            }));
                        } else {
                            tool_result_items.push(json!({
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
            // 抽出的 tool_result 图片 → 该消息末尾独立 user message
            if !result_image_parts.is_empty() {
                out_items.push(json!({
                    "type": "message",
                    "role": "user",
                    "content": result_image_parts
                }));
            }

            // 发射时序对齐 CPA: tool_result 先发 -> pending reminders -> 正文 out_items
            if !tool_result_items.is_empty() {
                openai["input"]
                    .as_array_mut()
                    .unwrap()
                    .extend(tool_result_items);
            }
            if !pending_system_reminders.is_empty() {
                openai["input"]
                    .as_array_mut()
                    .unwrap()
                    .extend(std::mem::take(&mut pending_system_reminders));
            }
            openai["input"].as_array_mut().unwrap().extend(out_items);
        }

        // EOF flush: 残余 reminder 照发
        if !pending_system_reminders.is_empty() {
            openai["input"]
                .as_array_mut()
                .unwrap()
                .extend(pending_system_reminders);
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

    // --- 采样参数不透传(对齐 CPA preserveXAIResponsesOutputControls:
    // claude 入站走 default 分支不 preserve。CC 的 max_tokens 常为 64000,
    // 透传 max_output_tokens 超 grok 上限会 400 触发客户端重试死循环) ---

    // --- reasoning.effort(对齐 thinking 分支,默认 medium) ---
    // 保留入站 reasoning.effort,钳制到模型支持级别(对齐 codex compact.rs:704 保留 turn_context.reasoning_effort)
    let effort = crate::thinking::resolve_effort_from_body(body).unwrap_or("medium");
    let effort = crate::thinking::clamp_effort(effort, upstream_model);
    // grok 模型对齐 grok-build:reasoning.summary=concise(其余模型不设)
    openai["reasoning"] = if upstream_model.to_ascii_lowercase().contains("grok") {
        json!({"effort": effort, "summary": "concise"})
    } else {
        json!({"effort": effort})
    };

    // --- service_tier:speed/service_tier fast → priority(对齐 normalizeCodexServiceTier) ---
    let service_tier = body.get("service_tier").and_then(|v| v.as_str());
    if body.get("speed").and_then(|v| v.as_str()) == Some("fast")
        || (gpt_upstream && matches!(service_tier, Some("fast" | "priority")))
    {
        openai["service_tier"] = json!("priority");
    }

    // --- codex 固定参数(一致) ---
    // stream 保留入站值(对齐 to_openai_chat)
    openai["stream"] = body.get("stream").unwrap_or(&json!(false)).clone();
    openai["store"] = json!(false);
    openai["include"] = json!(["reasoning.encrypted_content"]);

    // --- output_config.format(json_schema)→ text.format ---
    // (对齐 convertClaudeRequestToCodex:name 缺省 cli_proxy_structured_output,
    //  strict 仅显式 false 时降级,schema 原样透传)
    if let Some(format) = body
        .get("output_config")
        .and_then(|v| v.get("format"))
        .filter(|f| {
            f.is_object()
                && f.get("type").and_then(|t| t.as_str()) == Some("json_schema")
                && f.get("schema").is_some_and(|s| s.is_object())
        })
    {
        let name = format
            .get("name")
            .and_then(|v| v.as_str())
            .filter(|s| !s.is_empty())
            .unwrap_or("cli_proxy_structured_output");
        let strict = format.get("strict") != Some(&Value::Bool(false));
        openai["text"] = json!({
            "format": {
                "type": "json_schema",
                "name": name,
                "strict": strict,
                "schema": format["schema"].clone(),
            }
        });
    }

    // --- stop 删除(对齐 CPA sanitizeXAIResponsesBody:responses 不支持 stop) ---
    if let Some(obj) = openai.as_object_mut() {
        obj.remove("stop");
    }

    // --- 无存活工具时三删(对齐 CPA normalizeXAIToolChoiceForTools) ---
    // tools 缺失或空数组时,tools/tool_choice/parallel_tool_calls 全部不发,
    // 否则 xAI 对无 tools 的 tool_choice 报 400
    let has_tools = openai
        .get("tools")
        .and_then(|v| v.as_array())
        .is_some_and(|a| !a.is_empty());
    if !has_tools {
        if let Some(obj) = openai.as_object_mut() {
            obj.remove("tools");
            obj.remove("tool_choice");
            obj.remove("parallel_tool_calls");
        }
    }

    // 反向映射 short→original,响应侧还原工具名
    let reverse = super::shorten::build_reverse_map(&tool_name_map);

    *body = openai;
    Ok(reverse)
}

/// role=system 消息的 reminder 文本(对齐 ClaudeMessageSystemReminderText)
fn claude_system_reminder_text(content: Option<&Value>, upstream_model: &str) -> Option<String> {
    let parts: Vec<String> = match content {
        Some(Value::String(s)) if !super::is_ignorable_system_text(s, upstream_model) => {
            vec![s.trim().to_string()]
        }
        Some(Value::Array(items)) => items
            .iter()
            .filter(|i| i.get("type").and_then(|v| v.as_str()) == Some("text"))
            .filter_map(|i| i.get("text").and_then(|v| v.as_str()))
            .filter(|t| !super::is_ignorable_system_text(t, upstream_model))
            .map(|t| t.trim().to_string())
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
    use base64::Engine;
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
    fn test_gpt_system_goes_to_developer_message() {
        // Codex 线将 system 作为 developer 输入，instructions 留空，且不注入阉割 adapter
        let mut body = json!({
            "model": "test",
            "system": "You are helpful",
            "messages": []
        });
        convert_to_openai_responses(&mut body, "gpt-5.6-terra").unwrap();
        assert_eq!(body["instructions"], "");
        assert_eq!(body["input"][0]["type"], "message");
        assert_eq!(body["input"][0]["role"], "developer");
        assert_eq!(body["input"][0]["content"][0]["text"], "You are helpful");
    }

    #[test]
    fn test_gpt_without_system_no_developer_message() {
        let mut body = json!({
            "model": "test",
            "messages": [{"role": "user", "content": "hi"}]
        });
        convert_to_openai_responses(&mut body, "gpt-5.6-sol").unwrap();
        assert_eq!(body["instructions"], "");
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["role"], "user");
        assert_eq!(input[0]["content"][0]["text"], "hi");
    }

    #[test]
    fn test_non_gpt_no_adapter_block() {
        // 非 gpt/grok 上游不注入 adapter block
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
    fn test_grok_system_and_adapter_go_to_developer_message() {
        // Grok 线将 system 与 GROK_ADAPTER_BLOCK 作为 developer 输入，instructions 留空。
        let mut body = json!({
            "model": "test",
            "system": "You are helpful",
            "messages": []
        });
        convert_to_openai_responses(&mut body, "grok-4.6").unwrap();
        assert_eq!(body["instructions"], "");
        assert_eq!(body["input"][0]["type"], "message");
        assert_eq!(body["input"][0]["role"], "developer");
        assert_eq!(
            body["input"][0]["content"][0]["text"],
            format!("You are helpful\n\n{}", GROK_ADAPTER_BLOCK)
        );
    }

    #[test]
    fn test_grok_adapter_creates_developer_message_without_system() {
        let mut body = json!({
            "model": "test",
            "messages": [{"role": "user", "content": "hi"}]
        });
        convert_to_openai_responses(&mut body, "grok-3").unwrap();
        assert_eq!(body["instructions"], "");
        let input = body["input"].as_array().unwrap();
        assert_eq!(input[0]["role"], "developer");
        assert_eq!(input[0]["content"][0]["text"], GROK_ADAPTER_BLOCK);
        assert_eq!(input[1]["role"], "user");
        assert_eq!(input[1]["content"][0]["text"], "hi");
    }

    #[test]
    fn test_grok_adapter_block_contains_key_constraints() {
        // 验证 GROK_ADAPTER_BLOCK 包含官方 prompt.md 的核心约束
        assert!(GROK_ADAPTER_BLOCK.contains("inside Claude Code's agent loop"));
        assert!(GROK_ADAPTER_BLOCK.contains("Your capabilities are exactly the tools declared"));
        assert!(GROK_ADAPTER_BLOCK.contains("only when tool output supports the claim"));
        assert!(GROK_ADAPTER_BLOCK.contains("Use specialized tools instead of bash commands"));
        assert!(GROK_ADAPTER_BLOCK.contains("Communicate directly and concisely"));
        assert!(GROK_ADAPTER_BLOCK.contains("in complete sentences"));
        assert!(GROK_ADAPTER_BLOCK.contains("NEVER coin acronyms"));
        assert!(GROK_ADAPTER_BLOCK.contains("Always respond in Simplified Chinese"));
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
    fn test_system_attribution_and_claude_identity_stripped() {
        let mut body = json!({
            "model": "test",
            "system": [
                {"type": "text", "text": "x-anthropic-billing-header: fp=abc"},
                {"type": "text", "text": "You are a Claude agent, built on Anthropic's Claude Agent SDK."},
                {"type": "text", "text": "You are Claude Code, Anthropic's official CLI for Claude."},
                {"type": "text", "text": "Real"}
            ],
            "messages": []
        });
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        // attribution 与 Claude 身份句被过滤，只保留 "Real"
        assert_eq!(body["instructions"], "Real");
    }

    #[test]
    fn test_claude_target_keeps_claude_identity() {
        let mut body = json!({
            "model": "test",
            "system": [
                {"type": "text", "text": "You are Claude Code, Anthropic's official CLI for Claude."}
            ],
            "messages": []
        });

        convert_to_openai_responses(&mut body, "claude-opus-5").unwrap();

        assert_eq!(
            body["instructions"],
            "You are Claude Code, Anthropic's official CLI for Claude."
        );
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
    fn test_tool_schema_union_non_object_simplified() {
        // xAI 拒收非 object-only 的 root union:整体简化为安全 schema(对齐 CPA)
        let mut body = json!({
            "model": "test",
            "messages": [],
            "tools": [
                {"name": "u", "input_schema": {
                    "type": "object",
                    "properties": {"v": {"type": "string"}},
                    "anyOf": [
                        {"type": "string"},
                        {"type": "object", "properties": {"a": {"type": "string"}}}
                    ]
                }}
            ]
        });
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        assert_eq!(
            body["tools"][0]["parameters"],
            json!({"type": "object", "properties": {}, "additionalProperties": true})
        );
    }

    #[test]
    fn test_tool_schema_union_missing_type_filled() {
        // object-only union 分支缺 type → 补 "object",保留原 schema 语义(对齐 CPA)
        let mut body = json!({
            "model": "test",
            "messages": [],
            "tools": [
                {"name": "u", "input_schema": {
                    "type": "object",
                    "properties": {"v": {"type": "string"}},
                    "oneOf": [
                        {"properties": {"a": {"type": "string"}}},
                        {"type": "object", "properties": {"b": {"type": "integer"}}}
                    ]
                }}
            ]
        });
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        assert_eq!(body["tools"][0]["parameters"]["oneOf"][0]["type"], "object");
        assert_eq!(
            body["tools"][0]["parameters"]["properties"]["v"]["type"],
            "string"
        );
    }

    #[test]
    fn test_tool_schema_local_refs_inlined() {
        // 本地 $ref 内联,内联后删除 $defs(对齐 CPA normalizeXAITool InlineLocalRefs)
        let mut body = json!({
            "model": "test",
            "messages": [],
            "tools": [
                {"name": "r", "input_schema": {
                    "type": "object",
                    "properties": {"user": {"$ref": "#/$defs/User"}},
                    "$defs": {"User": {
                        "type": "object",
                        "properties": {"name": {"type": "string"}}
                    }}
                }}
            ]
        });
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        let params = &body["tools"][0]["parameters"];
        assert!(params.get("$defs").is_none());
        assert_eq!(params["properties"]["user"]["type"], "object");
        assert_eq!(
            params["properties"]["user"]["properties"]["name"]["type"],
            "string"
        );
    }

    #[test]
    fn test_tool_schema_union_unresolved_ref_simplified() {
        // 无法内联的 $ref 分支不补 type,非 object-only → 整体简化(对齐 CPA)
        let mut body = json!({
            "model": "test",
            "messages": [],
            "tools": [
                {"name": "r", "input_schema": {
                    "type": "object",
                    "oneOf": [
                        {"$ref": "#/$defs/Missing"},
                        {"type": "object", "properties": {"a": {"type": "string"}}}
                    ]
                }}
            ]
        });
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        assert_eq!(
            body["tools"][0]["parameters"],
            json!({"type": "object", "properties": {}, "additionalProperties": true})
        );
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
        // 保序:reasoning → message(output_text) → function_call。
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
        assert_eq!(input.len(), 3);
        assert_eq!(input[0]["type"], "reasoning");
        assert_eq!(input[0]["content"], "t1");
        assert_eq!(input[1]["type"], "message");
        assert_eq!(input[1]["content"][0]["text"], "answer");
        assert_eq!(input[2]["type"], "function_call");
        assert_eq!(input[2]["call_id"], "c1");
    }

    #[test]
    fn test_gpt_unsigned_thinking_is_dropped() {
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "private trace"},
                    {"type": "text", "text": "answer"}
                ]}
            ]
        });
        convert_to_openai_responses(&mut body, "gpt-5.6-terra").unwrap();
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["type"], "message");
        assert_eq!(input[0]["content"][0]["text"], "answer");
        // 无签名 thinking 被丢弃(无 reasoning 项)
        assert!(!input.iter().any(|i| i["type"] == "reasoning"));
    }

    #[test]
    fn test_thinking_signature_gpt_compat_kept_else_dropped() {
        // 严格 Fernet 校验:gAAAA + 合法密文长度 → reasoning;其余首字节 → 丢弃
        const VALID: &str = "gAAAAAAAAAAAAQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyAhIiMkJSYnKCkqKywtLi8wMTIzNDU2Nzg5Ojs8PT4_QA";
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "t1", "signature": VALID},
                    {"type": "thinking", "thinking": "t2", "signature": "C4x2 weird"},
                    {"type": "text", "text": "answer"}
                ]}
            ]
        });
        convert_to_openai_responses(&mut body, "gpt-5.6-terra").unwrap();
        assert_eq!(body["input"][0]["type"], "reasoning");
        assert_eq!(body["input"][0]["encrypted_content"], VALID);
        assert_eq!(body["input"][0]["summary"], json!([]));
        assert_eq!(body["input"][1]["type"], "message");
        assert_eq!(body["input"][1]["content"][0]["text"], "answer");
    }

    #[test]
    fn test_thinking_grok_model_drops_foreign_envelope() {
        // grok 目标:Claude/GPT/Gemini 信封剥离,opaque 才回放
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "t", "signature": "C4x2 opaque"}
                ]}
            ]
        });
        convert_to_openai_responses(&mut body, "grok-3").unwrap();
        let input = body["input"].as_array().unwrap();
        // grok 注入 developer message，外来信封被丢弃后只剩 developer
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["role"], "developer");
    }

    #[test]
    fn test_thinking_grok_model_drops_gpt_envelope() {
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "t", "signature": "gAAAA-from-gpt"}
                ]}
            ]
        });
        convert_to_openai_responses(&mut body, "grok-3").unwrap();
        let input = body["input"].as_array().unwrap();
        // grok 注入 developer message，GPT 信封被丢弃后只剩 developer
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["role"], "developer");
    }

    #[test]
    fn test_thinking_grok_model_replays_opaque() {
        // 构造合法的高熵 Grok encrypted_content(standard base64, 无填充, 高熵)
        let mut high_entropy = vec![0u8; 64];
        for (i, byte) in high_entropy.iter_mut().enumerate() {
            *byte = (i * 13 % 256) as u8;
        }
        let valid_grok_sig = base64::engine::general_purpose::STANDARD_NO_PAD.encode(&high_entropy);

        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "t", "signature": valid_grok_sig}
                ]}
            ]
        });
        convert_to_openai_responses(&mut body, "grok-3").unwrap();
        let input = body["input"].as_array().unwrap();
        // grok 注入 developer message，opaque 签名回放为 reasoning
        assert_eq!(input.len(), 2);
        assert_eq!(input[0]["role"], "developer");
        assert_eq!(input[1]["type"], "reasoning");
        assert_eq!(input[1]["encrypted_content"], valid_grok_sig);
    }

    #[test]
    fn test_grok_unsigned_thinking_is_dropped() {
        // grok 对齐 GPT:无签名 thinking 丢弃(明文 reasoning 无法缓存会导致死循环)
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "private trace"},
                    {"type": "text", "text": "answer"}
                ]}
            ]
        });
        convert_to_openai_responses(&mut body, "grok-4.6").unwrap();
        let input = body["input"].as_array().unwrap();
        // grok 现在注入 developer message，input[0] 是 developer，input[1] 是 message
        assert_eq!(input.len(), 2);
        assert_eq!(input[0]["role"], "developer");
        assert_eq!(input[1]["type"], "message");
        assert_eq!(input[1]["role"], "assistant");
        assert_eq!(input[1]["content"][0]["text"], "answer");
        // 无签名 thinking 被丢弃(无 reasoning 项)
        assert!(!input.iter().any(|i| i["type"] == "reasoning"));
    }

    #[test]
    fn test_trim_encrypted_reasoning_items() {
        let mut body = json!({
            "store": false,
            "input": [
                {"type": "reasoning", "id": "rs_1", "encrypted_content": "gAAAA-bad", "content": null},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]},
                {"type": "reasoning", "encrypted_content": "gAAAA-keep-text", "content": "think"}
            ]
        });
        assert!(trim_encrypted_reasoning_items(&mut body));
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 2);
        assert_eq!(input[0]["type"], "message");
        assert_eq!(input[1]["type"], "reasoning");
        assert_eq!(input[1]["content"], "think");
        assert!(input[1].get("encrypted_content").is_none());
        assert!(!trim_encrypted_reasoning_items(&mut body));
    }

    #[test]
    fn test_sanitize_gpt_reasoning_items() {
        const VALID: &str = "gAAAAAAAAAAAAQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyAhIiMkJSYnKCkqKywtLi8wMTIzNDU2Nzg5Ojs8PT4_QA";
        let mut body = json!({
            "store": false,
            "input": [
                {"type": "reasoning", "id": "rs_bad", "encrypted_content": "gAAAA-replay", "content": null, "summary": []},
                {"type": "reasoning", "id": "rs_text", "encrypted_content": " ", "content": "keep"},
                {"type": "reasoning", "id": "rs_summary", "encrypted_content": null, "summary": ["keep"]},
                {"type": "reasoning", "id": "rs_valid", "encrypted_content": VALID, "content": null, "summary": []},
                {"type": "reasoning", "id": "rs_orphan", "content": null, "summary": []},
                {"type": "message", "role": "user", "content": [{"type": "input_text", "text": "hi"}]}
            ]
        });

        assert!(sanitize_gpt_reasoning_items(&mut body));
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 4);
        assert_eq!(input[0]["content"], "keep");
        assert!(input[0].get("encrypted_content").is_none());
        assert!(input[0].get("id").is_none());
        assert_eq!(input[1]["summary"], json!(["keep"]));
        assert!(input[1].get("encrypted_content").is_none());
        assert!(input[1].get("id").is_none());
        assert_eq!(input[2]["encrypted_content"], VALID);
        assert_eq!(input[2]["id"], "rs_valid");
        assert_eq!(input[3]["type"], "message");
        assert!(!sanitize_gpt_reasoning_items(&mut body));
    }

    #[test]
    fn test_sanitize_gpt_reasoning_keeps_id_with_store_true() {
        let mut body = json!({
            "store": true,
            "input": [{
                "type": "reasoning",
                "id": "rs_stored",
                "encrypted_content": null,
                "content": "keep"
            }]
        });

        assert!(sanitize_gpt_reasoning_items(&mut body));
        assert_eq!(body["input"][0]["id"], "rs_stored");
        assert!(body["input"][0].get("encrypted_content").is_none());
        assert_eq!(body["input"][0]["content"], "keep");
    }

    #[test]
    fn test_is_thinking_signature_invalid() {
        assert!(is_thinking_signature_invalid(
            br#"{"error":{"code":"invalid_encrypted_content","message":"bad"}}"#
        ));
        assert!(is_thinking_signature_invalid(
            br#"{"error":{"message":"Invalid signature in thinking block"}}"#
        ));
        assert!(is_thinking_signature_invalid(
            br#"{"error":{"message":"Could not decrypt"}}"#
        ));
        assert!(!is_thinking_signature_invalid(
            br#"{"error":{"message":"context_length_exceeded"}}"#
        ));
    }

    #[test]
    fn test_redacted_thinking_grok_model_skipped() {
        // grok 不回放 redacted_thinking(对齐 grok-build parse-only 丢弃)
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "redacted_thinking", "data": "opaque_payload_xyz"}
                ]}
            ]
        });
        convert_to_openai_responses(&mut body, "grok-3").unwrap();
        let input = body["input"].as_array().unwrap();
        // grok 注入 developer message，redacted_thinking 被丢弃后只剩 developer
        assert_eq!(input.len(), 1);
        assert_eq!(input[0]["role"], "developer");
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
    fn test_output_config_format_maps_to_text_format() {
        // output_config.format(json_schema)→ text.format(对齐 convertClaudeRequestToCodex)
        let mut body = json!({
            "model": "test",
            "output_config": {"format": {
                "type": "json_schema",
                "schema": {
                    "type": "object",
                    "properties": {"answer": {"type": "string"}},
                    "required": ["answer"],
                    "additionalProperties": false
                }
            }},
            "messages": []
        });
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        assert_eq!(body["text"]["format"]["type"], "json_schema");
        // name 缺省 → cli_proxy_structured_output;strict 缺省 → true
        assert_eq!(
            body["text"]["format"]["name"],
            "cli_proxy_structured_output"
        );
        assert_eq!(body["text"]["format"]["strict"], true);
        assert_eq!(
            body["text"]["format"]["schema"]["properties"]["answer"]["type"],
            "string"
        );
    }

    #[test]
    fn test_output_config_format_custom_name_and_strict_false() {
        let mut body = json!({
            "model": "test",
            "output_config": {"format": {
                "type": "json_schema",
                "name": "custom_schema",
                "strict": false,
                "schema": {"type": "object"}
            }},
            "messages": []
        });
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        assert_eq!(body["text"]["format"]["name"], "custom_schema");
        assert_eq!(body["text"]["format"]["strict"], false);
    }

    #[test]
    fn test_output_config_without_format_no_text() {
        // 仅 effort / 缺 format → 不发 text(对齐 CPA effort-only 子例)
        let mut body = json!({
            "model": "test",
            "thinking": {"type": "adaptive"},
            "output_config": {"effort": "high"},
            "messages": []
        });
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        assert!(body.get("text").is_none());
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
    fn test_service_tier_fast_is_priority() {
        let mut body = json!({"model": "test", "messages": [], "service_tier": "fast"});
        convert_to_openai_responses(&mut body, "gpt-5.6-terra").unwrap();
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
        // 无 stream 字段默认非流(对齐 Anthropic API 语义)
        assert_eq!(body["stream"], false);
        assert_eq!(body["store"], false);
        assert_eq!(body["include"][0], "reasoning.encrypted_content");
        // 无工具时三删,parallel_tool_calls 不发(对齐 CPA)
        assert!(body.get("parallel_tool_calls").is_none());
    }

    #[test]
    fn test_parallel_tool_calls_disabled() {
        let mut body = json!({
            "model": "test",
            "messages": [],
            "tool_choice": {"type": "auto", "disable_parallel_tool_use": true},
            "tools": [{"name": "f", "input_schema": {"type": "object"}}]
        });
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        assert_eq!(body["parallel_tool_calls"], false);
    }

    #[test]
    fn test_no_tools_drops_tool_fields() {
        // 无工具时三删(对齐 CPA normalizeXAIToolChoiceForTools)
        let mut body = json!({"model": "test", "messages": []});
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        assert!(body.get("tools").is_none());
        assert!(body.get("tool_choice").is_none());
        assert!(body.get("parallel_tool_calls").is_none());

        // 空 tools 数组同样三删
        let mut body2 = json!({"model": "test", "messages": [], "tools": []});
        convert_to_openai_responses(&mut body2, "test-model").unwrap();
        assert!(body2.get("tools").is_none());
        assert!(body2.get("tool_choice").is_none());
        assert!(body2.get("parallel_tool_calls").is_none());

        // 有工具时保留三者
        let mut body3 = json!({
            "model": "test",
            "messages": [],
            "tools": [{"name": "search", "input_schema": {"type": "object"}}]
        });
        convert_to_openai_responses(&mut body3, "test-model").unwrap();
        assert!(body3.get("tools").is_some());
        assert_eq!(body3["tool_choice"], "auto");
        assert_eq!(body3["parallel_tool_calls"], true);
    }

    #[test]
    fn test_sampling_params_not_passed_through() {
        // 采样参数不透传(对齐 CPA:claude 入站不 preserve;
        // CC max_tokens=64000 透传超 grok 上限会 400 死循环)
        let mut body = json!({
            "model": "test",
            "messages": [],
            "max_tokens": 64000,
            "temperature": 0.7,
            "top_p": 0.9,
            "top_k": 40
        });
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        assert!(body.get("max_output_tokens").is_none());
        assert!(body.get("temperature").is_none());
        assert!(body.get("top_p").is_none());
        assert!(body.get("top_k").is_none());
    }

    #[test]
    fn test_strict_only_when_simplified() {
        // strict 恒设 false(对齐 CPA ConvertClaudeRequestToCodex:
        // strict != false 即强制 false,claude 入站路径无 xAI 后处理层)
        let mut body = json!({
            "model": "test",
            "messages": [],
            "tools": [{
                "name": "search",
                "input_schema": {
                    "type": "object",
                    "properties": {"q": {"type": "string"}}
                }
            }]
        });
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        assert_eq!(body["tools"][0]["strict"], false);

        // root union 非 object-only 被简化:同样 strict=false
        let mut body2 = json!({
            "model": "test",
            "messages": [],
            "tools": [{
                "name": "search",
                "input_schema": {
                    "type": "object",
                    "anyOf": [{"type": "string"}, {"type": "object"}]
                }
            }]
        });
        convert_to_openai_responses(&mut body2, "test-model").unwrap();
        assert_eq!(body2["tools"][0]["strict"], false);
    }

    #[test]
    fn test_stop_removed() {
        // responses 不支持 stop,转换后删除(对齐 CPA sanitizeXAIResponsesBody)
        let mut body = json!({
            "model": "test",
            "messages": [],
            "stop_sequences": ["\n\n"]
        });
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        assert!(body.get("stop").is_none());
    }

    #[test]
    fn test_tool_choice_mappings() {
        let mut body = json!({
            "model": "test",
            "messages": [],
            "tool_choice": {"type": "any"},
            "tools": [{"name": "f", "input_schema": {"type": "object"}}]
        });
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
    fn test_document_block_to_input_file() {
        // 对齐 CPA TestConvertClaudeRequestToCodex_PreservesBase64PDFDocumentContent
        let mut body = json!({
            "model": "test",
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "text", "text": "before"},
                    {"type": "document", "source": {"type": "base64", "media_type": "application/pdf", "data": "JVBERi0xLjQK"}},
                    {"type": "text", "text": "after"}
                ]
            }]
        });
        convert_to_openai_responses(&mut body, "gpt-5.6-sol").unwrap();
        let content = body["input"][0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 3);
        assert_eq!(content[0]["type"], "input_text");
        assert_eq!(content[0]["text"], "before");
        assert_eq!(content[1]["type"], "input_file");
        assert_eq!(
            content[1]["file_data"],
            "data:application/pdf;base64,JVBERi0xLjQK"
        );
        assert_eq!(content[1]["filename"], "document.pdf");
        assert_eq!(content[2]["type"], "input_text");
        assert_eq!(content[2]["text"], "after");
    }

    #[test]
    fn test_document_non_pdf_or_non_base64_ignored() {
        let mut body = json!({
            "model": "test",
            "messages": [{
                "role": "user",
                "content": [
                    {"type": "document", "source": {"type": "url", "url": "https://example.com/doc.pdf"}},
                    {"type": "document", "source": {"type": "base64", "media_type": "text/plain", "data": "aGVsbG8="}},
                    {"type": "text", "text": "only text"}
                ]
            }]
        });
        convert_to_openai_responses(&mut body, "gpt-5.6-sol").unwrap();
        let content = body["input"][0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["type"], "input_text");
        assert_eq!(content[0]["text"], "only text");
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
        // 图片抽出(对齐 sub2api):output 只留文本,图片进随后的 user message
        let output = body["input"][0]["output"].as_array().unwrap();
        assert_eq!(output[0]["type"], "input_text");
        assert_eq!(body["input"][1]["type"], "message");
        assert_eq!(body["input"][1]["role"], "user");
        let parts = body["input"][1]["content"].as_array().unwrap();
        assert_eq!(parts[0]["type"], "input_image");
    }

    #[test]
    fn test_tool_result_image_only_uses_placeholder() {
        // 只有图片的 tool_result:output 用占位,图片进随后的 user message
        let mut body = json!({
            "model": "test",
            "messages": [{
                "role": "user",
                "content": [{
                    "type": "tool_result",
                    "tool_use_id": "t1",
                    "content": [
                        {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "aGVsbG8="}}
                    ]
                }]
            }]
        });
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        assert_eq!(body["input"][0]["output"], "(no output)");
        assert_eq!(body["input"][1]["content"][0]["type"], "input_image");
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

    #[test]
    fn test_redacted_thinking_non_grok_skipped() {
        // 非 grok 同样不回放 redacted_thinking(对齐 grok-build parse-only 丢弃)
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "redacted_thinking", "data": "opaque_payload_xyz"}
                ]}
            ]
        });
        convert_to_openai_responses(&mut body, "gpt-5").unwrap();
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 0);
    }

    #[test]
    fn test_redacted_thinking_empty_data_skipped() {
        // redacted_thinking 块跳过后,后续 text 块仍正常处理为 message
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "redacted_thinking", "data": ""},
                    {"type": "text", "text": "result"}
                ]}
            ]
        });
        convert_to_openai_responses(&mut body, "gpt-5").unwrap();
        assert_eq!(body["input"].as_array().unwrap().len(), 1);
        assert_eq!(body["input"][0]["type"], "message");
        assert_eq!(body["input"][0]["content"][0]["text"], "result");
    }

    #[test]
    fn test_redacted_thinking_user_role_ignored() {
        // user 角色的 redacted_thinking 块应被忽略（只处理 assistant）
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "user", "content": [
                    {"type": "redacted_thinking", "data": "should_ignore"}
                ]}
            ]
        });
        convert_to_openai_responses(&mut body, "gpt-5").unwrap();
        let input = body["input"].as_array().unwrap();
        assert_eq!(input.len(), 0);
    }

    #[test]
    fn test_grok_rejects_foreign_signatures() {
        // Grok 目标应拒绝 GPT/Claude/Gemini 签名(防止上游 400)
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "thinking", "thinking": "t1", "signature": "gAAAA-gpt-fernet"},
                    {"type": "thinking", "thinking": "t2", "signature": "Cais-claude"},
                    {"type": "thinking", "thinking": "t3", "signature": "Egemini"},
                    {"type": "text", "text": "answer"}
                ]}
            ]
        });
        convert_to_openai_responses(&mut body, "grok-4.6").unwrap();
        let input = body["input"].as_array().unwrap();
        // grok 注入 developer,message 包含 text 内容,所有外来签名被过滤
        assert_eq!(input.len(), 2);
        assert_eq!(input[0]["role"], "developer");
        assert_eq!(input[1]["type"], "message");
        assert_eq!(input[1]["role"], "assistant");
        assert_eq!(input[1]["content"][0]["text"], "answer");
        // 无 reasoning 项(签名全部过滤)
        assert!(!input.iter().any(|i| i["type"] == "reasoning"));
    }

    #[test]
    fn test_gpt_compatible_signature() {
        // 对齐 CPA codex_claude_request.appendReasoningContent
        const VALID: &str = "gAAAAAAAAAAAAQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyAhIiMkJSYnKCkqKywtLi8wMTIzNDU2Nzg5Ojs8PT4_QA";
        // GPT 目标:Fernet 形状通过,provider 前缀被剥掉
        assert_eq!(
            gpt_compatible_signature(Some(VALID), "gpt-5"),
            Some(VALID.to_string())
        );
        let prefixed = format!("gpt#{VALID}");
        assert_eq!(
            gpt_compatible_signature(Some(&prefixed), "gpt-5"),
            Some(VALID.to_string())
        );
        // 非 GPT 信封、空签名、缺字段一律丢弃
        assert_eq!(gpt_compatible_signature(Some("Cais-claude"), "gpt-5"), None);
        assert_eq!(gpt_compatible_signature(Some("gAAAA-short"), "gpt-5"), None);
        assert_eq!(gpt_compatible_signature(Some(""), "gpt-5"), None);
        assert_eq!(gpt_compatible_signature(None, "gpt-5"), None);
        // grok 目标:无信封高熵 blob 通过
        let mut opaque = vec![0u8; 64];
        for (i, byte) in opaque.iter_mut().enumerate() {
            *byte = (i * 7 % 256) as u8;
        }
        let grok = base64::engine::general_purpose::STANDARD_NO_PAD.encode(&opaque);
        assert_eq!(
            gpt_compatible_signature(Some(&grok), "grok-4.6"),
            Some(grok.clone())
        );
        // CPA 先无条件试 GPT 兼容,再看 grok:合法 Fernet 对 grok 目标同样保留
        assert_eq!(
            gpt_compatible_signature(Some(VALID), "grok-beta"),
            Some(VALID.to_string())
        );
        // 非 grok 目标不接受无信封 blob
        assert_eq!(gpt_compatible_signature(Some(&grok), "gpt-5"), None);
    }

    #[test]
    fn test_responses_tool_pairing_across_system_message() {
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "f1", "input": {}},
                    {"type": "tool_use", "id": "t2", "name": "f2", "input": {}}
                ]},
                {"role": "system", "content": "Usage note: 50% remaining"},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t2", "content": "res2"},
                    {"type": "tool_result", "tool_use_id": "t1", "content": "res1"},
                    {"type": "text", "text": "next instruction"}
                ]}
            ]
        });
        convert_to_openai_responses(&mut body, "test-model").unwrap();
        let input = body["input"].as_array().unwrap();
        // 验证时序:
        // 0: function_call(t1)
        // 1: function_call(t2)
        // 2: function_call_output(t1) - 对齐重排到前
        // 3: function_call_output(t2)
        // 4: message(user, system-reminder) - 延后发射
        // 5: message(user, next instruction) - 正文
        assert_eq!(input[0]["type"], "function_call");
        assert_eq!(input[0]["call_id"], "t1");
        assert_eq!(input[1]["type"], "function_call");
        assert_eq!(input[1]["call_id"], "t2");
        assert_eq!(input[2]["type"], "function_call_output");
        assert_eq!(input[2]["call_id"], "t1");
        assert_eq!(input[3]["type"], "function_call_output");
        assert_eq!(input[3]["call_id"], "t2");
        assert_eq!(input[4]["type"], "message");
        assert_eq!(input[4]["role"], "user");
        assert!(input[4]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("<system-reminder>"));
        assert_eq!(input[5]["type"], "message");
        assert_eq!(input[5]["role"], "user");
        assert_eq!(input[5]["content"][0]["text"], "next instruction");
    }
}
