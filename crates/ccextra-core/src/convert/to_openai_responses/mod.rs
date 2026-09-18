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

pub mod instructions;
pub mod messages;
pub mod reasoning;
pub mod schema;

#[cfg(test)]
mod tests;

use std::collections::{HashMap, HashSet};
use serde_json::{json, Value};

use crate::convert::shorten::{build_short_name_map, shorten_name_if_needed};
use crate::convert::Result;

pub use instructions::{
    is_gpt_upstream, strip_claude_system_for_chat, strip_claude_system_for_gemini,
};
pub use reasoning::{
    is_thinking_signature_invalid, sanitize_gpt_reasoning_items, trim_encrypted_reasoning_items,
};

use instructions::*;
use messages::*;
use reasoning::*;
use schema::*;

pub fn convert_to_openai_responses(
    body: &mut Value,
    upstream_model: &str,
) -> Result<HashMap<String, String>> {
    convert_to_openai_responses_with(body, upstream_model, &[])
}

/// 带 reasoning 注册表的转换入口(HTTP 热重载快照注入;查不到不钳)
pub fn convert_to_openai_responses_with(
    body: &mut Value,
    upstream_model: &str,
    registry: &[crate::thinking::ModelCapability],
) -> Result<HashMap<String, String>> {
    // --- system → instructions / developer message ---
    // 对齐 CPA convertClaudeRequestToCodex:
    // GPT/Grok 上游将 system 配合 ADAPTER_BLOCK 作为 developer message 放入 input[]
    // (instructions 留空);两者均清洗 system 剥离触发过度推理的块;
    // 其余 responses 上游保持 system → instructions
    let system = body
        .get("system")
        .map(|system| system_to_instructions_text(system, upstream_model))
        .unwrap_or_default();
    let needs_adapter = needs_adapter_block(upstream_model);
    let instructions = if needs_adapter {
        String::new()
    } else {
        system.clone()
    };

    let mut openai = json!({
        "model": upstream_model,
        "instructions": instructions,
        "input": [],
    });
    if needs_adapter {
        let adapter = if is_gpt6_astra(upstream_model) {
            GPT_6_ASTRA_ADAPTER_BLOCK
        } else if is_gpt_upstream(upstream_model) {
            GPT_CODEX_ADAPTER_BLOCK
        } else {
            GROK_ADAPTER_BLOCK
        };

        // GPT/Grok 上游均清洗 system,剥离 Claude 触发块
        let base_system = strip_claude_system_for_gpt(&system);

        let mut developer = String::from(adapter);
        if !base_system.is_empty() {
            developer.push_str("\n\n");
            developer.push_str(&base_system);
        }
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
    // 已发射 function_call 的 call_id 集合(孤儿 tool_result 判定用)
    let mut emitted_call_ids: HashSet<String> = HashSet::new();
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
                                    if is_gpt_upstream(upstream_model) {
                                        reasoning["summary"] = json!([]);
                                    }
                                    out_items.push(reasoning);
                                }
                            } else if !is_gpt_upstream(upstream_model)
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
                        emitted_call_ids.insert(short_id.clone());
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
                        } else if !emitted_call_ids.contains(&short_id) {
                            // 孤儿 output(无配对 function_call)不发,转 user text
                            // (对齐 CPA 8c984672 appendStandaloneResponsesToolOutputAsUser)
                            if let Some(item) = orphan_tool_output_as_user(part.get("content")) {
                                tool_result_items.push(item);
                            }
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
                // 对齐 grok-build xai-grok-sampling-types tool_overrides::to_tool_entry:
                // xAI filters 域过滤键为 excluded_domains(OpenAI 官方为 blocked_domains);
                // 两者互斥,并存时按 allowed 优先(与上游 validate 报错一致)
                if let Some(domains) = tool.get("allowed_domains").and_then(|v| v.as_array()) {
                    ws["filters"] = json!({"allowed_domains": domains});
                } else if let Some(domains) = tool.get("blocked_domains").and_then(|v| v.as_array())
                {
                    let key = if is_grok_upstream(upstream_model) {
                        "excluded_domains"
                    } else {
                        "blocked_domains"
                    };
                    ws["filters"] = json!({key: domains});
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
            let mut params = normalize_tool_parameters(&schema);
            simplify_pure_const_unions(&mut params);
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

    // --- reasoning.effort(对齐 thinking 分支;GPT-6 Astra 默认 low,其余 medium) ---
    // 保留入站 reasoning.effort,钳制到模型支持级别(对齐 codex compact.rs:704 保留 turn_context.reasoning_effort)
    let default_effort = if is_gpt6_astra(upstream_model) {
        "low"
    } else {
        "medium"
    };
    let effort = crate::thinking::resolve_effort_from_body(body).unwrap_or(default_effort);
    // 固定 effort 优先(生效范围与 clamp 一致,值不钳制)
    let effort = match crate::thinking::forced_effort(upstream_model, registry) {
        Some(forced) => forced,
        None => crate::thinking::clamp_effort(effort, upstream_model, registry),
    };
    // grok 模型对齐 grok-build:reasoning.summary=concise(其余模型不设)
    openai["reasoning"] = if upstream_model.to_ascii_lowercase().contains("grok") {
        json!({"effort": effort, "summary": "concise"})
    } else {
        json!({"effort": effort})
    };

    // --- service_tier:speed/service_tier fast → priority(对齐 normalizeCodexServiceTier) ---
    let service_tier = body.get("service_tier").and_then(|v| v.as_str());
    if body.get("speed").and_then(|v| v.as_str()) == Some("fast")
        || (is_gpt_upstream(upstream_model) && matches!(service_tier, Some("fast" | "priority")))
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
        let mut strict = format.get("strict") != Some(&Value::Bool(false));
        // OpenAI strict 模式要求 declared properties 必须全部在 required 列表中(递归检查)
        // 若缺失则降级 strict=false,避免上游报 400(对齐 CPA codexSchemaMissesRequired)
        if strict && codex_schema_misses_required(&format["schema"]) {
            strict = false;
        }
        openai["text"] = json!({
            "format": {
                "type": "json_schema",
                "name": name,
                "strict": strict,
                "schema": format["schema"].clone(),
            }
        });
    }

    // --- text.verbosity(仅 GPT 目标对齐 Codex: 默认 low,压制推理发散与冗余输出) ---
    if is_gpt_upstream(upstream_model) {
        if let Some(text_obj) = openai.get_mut("text").and_then(|v| v.as_object_mut()) {
            text_obj.insert("verbosity".to_string(), json!("low"));
        } else {
            openai["text"] = json!({"verbosity": "low"});
        }
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
