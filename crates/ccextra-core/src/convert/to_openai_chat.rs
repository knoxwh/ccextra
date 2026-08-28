// Anthropic → OpenAI chat/completions 转换
//
// 参考转换语义:字段映射完整不丢字段(参考实现的 Standard 中间层有损:
// cache_control/image 丢失,故不采用)。
//
// 主要映射:
// - system → messages[0] {role: system}
// - thinking.budget_tokens → reasoning_effort
// - content 块逐项转换:thinking→reasoning_content, image→data URL,
//   tool_use→tool_calls, tool_result→role=tool(tool_result 先发保相邻,
//   文本 "\n\n" join 留 tool 消息;内嵌 image 抽出到随后 user 消息 parts)
// - tools input_schema → parameters
// - tool_choice 映射
// - stop_sequences → stop

use std::collections::HashSet;

use serde_json::{json, Value};

use super::{ConvertError, Result};

/// Anthropic messages → OpenAI chat/completions
///
/// 对齐 顶层键序、system 数组形态、messages 内 role=system 提取、user 透传。
/// 差异:content 字符串保持数组(采用跨协议形态归一化,解决客户端跨轮
/// 数组/字符串漂移,见 convert_message 注释)。
pub fn convert_to_openai_chat(body: &mut Value, upstream_model: &str) -> Result<()> {
    let mut openai = serde_json::Map::new();

    // 对齐 顶层键序:model,max_tokens,temperature/top_p,stop,stream,
    // reasoning_effort,messages,tools,tool_choice,user
    openai.insert("model".into(), json!(upstream_model));

    // max_tokens 透传
    if let Some(val) = body.get("max_tokens") {
        openai.insert("max_tokens".into(), val.clone());
    }
    // 对齐:temperature 与 top_p 互斥,top_p 仅在无 temperature 时发
    if let Some(val) = body.get("temperature") {
        openai.insert("temperature".into(), val.clone());
    } else if let Some(val) = body.get("top_p") {
        openai.insert("top_p".into(), val.clone());
    }
    // stop_sequences → stop(单元素发字符串,一致)
    if let Some(seqs) = body.get("stop_sequences").and_then(|v| v.as_array()) {
        let stops: Vec<&str> = seqs.iter().filter_map(|s| s.as_str()).collect();
        if stops.len() == 1 {
            openai.insert("stop".into(), json!(stops[0]));
        } else if !stops.is_empty() {
            openai.insert("stop".into(), json!(stops));
        }
    }
    openai.insert(
        "stream".into(),
        body.get("stream").unwrap_or(&json!(false)).clone(),
    );

    // thinking → reasoning_effort(忠实 thinking 映射;顶层 output_config 优先)
    if let Some(effort) = crate::thinking::resolve_effort_from_body(body) {
        // 钳制到模型支持级别(查注册表)
        let effort = crate::thinking::clamp_effort(effort, upstream_model);
        openai.insert("reasoning_effort".into(), json!(effort));
    }

    let mut messages: Vec<Value> = Vec::new();

    // System → messages[0](逐块剥离计费归属、非 Claude 目标身份声明与空白,输出 text 数组,一致)
    if let Some(system) = body.get("system") {
        let mut items: Vec<Value> = Vec::new();
        match system {
            Value::String(s) => {
                if !super::is_ignorable_system_text(s, upstream_model) {
                    items.push(json!({"type": "text", "text": s.trim()}));
                }
            }
            Value::Array(blocks) => {
                for b in blocks {
                    if let Some(t) = b.get("text").and_then(|v| v.as_str()) {
                        if !super::is_ignorable_system_text(t, upstream_model) {
                            items.push(json!({"type": "text", "text": t.trim()}));
                        }
                    }
                }
            }
            _ => return Err(ConvertError::InvalidType("system".into())),
        }
        if !items.is_empty() {
            messages.push(json!({"role": "system", "content": items}));
        }
    }

    // Messages 逐条转换
    if let Some(message_arr) = body.get("messages").and_then(|v| v.as_array()) {
        for msg in message_arr {
            let role = msg
                .get("role")
                .and_then(|v| v.as_str())
                .ok_or_else(|| ConvertError::MissingField("role".into()))?;
            let content = msg.get("content").cloned().unwrap_or(json!(""));

            // messages 内 role=system:提取文本包 <system-reminder> 转 user
            if role == "system" {
                if let Some(reminder) = system_reminder_text(&content, upstream_model) {
                    let converted = convert_message("user", &json!(reminder))?;
                    messages.extend(converted);
                }
                continue;
            }

            // 单条消息转换结果(可能展开成多条:tool_result 在前)
            let converted = convert_message(role, &content)?;
            messages.extend(converted);
        }
    }

    // 对齐 键序:messages 在 tools 之前
    openai.insert("messages".into(), json!(messages));

    // Tools: input_schema → parameters
    // 存活工具名集合,供 tool_choice declared 校验:命名 choice 只有在指向
    // 已声明工具时保留(对齐 sub2api convertAnthropicToolChoiceToChat 的
    // declared 集合语义)。无存活工具时不写 tools 字段(对齐 sub2api
    // `len(out.Tools) > 0` 才处理)。
    let mut declared_tools: HashSet<String> = HashSet::new();
    if let Some(tools) = body.get("tools").and_then(|v| v.as_array()) {
        let mut openai_tools = Vec::new();
        for tool in tools {
            let name = tool.get("name").and_then(|v| v.as_str()).unwrap_or("");
            // web 服务端工具丢弃(无 Chat Completions 等价,对齐 anthropicToolsToChatTools)
            let tool_type = tool.get("type").and_then(|v| v.as_str()).unwrap_or("");
            if super::is_web_search_tool_type(tool_type) {
                continue;
            }
            if !name.is_empty() {
                declared_tools.insert(name.to_string());
            }
            let description = tool
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let mut fn_obj = json!({
                "type": "function",
                "function": {
                    "name": name,
                    "description": description,
                    "parameters": {"type": "object", "properties": {}, "required": []}
                }
            });
            if let Some(schema) = tool.get("input_schema") {
                fn_obj["function"]["parameters"] =
                    super::normalize_object_schema_properties(schema.clone());
            }
            // 严格上游(xAI/new-api)校验:object schema 有 properties 时必须带
            // required 数组,缺失报 standard_violation "required: null is not
            // of type array"。幂等,不改变已存在的 required。
            let params = &mut fn_obj["function"]["parameters"];
            if params.get("type").and_then(|v| v.as_str()) == Some("object")
                && params.get("properties").is_some()
                && params.get("required").is_none()
            {
                params["required"] = json!([]);
            }
            openai_tools.push(fn_obj);
        }
        if !openai_tools.is_empty() {
            openai.insert("tools".into(), json!(openai_tools));
        }
    }

    // tool_choice 映射(仅当有存活工具时处理,对齐 sub2api 请求侧入口判断)
    if !declared_tools.is_empty() {
        if let Some(tc) = body.get("tool_choice") {
            if let Some(choice) = convert_tool_choice(tc, &declared_tools) {
                openai.insert("tool_choice".into(), choice);
            }
        }
    }

    // user 参数透传(一致)
    if let Some(user) = body.get("user") {
        openai.insert("user".into(), user.clone());
    }

    // 对齐 SetBoolIfDifferent(stream_options.include_usage, true):
    // 强制上游在流尾发 usage chunk。kimi/moonshot 等上游未开启时全程无
    // usage,响应 usage 全 0,客户端 ccstatusline 无 context 显示。
    // 仅流式注入:非流请求带 stream_options 部分上游可能拒绝。
    if body
        .get("stream")
        .and_then(|v| v.as_bool())
        .unwrap_or(false)
    {
        openai.insert("stream_options".into(), json!({"include_usage": true}));
    }

    *body = Value::Object(openai);
    Ok(())
}

/// messages 内 role=system 消息 → 文本提取,包 <system-reminder> 标记
/// (对齐 common.ClaudeMessageSystemReminderText)。
fn system_reminder_text(content: &Value, upstream_model: &str) -> Option<String> {
    let parts: Vec<&str> = match content {
        Value::String(s) => {
            if super::is_ignorable_system_text(s, upstream_model) {
                return None;
            }
            vec![s.trim()]
        }
        Value::Array(blocks) => blocks
            .iter()
            .filter_map(|b| {
                if b.get("type").and_then(|t| t.as_str()) != Some("text") {
                    return None;
                }
                let t = b.get("text").and_then(|v| v.as_str())?;
                if super::is_ignorable_system_text(t, upstream_model) {
                    None
                } else {
                    Some(t.trim())
                }
            })
            .collect(),
        _ => return None,
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

/// 对齐 CPA `shouldMapClaudeThinkingToGPTReasoning` 默认路径。
///
/// 无/空签名过(同链路 chat 历史按设计无签)。有签名只认 GPT Fernet
/// 形状(`gAAAA` 前缀,CPA `InspectGPTReasoningSignature` 的廉价判别)。
/// 过门回放 thinking 正文;签名本身不进 Chat Completions。
/// 不抄 responses 的 grok 任意放行,也不抄 CPA compat。
fn should_map_thinking_to_reasoning(part: &Value) -> bool {
    let sig = part
        .get("signature")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    sig.is_empty() || sig.starts_with("gAAAA")
}

/// 转换单条消息,返回一个或多个 openai 消息(tool_result 展开在前)
fn convert_message(role: &str, content: &Value) -> Result<Vec<Value>> {
    // 字符串内容:统一为 text 数组(保留 跨协议形态归一化)。
    // CC 客户端同一条消息当轮发数组、历史重建发字符串,若不统一,跨轮字节
    // 漂移破坏上游缓存前缀(4096 回归)。空字符串丢弃消息(一致)。
    if let Some(s) = content.as_str() {
        if s.is_empty() {
            return Ok(Vec::new());
        }
        return Ok(vec![
            json!({"role": role, "content": [{"type": "text", "text": s}]}),
        ]);
    }

    // 空内容(缺失/null):丢弃消息(一致)
    if content.is_null() {
        return Ok(Vec::new());
    }

    let Some(parts) = content.as_array() else {
        return Err(ConvertError::InvalidType("content".into()));
    };

    let mut content_items: Vec<Value> = Vec::new();
    let mut reasoning_parts: Vec<String> = Vec::new();
    let mut tool_calls: Vec<Value> = Vec::new();
    let mut tool_results: Vec<Value> = Vec::new();
    // tool_result 抽出的图片 parts(对齐 sub2api toolResultImageParts),
    // 并入该消息末尾的 user 消息 parts 尾部;文本始终留在 tool 消息
    let mut deferred_images: Vec<Value> = Vec::new();

    for part in parts {
        let ptype = part.get("type").and_then(|v| v.as_str()).unwrap_or("");
        match ptype {
            "thinking" => {
                // 仅 assistant 映射(防注入)。门闩见 should_map_thinking_to_reasoning。
                if role == "assistant" && should_map_thinking_to_reasoning(part) {
                    if let Some(t) = part.get("thinking").and_then(|v| v.as_str()) {
                        if !t.trim().is_empty() {
                            reasoning_parts.push(t.to_string());
                        }
                    }
                }
            }
            "redacted_thinking" => { /* 显式忽略 */ }
            "text" => {
                if let Some(t) = part.get("text").and_then(|v| v.as_str()) {
                    // 对齐 convertClaudeContentPart:空白块与计费归属块剥离
                    if !t.trim().is_empty() && !super::is_attribution_text(t) {
                        content_items.push(json!({"type": "text", "text": t}));
                    }
                }
            }
            "image" => {
                if let Some(url) = image_to_url(part) {
                    content_items.push(json!({
                        "type": "image_url",
                        "image_url": {"url": url}
                    }));
                }
            }
            "tool_use" => {
                // 仅 assistant 允许
                if role == "assistant" {
                    let id = part.get("id").and_then(|v| v.as_str()).unwrap_or("");
                    let name = part.get("name").and_then(|v| v.as_str()).unwrap_or("");
                    let args = part
                        .get("input")
                        .map(|i| i.to_string())
                        .unwrap_or_else(|| "{}".to_string());
                    tool_calls.push(json!({
                        "id": id,
                        "type": "function",
                        "function": {"name": name, "arguments": args}
                    }));
                }
            }
            "tool_result" => {
                let tool_call_id = part
                    .get("tool_use_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("");
                let content_val = part.get("content").cloned().unwrap_or(json!(""));
                let (out_content, images) = convert_tool_result_content(&content_val);
                deferred_images.extend(images);
                tool_results.push(json!({
                    "role": "tool",
                    "tool_call_id": tool_call_id,
                    "content": out_content
                }));
            }
            _ => {}
        }
    }

    let mut out = Vec::new();
    // tool_result 先发(紧跟上一轮 assistant 的 tool_calls)
    out.extend(tool_results);

    // assistant:单消息合并 content + reasoning_content + tool_calls
    if role == "assistant" {
        let has_content = !content_items.is_empty();
        let has_reasoning = !reasoning_parts.is_empty();
        let has_tool_calls = !tool_calls.is_empty();
        if has_content || has_reasoning || has_tool_calls {
            let mut msg = json!({"role": "assistant"});
            if has_content {
                msg["content"] = json!(content_items);
            } else {
                msg["content"] = json!("");
            }
            if has_reasoning {
                msg["reasoning_content"] = json!(reasoning_parts.join("\n\n"));
            }
            if has_tool_calls {
                msg["tool_calls"] = json!(tool_calls);
            }
            out.push(msg);
        }
    } else {
        // 非 assistant(通常 user):纯 content。tool_result 抽出的图片追加到
        // parts 尾部(对齐 sub2api anthropicUserToChatMessages:先收集原 text/
        // image blocks,再 append toolResultImageParts;图片存在才用数组形态)
        let mut items = content_items.clone();
        items.extend(deferred_images);
        if !items.is_empty() {
            out.push(json!({"role": role, "content": items}));
        }
    }

    Ok(out)
}

/// tool_result content → (tool 消息 content, 图片 parts)。
/// 对齐 sub2api convertToolResultOutput:文本(text 数组 "\n\n" join)始终留在
/// tool 消息,有图也不例外(sub2api 同样不清文本);内嵌 image 抽出到随后的
/// user 消息 parts。空内容 → "(no output)" 占位(对齐 DSH flattenText(...) ||
/// '(no output)' / sub2api "(empty)")。
fn convert_tool_result_content(content: &Value) -> (Value, Vec<Value>) {
    match content {
        Value::String(s) => {
            if s.trim().is_empty() {
                (Value::String("(no output)".to_string()), Vec::new())
            } else {
                (content.clone(), Vec::new())
            }
        }
        Value::Array(parts) => {
            let mut texts: Vec<&str> = Vec::new();
            let mut images: Vec<Value> = Vec::new();
            for p in parts {
                match p.get("type").and_then(|t| t.as_str()) {
                    // 对齐 sub2api b.Text != "":仅精确空串滤除
                    Some("text") => {
                        if let Some(t) = p.get("text").and_then(|v| v.as_str()) {
                            if !t.is_empty() {
                                texts.push(t);
                            }
                        }
                    }
                    Some("image") => {
                        if let Some(url) = image_to_url(p) {
                            images.push(json!({"type": "image_url", "image_url": {"url": url}}));
                        }
                    }
                    _ => {}
                }
            }
            let joined = texts.join("\n\n");
            if joined.is_empty() {
                (Value::String("(no output)".to_string()), images)
            } else {
                (Value::String(joined), images)
            }
        }
        Value::Object(_) => {
            if content.get("type").and_then(|t| t.as_str()) == Some("image") {
                if let Some(url) = image_to_url(content) {
                    return (
                        Value::String("(no output)".to_string()),
                        vec![json!({"type": "image_url", "image_url": {"url": url}})],
                    );
                }
            }
            match content.get("text").and_then(|v| v.as_str()) {
                Some(t) if !t.trim().is_empty() => (Value::String(t.to_string()), Vec::new()),
                _ => (Value::String("(no output)".to_string()), Vec::new()),
            }
        }
        _ => (Value::String("(no output)".to_string()), Vec::new()),
    }
}

/// image block → data URL(base64)或 url
/// 对齐 convertClaudeContentPart:media_type 空默认 application/octet-stream,
/// 无 source 时回退到顶层 url。
fn image_to_url(part: &Value) -> Option<String> {
    let url = part.get("source").and_then(|source| {
        let media_type = source
            .get("media_type")
            .and_then(|v| v.as_str())
            .unwrap_or("application/octet-stream");
        match source.get("type").and_then(|v| v.as_str()) {
            Some("base64") => {
                let data = source.get("data").and_then(|v| v.as_str())?;
                Some(format!("data:{media_type};base64,{data}"))
            }
            Some("url") => source
                .get("url")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string()),
            _ => None,
        }
    });
    url.or_else(|| {
        part.get("url")
            .and_then(|v| v.as_str())
            .map(|s| s.to_string())
    })
}

/// tool_choice 映射:auto→auto, any→required, tool→specific function
/// 命名 choice(tool)只有在指向已声明工具时保留,否则丢弃
/// (对齐 sub2api convertAnthropicToolChoiceToChat 的 tool 分支)。
fn convert_tool_choice(tc: &Value, declared: &HashSet<String>) -> Option<Value> {
    let ty = tc.get("type").and_then(|v| v.as_str())?;
    match ty {
        "auto" => Some(json!("auto")),
        "any" => Some(json!("required")),
        "none" => Some(json!("none")),
        "tool" => {
            let name = tc.get("name").and_then(|v| v.as_str()).unwrap_or("");
            if !declared.contains(name) {
                return None;
            }
            Some(json!({"type": "function", "function": {"name": name}}))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_basic_conversion() {
        let mut body = json!({
            "model": "test-opus-5",
            "messages": [{"role": "user", "content": "hello"}],
            "max_tokens": 1024
        });
        convert_to_openai_chat(&mut body, "gpt-4").unwrap();
        assert_eq!(body["model"], "gpt-4");
        assert_eq!(body["messages"][0]["role"], "user");
    }

    #[test]
    fn test_system_string() {
        let mut body = json!({
            "model": "test",
            "system": "You are helpful",
            "messages": [{"role": "user", "content": "hi"}]
        });
        convert_to_openai_chat(&mut body, "gpt").unwrap();
        assert_eq!(body["messages"][0]["role"], "system");
        // content 为 text 数组(对齐 system 数组形态)
        assert_eq!(body["messages"][0]["content"][0]["text"], "You are helpful");
    }

    #[test]
    fn test_thinking_budget_to_effort() {
        let mut body = json!({
            "model": "test",
            "thinking": {"type": "enabled", "budget_tokens": 20000},
            "messages": []
        });
        convert_to_openai_chat(&mut body, "gpt").unwrap();
        assert_eq!(body["reasoning_effort"], "high");
    }

    #[test]
    fn test_tool_use_and_result() {
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "user", "content": "weather please"},
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "get_weather", "input": {"city": "beijing"}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": "sunny"}
                ]}
            ]
        });
        convert_to_openai_chat(&mut body, "gpt").unwrap();
        let msgs = body["messages"].as_array().unwrap();
        // [user, assistant(tool_calls), tool] —— tool_result 紧跟 assistant tool_calls
        assert_eq!(msgs[1]["role"], "assistant");
        assert_eq!(msgs[1]["tool_calls"][0]["function"]["name"], "get_weather");
        assert_eq!(msgs[2]["role"], "tool");
        assert_eq!(msgs[2]["tool_call_id"], "t1");
    }

    #[test]
    fn test_consecutive_assistant_turns_not_merged() {
        // Claude→OpenAI 不 merge(对齐 CPA,只有反向需要 merge)
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "assistant", "content": [{"type": "thinking", "thinking": "t1"}]},
                {"role": "assistant", "content": [
                    {"type": "text", "text": "answer"},
                    {"type": "tool_use", "id": "c1", "name": "Read", "input": {"p": "a"}}
                ]},
                {"role": "user", "content": "go"}
            ]
        });
        convert_to_openai_chat(&mut body, "gpt").unwrap();
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 3);
        assert_eq!(msgs[0]["role"], "assistant");
        assert_eq!(msgs[0]["reasoning_content"], "t1");
        assert_eq!(msgs[1]["role"], "assistant");
        assert_eq!(msgs[1]["content"][0]["text"], "answer");
        assert_eq!(msgs[1]["tool_calls"][0]["id"], "c1");
        assert_eq!(msgs[2]["role"], "user");
    }

    #[test]
    fn test_image_base64() {
        let mut body = json!({
            "model": "test",
            "messages": [{"role": "user", "content": [
                {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AAAA"}}
            ]}]
        });
        convert_to_openai_chat(&mut body, "gpt").unwrap();
        let c = &body["messages"][0]["content"][0];
        assert_eq!(c["type"], "image_url");
        assert_eq!(c["image_url"]["url"], "data:image/png;base64,AAAA");
    }

    #[test]
    fn test_tools_schema() {
        let mut body = json!({
            "model": "test",
            "messages": [],
            "tools": [{
                "name": "t",
                "description": "tool",
                "input_schema": {"type": "object", "properties": {"a": {"type": "string"}}}
            }]
        });
        convert_to_openai_chat(&mut body, "gpt").unwrap();
        assert_eq!(body["tools"][0]["type"], "function");
        assert_eq!(
            body["tools"][0]["function"]["parameters"]["properties"]["a"]["type"],
            "string"
        );
    }

    #[test]
    fn test_system_array_with_empty_blocks() {
        let mut body = json!({
            "model": "test",
            "system": [
                {"type": "text", "text": "You are helpful"},
                {"type": "text", "text": ""},
                {"type": "text", "text": "Answer concisely"}
            ],
            "messages": []
        });
        convert_to_openai_chat(&mut body, "gpt").unwrap();
        // 空块剥离,text 数组保留非空项(对齐 appendSystemContent)
        let content = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 2);
        assert_eq!(content[0]["text"], "You are helpful");
        assert_eq!(content[1]["text"], "Answer concisely");
    }

    #[test]
    fn test_redacted_thinking_ignored() {
        let mut body = json!({
            "model": "test",
            "messages": [{
                "role": "assistant",
                "content": [
                    {"type": "text", "text": "answer"},
                    {"type": "redacted_thinking"}
                ]
            }]
        });
        convert_to_openai_chat(&mut body, "gpt").unwrap();
        // redacted_thinking 应被显式忽略
        let msg = &body["messages"][0];
        assert_eq!(msg["content"][0]["text"], "answer");
        assert_eq!(msg["content"].as_array().unwrap().len(), 1);
    }

    #[test]
    fn test_empty_messages_array() {
        let mut body = json!({
            "model": "test",
            "messages": []
        });
        convert_to_openai_chat(&mut body, "gpt").unwrap();
        assert_eq!(body["messages"].as_array().unwrap().len(), 0);
    }

    #[test]
    fn test_empty_tools_array() {
        // 无存活工具时不写 tools 字段(对齐 sub2api `len(out.Tools) > 0`)
        let mut body = json!({
            "model": "test",
            "messages": [],
            "tools": []
        });
        convert_to_openai_chat(&mut body, "gpt").unwrap();
        assert!(body.get("tools").is_none());
    }

    #[test]
    fn test_web_search_tools_filtered_dropped() {
        // web 服务端工具丢弃(无 Chat Completions 等价,对齐 anthropicToolsToChatTools);
        // 全被过滤时整个 tools 字段不写
        let mut body = json!({
            "model": "test",
            "messages": [],
            "tools": [
                {"type": "web_search_20250305", "name": "web_search", "input_schema": {"type": "object"}},
                {"name": "regular", "input_schema": {"type": "object"}}
            ],
            "tool_choice": {"type": "tool", "name": "regular"}
        });
        convert_to_openai_chat(&mut body, "gpt").unwrap();
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert_eq!(tools[0]["function"]["name"], "regular");
        assert_eq!(body["tool_choice"]["function"]["name"], "regular");

        // 只剩 web_search → tools 不写,choice 丢弃
        let mut body2 = json!({
            "model": "test",
            "messages": [],
            "tools": [{"type": "web_search_20260209", "name": "web_search", "input_schema": {"type": "object"}}],
            "tool_choice": {"type": "tool", "name": "web_search"}
        });
        convert_to_openai_chat(&mut body2, "gpt").unwrap();
        assert!(body2.get("tools").is_none());
        assert!(body2.get("tool_choice").is_none());
    }

    #[test]
    fn test_system_attribution_stripped() {
        let mut body = json!({
            "model": "test",
            "system": [
                {"type": "text", "text": "x-anthropic-billing-header: fp=abc123"},
                {"type": "text", "text": "Real instructions"}
            ],
            "messages": []
        });
        convert_to_openai_chat(&mut body, "gpt").unwrap();
        let content = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["text"], "Real instructions");
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

        convert_to_openai_chat(&mut body, "claude-opus-5").unwrap();

        assert_eq!(
            body["messages"][0]["content"][0]["text"],
            "You are Claude Code, Anthropic's official CLI for Claude."
        );
    }

    #[test]
    fn test_system_only_attribution_dropped() {
        let mut body = json!({
            "model": "test",
            "system": "  x-anthropic-billing-header: fp=abc123",
            "messages": [{"role": "user", "content": "hi"}]
        });
        convert_to_openai_chat(&mut body, "gpt").unwrap();
        // system 消息不应存在,messages[0] 直接是 user
        assert_eq!(body["messages"][0]["role"], "user");
    }

    #[test]
    fn test_system_filters_attribution_and_claude_identity() {
        let mut body = json!({
            "model": "test",
            "system": [
                {"type": "text", "text": "x-anthropic-billing-header: fp=xyz"},
                {"type": "text", "text": "You are a Claude agent, built on Anthropic's Claude Agent SDK."},
                {"type": "text", "text": "You are Claude Code, Anthropic's official CLI for Claude."},
                {"type": "text", "text": "You are helpful"}
            ],
            "messages": [{"role": "user", "content": "hi"}]
        });
        convert_to_openai_chat(&mut body, "gpt").unwrap();
        let content = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["text"], "You are helpful");
    }

    #[test]
    fn test_system_omitted_when_all_filtered() {
        let mut body = json!({
            "model": "test",
            "system": "You are Claude Code, Anthropic's official CLI for Claude.",
            "messages": [{"role": "user", "content": "hi"}]
        });
        convert_to_openai_chat(&mut body, "gpt").unwrap();
        assert_eq!(body["messages"].as_array().unwrap().len(), 1);
        assert_eq!(body["messages"][0]["role"], "user");
    }

    #[test]
    fn test_message_text_attribution_stripped() {
        let mut body = json!({
            "model": "test",
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "x-anthropic-billing-header: fp=xyz"},
                {"type": "text", "text": "real question"}
            ]}]
        });
        convert_to_openai_chat(&mut body, "gpt").unwrap();
        let content = body["messages"][0]["content"].as_array().unwrap();
        assert_eq!(content.len(), 1);
        assert_eq!(content[0]["text"], "real question");
    }

    #[test]
    fn test_thinking_signature_gate_maps_unsigned_and_gpt() {
        // 无签回放; Claude/未知签名整块扔; GPT gAAAA 回放正文,签名不进 body
        let mut body = json!({
            "model": "test",
            "messages": [{"role": "assistant", "content": [
                {"type": "thinking", "thinking": "unsigned"},
                {"type": "thinking", "thinking": "claude-signed", "signature": "C4x2-opaque"},
                {"type": "thinking", "thinking": "gpt-signed", "signature": "gAAAA-fake"},
                {"type": "text", "text": "answer"}
            ]}]
        });
        convert_to_openai_chat(&mut body, "gpt").unwrap();
        let msg = &body["messages"][0];
        assert_eq!(msg["reasoning_content"], "unsigned\n\ngpt-signed");
        assert_eq!(msg["content"][0]["text"], "answer");
        let reasoning = msg["reasoning_content"].as_str().unwrap();
        assert!(!reasoning.contains("gAAAA"));
        assert!(!reasoning.contains("C4x2"));
    }

    #[test]
    fn test_thinking_grok_model_does_not_pass_foreign_signature() {
        // chat 无 grok 特例: Claude 签名整块扔,不抄 responses 任意放行
        let mut body = json!({
            "model": "test",
            "messages": [{"role": "assistant", "content": [
                {"type": "thinking", "thinking": "keep-out", "signature": "C4x2 opaque"},
                {"type": "text", "text": "answer"}
            ]}]
        });
        convert_to_openai_chat(&mut body, "grok-3").unwrap();
        let msg = &body["messages"][0];
        assert!(msg.get("reasoning_content").is_none());
        assert_eq!(msg["content"][0]["text"], "answer");
    }

    #[test]
    fn test_tool_result_array_joined_to_string() {
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "f", "input": {}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": [
                        {"type": "text", "text": "part1"},
                        {"type": "text", "text": "part2"}
                    ]}
                ]}
            ]
        });
        convert_to_openai_chat(&mut body, "gpt").unwrap();
        let tool_msg = &body["messages"][1];
        assert_eq!(tool_msg["role"], "tool");
        assert_eq!(tool_msg["content"], "part1\n\npart2");
    }

    #[test]
    fn test_tool_result_image_extracted_to_user_message() {
        // 图片抽出(对齐 sub2api):图片并入随后 user 消息 parts 尾部,
        // 文本留 tool 消息
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "f", "input": {}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": [
                        {"type": "text", "text": "see:"},
                        {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AA"}}
                    ]}
                ]}
            ]
        });
        convert_to_openai_chat(&mut body, "gpt").unwrap();
        // 对齐 sub2api:文本始终留 tool 消息,仅图片抽出
        let tool_msg = &body["messages"][1];
        assert_eq!(tool_msg["role"], "tool");
        assert_eq!(tool_msg["content"], "see:");
        let user_msg = &body["messages"][2];
        assert_eq!(user_msg["role"], "user");
        assert_eq!(user_msg["content"][0]["type"], "image_url");
    }

    #[test]
    fn test_tool_result_image_only_placeholder() {
        // 只有图片:tool 消息占位,图片进随后 user 消息
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "f", "input": {}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": [
                        {"type": "image", "source": {"type": "base64", "media_type": "image/png", "data": "AA"}}
                    ]}
                ]}
            ]
        });
        convert_to_openai_chat(&mut body, "gpt").unwrap();
        assert_eq!(body["messages"][1]["content"], "(no output)");
        assert_eq!(body["messages"][2]["content"][0]["type"], "image_url");
    }

    #[test]
    fn test_tool_result_empty_string_fallback() {
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "f", "input": {}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": ""}
                ]}
            ]
        });
        convert_to_openai_chat(&mut body, "gpt").unwrap();
        assert_eq!(body["messages"][1]["content"], "(no output)");
    }

    #[test]
    fn test_tool_result_empty_array_fallback() {
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "f", "input": {}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": []}
                ]}
            ]
        });
        convert_to_openai_chat(&mut body, "gpt").unwrap();
        assert_eq!(body["messages"][1]["content"], "(no output)");
    }

    #[test]
    fn test_tool_result_whitespace_only_fallback() {
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "assistant", "content": [
                    {"type": "tool_use", "id": "t1", "name": "f", "input": {}}
                ]},
                {"role": "user", "content": [
                    {"type": "tool_result", "tool_use_id": "t1", "content": [
                        {"type": "text", "text": "  \n  "}
                    ]}
                ]}
            ]
        });
        convert_to_openai_chat(&mut body, "gpt").unwrap();
        // 对齐 sub2api:文本过滤仅精确空串,纯空白保留原样
        assert_eq!(body["messages"][1]["content"], "  \n  ");
    }

    #[test]
    fn test_schema_properties_filled() {
        let mut body = json!({
            "model": "test",
            "messages": [],
            "tools": [{
                "name": "t",
                "description": "tool",
                "input_schema": {"type": "object"}
            }]
        });
        convert_to_openai_chat(&mut body, "gpt").unwrap();
        assert_eq!(
            body["tools"][0]["function"]["parameters"]["properties"],
            json!({})
        );
    }

    #[test]
    fn test_stop_single_is_string() {
        let mut body = json!({
            "model": "test",
            "messages": [],
            "stop_sequences": ["END"]
        });
        convert_to_openai_chat(&mut body, "gpt").unwrap();
        assert_eq!(body["stop"], "END");
    }

    #[test]
    fn test_temperature_suppresses_top_p() {
        let mut body = json!({
            "model": "test",
            "messages": [],
            "temperature": 0.5,
            "top_p": 0.9
        });
        convert_to_openai_chat(&mut body, "gpt").unwrap();
        assert_eq!(body["temperature"], 0.5);
        assert!(body.get("top_p").is_none());
    }

    #[test]
    fn test_empty_string_content_dropped() {
        // 空字符串 content 丢弃消息(一致)
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "user", "content": ""},
                {"role": "assistant", "content": ""}
            ]
        });
        convert_to_openai_chat(&mut body, "gpt").unwrap();
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 0);
    }

    #[test]
    fn test_schema_required_array_filled() {
        // 严格上游(xAI)校验:properties 存在时 required 必须为数组。
        // 缺失补 []。已有 required 保持不动(不破坏约束)。
        let mut body = json!({
            "model": "test",
            "messages": [{"role": "user", "content": "hi"}],
            "tools": [
                {"name": "bash", "description": "run", "input_schema": {"type": "object", "properties": {"cmd": {"type": "string"}}}},
                {"name": "strict", "description": "s", "input_schema": {"type": "object", "properties": {"x": {"type": "string"}}, "required": ["x"]}},
                {"name": "bare", "description": "b"}
            ]
        });
        convert_to_openai_chat(&mut body, "gpt").unwrap();
        let tools = body["tools"].as_array().unwrap();
        assert_eq!(tools[0]["function"]["parameters"]["required"], json!([]));
        assert_eq!(tools[1]["function"]["parameters"]["required"], json!(["x"]));
        assert_eq!(tools[2]["function"]["parameters"]["required"], json!([]));
    }

    #[test]
    fn test_null_content_dropped() {
        // null content 丢弃消息(一致)
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "user", "content": null},
                {"role": "assistant", "content": null}
            ]
        });
        convert_to_openai_chat(&mut body, "gpt").unwrap();
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 0);
    }

    #[test]
    fn test_empty_array_content_no_message() {
        // 空数组 content 无实际内容项 → 不输出消息(一致)
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "user", "content": []},
                {"role": "assistant", "content": []}
            ]
        });
        convert_to_openai_chat(&mut body, "gpt").unwrap();
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 0);
    }

    #[test]
    fn test_tool_choice_mappings() {
        // 无存活工具 → 不写 tool_choice(对齐 sub2api 入口判断)
        let mut body = json!({
            "model": "test",
            "messages": [],
            "tool_choice": {"type": "auto"}
        });
        convert_to_openai_chat(&mut body, "gpt").unwrap();
        assert!(body.get("tool_choice").is_none());

        let mut body2 = json!({
            "model": "test",
            "messages": [],
            "tools": [{"name": "t", "input_schema": {"type": "object"}}],
            "tool_choice": {"type": "any"}
        });
        convert_to_openai_chat(&mut body2, "gpt").unwrap();
        assert_eq!(body2["tool_choice"], "required");

        // 命名 choice 指向已声明工具 → 保留
        let mut body3 = json!({
            "model": "test",
            "messages": [],
            "tools": [{"name": "search", "input_schema": {"type": "object"}}],
            "tool_choice": {"type": "tool", "name": "search"}
        });
        convert_to_openai_chat(&mut body3, "gpt").unwrap();
        assert_eq!(body3["tool_choice"]["type"], "function");
        assert_eq!(body3["tool_choice"]["function"]["name"], "search");

        // 命名 choice 指向未声明工具 → 丢弃(对齐 sub2api tool 分支)
        let mut body4 = json!({
            "model": "test",
            "messages": [],
            "tools": [{"name": "search", "input_schema": {"type": "object"}}],
            "tool_choice": {"type": "tool", "name": "ghost"}
        });
        convert_to_openai_chat(&mut body4, "gpt").unwrap();
        assert!(body4.get("tool_choice").is_none());
    }

    #[test]
    fn test_message_system_reminder_to_user() {
        // messages 内 role=system:提取文本包 <system-reminder> 转 user(对齐 CPA)
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "system", "content": [
                    {"type": "text", "text": "Token usage: 1/2; 1 remaining"}
                ]}
            ]
        });
        convert_to_openai_chat(&mut body, "gpt").unwrap();
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 2);
        assert_eq!(msgs[0]["role"], "user");
        assert_eq!(msgs[0]["content"], json!([{"type": "text", "text": "hi"}]));
        assert_eq!(msgs[1]["role"], "user");
        // system reminder 也走 convert_message 归一化为数组
        assert_eq!(msgs[1]["content"][0]["type"], "text");
        assert!(msgs[1]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("<system-reminder>"));
        assert!(msgs[1]["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("Token usage"));
    }

    #[test]
    fn test_message_system_attribution_reminder_dropped() {
        // role=system 内容全为 attribution → 不输出 user 消息(一致)
        let mut body = json!({
            "model": "test",
            "messages": [
                {"role": "system", "content": "  x-anthropic-billing-header: fp=abc"}
            ]
        });
        convert_to_openai_chat(&mut body, "gpt").unwrap();
        let msgs = body["messages"].as_array().unwrap();
        assert_eq!(msgs.len(), 0);
    }

    #[test]
    fn test_user_param_passthrough() {
        // user 参数透传(一致)
        let mut body = json!({
            "model": "test",
            "messages": [],
            "user": "user-abc"
        });
        convert_to_openai_chat(&mut body, "gpt").unwrap();
        assert_eq!(body["user"], "user-abc");
    }

    #[test]
    fn test_top_level_key_order() {
        // 对齐 顶层键序:model,max_tokens,temperature/top_p,stop,stream,
        // reasoning_effort,messages,tools,tool_choice,user
        let mut body = json!({
            "model": "test",
            "messages": [{"role": "user", "content": "hi"}],
            "max_tokens": 100,
            "temperature": 0.5,
            "stop_sequences": ["END"],
            "stream": true,
            "thinking": {"type": "enabled", "budget_tokens": 4096},
            "tools": [{"name": "t", "input_schema": {"type": "object"}}],
            "tool_choice": {"type": "auto"},
            "user": "u1"
        });
        convert_to_openai_chat(&mut body, "gpt").unwrap();
        let keys: Vec<&str> = body
            .as_object()
            .unwrap()
            .keys()
            .map(|k| k.as_str())
            .collect();
        let expected = [
            "model",
            "max_tokens",
            "temperature",
            "stop",
            "stream",
            "reasoning_effort",
            "messages",
            "tools",
            "tool_choice",
            "user",
            "stream_options",
        ];
        assert_eq!(keys, expected);
        assert_eq!(body["stream_options"]["include_usage"], true);
    }

    #[test]
    fn test_stream_options_only_when_streaming() {
        // 非流请求不注入 stream_options(部分上游会拒绝该字段)
        let mut body = json!({
            "model": "test",
            "stream": false,
            "messages": [{"role": "user", "content": "hi"}]
        });
        convert_to_openai_chat(&mut body, "gpt").unwrap();
        assert!(
            body.get("stream_options").is_none(),
            "非流不应注入 stream_options"
        );

        // stream 缺省 = false(对齐 Anthropic API 语义)→ 不注入
        let mut body2 = json!({
            "model": "test",
            "messages": [{"role": "user", "content": "hi"}]
        });
        convert_to_openai_chat(&mut body2, "gpt").unwrap();
        assert!(
            body2.get("stream_options").is_none(),
            "stream 缺省应按非流处理"
        );
    }

    #[test]
    fn test_image_media_type_default_octet_stream() {
        // media_type 缺省 → application/octet-stream(一致)
        let mut body = json!({
            "model": "test",
            "messages": [{"role": "user", "content": [
                {"type": "image", "source": {"type": "base64", "data": "AAAA"}}
            ]}]
        });
        convert_to_openai_chat(&mut body, "gpt").unwrap();
        let url = body["messages"][0]["content"][0]["image_url"]["url"]
            .as_str()
            .unwrap();
        assert!(url.starts_with("data:application/octet-stream;base64,AAAA"));
    }

    #[test]
    fn test_image_top_level_url_fallback() {
        // 无 source 时回退顶层 url(一致)
        let mut body = json!({
            "model": "test",
            "messages": [{"role": "user", "content": [
                {"type": "image", "url": "https://example.com/img.png"}
            ]}]
        });
        convert_to_openai_chat(&mut body, "gpt").unwrap();
        let url = body["messages"][0]["content"][0]["image_url"]["url"]
            .as_str()
            .unwrap();
        assert_eq!(url, "https://example.com/img.png");
    }

    #[test]
    fn test_max_reasoning_effort_downgraded_to_xhigh() {
        // glm-5.1 注册表支持到 xhigh,max 自动降级
        let mut body = json!({
            "model": "test",
            "output_config": {"effort": "max"},
            "thinking": {"type": "adaptive"},
            "messages": [{"role": "user", "content": "test"}]
        });
        convert_to_openai_chat(&mut body, "glm-5.1").unwrap();
        assert_eq!(body["reasoning_effort"], "xhigh");
    }
}
