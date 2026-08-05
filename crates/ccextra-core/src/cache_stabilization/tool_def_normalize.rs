//! PR-E1：工具数组确定性排序 + PR-E2：schema 键排序。
//!
//! 本模块为 Anthropic 和 OpenAI 两种工具形态提供 Phase E 归一化。
//! 当任一工具带有显式 `cache_control` 标记时，跳过归一化。

use super::json_walker::ToolWalker;
use md5::{Digest, Md5};
use serde_json::Value;
use sha2::Sha256;

/// 为可观测性记录的 Phase E 归一化标志。
#[derive(Debug, Clone, Copy, Default)]
pub struct NormalizationApplied {
    pub e1_tool_sort: bool,
    pub e2_schema_sort: bool,
}

/// 按名称确定性地对 `tools[]` 排序（原地操作）。
///
/// 排序键：`tool["name"]` 作为字符串。对于缺少名称的工具（罕见；API
/// 要求必须有名称，但畸形输入确实会到达代理），回退键为工具对象
/// canonical-JSON 序列化的 MD5 十六进制摘要。MD5 足够 —— 该值不透明，
/// 仅用于进程内排序，从不持久化，也从不跨主机比较。
///
/// 若排序改变了顺序则返回 `true`，若数组已有序则返回 `false`
/// （幂等信号）。调用方使用该信号发出结构化事件，使仪表盘可以看到
/// 该策略触发的频率。
///
/// # 稳定性
///
/// 使用稳定排序（`sort_by_key`）：相同键保留原始顺序。两个恰好发生
/// MD5 碰撞的无名工具将保持其原始相对顺序 —— 对任何实际输入而言
/// 碰撞都极其罕见，但该契约仍然成立。
///
/// 切片签名（`&mut [Value]`）优于 `&mut Vec<Value>`，符合 clippy
/// 的 `ptr_arg` 建议：调用方可传入 `Vec` 或任意 `&mut [Value]`，
/// 且我们不需要 `Vec` 特有的操作。
pub fn sort_tools_deterministically(tools: &mut [Value]) -> bool {
    // 捕获排序前的键序列，使返回值契约（`true` 当且仅当有元素移动）
    // 精确成立。我们比较键而非完整值，因为排序是按键进行的 ——
    // 相同键的交换不会影响缓存字节。
    let before: Vec<String> = tools.iter().map(sort_key).collect();
    tools.sort_by_key(sort_key);
    let after: Vec<String> = tools.iter().map(sort_key).collect();
    before != after
}

/// 为工具构建确定性排序键。仅在本模块内公开；公开 API 是
/// [`sort_tools_deterministically`]。
///
/// 在两个已知位置查找名称：
///
///   1. `tool["name"]` —— Anthropic 形态（`{"name": "...",
///      "input_schema": ...}`）。
///   2. `tool["function"]["name"]` —— OpenAI Chat Completions 形态
///      （`{"type": "function", "function": {"name": "...",
///      "parameters": ...}}`）。
///
/// 两家提供方都恰好把工具名称放在其中一个位置；两者都不匹配的工具
/// 是罕见的畸形输入，会回退到 MD5-of-canonical-JSON 回退方案。
fn sort_key(tool: &Value) -> String {
    if let Some(name) = tool.get("name").and_then(Value::as_str) {
        return name.to_string();
    }
    if let Some(name) = tool
        .get("function")
        .and_then(|f| f.get("name"))
        .and_then(Value::as_str)
    {
        return name.to_string();
    }
    // 无名工具的回退：canonical-JSON 序列化的 MD5。对于给定的 `Value`，
    // `serde_json::to_vec` 是确定性的，因为 `preserve_order` 工作区特性
    // 将对象键顺序固定为插入顺序。
    let serialized = serde_json::to_vec(tool).unwrap_or_default();
    let mut hasher = Md5::new();
    hasher.update(&serialized);
    let digest = hasher.finalize();
    // 手动十六进制编码以保持依赖面最小 —— 使用 `{:02x}` 的 `format!`
    // 会生成与 `hex::encode` 相同的小写十六进制。
    let mut out = String::with_capacity(32);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

/// 若任一工具对象在其顶层带有 `cache_control` 字段，则返回 `true`。
///
/// Anthropic API 把 `cache_control` 放在工具对象本身
/// （例如 `{"name": "x", "cache_control": {"type": "ephemeral"}, ...}`）。
/// 客户用它标记一个依赖工具在数组中*位置*的缓存断点 —— 对工具重新排序
/// 会改变标记"之前"的内容，从而悄然改变缓存范围，违背客户意图。
/// 因此当任一工具带有该标记时，我们跳过排序。
///
/// 此函数只检查顶层字段。嵌套在 `input_schema` 内的标记（任何公开 API
/// 都不会在那里放置）不会被捕获 —— 但它们也不依赖位置，因此安全契约
/// 仍然成立。
pub fn any_tool_has_cache_control(tools: &[Value]) -> bool {
    tools.iter().any(|tool| tool.get("cache_control").is_some())
}

/// 递归地对 `value` 中每个 JSON 对象节点的键进行排序（原地操作）。
/// PR-E2。
///
/// JSON Schema 允许任意键序，但缓存命中要求字节完全一致。不同 SDK
/// 序列化器以不同顺序发出键（有些排序，有些保留插入顺序，有些是
/// 哈希随机化的）。此遍历器将每个 `Value::Object` 重写为字母顺序的键，
/// 使相同的逻辑 schema 无论上游序列化器行为如何都序列化成相同的字节。
///
/// # 数组语义
///
/// JSON Schema 数组是有序的。`oneOf`、`anyOf`、`allOf`、`prefixItems`
/// 和 `enum` 的元素顺序都带有语义含义 —— 因此该遍历器逐元素递归
/// 进入数组，但不会对数组本身重新排序。（对 `oneOf` 重新排序在语义上
/// 是无操作，但仍会改变字节；我们保留客户顺序以尊重其意图。）
///
/// # 幂等性
///
/// 对已排序的 map 再次排序会得到字节一致的输出：我们重建一个按字母
/// 顺序填充的全新 `serde_json::Map`，而 `Map`（配合工作区 `preserve_order`
/// 特性）按插入顺序发出键。因此第二遍会生成相同的 `Map` 字面量。
///
/// # 标记安全
///
/// 与 PR-E1 不同，此函数没有 `cache_control` 标记检查。Anthropic API
/// 把 `cache_control` 放在工具*对象*本身
/// （`{"name": ..., "cache_control": {...}, "input_schema": {...}}`），
/// 而不是放在 `input_schema` 内部。对 `input_schema` 内的键排序不会移动
/// 标记，因此无论哪种情况，客户的缓存断点意图都得到保留。调用方因此
/// 可以自由地为任何工具传入 schema，无论其是否带有标记。
pub fn sort_schema_keys_recursive(value: &mut Value) {
    match value {
        Value::Object(map) => {
            // 先递归，使子节点在重建父节点之前完成归一化。递归顺序
            // 不影响正确性（每个子节点相互独立），但先做意味着父节点的
            // 排序 Map 一次性基于已排序的子节点构建 —— 无重复工作。
            for (_k, v) in map.iter_mut() {
                sort_schema_keys_recursive(v);
            }
            // 收集现有条目，按键排序，重建 map。克隆不可避免：
            // `serde_json::Map` 不提供原地键重排。该克隆是浅层
            // Value 克隆 —— 子节点已在上面原地修改过，因此不会丢失
            // 递归排序。
            let mut entries: Vec<(String, Value)> =
                map.iter().map(|(k, v)| (k.clone(), v.clone())).collect();
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            map.clear();
            for (k, v) in entries {
                map.insert(k, v);
            }
        }
        Value::Array(items) => {
            // 保留数组顺序 —— JSON Schema 数组是有序的。
            // 递归进入每个元素，使数组内嵌套的对象仍能按键排序。
            for item in items.iter_mut() {
                sort_schema_keys_recursive(item);
            }
        }
        // 字符串、数字、布尔值、null 没有可排序的键。
        _ => {}
    }
}

/// OpenAI Chat Completions 工具定义的 Phase E 归一化。
///
/// 仅 PAYG：确定性地排序 `tools[]`（PR-E1），并递归排序
/// `function.parameters` 的 schema 键（PR-E2）。当任一工具带有
/// `cache_control` 标记时跳过 E1 工具排序（Anthropic 标记由代理放置；
/// OpenAI 没有原生等价物）；E2 schema 键排序无论标记与否都无条件运行。
///
/// 返回 `NormalizationApplied` 标志以供可观测性使用。
pub fn normalize_tool_definitions_openai_chat(body: &mut Value) -> NormalizationApplied {
    let mut applied = NormalizationApplied::default();

    if let Some(tools) = body.get_mut("tools").and_then(|t| t.as_array_mut()) {
        // E1：按名称排序工具（若任一工具有 cache_control 则跳过）。
        if !any_tool_has_cache_control(tools) {
            let changed = sort_tools_deterministically(tools);
            if changed {
                tracing::debug!("OpenAI tools sorted deterministically (E1)");
            }
            applied.e1_tool_sort = changed;
        }
    }
    // E2：在 function.parameters 上递归排序 schema 键。
    if let Some(mut walker) = ToolWalker::new(body) {
        walker.for_each(|tool| {
            if let Some(schema) = tool
                .get_mut("function")
                .and_then(|f| f.get_mut("parameters"))
            {
                sort_schema_keys_recursive(schema);
                applied.e2_schema_sort = true;
            }
            0
        });
    }

    applied
}

/// OpenAI Responses 工具定义的 Phase E 归一化。
///
/// 同时支持直接 Responses 工具（`tools[].{name,parameters}`）和
/// Chat 兼容的 function 工具（`tools[].function.{name,parameters}`）。
pub fn normalize_tool_definitions_responses(body: &mut Value) -> NormalizationApplied {
    let mut applied = NormalizationApplied::default();

    if let Some(mut walker) = ToolWalker::new(body) {
        walker.for_each(|tool| {
            if sort_responses_tool_schema_keys(tool) {
                applied.e2_schema_sort = true;
            }
            0
        });

        if let Some(tools) = walker.tools_mut() {
            if !any_tool_has_cache_control(tools) {
                let keys: Vec<String> = tools.iter().map(responses_tool_sort_key).collect();
                let changed = keys.windows(2).any(|pair| pair[0] > pair[1]);
                if changed {
                    tools.sort_by_cached_key(responses_tool_sort_key);
                    tracing::debug!("OpenAI Responses tools sorted deterministically (E1)");
                }
                applied.e1_tool_sort = changed;
            }
        }
    }

    applied
}

fn sort_responses_tool_schema_keys(tool: &mut Value) -> bool {
    let mut applied = false;
    for path in ["parameters", "parametersJsonSchema", "input_schema"] {
        if let Some(schema) = tool.get_mut(path) {
            sort_schema_keys_recursive(schema);
            applied = true;
        }
    }
    if let Some(function) = tool.get_mut("function") {
        for path in ["parameters", "parametersJsonSchema"] {
            if let Some(schema) = function.get_mut(path) {
                sort_schema_keys_recursive(schema);
                applied = true;
            }
        }
    }
    applied
}

fn responses_tool_sort_key(tool: &Value) -> String {
    let tool_type = tool.get("type").and_then(Value::as_str).unwrap_or("");
    let name = tool
        .get("name")
        .and_then(Value::as_str)
        .or_else(|| {
            tool.get("function")
                .and_then(|f| f.get("name"))
                .and_then(Value::as_str)
        })
        .unwrap_or("");
    let schema_hash = responses_tool_schema_hash(tool);
    if !tool_type.is_empty() || !name.is_empty() {
        return format!("{tool_type}\x00{name}\x00{schema_hash}");
    }
    sort_key(tool)
}

fn responses_tool_schema_hash(tool: &Value) -> String {
    let schema = tool
        .get("parameters")
        .or_else(|| tool.get("parametersJsonSchema"))
        .or_else(|| tool.get("input_schema"))
        .or_else(|| tool.get("function").and_then(|f| f.get("parameters")))
        .or_else(|| {
            tool.get("function")
                .and_then(|f| f.get("parametersJsonSchema"))
        });
    let Some(schema) = schema else {
        return String::new();
    };
    let mut normalized = schema.clone();
    sort_schema_keys_recursive(&mut normalized);
    let mut hasher = Sha256::new();
    hasher.update(serde_json::to_vec(&normalized).unwrap_or_default());
    let digest = hasher.finalize();
    let mut out = String::with_capacity(64);
    for byte in digest {
        out.push_str(&format!("{byte:02x}"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    // ─── E1：sort_tools_deterministically ─────────────────────────

    #[test]
    fn sort_alphabetic_by_name() {
        let mut tools = vec![
            json!({"name": "B"}),
            json!({"name": "A"}),
            json!({"name": "C"}),
        ];
        let changed = sort_tools_deterministically(&mut tools);
        assert!(changed, "out-of-order input should report a reorder");
        let names: Vec<&str> = tools
            .iter()
            .map(|t| t.get("name").and_then(Value::as_str).unwrap())
            .collect();
        assert_eq!(names, vec!["A", "B", "C"]);
    }

    #[test]
    fn idempotent_resort_no_change() {
        let mut tools = vec![
            json!({"name": "A"}),
            json!({"name": "B"}),
            json!({"name": "C"}),
        ];
        let changed = sort_tools_deterministically(&mut tools);
        assert!(!changed, "already-sorted input must report no reorder");
        let names: Vec<&str> = tools
            .iter()
            .map(|t| t.get("name").and_then(Value::as_str).unwrap())
            .collect();
        assert_eq!(names, vec!["A", "B", "C"]);
    }

    #[test]
    fn byte_stable_across_runs() {
        // 两个独立打乱的输入在排序后产生字节一致的序列化输出。
        // 这是核心不变量：无论上游客户端工具收集顺序如何，
        // 上游看到的都是相同的字节。
        let mut input_a = vec![
            json!({"name": "search", "description": "x"}),
            json!({"name": "fetch", "description": "y"}),
            json!({"name": "edit", "description": "z"}),
        ];
        let mut input_b = vec![
            json!({"name": "edit", "description": "z"}),
            json!({"name": "search", "description": "x"}),
            json!({"name": "fetch", "description": "y"}),
        ];
        sort_tools_deterministically(&mut input_a);
        sort_tools_deterministically(&mut input_b);
        let a_bytes = serde_json::to_vec(&input_a).unwrap();
        let b_bytes = serde_json::to_vec(&input_b).unwrap();
        assert_eq!(
            a_bytes, b_bytes,
            "different inputs with same tool set must serialize identically after sort"
        );
    }

    #[test]
    fn sort_alphabetic_by_openai_function_name() {
        // OpenAI Chat 形态：名称位于 `tool.function.name`。
        let mut tools = vec![
            json!({"type": "function", "function": {"name": "Z_tool"}}),
            json!({"type": "function", "function": {"name": "A_tool"}}),
            json!({"type": "function", "function": {"name": "M_tool"}}),
        ];
        let changed = sort_tools_deterministically(&mut tools);
        assert!(changed);
        let names: Vec<&str> = tools
            .iter()
            .map(|t| {
                t.get("function")
                    .and_then(|f| f.get("name"))
                    .and_then(Value::as_str)
                    .unwrap()
            })
            .collect();
        assert_eq!(names, vec!["A_tool", "M_tool", "Z_tool"]);
    }

    #[test]
    fn unnamed_tool_uses_md5_fallback() {
        // 两个无名工具 —— canonical JSON 的 MD5 确定性地打破平局。
        // 序列化输出必须跨运行稳定。
        let mut tools = vec![
            json!({"description": "second"}),
            json!({"description": "first"}),
        ];
        let _ = sort_tools_deterministically(&mut tools);
        let bytes_run1 = serde_json::to_vec(&tools).unwrap();

        let mut tools2 = vec![
            json!({"description": "first"}),
            json!({"description": "second"}),
        ];
        let _ = sort_tools_deterministically(&mut tools2);
        let bytes_run2 = serde_json::to_vec(&tools2).unwrap();

        assert_eq!(
            bytes_run1, bytes_run2,
            "unnamed-tool MD5 fallback must produce stable byte output"
        );
    }

    #[test]
    fn cache_control_detection_finds_marker() {
        let with_marker = vec![
            json!({"name": "A"}),
            json!({"name": "B", "cache_control": {"type": "ephemeral"}}),
            json!({"name": "C"}),
        ];
        assert!(any_tool_has_cache_control(&with_marker));

        let without_marker = vec![
            json!({"name": "A"}),
            json!({"name": "B"}),
            json!({"name": "C"}),
        ];
        assert!(!any_tool_has_cache_control(&without_marker));
    }

    #[test]
    fn cache_control_detection_returns_false_on_empty_tools() {
        let empty: Vec<Value> = Vec::new();
        assert!(!any_tool_has_cache_control(&empty));
    }

    // proptest 性能测试已删除(dev-dependencies 限制)
    // 核心不变量已被上面的确定性测试覆盖

    // ─── E2：sort_schema_keys_recursive ───────────────────────────

    #[test]
    fn sorts_top_level_object_keys() {
        let mut value = json!({
            "type": "object",
            "properties": {},
            "required": [],
        });
        sort_schema_keys_recursive(&mut value);
        let serialized = serde_json::to_string(&value).unwrap();
        let p_pos = serialized.find("\"properties\"").unwrap();
        let r_pos = serialized.find("\"required\"").unwrap();
        let t_pos = serialized.find("\"type\"").unwrap();
        assert!(
            p_pos < r_pos && r_pos < t_pos,
            "expected alphabetic order properties < required < type, got: {serialized}"
        );
    }

    #[test]
    fn sorts_nested_property_keys() {
        let mut value = json!({
            "type": "object",
            "properties": {
                "z_field": {"type": "string"},
                "a_field": {"type": "integer"},
                "m_field": {"type": "boolean"},
            },
        });
        sort_schema_keys_recursive(&mut value);
        let serialized = serde_json::to_string(&value).unwrap();
        let a_pos = serialized.find("\"a_field\"").unwrap();
        let m_pos = serialized.find("\"m_field\"").unwrap();
        let z_pos = serialized.find("\"z_field\"").unwrap();
        assert!(
            a_pos < m_pos && m_pos < z_pos,
            "nested property keys must be sorted alphabetically; got: {serialized}"
        );
    }

    #[test]
    fn preserves_array_order_in_oneof() {
        let mut value = json!({
            "oneOf": [
                {"const": "third"},
                {"const": "first"},
                {"const": "second"},
            ],
        });
        sort_schema_keys_recursive(&mut value);
        let arr = value.get("oneOf").and_then(Value::as_array).unwrap();
        let consts: Vec<&str> = arr
            .iter()
            .map(|v| v.get("const").and_then(Value::as_str).unwrap())
            .collect();
        assert_eq!(
            consts,
            vec!["third", "first", "second"],
            "JSON Schema arrays (oneOf) must preserve element order"
        );
    }

    #[test]
    fn idempotent_resort_schema() {
        let mut value = json!({
            "type": "object",
            "properties": {
                "z": {"type": "integer", "default": 1, "description": "z field"},
                "a": {"type": "string", "minLength": 1, "default": "x"},
            },
            "additionalProperties": false,
            "required": ["a", "z"],
        });
        sort_schema_keys_recursive(&mut value);
        let bytes_first = serde_json::to_vec(&value).unwrap();
        sort_schema_keys_recursive(&mut value);
        let bytes_second = serde_json::to_vec(&value).unwrap();
        assert_eq!(
            bytes_first, bytes_second,
            "second sort over already-sorted schema must be a byte-equal no-op"
        );
    }

    #[test]
    fn does_not_alter_arrays_within_arrays() {
        // 嵌套数组在每一层都保留顺序。
        let mut value = json!({
            "examples": [
                [3, 1, 2],
                ["c", "a", "b"],
            ],
        });
        sort_schema_keys_recursive(&mut value);
        let outer = value.get("examples").and_then(Value::as_array).unwrap();
        let inner_nums: Vec<i64> = outer[0]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_i64().unwrap())
            .collect();
        assert_eq!(inner_nums, vec![3, 1, 2]);
        let inner_strs: Vec<&str> = outer[1]
            .as_array()
            .unwrap()
            .iter()
            .map(|v| v.as_str().unwrap())
            .collect();
        assert_eq!(inner_strs, vec!["c", "a", "b"]);
    }

    #[test]
    fn handles_deeply_nested_schemas() {
        let mut value = json!({
            "type": "object",
            "properties": {
                "outer": {
                    "type": "object",
                    "properties": {
                        "inner": {
                            "type": "object",
                            "properties": {
                                "z_deep": {"type": "string"},
                                "a_deep": {"type": "integer"},
                            },
                            "required": ["z_deep", "a_deep"],
                        },
                    },
                },
            },
        });
        sort_schema_keys_recursive(&mut value);
        let serialized = serde_json::to_string(&value).unwrap();
        let a_pos = serialized.find("\"a_deep\"").unwrap();
        let z_pos = serialized.find("\"z_deep\"").unwrap();
        assert!(
            a_pos < z_pos,
            "deeply-nested keys must be sorted alphabetically; got: {serialized}"
        );
    }

    #[test]
    fn normalize_responses_direct_tool_parameters_sorts_schema_keys() {
        let mut body_a = json!({
            "tools": [{
                "type": "function",
                "name": "search",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "b": {"type": "string"},
                        "a": {"type": "string"}
                    },
                    "required": ["b", "a"]
                }
            }],
            "input": "hello"
        });
        let mut body_b = json!({
            "tools": [{
                "type": "function",
                "name": "search",
                "parameters": {
                    "required": ["b", "a"],
                    "properties": {
                        "a": {"type": "string"},
                        "b": {"type": "string"}
                    },
                    "type": "object"
                }
            }],
            "input": "hello"
        });

        normalize_tool_definitions_responses(&mut body_a);
        normalize_tool_definitions_responses(&mut body_b);

        assert_eq!(
            serde_json::to_vec(&body_a).unwrap(),
            serde_json::to_vec(&body_b).unwrap()
        );
    }

    #[test]
    fn normalize_responses_direct_tools_sorts_duplicate_names_by_schema() {
        let mut body_a = json!({
            "tools": [
                {"type": "function", "name": "lookup", "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}},
                {"type": "function", "name": "lookup", "parameters": {"type": "object", "properties": {"query": {"type": "string"}}}}
            ],
            "input": "hello"
        });
        let mut body_b = json!({
            "tools": [
                {"type": "function", "name": "lookup", "parameters": {"type": "object", "properties": {"query": {"type": "string"}}}},
                {"type": "function", "name": "lookup", "parameters": {"type": "object", "properties": {"city": {"type": "string"}}}}
            ],
            "input": "hello"
        });

        normalize_tool_definitions_responses(&mut body_a);
        normalize_tool_definitions_responses(&mut body_b);

        assert_eq!(
            serde_json::to_vec(&body_a).unwrap(),
            serde_json::to_vec(&body_b).unwrap()
        );
    }

    #[test]
    fn normalize_responses_cache_control_marker_skips_direct_tool_reorder() {
        let mut body = json!({
            "tools": [
                {"type": "function", "name": "z_tool", "parameters": {"type": "object"}},
                {"type": "function", "name": "a_tool", "cache_control": {"type": "ephemeral"}, "parameters": {"type": "object"}}
            ],
            "input": "hello"
        });

        let applied = normalize_tool_definitions_responses(&mut body);

        assert!(!applied.e1_tool_sort);
        assert_eq!(body["tools"][0]["name"].as_str(), Some("z_tool"));
        assert_eq!(body["tools"][1]["name"].as_str(), Some("a_tool"));
    }
}
