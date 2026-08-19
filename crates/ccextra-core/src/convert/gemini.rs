// Anthropic → Gemini 协议转换(对齐 CLIProxyAPI convertClaudeRequestToGemini)

use serde_json::Value;
use std::collections::{HashMap, HashSet};

use super::gemini_schema::{clean_json_schema_for_gemini, clean_nested_schema_for_antigravity};
use super::is_attribution_text;
use super::message_convert::convert_messages;
use super::tool_sanitize::{sanitize_function_name, sanitized_function_name_map};

/// schema 清洗语义(对齐 CPA:Gemini 直连 vs Antigravity VALIDATED)
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum SchemaFlavor {
    Gemini,
    Antigravity,
}

/// 转换 Anthropic 请求体为 Gemini 格式
///
/// 返回 (Gemini 请求体, 短名→原名映射)
pub fn convert_to_gemini(body: &Value, upstream_model: &str) -> (Value, HashMap<String, String>) {
    convert_to_gemini_with(body, upstream_model, SchemaFlavor::Gemini)
}

/// 带 schema 语义的转换入口(Antigravity 走 VALIDATED 清洗)
pub fn convert_to_gemini_with(
    body: &Value,
    upstream_model: &str,
    flavor: SchemaFlavor,
) -> (Value, HashMap<String, String>) {
    let mut gemini = serde_json::json!({ "contents": [] });
    gemini["model"] = serde_json::json!(upstream_model);

    // 工具定义
    let (short_to_original, original_to_short) = match body.get("tools").and_then(|t| t.as_array())
    {
        Some(tools_arr) => {
            let (gemini_tools, short_map) = convert_tool_definitions(tools_arr, flavor);
            if !gemini_tools.is_empty() {
                gemini["tools"] = serde_json::json!([{ "functionDeclarations": gemini_tools }]);
            }
            let orig_map: HashMap<String, String> = short_map
                .iter()
                .map(|(s, o)| (o.clone(), s.clone()))
                .collect();
            (short_map, orig_map)
        }
        None => (HashMap::new(), HashMap::new()),
    };

    // system → systemInstruction(过滤 attribution 文本,对齐 CPA)
    let mut system_parts = Vec::new();
    match body.get("system") {
        Some(Value::String(s)) if !s.is_empty() && !is_attribution_text(s) => {
            system_parts.push(serde_json::json!({"text": s}));
        }
        Some(Value::Array(arr)) => {
            for block in arr {
                if block.get("type").and_then(|t| t.as_str()) != Some("text") {
                    continue;
                }
                if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                    if !text.is_empty() && !is_attribution_text(text) {
                        system_parts.push(serde_json::json!({"text": text}));
                    }
                }
            }
        }
        _ => {}
    }
    if !system_parts.is_empty() {
        // 对齐 CPA:数组形式带 role:"user"
        gemini["systemInstruction"] = serde_json::json!({
            "role": "user",
            "parts": system_parts
        });
    }

    // 消息
    if let Some(messages_arr) = body.get("messages").and_then(|m| m.as_array()) {
        // 对齐 CPA:Antigravity 保留带签名 thinking 块,Gemini 直连全丢
        gemini["contents"] = serde_json::json!(convert_messages(
            messages_arr,
            &original_to_short,
            flavor == SchemaFlavor::Antigravity
        ));
    }

    // 生成配置
    let mut generation_config = serde_json::Map::new();
    if let Some(max_tokens) = body.get("max_tokens") {
        generation_config.insert("maxOutputTokens".into(), max_tokens.clone());
    }
    if let Some(temperature) = body.get("temperature") {
        generation_config.insert("temperature".into(), temperature.clone());
    }
    if let Some(top_p) = body.get("top_p") {
        generation_config.insert("topP".into(), top_p.clone());
    }
    if let Some(top_k) = body.get("top_k") {
        generation_config.insert("topK".into(), top_k.clone());
    }
    if let Some(stop) = body.get("stop_sequences") {
        generation_config.insert("stopSequences".into(), stop.clone());
    }

    // thinking → thinkingConfig(对齐 CPA 字段名:thinkingBudget / thinkingLevel)
    if let Some(thinking_obj) = body.get("thinking").and_then(|t| t.as_object()) {
        match thinking_obj.get("type").and_then(|t| t.as_str()) {
            Some("enabled") => {
                if let Some(budget) = thinking_obj.get("budget_tokens").and_then(|b| b.as_i64()) {
                    generation_config.insert(
                        "thinkingConfig".into(),
                        serde_json::json!({"thinkingBudget": budget}),
                    );
                }
            }
            Some("adaptive") | Some("auto") => {
                // 对齐 CPA:effort 显式给则透传 thinkingLevel;否则按目标模型
                // thinking.max 发 thinkingBudget,查不到再兜底 "high"
                let effort = body
                    .pointer("/output_config/effort")
                    .and_then(|e| e.as_str())
                    .map(|e| e.trim().to_lowercase())
                    .filter(|e| !e.is_empty());

                // effort="max" 或缺失时走预算表 → thinkingBudget,兜底 high
                let use_budget_path = effort.as_deref() == Some("max") || effort.is_none();

                if use_budget_path {
                    // 对齐 CPA:thinking.max 预算表仅 Gemini 直连翻译器使用,
                    // Antigravity 翻译器缺省 thinkingLevel=high
                    let max_budget = if flavor == SchemaFlavor::Gemini {
                        gemini_thinking_max_budget(upstream_model)
                    } else {
                        None
                    };
                    if let Some(max_budget) = max_budget {
                        generation_config.insert(
                            "thinkingConfig".into(),
                            serde_json::json!({"thinkingBudget": max_budget}),
                        );
                    } else {
                        generation_config.insert(
                            "thinkingConfig".into(),
                            serde_json::json!({"thinkingLevel": "high"}),
                        );
                    }
                } else if let Some(e) = effort {
                    // 非 max 的 effort 值校验白名单(Gemini 接受 low/medium/high)
                    let valid_levels = ["low", "medium", "high"];
                    let level = if valid_levels.contains(&e.as_str()) {
                        e
                    } else {
                        "high".to_string() // 不合法值兜底
                    };
                    generation_config.insert(
                        "thinkingConfig".into(),
                        serde_json::json!({"thinkingLevel": level}),
                    );
                }
            }
            _ => {}
        }
    }

    if !generation_config.is_empty() {
        gemini["generationConfig"] = Value::Object(generation_config);
    }

    // tool_choice(对齐 CPA:auto/none/any/tool,名称清洗后与声明一致)
    if let Some(tool_choice) = body.get("tool_choice") {
        let (choice_type, choice_name) = match tool_choice {
            Value::Object(obj) => (
                obj.get("type").and_then(|t| t.as_str()).unwrap_or(""),
                obj.get("name").and_then(|n| n.as_str()).unwrap_or(""),
            ),
            Value::String(s) => (s.as_str(), ""),
            _ => ("", ""),
        };
        match choice_type {
            "auto" => {
                gemini["toolConfig"]["functionCallingConfig"]["mode"] = serde_json::json!("AUTO");
            }
            "none" => {
                gemini["toolConfig"]["functionCallingConfig"]["mode"] = serde_json::json!("NONE");
            }
            "any" => {
                gemini["toolConfig"]["functionCallingConfig"]["mode"] = serde_json::json!("ANY");
            }
            "tool" => {
                gemini["toolConfig"]["functionCallingConfig"]["mode"] = serde_json::json!("ANY");
                if !choice_name.is_empty() {
                    // 用本轮声明的短名;未声明时清洗兜底(对齐 CPA MapSanitizedFunctionName)
                    let gemini_name = original_to_short
                        .get(choice_name)
                        .cloned()
                        .unwrap_or_else(|| sanitize_function_name(choice_name));
                    gemini["toolConfig"]["functionCallingConfig"]["allowedFunctionNames"] =
                        serde_json::json!([gemini_name]);
                }
            }
            _ => {}
        }
    }

    (gemini, short_to_original)
}

/// 对齐 CPA registry models.json gemini 节 thinking.max(仅 Gemini 直连目标)
/// 未收录模型返回 None(兜底 thinkingLevel=high)
fn gemini_thinking_max_budget(model: &str) -> Option<i64> {
    Some(match model {
        "gemini-2.5-flash" | "gemini-2.5-flash-lite" => 24576,
        // 2.5-pro 与 3.x 系列统一 32768
        m if m.starts_with("gemini-") => 32768,
        _ => return None,
    })
}

/// 转换工具定义: Anthropic tools → Gemini functionDeclarations
///
/// 对齐 CPA:input_schema 深度清洗后以 parametersJsonSchema 承载,
/// 工具级多余字段(strict/input_examples/type/cache_control 等)不重建即丢弃,
/// web_search 服务端工具剥离(对齐 CPA:无 input_schema 自然排除)
///
/// 返回 (转换后的定义数组, 短名→原名映射)
pub fn convert_tool_definitions(
    tools: &[Value],
    flavor: SchemaFlavor,
) -> (Vec<Value>, HashMap<String, String>) {
    let mut gemini_tools = Vec::new();
    let mut short_to_original = HashMap::new();

    // 先收集原名,一次性构建映射(对齐 CPA SanitizedFunctionNameMap:
    // 重名共享映射,撞车加确定性 hash 后缀)
    let names: Vec<&str> = tools
        .iter()
        .filter(|t| {
            !super::is_web_search_tool_type(t.get("type").and_then(|x| x.as_str()).unwrap_or(""))
        })
        .filter_map(|t| t["name"].as_str())
        .filter(|n| !n.is_empty())
        .collect();
    let original_to_short = sanitized_function_name_map(&names);

    let mut seen_originals = HashSet::new();
    for tool in tools {
        // 服务端 web_search 工具剥离(对齐 CPA web_search.go 判定)
        if super::is_web_search_tool_type(tool.get("type").and_then(|t| t.as_str()).unwrap_or("")) {
            continue;
        }
        let original_name = tool["name"].as_str().unwrap_or("");
        if original_name.is_empty() {
            continue;
        }
        // 完全重名的声明去重(对齐 CPA DeduplicateFunctionDeclarations)
        if !seen_originals.insert(original_name.to_string()) {
            continue;
        }
        let short_name = original_to_short
            .get(original_name)
            .cloned()
            .unwrap_or_else(|| sanitize_function_name(original_name));

        let mut gemini_tool = serde_json::json!({ "name": short_name });
        if let Some(desc) = tool.get("description") {
            gemini_tool["description"] = desc.clone();
        }
        if let Some(input_schema) = tool.get("input_schema") {
            gemini_tool["parametersJsonSchema"] = match flavor {
                SchemaFlavor::Gemini => clean_json_schema_for_gemini(input_schema),
                SchemaFlavor::Antigravity => clean_nested_schema_for_antigravity(input_schema),
            };
        }

        gemini_tools.push(gemini_tool);
        short_to_original.insert(short_name.clone(), original_name.to_string());
    }

    (gemini_tools, short_to_original)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_convert_to_gemini_basic() {
        let anthropic = json!({
            "model": "claude-opus-5",
            "max_tokens": 4096,
            "messages": [{"role": "user", "content": "Hello"}]
        });
        let (gemini, short_map) = convert_to_gemini(&anthropic, "gemini-2.0");

        assert_eq!(gemini["model"], "gemini-2.0");
        assert_eq!(gemini["contents"].as_array().unwrap().len(), 1);
        assert_eq!(gemini["generationConfig"]["maxOutputTokens"], 4096);
        assert!(short_map.is_empty());
    }

    #[test]
    fn test_convert_to_gemini_sampling_params() {
        let anthropic = json!({
            "model": "m", "max_tokens": 100, "temperature": 0.5, "top_p": 0.9, "top_k": 40,
            "messages": [{"role": "user", "content": "hi"}]
        });
        let (gemini, _) = convert_to_gemini(&anthropic, "gemini-2.0");
        let gc = &gemini["generationConfig"];
        assert_eq!(gc["temperature"], 0.5);
        assert_eq!(gc["topP"], 0.9);
        assert_eq!(gc["topK"], 40);
    }

    #[test]
    fn test_convert_to_gemini_system_filters_attribution() {
        let anthropic = json!({
            "model": "m", "max_tokens": 100,
            "system": [
                {"type": "text", "text": "x-anthropic-billing-header: abc"},
                {"type": "text", "text": "You are helpful"}
            ],
            "messages": [{"role": "user", "content": "hi"}]
        });
        let (gemini, _) = convert_to_gemini(&anthropic, "gemini-2.0");
        let parts = gemini["systemInstruction"]["parts"].as_array().unwrap();
        assert_eq!(parts.len(), 1);
        assert_eq!(parts[0]["text"], "You are helpful");
        assert_eq!(gemini["systemInstruction"]["role"], "user");
    }

    #[test]
    fn test_convert_to_gemini_thinking_enabled() {
        let anthropic = json!({
            "model": "m", "max_tokens": 1000,
            "thinking": {"type": "enabled", "budget_tokens": 512},
            "messages": [{"role": "user", "content": "hi"}]
        });
        let (gemini, _) = convert_to_gemini(&anthropic, "gemini-2.0");
        // 对齐 CPA:thinkingBudget(int),非 budgetTokens/type
        assert_eq!(
            gemini["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            512
        );
        assert!(gemini["generationConfig"]["thinkingConfig"]
            .get("type")
            .is_none());
    }

    #[test]
    fn test_convert_to_gemini_thinking_adaptive() {
        let anthropic = json!({
            "model": "m", "max_tokens": 1000,
            "thinking": {"type": "adaptive"},
            "messages": [{"role": "user", "content": "hi"}]
        });
        // 对齐 CPA:无 effort 时已知模型发 thinkingBudget=max
        let (gemini, _) = convert_to_gemini(&anthropic, "gemini-2.5-pro");
        assert_eq!(
            gemini["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            32768
        );
        // 未知模型兜底 thinkingLevel=high
        let (gemini, _) = convert_to_gemini(&anthropic, "unknown-model");
        assert_eq!(
            gemini["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "high"
        );
        // 显式 effort 透传
        let mut with_effort = anthropic.clone();
        with_effort["output_config"] = json!({"effort": "LOW"});
        let (gemini, _) = convert_to_gemini(&with_effort, "gemini-2.5-pro");
        assert_eq!(
            gemini["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "low"
        );
    }

    #[test]
    fn test_convert_to_gemini_thinking_effort_max() {
        // effort="max" 应走预算表 → thinkingBudget,不发 thinkingLevel="max"
        let anthropic = json!({
            "model": "m", "max_tokens": 1000,
            "thinking": {"type": "adaptive"},
            "output_config": {"effort": "max"},
            "messages": [{"role": "user", "content": "hi"}]
        });
        let (gemini, _) = convert_to_gemini(&anthropic, "gemini-3.7");
        // 3.x 模型预算表返回 32768
        assert_eq!(
            gemini["generationConfig"]["thinkingConfig"]["thinkingBudget"],
            32768
        );
        assert!(gemini["generationConfig"]["thinkingConfig"]
            .get("thinkingLevel")
            .is_none());
    }

    #[test]
    fn test_convert_to_gemini_thinking_effort_invalid() {
        // 不合法 effort 值兜底 "high"
        let anthropic = json!({
            "model": "m", "max_tokens": 1000,
            "thinking": {"type": "adaptive"},
            "output_config": {"effort": "ultra"},
            "messages": [{"role": "user", "content": "hi"}]
        });
        let (gemini, _) = convert_to_gemini(&anthropic, "gemini-2.5-pro");
        assert_eq!(
            gemini["generationConfig"]["thinkingConfig"]["thinkingLevel"],
            "high"
        );
    }

    #[test]
    fn test_convert_to_gemini_tool_choice() {
        let base = |tc: Value| {
            json!({
                "model": "m", "max_tokens": 100,
                "tool_choice": tc,
                "tools": [{"name": "Read", "input_schema": {"type": "object"}}],
                "messages": [{"role": "user", "content": "hi"}]
            })
        };
        let (g, _) = convert_to_gemini(&base(json!({"type": "none"})), "m");
        assert_eq!(g["toolConfig"]["functionCallingConfig"]["mode"], "NONE");

        let (g, _) = convert_to_gemini(&base(json!({"type": "tool", "name": "Read"})), "m");
        assert_eq!(g["toolConfig"]["functionCallingConfig"]["mode"], "ANY");
        assert_eq!(
            g["toolConfig"]["functionCallingConfig"]["allowedFunctionNames"][0],
            "Read"
        );
    }

    #[test]
    fn test_convert_tool_definitions_uses_parameters_json_schema() {
        let tools = vec![json!({
            "name": "Read",
            "description": "Read a file",
            "cache_control": {"type": "ephemeral"},
            "input_schema": {
                "type": "object",
                "additionalProperties": false,
                "properties": {"path": {"type": "STRING"}}
            }
        })];
        let (gemini_tools, short_map) = convert_tool_definitions(&tools, SchemaFlavor::Gemini);

        assert_eq!(gemini_tools.len(), 1);
        assert_eq!(gemini_tools[0]["name"], "Read");
        // 对齐 CPA:parametersJsonSchema 承载,深度清洗后不转小写 type
        let schema = &gemini_tools[0]["parametersJsonSchema"];
        assert!(schema.get("additionalProperties").is_none());
        assert!(schema["description"]
            .as_str()
            .unwrap()
            .contains("No extra properties allowed"));
        assert_eq!(schema["properties"]["path"]["type"], "STRING");
        assert!(gemini_tools[0].get("input_schema").is_none());
        assert!(gemini_tools[0].get("cache_control").is_none());
        assert_eq!(short_map.get("Read").unwrap(), "Read");
    }

    #[test]
    fn test_convert_tool_definitions_sanitize_conflict() {
        let tools = vec![
            json!({"name": "tool-name", "input_schema": {"type": "object"}}),
            json!({"name": "tool_name", "input_schema": {"type": "object"}}),
        ];
        let (gemini_tools, short_map) = convert_tool_definitions(&tools, SchemaFlavor::Gemini);
        // 对齐 CPA:"-" 保留,不再冲突
        assert_eq!(gemini_tools[0]["name"], "tool-name");
        assert_eq!(gemini_tools[1]["name"], "tool_name");
        assert_eq!(short_map.get("tool-name").unwrap(), "tool-name");
        assert_eq!(short_map.get("tool_name").unwrap(), "tool_name");
    }
}
