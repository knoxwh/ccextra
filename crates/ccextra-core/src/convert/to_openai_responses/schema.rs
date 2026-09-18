use serde_json::{json, Value};
use std::collections::HashSet;

use crate::convert::gemini_schema::inline_local_refs;
use crate::convert::has_unsupported_unicode_property_escape;
use crate::convert::{SCHEMA_MAP_KEYWORDS, SCHEMA_VALUE_KEYWORDS};

/// input_schema → parameters(对齐 normalizeToolParameters)
pub(crate) fn normalize_tool_parameters(schema: &Value) -> Value {
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
    // 对齐 CPA 7fac6b15:递归删除 $schema/$id dialect 关键字
    let mut s = schema.clone();
    strip_dialect_keywords_from_schema(&mut s);

    // xAI 系上游(对齐 CPA normalizeXAIObjectRootUnionBranchTypes +
    // xaiFunctionParametersNeedSimplification):root 为 object 且带 root union 时,
    // 先补缺失 type,仍非 object-only → 整体简化,宁可工具参数不可用也不让请求被拒。
    // root 非 object 的 schema 不处理直透(对齐 CPA root 检查)。
    if s.get("type").and_then(|v| v.as_str()) == Some("object")
        && ["anyOf", "oneOf"]
            .iter()
            .any(|k| s.get(*k).is_some_and(|v| v.is_array()))
    {
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
    // 对齐 CPA 7fac6b15:type 可为字符串或数组(union ["object","null"]);
    // 数组含 "object" 时保留原数组仅补 properties,不得覆盖成字符串
    let type_is_object = match s.get("type") {
        None | Some(Value::Null) => {
            s["type"] = json!("object");
            true
        }
        Some(Value::String(t)) if t.is_empty() => {
            s["type"] = json!("object");
            true
        }
        Some(Value::String(t)) => t == "object",
        Some(Value::Array(arr)) => arr.iter().any(|e| e.as_str() == Some("object")),
        _ => false,
    };
    if type_is_object && s.get("properties").map_or(true, |v| !v.is_object()) {
        s["properties"] = json!({});
    }
    s
}

/// 递归删除 $schema/$id dialect 关键字(对齐 CPA 7fac6b15 stripDialectKeywordsFromSchema),
/// 并剥离含 \p{...}/\P{...} 的 pattern(对齐 CPA e56abd56)
fn strip_dialect_keywords_from_schema(v: &mut Value) {
    match v {
        Value::Object(map) => {
            map.remove("$schema");
            map.remove("$id");

            // 对齐 CPA e56abd56:Python re 编译失败的 pattern 直接删
            if let Some(Value::String(p)) = map.get("pattern") {
                if has_unsupported_unicode_property_escape(p) {
                    map.shift_remove("pattern");
                }
            }

            // 对齐 CPA 37ce368c:patternProperties 正则键含 \p{...} 时整键删除
            if let Some(Value::Object(pat_props)) = map.get_mut("patternProperties") {
                let bad_keys: Vec<String> = pat_props
                    .keys()
                    .filter(|k| has_unsupported_unicode_property_escape(k))
                    .cloned()
                    .collect();
                for k in bad_keys {
                    pat_props.shift_remove(&k);
                }
            }

            // 对齐 CPA codexSchemaMapKeywords(即 SCHEMA_MAP_KEYWORDS)
            for map_key in SCHEMA_MAP_KEYWORDS {
                if let Some(Value::Object(sub_map)) = map.get_mut(*map_key) {
                    for sub_schema in sub_map.values_mut() {
                        strip_dialect_keywords_from_schema(sub_schema);
                    }
                }
            }

            // 对齐 CPA codexSchemaValueKeywords(即 SCHEMA_VALUE_KEYWORDS)
            for val_key in SCHEMA_VALUE_KEYWORDS {
                if let Some(val) = map.get_mut(*val_key) {
                    match val {
                        Value::Object(_) => strip_dialect_keywords_from_schema(val),
                        Value::Array(arr) => {
                            for item in arr {
                                strip_dialect_keywords_from_schema(item);
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        Value::Array(arr) => {
            for item in arr {
                strip_dialect_keywords_from_schema(item);
            }
        }
        _ => {}
    }
}

/// 纯 const union → enum(对齐 CPA helps/codex_tool_schema.go NormalizeCodexToolSchemas)
///
/// 仅当 properties.* 的 oneOf/anyOf 分支数 ≥ 阈值、每分支只含 const(外加
/// description/title)且语义值唯一时,才把 union 替换为等价 enum(MCP 服务器
/// 生成的大 union 会让上游中止);已有 enum 且与 union 语义一致时仅删冗余
/// union。其余结构一律不动。
const CODEX_UNION_BRANCH_THRESHOLD: usize = 8;

pub(crate) fn simplify_pure_const_unions(params: &mut Value) {
    let Some(props) = params.get_mut("properties").and_then(|v| v.as_object_mut()) else {
        return;
    };
    for (_key, prop) in props.iter_mut() {
        if !prop.is_object() {
            continue;
        }
        let has_one_of = prop.get("oneOf").is_some();
        let has_any_of = prop.get("anyOf").is_some();
        // 同一属性同时带 oneOf 和 anyOf:保留复合约束,不动
        if has_one_of && has_any_of {
            continue;
        }
        let union_name = if has_one_of {
            "oneOf"
        } else if has_any_of {
            "anyOf"
        } else {
            continue;
        };
        let Some(Value::Array(branches)) = prop.get(union_name) else {
            continue;
        };
        if branches.len() < CODEX_UNION_BRANCH_THRESHOLD {
            continue;
        }
        // 逐分支证明为纯、唯一 const 定义,收集语义键与原始 JSON
        let mut semantic_keys: Vec<String> = Vec::with_capacity(branches.len());
        let mut raw_values: Vec<String> = Vec::with_capacity(branches.len());
        let mut seen: HashSet<String> = HashSet::new();
        let mut pure = true;
        for branch in branches {
            let Some((key, raw)) = pure_const_branch(branch) else {
                pure = false;
                break;
            };
            if !seen.insert(key.clone()) {
                // 语义值重复违反互斥性,保留原 schema
                pure = false;
                break;
            }
            semantic_keys.push(key);
            raw_values.push(raw);
        }
        if !pure || raw_values.is_empty() {
            continue;
        }
        let obj = prop.as_object_mut().unwrap();
        if let Some(existing) = obj.get("enum") {
            // 已有 enum:仅当与 union 语义一致时删冗余 union
            let identical = existing
                .as_array()
                .map(|arr| {
                    arr.len() == semantic_keys.len()
                        && arr
                            .iter()
                            .all(|v| semantic_keys.contains(&canonical_json_value_key(v)))
                })
                .unwrap_or(false);
            if identical {
                obj.remove(union_name);
            }
            continue;
        }
        // 用原始 JSON token 组装 enum,避免数值精度损失
        if let Ok(enum_val) = serde_json::from_str::<Value>(&format!("[{}]", raw_values.join(",")))
        {
            obj.insert("enum".to_string(), enum_val);
            obj.remove(union_name);
        }
    }
}

/// 分支是否为纯 const 定义:只允许 const/description/title 键
fn pure_const_branch(branch: &Value) -> Option<(String, String)> {
    let obj = branch.as_object()?;
    let const_val = obj.get("const")?;
    for key in obj.keys() {
        if key != "const" && key != "description" && key != "title" {
            return None;
        }
    }
    Some((canonical_json_value_key(const_val), const_val.to_string()))
}

/// JSON 值的语义键(对齐 CPA canonicalJSONValueKey:类型前缀 + 数值有理数标准化)
fn canonical_json_value_key(val: &Value) -> String {
    match val {
        Value::String(s) => format!("s:{s}"),
        Value::Number(n) => format!("n:{}", rational_number_key(&n.to_string())),
        Value::Bool(b) => format!("b:{b}"),
        Value::Null => "null".to_string(),
        _ => String::new(),
    }
}

/// 数值 token 有理数标准化("1.50"/"150e-2" → "3/2",整数千进制尾零消去),
/// 用于语义去重;无法解析时回退原始 token(仅影响重复判定,不改发出去的值)
fn rational_number_key(raw: &str) -> String {
    let raw = raw.trim();
    let (mantissa, exp) = match raw.split_once(['e', 'E']) {
        Some((m, e)) => match e.trim().parse::<i32>() {
            Ok(v) => (m, v),
            Err(_) => return raw.to_string(),
        },
        None => (raw, 0),
    };
    let (sign, mantissa) = match mantissa.strip_prefix('-') {
        Some(rest) => ("-", rest),
        None => ("", mantissa.trim_start_matches('+')),
    };
    let (int_part, frac_part) = match mantissa.split_once('.') {
        Some((i, f)) => (i, f),
        None => (mantissa, ""),
    };
    if int_part.is_empty() && frac_part.is_empty() {
        return raw.to_string();
    }
    if !int_part
        .chars()
        .chain(frac_part.chars())
        .all(|c| c.is_ascii_digit())
    {
        return raw.to_string();
    }
    let digits = format!("{int_part}{frac_part}");
    let den_exp: i32 = frac_part.len() as i32 - exp;
    // 大指数(>10^18 量级)超 i128 表示,直接回退
    let num_shift = if den_exp > 0 { 0 } else { -den_exp };
    let den_shift = if den_exp > 0 { den_exp } else { 0 };
    if den_shift > 30 || num_shift > 30 {
        return raw.to_string();
    }
    let scaled = |value: u128, shift: i32| -> u128 {
        (0..shift)
            .try_fold(value, |acc, _| acc.checked_mul(10))
            .unwrap_or(u128::MAX)
    };
    let digits_val = match digits.parse::<u128>() {
        Ok(v) => v,
        Err(_) => return raw.to_string(),
    };
    let num = scaled(digits_val, num_shift);
    let den = scaled(1, den_shift);
    if num == 0 {
        return "0".to_string();
    }
    let gcd = |mut a: u128, mut b: u128| {
        while b != 0 {
            (a, b) = (b, a % b);
        }
        a
    };
    let g = gcd(num, den);
    format!("{sign}{}/{}", num / g, den / g)
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

/// JSON Schema 关键字表已统一到 mod.rs(SCHEMA_MAP_KEYWORDS / SCHEMA_VALUE_KEYWORDS,
/// 对齐 CPA e56abd56 util 导出)
/// 递归检查 JSON Schema 是否有 declared property 遗漏在 sibling required 列表中
/// (对齐 CPA codexSchemaMissesRequired)
pub(crate) fn codex_schema_misses_required(schema: &Value) -> bool {
    let Value::Object(map) = schema else {
        if let Value::Array(arr) = schema {
            return arr.iter().any(codex_schema_misses_required);
        }
        return false;
    };

    if let Some(Value::Object(props)) = map.get("properties") {
        match map.get("required") {
            Some(Value::Array(req_arr)) => {
                let required_names: HashSet<&str> =
                    req_arr.iter().filter_map(|v| v.as_str()).collect();
                for prop_name in props.keys() {
                    if !required_names.contains(prop_name.as_str()) {
                        return true;
                    }
                }
            }
            _ => {
                if !props.is_empty() {
                    return true;
                }
            }
        }
    }

    for &keyword in SCHEMA_MAP_KEYWORDS {
        if let Some(Value::Object(children)) = map.get(keyword) {
            for child in children.values() {
                if codex_schema_misses_required(child) {
                    return true;
                }
            }
        }
    }

    for &keyword in SCHEMA_VALUE_KEYWORDS {
        if let Some(child) = map.get(keyword) {
            if codex_schema_misses_required(child) {
                return true;
            }
        }
    }

    false
}
