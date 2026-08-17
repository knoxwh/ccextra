// Anthropic → Gemini 协议转换

use serde_json::Value;
use std::collections::{HashMap, HashSet};

use super::tool_id::generate_claude_tool_id;
use super::tool_sanitize::sanitize_tool_name;

/// 转换 Anthropic 请求体为 Gemini 格式
///
/// 返回 (Gemini 请求体, 短名→原名映射, 原名→Claude ID映射)
pub fn convert_to_gemini(
    body: &Value,
    upstream_model: &str,
) -> (Value, HashMap<String, String>, HashMap<String, String>) {
    use super::carrier::{extract_thought_signature, inject_thought_signature};
    use super::message_convert::convert_messages;

    let mut gemini = serde_json::json!({
        "contents": [],
        "generationConfig": {}
    });

    // 1. 模型名映射
    gemini["model"] = serde_json::json!(upstream_model);

    // 2. 转换工具定义
    let (short_to_original, original_to_claude_id) = if let Some(tools) = body.get("tools") {
        if let Some(tools_arr) = tools.as_array() {
            let (gemini_tools, short_map, claude_map) = convert_tool_definitions(tools_arr);

            if !gemini_tools.is_empty() {
                gemini["tools"] = serde_json::json!([{
                    "functionDeclarations": gemini_tools
                }]);
            }

            (short_map, claude_map)
        } else {
            (HashMap::new(), HashMap::new())
        }
    } else {
        (HashMap::new(), HashMap::new())
    };

    // 3. 提取思考签名
    let thought_signature = if let Some(system) = body.get("system") {
        if let Some(system_arr) = system.as_array() {
            extract_thought_signature(system_arr)
        } else {
            None
        }
    } else {
        None
    };

    // 4. 处理 system
    let mut system_parts = Vec::new();
    if let Some(system) = body.get("system") {
        if let Some(system_str) = system.as_str() {
            // system 是字符串
            if !system_str.is_empty() {
                system_parts.push(serde_json::json!({"text": system_str}));
            }
        } else if let Some(system_arr) = system.as_array() {
            // system 是数组，提取 text 块
            for block in system_arr {
                if let Some(block_type) = block.get("type").and_then(|t| t.as_str()) {
                    if block_type == "text" {
                        if let Some(text) = block.get("text").and_then(|t| t.as_str()) {
                            if !text.is_empty() {
                                system_parts.push(serde_json::json!({"text": text}));
                            }
                        }
                    }
                }
            }
        }
    }

    // systemInstruction 必须是 {"parts": [...]} 结构
    if !system_parts.is_empty() {
        gemini["systemInstruction"] = serde_json::json!({"parts": system_parts});
    }

    // 5. 转换消息
    if let Some(messages) = body.get("messages") {
        if let Some(messages_arr) = messages.as_array() {
            let contents = convert_messages(messages_arr, &original_to_claude_id);
            gemini["contents"] = serde_json::json!(contents);
        }
    }

    // 5. 注入思考签名
    if let Some(signature) = thought_signature {
        inject_thought_signature(&mut gemini, &signature);
    }

    // 6. 生成配置
    if let Some(max_tokens) = body.get("max_tokens") {
        gemini["generationConfig"]["maxOutputTokens"] = max_tokens.clone();
    }

    if let Some(top_p) = body.get("top_p") {
        gemini["generationConfig"]["topP"] = top_p.clone();
    }

    if let Some(stop) = body.get("stop_sequences") {
        gemini["generationConfig"]["stopSequences"] = stop.clone();
    }

    // thinking 配置转换
    if let Some(thinking) = body.get("thinking") {
        if let Some(thinking_obj) = thinking.as_object() {
            let mut thinking_config = serde_json::json!({});

            if let Some(thinking_type) = thinking_obj.get("type").and_then(|t| t.as_str()) {
                thinking_config["type"] = serde_json::json!(thinking_type.to_uppercase());
            }

            if let Some(budget_tokens) = thinking_obj.get("budget_tokens") {
                thinking_config["budgetTokens"] = budget_tokens.clone();
            }

            if !thinking_config.as_object().unwrap().is_empty() {
                gemini["generationConfig"]["thinkingConfig"] = thinking_config;
            }
        }
    }

    // 7. tool_choice 转换
    if let Some(tool_choice) = body.get("tool_choice") {
        if let Some(tc_obj) = tool_choice.as_object() {
            let mut tool_config = serde_json::json!({
                "functionCallingConfig": {}
            });

            if let Some(choice_type) = tc_obj.get("type").and_then(|t| t.as_str()) {
                match choice_type {
                    "auto" => {
                        tool_config["functionCallingConfig"]["mode"] = serde_json::json!("AUTO");
                    }
                    "any" => {
                        tool_config["functionCallingConfig"]["mode"] = serde_json::json!("ANY");
                    }
                    "tool" => {
                        if let Some(name) = tc_obj.get("name").and_then(|n| n.as_str()) {
                            // 查找对应的短名称（Gemini 侧使用的名称）
                            let gemini_name = short_to_original
                                .iter()
                                .find(|(_, orig)| orig.as_str() == name)
                                .map(|(short, _)| short.as_str())
                                .unwrap_or(name);

                            tool_config["functionCallingConfig"]["mode"] = serde_json::json!("ANY");
                            tool_config["functionCallingConfig"]["allowedFunctionNames"] =
                                serde_json::json!([gemini_name]);
                        }
                    }
                    _ => {}
                }
            }

            gemini["toolConfig"] = tool_config;
        }
    }

    (gemini, short_to_original, original_to_claude_id)
}

/// 转换工具定义: Anthropic tools → Gemini function_declarations
///
/// 返回 (转换后的定义数组, 短名→原名映射, 原名→Claude侧ID映射)
pub fn convert_tool_definitions(
    tools: &[Value],
) -> (Vec<Value>, HashMap<String, String>, HashMap<String, String>) {
    let mut gemini_tools = Vec::new();
    let mut short_to_original = HashMap::new();
    let mut original_to_claude_id = HashMap::new();
    let mut used_names = HashSet::new();

    for tool in tools {
        let original_name = tool["name"].as_str().unwrap_or("");
        if original_name.is_empty() {
            continue;
        }

        // 生成 Gemini 短名
        let short_name = sanitize_tool_name(original_name, &mut used_names);

        // 生成 Claude 侧 ID
        let claude_id = generate_claude_tool_id(original_name);

        // 构造 Gemini 工具定义
        let mut gemini_tool = serde_json::json!({
            "name": short_name,
        });

        if let Some(desc) = tool.get("description") {
            gemini_tool["description"] = desc.clone();
        }

        if let Some(input_schema) = tool.get("input_schema") {
            // 清理并规范化 schema
            let cleaned_schema = clean_schema(input_schema);
            gemini_tool["parameters"] = cleaned_schema;
        }

        gemini_tools.push(gemini_tool);
        short_to_original.insert(short_name.clone(), original_name.to_string());
        original_to_claude_id.insert(original_name.to_string(), claude_id);
    }

    (gemini_tools, short_to_original, original_to_claude_id)
}

/// 清理 JSON schema: 移除 additionalProperties、$schema，小写 type 字段
fn clean_schema(schema: &Value) -> Value {
    match schema {
        Value::Object(map) => {
            let mut cleaned = serde_json::Map::new();

            for (key, value) in map {
                // 跳过 additionalProperties 和 $schema
                if key == "additionalProperties" || key == "$schema" {
                    continue;
                }

                // 递归清理嵌套对象/数组
                let cleaned_value = clean_schema(value);

                // 特殊处理 type 字段：确保小写
                if key == "type" {
                    if let Some(type_str) = cleaned_value.as_str() {
                        cleaned.insert(key.clone(), serde_json::json!(type_str.to_lowercase()));
                    } else {
                        cleaned.insert(key.clone(), cleaned_value);
                    }
                } else {
                    cleaned.insert(key.clone(), cleaned_value);
                }
            }

            Value::Object(cleaned)
        }
        Value::Array(arr) => {
            Value::Array(arr.iter().map(|v| clean_schema(v)).collect())
        }
        _ => schema.clone(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_convert_to_gemini_basic() {
        let anthropic = serde_json::json!({
            "model": "claude-opus-5",
            "max_tokens": 4096,
            "messages": [
                {
                    "role": "user",
                    "content": "Hello"
                }
            ]
        });

        let (gemini, short_to_orig, orig_to_claude) = convert_to_gemini(&anthropic, "gemini-2.0");

        // 验证基本结构
        assert!(gemini.is_object());
        assert_eq!(gemini["model"], "gemini-2.0");
        assert!(gemini["contents"].is_array());
        assert_eq!(gemini["contents"].as_array().unwrap().len(), 1);
        assert_eq!(gemini["generationConfig"]["maxOutputTokens"], 4096);
        assert!(short_to_orig.is_empty());
        assert!(orig_to_claude.is_empty());
    }

    #[test]
    fn test_convert_to_gemini_with_tools() {
        let anthropic = serde_json::json!({
            "model": "claude-opus-5",
            "max_tokens": 2000,
            "messages": [
                {
                    "role": "user",
                    "content": "Use the Read tool"
                }
            ],
            "tools": [
                {
                    "name": "Read",
                    "description": "Read a file",
                    "input_schema": {
                        "type": "object",
                        "properties": {
                            "path": {"type": "string"}
                        }
                    }
                }
            ]
        });

        let (gemini, short_to_orig, orig_to_claude) =
            convert_to_gemini(&anthropic, "gemini-2.0-flash");

        // 验证工具转换
        assert!(gemini["tools"].is_array());
        let tools = gemini["tools"].as_array().unwrap();
        assert_eq!(tools.len(), 1);
        assert!(tools[0]["functionDeclarations"].is_array());

        let func_decls = tools[0]["functionDeclarations"].as_array().unwrap();
        assert_eq!(func_decls.len(), 1);
        assert_eq!(func_decls[0]["name"], "Read");

        // 验证映射
        assert_eq!(short_to_orig.get("Read").unwrap(), "Read");
        assert!(orig_to_claude
            .get("Read")
            .unwrap()
            .starts_with("cpa_gemini_"));
    }

    #[test]
    fn test_convert_tool_definitions_basic() {
        let tools = vec![
            serde_json::json!({
                "name": "Read",
                "description": "Read a file",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"}
                    }
                }
            }),
            serde_json::json!({
                "name": "Write",
                "description": "Write a file",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "content": {"type": "string"}
                    }
                }
            }),
        ];

        let (gemini_tools, short_to_orig, orig_to_claude) = convert_tool_definitions(&tools);

        assert_eq!(gemini_tools.len(), 2);
        assert_eq!(gemini_tools[0]["name"], "Read");
        assert_eq!(gemini_tools[1]["name"], "Write");

        assert_eq!(short_to_orig.get("Read").unwrap(), "Read");
        assert_eq!(short_to_orig.get("Write").unwrap(), "Write");

        assert!(orig_to_claude
            .get("Read")
            .unwrap()
            .starts_with("cpa_gemini_"));
        assert!(orig_to_claude
            .get("Write")
            .unwrap()
            .starts_with("cpa_gemini_"));
    }

    #[test]
    fn test_convert_tool_definitions_sanitize() {
        let tools = vec![serde_json::json!({
            "name": "mcp__context7__query-docs",
            "description": "Query documentation",
            "input_schema": {
                "type": "object",
                "properties": {}
            }
        })];

        let (gemini_tools, short_to_orig, _) = convert_tool_definitions(&tools);

        assert_eq!(gemini_tools.len(), 1);
        assert_eq!(gemini_tools[0]["name"], "mcp__context7__query_docs");
        assert_eq!(
            short_to_orig.get("mcp__context7__query_docs").unwrap(),
            "mcp__context7__query-docs"
        );
    }

    #[test]
    fn test_convert_tool_definitions_conflict() {
        let tools = vec![
            serde_json::json!({
                "name": "tool-name",
                "description": "First",
                "input_schema": {"type": "object"}
            }),
            serde_json::json!({
                "name": "tool_name",
                "description": "Second",
                "input_schema": {"type": "object"}
            }),
        ];

        let (gemini_tools, short_to_orig, _) = convert_tool_definitions(&tools);

        assert_eq!(gemini_tools.len(), 2);
        // 两个都会清洗为 tool_name,第二个追加后缀
        assert_eq!(gemini_tools[0]["name"], "tool_name");
        assert_eq!(gemini_tools[1]["name"], "tool_name_2");

        assert_eq!(short_to_orig.get("tool_name").unwrap(), "tool-name");
        assert_eq!(short_to_orig.get("tool_name_2").unwrap(), "tool_name");
    }
}
