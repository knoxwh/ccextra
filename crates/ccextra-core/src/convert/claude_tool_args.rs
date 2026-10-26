//! Claude tool_use 参数清洗(对齐 CPA CleanClaudeToolArguments / ClaudeToolIsParameterless)。
//!
//! 请求侧对空/可选 schema 注入的 `"reason"` / `"_"` 占位,在 Gemini/AG → Claude 响应侧
//! 按原始 Claude tools 声明回剥。本模块为 original-only 路径(不对齐
//! CleanClaudeToolArgumentsWithTranslated)。

use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};

/// 对齐 CPA CanonicalToolName:去首尾空白、剥前导 `_`、小写。
fn canonical_tool_name(name: &str) -> String {
    let trimmed = name.trim().trim_start_matches('_');
    trimmed.to_ascii_lowercase()
}

#[derive(Clone, Default)]
struct ObjectSchemaInfo {
    properties: HashMap<String, Vec<Value>>,
    required: HashSet<String>,
    has_properties: bool,
    allows_dynamic: bool,
    forbids_dynamic: bool,
    has_pattern_properties: bool,
    complex: bool,
    unsafe_complex: bool,
    declares_object: bool,
    branch_infos: Vec<ObjectSchemaInfo>,
}

fn schema_allows_dynamic(schema: &Value) -> bool {
    if schema
        .get("patternProperties")
        .and_then(|p| p.as_object())
        .is_some_and(|p| !p.is_empty())
    {
        return true;
    }
    match schema.get("additionalProperties") {
        Some(Value::Bool(false)) => false,
        Some(_) => true,
        None => false,
    }
}

fn schema_has_pattern_properties(schema: &Value) -> bool {
    schema
        .get("patternProperties")
        .and_then(|p| p.as_object())
        .is_some_and(|p| !p.is_empty())
}

fn schema_forbids_dynamic(schema: &Value) -> bool {
    schema.get("additionalProperties") == Some(&Value::Bool(false))
}

fn schema_declares_object(schema: &Value) -> bool {
    match schema.get("type") {
        Some(Value::String(s)) => s == "object",
        Some(Value::Array(arr)) => arr.iter().any(|t| t.as_str() == Some("object")),
        _ => false,
    }
}

const ANALYZED_KEYWORDS: &[&str] = &[
    "type",
    "description",
    "title",
    "properties",
    "required",
    "items",
    "additionalProperties",
    "patternProperties",
    "anyOf",
    "oneOf",
    "allOf",
    "$ref",
    "$defs",
    "definitions",
    "nullable",
    "minLength",
    "maxLength",
    "minimum",
    "maximum",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "multipleOf",
    "pattern",
    "format",
    "default",
    "examples",
    "minItems",
    "maxItems",
    "maxProperties",
    "$schema",
    "$id",
    "id",
    "$comment",
    "deprecated",
    "readOnly",
    "writeOnly",
    "example",
    "contentEncoding",
    "contentMediaType",
];

fn schema_has_unanalyzed_keyword(schema: &Value) -> bool {
    let Some(obj) = schema.as_object() else {
        return false;
    };
    obj.keys()
        .any(|k| !k.starts_with("x-") && !ANALYZED_KEYWORDS.contains(&k.as_str()))
}

fn resolve_local_ref<'a>(root: &'a Value, ref_: &str) -> Option<&'a Value> {
    let path = ref_.strip_prefix("#/")?;
    let mut current = root;
    for segment in path.split('/') {
        let segment = segment.replace("~1", "/").replace("~0", "~");
        current = match current {
            Value::Object(map) => map.get(&segment)?,
            Value::Array(arr) => {
                let idx: usize = segment.parse().ok()?;
                arr.get(idx)?
            }
            _ => return None,
        };
    }
    Some(current)
}

fn union_branches(schema: &Value) -> Option<Vec<&Value>> {
    let mut branches = Vec::new();
    let mut seen = HashSet::new();
    for key in ["anyOf", "oneOf"] {
        let Some(Value::Array(list)) = schema.get(key) else {
            if schema.get(key).is_some() {
                return None;
            }
            continue;
        };
        for branch in list {
            let is_null_only = match branch.get("type") {
                Some(Value::String(s)) => s == "null",
                Some(Value::Array(arr)) => arr.len() == 1 && arr[0].as_str() == Some("null"),
                _ => false,
            };
            if is_null_only {
                continue;
            }
            let raw = branch.to_string();
            if !seen.insert(raw) {
                continue;
            }
            branches.push(branch);
        }
    }
    Some(branches)
}

fn append_unique_schema(schemas: &mut Vec<Value>, value: &Value) {
    if schemas.iter().any(|s| s == value) {
        return;
    }
    schemas.push(value.clone());
}

fn merge_info(dst: &mut ObjectSchemaInfo, src: ObjectSchemaInfo) {
    if src.has_properties {
        dst.has_properties = true;
        for (key, values) in src.properties {
            let entry = dst.properties.entry(key).or_default();
            for value in values {
                append_unique_schema(entry, &value);
            }
        }
    }
    dst.required.extend(src.required);
    dst.branch_infos.extend(src.branch_infos);
    dst.allows_dynamic |= src.allows_dynamic;
    dst.forbids_dynamic |= src.forbids_dynamic;
    dst.has_pattern_properties |= src.has_pattern_properties;
    dst.complex |= src.complex;
    dst.unsafe_complex |= src.unsafe_complex;
    dst.declares_object |= src.declares_object;
}

fn analyze_object_schema(schema: &Value, root: &Value) -> ObjectSchemaInfo {
    analyze_object_schema_depth(schema, root, 0, &mut HashSet::new())
}

fn analyze_object_schema_depth(
    schema: &Value,
    root: &Value,
    depth: usize,
    visited_refs: &mut HashSet<String>,
) -> ObjectSchemaInfo {
    let mut info = ObjectSchemaInfo::default();
    if depth > 16 || !schema.is_object() {
        info.complex = true;
        return info;
    }
    if schema_declares_object(schema) {
        info.declares_object = true;
    }
    if schema_has_unanalyzed_keyword(schema) {
        info.complex = true;
        info.unsafe_complex = true;
        return info;
    }
    if schema.get("anyOf").is_some() || schema.get("oneOf").is_some() {
        let Some(branches) = union_branches(schema) else {
            info.complex = true;
            return info;
        };
        if branches.is_empty() {
            info.complex = true;
            return info;
        }
        if branches.len() > 1 {
            for branch in &branches {
                let branch_info =
                    analyze_object_schema_depth(branch, root, depth + 1, visited_refs);
                info.branch_infos.push(branch_info.clone());
                merge_info(&mut info, branch_info);
            }
            if schema_allows_dynamic(schema) {
                info.allows_dynamic = true;
            }
            if schema_forbids_dynamic(schema) {
                info.forbids_dynamic = true;
            }
            if schema_has_pattern_properties(schema) {
                info.has_pattern_properties = true;
            }
            if let Some(Value::Array(req)) = schema.get("required") {
                for name in req {
                    if let Some(s) = name.as_str() {
                        info.required.insert(s.to_string());
                    }
                }
            }
            if let Some(Value::Object(props)) = schema.get("properties") {
                info.has_properties = true;
                info.declares_object = true;
                for (key, value) in props {
                    append_unique_schema(info.properties.entry(key.clone()).or_default(), value);
                }
            }
            if let Some(Value::String(ref_)) = schema.get("$ref") {
                if !visited_refs.contains(ref_) {
                    if let Some(target) = resolve_local_ref(root, ref_) {
                        visited_refs.insert(ref_.clone());
                        let resolved =
                            analyze_object_schema_depth(target, root, depth + 1, visited_refs);
                        visited_refs.remove(ref_);
                        merge_info(&mut info, resolved);
                    }
                }
            }
            if let Some(Value::Array(all_of)) = schema.get("allOf") {
                for branch in all_of {
                    let branch_info =
                        analyze_object_schema_depth(branch, root, depth + 1, visited_refs);
                    merge_info(&mut info, branch_info);
                }
            }
            info.complex = true;
            return info;
        }
        let branch_info = analyze_object_schema_depth(branches[0], root, depth + 1, visited_refs);
        merge_info(&mut info, branch_info);
    }
    if let Some(Value::String(ref_)) = schema.get("$ref") {
        if visited_refs.contains(ref_) {
            info.complex = true;
            return info;
        }
        let Some(target) = resolve_local_ref(root, ref_) else {
            info.complex = true;
            return info;
        };
        visited_refs.insert(ref_.clone());
        let resolved = analyze_object_schema_depth(target, root, depth + 1, visited_refs);
        visited_refs.remove(ref_);
        merge_info(&mut info, resolved);
    }
    match schema.get("allOf") {
        Some(Value::Array(all_of)) => {
            for branch in all_of {
                let branch_info =
                    analyze_object_schema_depth(branch, root, depth + 1, visited_refs);
                merge_info(&mut info, branch_info);
            }
        }
        Some(_) => info.complex = true,
        None => {}
    }
    if schema_allows_dynamic(schema) {
        info.allows_dynamic = true;
    }
    if schema_forbids_dynamic(schema) {
        info.forbids_dynamic = true;
    }
    if schema_has_pattern_properties(schema) {
        info.has_pattern_properties = true;
    }
    match schema.get("properties") {
        Some(Value::Object(props)) => {
            info.has_properties = true;
            info.declares_object = true;
            for (key, value) in props {
                append_unique_schema(info.properties.entry(key.clone()).or_default(), value);
            }
        }
        Some(_) => info.complex = true,
        None => {}
    }
    if let Some(Value::Array(req)) = schema.get("required") {
        for name in req {
            if let Some(s) = name.as_str() {
                info.required.insert(s.to_string());
            }
        }
    }
    info
}

fn info_any_branch_allows_dynamic(info: &ObjectSchemaInfo) -> bool {
    if info.allows_dynamic || info.has_pattern_properties {
        return true;
    }
    info.branch_infos
        .iter()
        .any(|b| b.allows_dynamic || b.has_pattern_properties || info_any_branch_allows_dynamic(b))
}

fn originals_any_branch_allows_dynamic(schemas: &[&Value], root: &Value) -> bool {
    schemas
        .iter()
        .any(|schema| info_any_branch_allows_dynamic(&analyze_object_schema(schema, root)))
}

fn candidate_accepts_implicitly(info: &ObjectSchemaInfo, current: &Value) -> bool {
    // 对齐 CPA:未声明 properties 且未禁动态时,视为可接受任意键(隐式开放)。
    if info.forbids_dynamic || info.has_pattern_properties {
        return false;
    }
    if info.has_properties {
        return false;
    }
    if info.complex && !info.declares_object {
        return false;
    }
    current.is_object()
}

fn originals_any_candidate_accepts_implicitly(
    schemas: &[&Value],
    root: &Value,
    current: &Value,
) -> bool {
    schemas.iter().any(|schema| {
        let info = analyze_object_schema(schema, root);
        candidate_accepts_implicitly(&info, current)
    })
}

fn parent_branch_accepts_child(info: &ObjectSchemaInfo, key: &str, current: &Value) -> bool {
    if info.branch_infos.is_empty() {
        return false;
    }
    // 任一分支允许动态,或声明了该键,则不剥。
    for branch in &info.branch_infos {
        if branch.allows_dynamic || branch.has_pattern_properties {
            return true;
        }
        if branch.properties.contains_key(key) || branch.required.contains(key) {
            return true;
        }
        if candidate_accepts_implicitly(branch, current) {
            return true;
        }
    }
    false
}

fn originals_may_permit_child(
    schemas: &[&Value],
    root: &Value,
    key: &str,
    current: &Value,
) -> bool {
    schemas.iter().any(|schema| {
        let info = analyze_object_schema(schema, root);
        if info.properties.contains_key(key) || info.required.contains(key) {
            return true;
        }
        if info.allows_dynamic || info.has_pattern_properties {
            return true;
        }
        parent_branch_accepts_child(&info, key, current)
            || candidate_accepts_implicitly(&info, current)
    })
}

/// 在原始 Claude 请求的 `tools` 中查找工具 input_schema。
fn find_claude_tool_schema<'a>(
    original_request: &'a Value,
    tool_name: &str,
) -> Option<Option<&'a Value>> {
    if tool_name.is_empty() {
        return None;
    }
    let tools = original_request.get("tools")?.as_array()?;
    let canonical = canonical_tool_name(tool_name);
    let mut exact: Vec<Option<&Value>> = Vec::new();
    let mut canonical_matches: HashMap<String, Vec<Option<&Value>>> = HashMap::new();
    for tool in tools {
        let mut name = tool
            .get("name")
            .and_then(|n| n.as_str())
            .unwrap_or("")
            .trim()
            .to_string();
        if name.is_empty() {
            name = tool
                .pointer("/function/name")
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
        }
        if name.is_empty() {
            continue;
        }
        let schema = tool
            .get("input_schema")
            .or_else(|| tool.get("parameters"))
            .or_else(|| tool.pointer("/function/parameters"));
        let schema_opt = schema.filter(|s| !s.is_null());
        if name == tool_name {
            exact.push(schema_opt);
        }
        if canonical_tool_name(&name) == canonical {
            canonical_matches.entry(name).or_default().push(schema_opt);
        }
    }
    if exact.len() == 1 {
        return Some(exact[0]);
    }
    if exact.len() > 1 {
        let first = exact[0];
        if exact.iter().all(|s| s == &first) {
            return Some(first);
        }
        return None;
    }
    if canonical_matches.len() == 1 {
        let matches = canonical_matches.values().next().unwrap();
        if matches.len() == 1 {
            return Some(matches[0]);
        }
        let first = matches[0];
        if matches.iter().all(|s| s == &first) {
            return Some(first);
        }
    }
    None
}

/// 对齐 CPA ClaudeToolIsParameterless。
pub fn claude_tool_is_parameterless(tool_name: &str, original_request: &Value) -> bool {
    let Some(schema_opt) = find_claude_tool_schema(original_request, tool_name) else {
        return false;
    };
    let Some(schema) = schema_opt else {
        return true;
    };
    let info = analyze_object_schema(schema, schema);
    // 对齐 CPA e1fb2521:仅 additionalProperties:false 的闭空对象才算 parameterless
    if info.complex
        || info_any_branch_allows_dynamic(&info)
        || !info.declares_object
        || !info.forbids_dynamic
    {
        return false;
    }
    info.required.is_empty() && (!info.has_properties || info.properties.is_empty())
}

fn resolve_array_items<'a>(
    schema: &'a Value,
    root: &'a Value,
    depth: usize,
    visited_refs: &mut HashSet<String>,
) -> Option<Vec<&'a Value>> {
    if depth > 16 || !schema.is_object() {
        return None;
    }
    let mut items = Vec::new();
    let mut deterministic = true;
    let mut collect = |candidate: &'a Value| {
        if !items.contains(&candidate) {
            items.push(candidate);
        }
    };
    if let Some(Value::String(ref_)) = schema.get("$ref") {
        if visited_refs.contains(ref_) {
            return None;
        }
        let target = resolve_local_ref(root, ref_)?;
        visited_refs.insert(ref_.clone());
        let resolved = resolve_array_items(target, root, depth + 1, visited_refs);
        visited_refs.remove(ref_);
        match resolved {
            Some(target_items) => {
                for item in target_items {
                    collect(item);
                }
            }
            None => deterministic = false,
        }
    }
    if schema.get("anyOf").is_some() || schema.get("oneOf").is_some() {
        match union_branches(schema) {
            Some(branches) if branches.len() == 1 => {
                match resolve_array_items(branches[0], root, depth + 1, visited_refs) {
                    Some(branch_items) => {
                        for item in branch_items {
                            collect(item);
                        }
                    }
                    None => deterministic = false,
                }
            }
            _ => deterministic = false,
        }
    }
    match schema.get("allOf") {
        Some(Value::Array(all_of)) => {
            for branch in all_of {
                match resolve_array_items(branch, root, depth + 1, visited_refs) {
                    Some(branch_items) => {
                        for item in branch_items {
                            collect(item);
                        }
                    }
                    None => deterministic = false,
                }
            }
        }
        Some(_) => deterministic = false,
        None => {}
    }
    if let Some(direct) = schema.get("items") {
        collect(direct);
    }
    if deterministic {
        Some(items)
    } else {
        None
    }
}

fn clean_array(args: &mut Value, schemas: &[&Value], root: &Value, path: &[String]) {
    for schema in schemas {
        if analyze_object_schema(schema, root).unsafe_complex {
            return;
        }
    }
    let mut item_schemas: Vec<Value> = Vec::new();
    let mut deterministic = true;
    for schema in schemas {
        match resolve_array_items(schema, root, 0, &mut HashSet::new()) {
            Some(resolved) if resolved.is_empty() => {
                append_unique_schema(&mut item_schemas, schema);
            }
            Some(resolved) => {
                for item in resolved {
                    append_unique_schema(&mut item_schemas, item);
                }
            }
            None => deterministic = false,
        }
    }
    if !deterministic || item_schemas.is_empty() {
        return;
    }
    let item_refs: Vec<&Value> = item_schemas.iter().collect();
    let Some(arr) = get_mut_path(args, path).and_then(|v| v.as_array_mut()) else {
        return;
    };
    let len = arr.len();
    for index in 0..len {
        let mut child_path = path.to_vec();
        child_path.push(index.to_string());
        let child = get_path(args, &child_path);
        if child.map(|c| c.is_object()).unwrap_or(false) {
            clean_object(args, &item_refs, root, &child_path, false);
        } else if child.map(|c| c.is_array()).unwrap_or(false) {
            clean_array(args, &item_refs, root, &child_path);
        }
    }
}

fn get_path<'a>(root: &'a Value, path: &[String]) -> Option<&'a Value> {
    let mut cur = root;
    for key in path {
        cur = match cur {
            Value::Object(map) => map.get(key)?,
            Value::Array(arr) => {
                let idx: usize = key.parse().ok()?;
                arr.get(idx)?
            }
            _ => return None,
        };
    }
    Some(cur)
}

fn get_mut_path<'a>(root: &'a mut Value, path: &[String]) -> Option<&'a mut Value> {
    let mut cur = root;
    for key in path {
        cur = match cur {
            Value::Object(map) => map.get_mut(key)?,
            Value::Array(arr) => {
                let idx: usize = key.parse().ok()?;
                arr.get_mut(idx)?
            }
            _ => return None,
        };
    }
    Some(cur)
}

fn set_path_object(root: &mut Value, path: &[String], value: Value) {
    if path.is_empty() {
        *root = value;
        return;
    }
    if let Some(slot) = get_mut_path(root, path) {
        *slot = value;
    }
}

fn delete_path_key(root: &mut Value, path: &[String], key: &str) {
    let parent = if path.is_empty() {
        root.as_object_mut()
    } else {
        get_mut_path(root, path).and_then(|v| v.as_object_mut())
    };
    if let Some(map) = parent {
        map.remove(key);
    }
}

fn clean_object(
    args: &mut Value,
    schemas: &[&Value],
    root: &Value,
    path: &[String],
    top_level: bool,
) {
    let Some(current) = get_path(args, path).filter(|v| v.is_object()).cloned() else {
        return;
    };
    if schemas.is_empty() {
        return;
    }
    let mut merged = ObjectSchemaInfo::default();
    for schema in schemas {
        merge_info(&mut merged, analyze_object_schema(schema, root));
    }
    if merged.complex || !merged.declares_object {
        return;
    }
    let is_empty_schema =
        merged.required.is_empty() && (!merged.has_properties || merged.properties.is_empty());
    let all_declare_object = schemas
        .iter()
        .all(|schema| analyze_object_schema(schema, root).declares_object);
    if is_empty_schema
        && all_declare_object
        && merged.forbids_dynamic
        && !info_any_branch_allows_dynamic(&merged)
        && !originals_any_branch_allows_dynamic(schemas, root)
        && !originals_any_candidate_accepts_implicitly(schemas, root, &current)
    {
        let empty = current.as_object().map(|m| m.is_empty()).unwrap_or(true);
        if empty {
            return;
        }
        if top_level {
            *args = json!({});
        } else {
            set_path_object(args, path, json!({}));
        }
        return;
    }

    let mut keys: Vec<String> = merged.properties.keys().cloned().collect();
    keys.sort();
    for key in keys {
        let property_schemas = merged.properties.get(&key).cloned().unwrap_or_default();
        let mut child_path = path.to_vec();
        child_path.push(key.clone());
        let Some(child) = get_path(args, &child_path).cloned() else {
            continue;
        };
        let current_for_branch = get_path(args, path).cloned().unwrap_or(json!({}));
        if parent_branch_accepts_child(&merged, &key, &current_for_branch)
            || originals_may_permit_child(schemas, root, &key, &current_for_branch)
        {
            continue;
        }
        let prop_refs: Vec<&Value> = property_schemas.iter().collect();
        if child.is_object() {
            clean_object(args, &prop_refs, root, &child_path, false);
        } else if child.is_array() {
            clean_array(args, &prop_refs, root, &child_path);
        }
    }

    if info_any_branch_allows_dynamic(&merged) {
        return;
    }
    let Some(current) = get_path(args, path).and_then(|v| v.as_object()).cloned() else {
        return;
    };
    let reason_declared = merged.properties.contains_key("reason");
    let underscore_declared = merged.properties.contains_key("_");
    for placeholder in ["reason", "_"] {
        if placeholder == "reason" && reason_declared {
            continue;
        }
        if placeholder == "_" && underscore_declared {
            continue;
        }
        if merged.required.contains(placeholder) {
            continue;
        }
        if info_any_branch_allows_dynamic(&merged)
            || originals_any_branch_allows_dynamic(schemas, root)
        {
            continue;
        }
        if parent_branch_accepts_child(&merged, placeholder, &Value::Object(current.clone())) {
            continue;
        }
        if originals_any_candidate_accepts_implicitly(
            schemas,
            root,
            &Value::Object(current.clone()),
        ) {
            continue;
        }
        if current.contains_key(placeholder) {
            delete_path_key(args, path, placeholder);
        }
    }

    if path.is_empty() {
        if let Some(obj) = args.as_object() {
            if obj.is_empty() {
                *args = json!({});
            }
        }
    }
}

/// 对齐 CPA CleanClaudeToolArguments(original-only)。
pub fn clean_claude_tool_arguments(
    args: &Value,
    tool_name: &str,
    original_request: &Value,
) -> Value {
    if !args.is_object() {
        return args.clone();
    }
    if args.as_object().is_some_and(|m| m.is_empty()) {
        return json!({});
    }
    let Some(schema_opt) = find_claude_tool_schema(original_request, tool_name) else {
        return args.clone();
    };
    let Some(schema) = schema_opt else {
        return json!({});
    };
    let info = analyze_object_schema(schema, schema);
    if info.complex || !info.declares_object {
        return args.clone();
    }
    // 对齐 CPA e1fb2521:隐式开放空对象不整包抹成 {}
    if info.required.is_empty()
        && (!info.has_properties || info.properties.is_empty())
        && info.forbids_dynamic
        && !info_any_branch_allows_dynamic(&info)
    {
        return json!({});
    }
    let mut cleaned = args.clone();
    clean_object(&mut cleaned, &[schema], schema, &[], true);
    cleaned
}

/// 流式 `partial_json` 字符串入口。
pub fn clean_claude_tool_arguments_raw(
    raw_args: &str,
    tool_name: &str,
    original_request: &Value,
) -> String {
    let trimmed = raw_args.trim();
    if trimmed.is_empty() || trimmed == "{}" {
        return "{}".to_string();
    }
    let Ok(args) = serde_json::from_str::<Value>(trimmed) else {
        return raw_args.to_string();
    };
    if !args.is_object() {
        return raw_args.to_string();
    }
    clean_claude_tool_arguments(&args, tool_name, original_request).to_string()
}

/// 流式工具块状态(对齐 CPA Params.CurrentTool*)。
#[derive(Debug, Default, Clone)]
pub struct ToolArgStreamState {
    pub current_tool_name: String,
    pub current_tool_is_parameterless: bool,
    pub current_tool_input_emitted: bool,
}

impl ToolArgStreamState {
    pub fn begin_tool(&mut self, tool_name: &str, original_request: Option<&Value>) {
        self.current_tool_name = tool_name.to_string();
        self.current_tool_is_parameterless = original_request
            .map(|orig| claude_tool_is_parameterless(tool_name, orig))
            .unwrap_or(false);
        self.current_tool_input_emitted = false;
    }

    /// 清洗并按无参工具规则抑制重复 `{}`。返回 None 表示跳过本 delta。
    pub fn clean_emitted_args(
        &mut self,
        raw_args: &str,
        tool_name: &str,
        original_request: Option<&Value>,
    ) -> Option<String> {
        let cleaned = match original_request {
            Some(orig) => clean_claude_tool_arguments_raw(raw_args, tool_name, orig),
            None => {
                let trimmed = raw_args.trim();
                if trimmed.is_empty() {
                    "{}".to_string()
                } else {
                    trimmed.to_string()
                }
            }
        };
        if self.current_tool_is_parameterless {
            if self.current_tool_input_emitted {
                return Some(String::new());
            }
            self.current_tool_input_emitted = true;
            return Some(cleaned);
        }
        if !cleaned.is_empty() {
            self.current_tool_input_emitted = true;
        }
        Some(cleaned)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn parameterless_normalizes_to_empty_object() {
        // 对齐 CPA e1fb2521:闭空对象(additionalProperties:false)才整对象归一成 {}
        let orig = json!({
            "tools": [{
                "name": "EnterPlanMode",
                "input_schema": {
                    "type": "object",
                    "properties": {},
                    "additionalProperties": false
                }
            }]
        });
        let args = json!({"reason": "Switching to plan mode"});
        assert_eq!(
            clean_claude_tool_arguments(&args, "EnterPlanMode", &orig),
            json!({})
        );
        assert!(claude_tool_is_parameterless("EnterPlanMode", &orig));
    }

    #[test]
    fn implicit_open_empty_object_preserves_user_args() {
        // additionalProperties 省略/true → 保留用户参数;false → 归一 {}
        for (schema, want_args, want_parameterless) in [
            (
                json!({"type": "object", "properties": {}}),
                json!({"user": "v"}),
                false,
            ),
            (json!({"type": "object"}), json!({"user": "v"}), false),
            (
                json!({"type": "object", "properties": {}, "additionalProperties": true}),
                json!({"user": "v"}),
                false,
            ),
            (
                json!({"type": "object", "properties": {}, "additionalProperties": false}),
                json!({}),
                true,
            ),
            (
                json!({"type": "object", "additionalProperties": false}),
                json!({}),
                true,
            ),
        ] {
            let orig = json!({"tools": [{"name": "echo", "input_schema": schema}]});
            let args = json!({"user": "v"});
            assert_eq!(
                clean_claude_tool_arguments(&args, "echo", &orig),
                want_args,
                "schema={schema}"
            );
            assert_eq!(
                claude_tool_is_parameterless("echo", &orig),
                want_parameterless,
                "schema={schema}"
            );
        }
    }

    #[test]
    fn implicit_open_strips_injected_placeholder_keeps_user_arg() {
        let orig = json!({
            "tools": [{
                "name": "echo",
                "input_schema": {"type": "object", "properties": {}}
            }]
        });
        let args = json!({"user": "v", "reason": "Brief explanation"});
        assert_eq!(
            clean_claude_tool_arguments(&args, "echo", &orig),
            json!({"user": "v"})
        );
        assert!(!claude_tool_is_parameterless("echo", &orig));
    }

    #[test]
    fn strips_undeclared_reason_and_underscore() {
        let orig = json!({
            "tools": [{
                "name": "Bash",
                "input_schema": {
                    "type": "object",
                    "properties": {"command": {"type": "string"}},
                    "required": ["command"],
                    "additionalProperties": false
                }
            }]
        });
        let args = json!({"command": "ls -la", "reason": "why", "_": true});
        assert_eq!(
            clean_claude_tool_arguments(&args, "Bash", &orig),
            json!({"command": "ls -la"})
        );
    }

    #[test]
    fn keeps_schema_declared_reason() {
        let orig = json!({
            "tools": [{
                "name": "CustomToolWithReason",
                "input_schema": {
                    "type": "object",
                    "properties": {"reason": {"type": "string"}},
                    "required": ["reason"]
                }
            }]
        });
        let args = json!({"reason": "valid"});
        assert_eq!(
            clean_claude_tool_arguments(&args, "CustomToolWithReason", &orig),
            json!({"reason": "valid"})
        );
    }

    #[test]
    fn strips_executor_injected_underscore_on_closed_object() {
        let orig = json!({
            "tools": [{
                "name": "WrappedPlaceholder",
                "input_schema": {
                    "type": "object",
                    "properties": {"flag": {"type": "string"}},
                    "additionalProperties": false
                }
            }]
        });
        let args = json!({"flag": "x", "_": true});
        assert_eq!(
            clean_claude_tool_arguments(&args, "WrappedPlaceholder", &orig),
            json!({"flag": "x"})
        );
    }

    #[test]
    fn stream_parameterless_continuation_emits_empty_once() {
        let orig = json!({
            "tools": [{"name": "EnterPlanMode", "input_schema": {"type": "object", "properties": {}, "additionalProperties": false}}]
        });
        let mut state = ToolArgStreamState::default();
        state.begin_tool("EnterPlanMode", Some(&orig));
        let first = state
            .clean_emitted_args(
                r#"{"reason":"Switching to plan mode"}"#,
                "EnterPlanMode",
                Some(&orig),
            )
            .unwrap();
        assert_eq!(first, "{}");
        let cont = state
            .clean_emitted_args(r#"{"reason":"more"}"#, "EnterPlanMode", Some(&orig))
            .unwrap();
        assert_eq!(cont, "");
    }

    #[test]
    fn unknown_tool_passthrough() {
        let orig = json!({"tools": []});
        let args = json!({"reason": "x"});
        assert_eq!(clean_claude_tool_arguments(&args, "Missing", &orig), args);
    }
}
