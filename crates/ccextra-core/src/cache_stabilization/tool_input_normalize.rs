//! PR-E11:将 `tool_use.input` 的键顺序归一化,以匹配工具 schema。
//!
//! 历史的 assistant `tool_use.input` 对象在跨轮次时键顺序不稳定,
//! 导致 cache prefix 出现字节漂移。在转换路径上,CPA 会把 `input.Raw`
//! 原样嵌入到 OpenAI 的 `arguments` 字符串中,因此该漂移会直接流向上游。
//!
//! 本模块对每个 `tool_use.input` 重新排序,使 schema 声明的键按 schema
//! `properties` 的顺序排在前面,其后是任何 schema 之外的键,按字典序排列。
//! 与 cache-fix 的 `tool-input-normalize`(它会 *丢弃* schema 之外的键——
//! 有损)不同,本模块保留它们:重排是为了稳定性,丢键是不可接受的。
//!
//! # 性质
//! - 幂等:已排序的 input 保持原样,返回 0。
//! - 无损:不丢弃任何键;仅改变顺序。
//! - 仅针对 Anthropic walker;OpenAI 分支返回 0。

use crate::cache_stabilization::drift_detector::ApiKind;
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};

/// 将 `tool_use.input` 的键顺序归一化,以匹配每个工具的 schema。
/// 返回被重建的 `tool_use` 块数。
pub fn normalize_tool_use_inputs(body: &mut Value, kind: ApiKind) -> usize {
    match kind {
        ApiKind::Anthropic => normalize_anthropic(body),
        // CC 特有模式;Anthropic 形式由 /v1/messages 和 /v1/pretransform/messages 负责
        ApiKind::OpenAiChat | ApiKind::OpenAiResponses => 0,
    }
}

fn normalize_anthropic(body: &mut Value) -> usize {
    // 构建 tool_name -> schema 属性键顺序的映射。
    let mut schemas: HashMap<String, Vec<String>> = HashMap::new();
    if let Some(Value::Array(tools)) = body.get("tools") {
        for tool in tools {
            let Some(name) = tool.get("name").and_then(Value::as_str) else {
                continue;
            };
            if let Some(Value::Object(props)) = tool.pointer("/input_schema/properties") {
                schemas.insert(name.to_string(), props.keys().cloned().collect());
            }
        }
    }
    if schemas.is_empty() {
        return 0;
    }

    let Some(Value::Array(messages)) = body.get_mut("messages") else {
        return 0;
    };
    let mut count = 0;

    for msg in messages.iter_mut() {
        if msg.get("role").and_then(Value::as_str) != Some("assistant") {
            continue;
        }
        let Some(Value::Array(content)) = msg.get_mut("content") else {
            continue;
        };
        for block in content.iter_mut() {
            if block.get("type").and_then(Value::as_str) != Some("tool_use") {
                continue;
            }
            let Some(name) = block.get("name").and_then(Value::as_str).map(String::from) else {
                continue;
            };
            let Some(schema_keys) = schemas.get(&name) else {
                continue;
            };
            let Some(Value::Object(input)) = block.get("input") else {
                continue;
            };

            // 期望顺序:input 中存在的 schema 键(按 schema 顺序),
            // 然后是 schema 之外的键(按字典序)。
            let schema_set: HashSet<&str> = schema_keys.iter().map(|s| s.as_str()).collect();
            let mut present_schema: Vec<String> = schema_keys
                .iter()
                .filter(|k| input.contains_key(k.as_str()))
                .cloned()
                .collect();
            let mut external: Vec<String> = input
                .keys()
                .filter(|k| !schema_set.contains(k.as_str()))
                .cloned()
                .collect();
            external.sort();

            let mut desired = Vec::with_capacity(present_schema.len() + external.len());
            desired.append(&mut present_schema);
            desired.append(&mut external);

            let current: Vec<String> = input.keys().cloned().collect();
            if current == desired {
                continue;
            }

            let mut rebuilt = Map::new();
            for k in &desired {
                if let Some(v) = input.get(k) {
                    rebuilt.insert(k.clone(), v.clone());
                }
            }
            block["input"] = Value::Object(rebuilt);
            count += 1;
        }
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn tools() -> Value {
        json!([{
            "name": "edit_file",
            "input_schema": {
                "type": "object",
                "properties": {
                    "path": {"type": "string"},
                    "content": {"type": "string"}
                }
            }
        }])
    }

    fn keys(input: &Value) -> Vec<String> {
        input.as_object().unwrap().keys().cloned().collect()
    }

    #[test]
    fn reorders_input_to_schema_order() {
        // input 以 content-then-path 到达;schema 声明 path-then-content
        let mut body = json!({
            "tools": tools(),
            "messages": [{
                "role": "assistant",
                "content": [{
                    "type": "tool_use",
                    "id": "t1",
                    "name": "edit_file",
                    "input": {"content": "hello", "path": "/a"}
                }]
            }]
        });
        let count = normalize_tool_use_inputs(&mut body, ApiKind::Anthropic);
        assert_eq!(count, 1);
        let input = &body["messages"][0]["content"][0]["input"];
        assert_eq!(keys(input), vec!["path", "content"]);
        assert_eq!(input["path"], "/a");
        assert_eq!(input["content"], "hello");
    }

    #[test]
    fn preserves_schema_external_keys_sorted_after() {
        // input 有 schema 之外的额外键——保留,按字典序排序,排在 schema 键之后
        let mut body = json!({
            "tools": tools(),
            "messages": [{
                "role": "assistant",
                "content": [{
                    "type": "tool_use",
                    "id": "t1",
                    "name": "edit_file",
                    "input": {"zeta": 1, "content": "c", "alpha": 2, "path": "/p"}
                }]
            }]
        });
        let count = normalize_tool_use_inputs(&mut body, ApiKind::Anthropic);
        assert_eq!(count, 1);
        let input = &body["messages"][0]["content"][0]["input"];
        assert_eq!(keys(input), vec!["path", "content", "alpha", "zeta"]);
        assert_eq!(input["zeta"], 1);
        assert_eq!(input["alpha"], 2);
    }

    #[test]
    fn already_ordered_input_is_noop() {
        let original = json!({
            "tools": tools(),
            "messages": [{
                "role": "assistant",
                "content": [{
                    "type": "tool_use", "id": "t1", "name": "edit_file",
                    "input": {"path": "/p", "content": "c"}
                }]
            }]
        });
        let mut body = original.clone();
        let count = normalize_tool_use_inputs(&mut body, ApiKind::Anthropic);
        assert_eq!(count, 0);
        assert_eq!(body, original);
    }

    #[test]
    fn idempotent() {
        let mut body = json!({
            "tools": tools(),
            "messages": [{
                "role": "assistant",
                "content": [{
                    "type": "tool_use", "id": "t1", "name": "edit_file",
                    "input": {"content": "c", "path": "/p"}
                }]
            }]
        });
        let c1 = normalize_tool_use_inputs(&mut body, ApiKind::Anthropic);
        assert_eq!(c1, 1);
        let snapshot = body.clone();
        let c2 = normalize_tool_use_inputs(&mut body, ApiKind::Anthropic);
        assert_eq!(c2, 0);
        assert_eq!(body, snapshot);
    }

    #[test]
    fn cross_turn_stable() {
        // 相同的 input,不同的键顺序——归一化后完全相同。
        let mk = |input: Value| {
            json!({
                "tools": tools(),
                "messages": [{
                    "role": "assistant",
                    "content": [{"type": "tool_use", "id": "t1", "name": "edit_file", "input": input}]
                }]
            })
        };
        let mut a = mk(json!({"content": "c", "path": "/p"}));
        let mut b = mk(json!({"path": "/p", "content": "c"}));
        normalize_tool_use_inputs(&mut a, ApiKind::Anthropic);
        normalize_tool_use_inputs(&mut b, ApiKind::Anthropic);
        assert_eq!(a, b);
    }

    #[test]
    fn no_tools_or_no_messages_returns_zero() {
        let mut no_tools = json!({"messages": [{"role": "assistant", "content": []}]});
        assert_eq!(
            normalize_tool_use_inputs(&mut no_tools, ApiKind::Anthropic),
            0
        );
        let mut no_msgs = json!({"tools": tools()});
        assert_eq!(
            normalize_tool_use_inputs(&mut no_msgs, ApiKind::Anthropic),
            0
        );
    }

    #[test]
    fn unknown_tool_name_skipped() {
        let original = json!({
            "tools": tools(),
            "messages": [{
                "role": "assistant",
                "content": [{
                    "type": "tool_use", "id": "t1", "name": "other_tool",
                    "input": {"z": 1, "a": 2}
                }]
            }]
        });
        let mut body = original.clone();
        let count = normalize_tool_use_inputs(&mut body, ApiKind::Anthropic);
        assert_eq!(count, 0);
        assert_eq!(body, original);
    }

    #[test]
    fn openai_branches_return_zero() {
        let mut body = json!({"tools": tools(), "messages": []});
        assert_eq!(normalize_tool_use_inputs(&mut body, ApiKind::OpenAiChat), 0);
        assert_eq!(
            normalize_tool_use_inputs(&mut body, ApiKind::OpenAiResponses),
            0
        );
    }

    #[test]
    fn skips_non_object_input() {
        // tool_use.input 是字符串而非对象——必须被安全跳过。
        let original = json!({
            "tools": tools(),
            "messages": [{
                "role": "assistant",
                "content": [{
                    "type": "tool_use", "id": "t1", "name": "edit_file",
                    "input": "not-an-object"
                }]
            }]
        });
        let mut body = original.clone();
        assert_eq!(normalize_tool_use_inputs(&mut body, ApiKind::Anthropic), 0);
        assert_eq!(body, original);
    }

    #[test]
    fn skips_tool_with_missing_properties() {
        // 工具声明了 input_schema 但没有 properties——不在 schemas 映射中,
        // 因此其 tool_use 块被跳过。
        let mut body = json!({
            "tools": [{
                "name": "no_props",
                "input_schema": {"type": "object"}
            }],
            "messages": [{
                "role": "assistant",
                "content": [{
                    "type": "tool_use", "id": "t1", "name": "no_props",
                    "input": {"zeta": 1, "alpha": 2}
                }]
            }]
        });
        let original = body.clone();
        assert_eq!(normalize_tool_use_inputs(&mut body, ApiKind::Anthropic), 0);
        assert_eq!(body, original);
    }
}
