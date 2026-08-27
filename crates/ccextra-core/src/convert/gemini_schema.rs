// JSON Schema 深度清洗(对齐 CLIProxyAPI internal/util/gemini_schema.go)
//
// 两个入口:
// - clean_json_schema_for_gemini: Gemini 直连(enum 强制 string、去 nullable/title/占位字段)
// - clean_json_schema_for_antigravity: Antigravity(内联 $ref、drop 全部 enum、
//   空对象 schema 补占位 required 属性,满足 Claude VALIDATED 模式)
//
// 只接受单个 schema 值,禁止传整个请求文档(对齐 CPA 注释:
// 清洗按 key 名重写,作用于会话历史会误删工具参数)

use serde_json::{json, Map, Value};

/// 清洗选项(对齐 CPA jsonSchemaCleanOptions,仅保留两个入口用到的字段)
#[derive(Clone, Copy, Default)]
struct CleanOptions {
    /// Claude VALIDATED 模式:空对象 schema 补占位属性
    add_placeholder: bool,
    /// Antigravity 语义:内联 $ref、drop 全部 enum、not 转 description、nullable 原生保留
    antigravity_semantics: bool,
    /// Gemini:去 nullable/title 与占位字段
    remove_gemini_metadata: bool,
    /// enum 值转 string 后强制 type=string(仅 Gemini)
    force_enum_string_type: bool,
}

/// 占位 reason 属性的描述文本(对齐 CPA placeholderReasonDescription)
const PLACEHOLDER_REASON_DESCRIPTION: &str = "Brief explanation of why you are calling this tool";

/// Gemini 工具 schema 清洗(对齐 CleanJSONSchemaForGemini)
pub fn clean_json_schema_for_gemini(schema: &Value) -> Value {
    clean_json_schema(
        schema,
        CleanOptions {
            remove_gemini_metadata: true,
            force_enum_string_type: true,
            ..Default::default()
        },
    )
}

/// Antigravity 工具 schema 清洗(对齐 CleanJSONSchemaForAntigravity = Tool + requirePlaceholder)
pub fn clean_json_schema_for_antigravity(schema: &Value) -> Value {
    clean_json_schema(
        schema,
        CleanOptions {
            add_placeholder: true,
            antigravity_semantics: true,
            ..Default::default()
        },
    )
}

/// 对齐 CPA cleanNestedSchema:套一层 `"schema"` 再清洗再 unwrap。
/// cleaner 顶层跳过 `_` 占位;套一层后工具 schema 根非顶层,全可选 properties 补 `_`。
pub fn clean_nested_schema_for_antigravity(schema: &Value) -> Value {
    let wrapped = json!({"schema": schema});
    let cleaned = clean_json_schema_for_antigravity(&wrapped);
    match cleaned.get("schema") {
        Some(inner) => inner.clone(),
        None => clean_json_schema_for_antigravity(schema),
    }
}

/// 主编排(对齐 cleanJSONSchema 四阶段)
fn clean_json_schema(schema: &Value, opts: CleanOptions) -> Value {
    let mut s = schema.clone();

    // Phase 0: 归一化畸形 schema(对齐 CPA normalizeMalformedSchemaObjects)
    s = normalize_malformed_schema_objects(s);

    // Phase 1: 转换与提示
    if opts.antigravity_semantics {
        s = inline_local_refs(&s);
    }
    convert_refs_to_hints(&mut s, opts.antigravity_semantics);
    convert_const_to_enum(&mut s);
    convert_enum_values_to_strings(&mut s, opts.force_enum_string_type);
    add_enum_hints(&mut s);
    drop_ignored_enums_to_hints(&mut s, opts);
    add_additional_properties_hints(&mut s);
    move_constraints_to_description(&mut s, opts);
    if opts.antigravity_semantics {
        move_not_to_description(&mut s);
    }

    // Phase 2: 展平复杂结构
    merge_conditionals(&mut s);
    merge_all_of(&mut s);
    flatten_any_of_one_of(&mut s);
    flatten_type_arrays(&mut s, opts.antigravity_semantics);

    // Phase 3: 清理
    remove_unsupported_keywords(&mut s, opts);
    if opts.remove_gemini_metadata {
        remove_keywords(&mut s, &["nullable", "title"]);
        remove_placeholder_fields(&mut s);
    }
    cleanup_required_fields(&mut s);

    // Phase 4: 空对象 schema 占位(Claude VALIDATED)
    if opts.add_placeholder {
        add_empty_schema_placeholder(&mut s);
    }

    s
}

// --- 遍历与判定辅助 ---

/// 对齐 CPA schemaNameMapKeywords:值是"名字→子 schema"映射的关键字
fn is_name_map_keyword(key: &str) -> bool {
    matches!(
        key,
        "properties" | "patternProperties" | "dependentSchemas" | "$defs" | "definitions"
    )
}

/// 对齐 CPA isPropertyDefinition:路径尾部连续 name-map 关键字数为奇数
/// 即路径指向属性名映射本身,其 key 是工具作者命名的属性名,不可按 schema 关键字删
fn is_property_definition(path: &[String]) -> bool {
    let mut trailing = 0;
    for seg in path.iter().rev() {
        if is_name_map_keyword(seg) {
            trailing += 1;
        } else {
            break;
        }
    }
    trailing % 2 == 1
}

/// 遍历回调类型(路径, 可变对象)
type WalkFn<'a> = dyn FnMut(&[String], &mut Map<String, Value>) + 'a;

/// 递归访问所有对象节点,回调收到 (路径, 可变对象)
/// 路径元素为对象 key;数组下钻以索引字符串记(仅遍历用,不回写路径)
fn walk_objects(value: &mut Value, path: &mut Vec<String>, f: &mut WalkFn<'_>) {
    match value {
        Value::Object(map) => {
            // 先处理当前节点
            f(path, map);
            // 再递归子节点(重新 collect 键,回调可能已改结构)
            let keys: Vec<String> = map.keys().cloned().collect();
            for key in keys {
                if let Some(child) = map.get_mut(&key) {
                    match child {
                        Value::Object(_) | Value::Array(_) => {
                            path.push(key.clone());
                            walk_node(child, path, f);
                            path.pop();
                        }
                        _ => {}
                    }
                }
            }
        }
        Value::Array(arr) => {
            for (i, item) in arr.iter_mut().enumerate() {
                if matches!(item, Value::Object(_) | Value::Array(_)) {
                    path.push(i.to_string());
                    walk_node(item, path, f);
                    path.pop();
                }
            }
        }
        _ => {}
    }
}

fn walk_node(value: &mut Value, path: &mut Vec<String>, f: &mut WalkFn<'_>) {
    match value {
        Value::Object(_) => walk_objects(value, path, f),
        Value::Array(arr) => {
            for (i, item) in arr.iter_mut().enumerate() {
                if matches!(item, Value::Object(_) | Value::Array(_)) {
                    path.push(i.to_string());
                    walk_node(item, path, f);
                    path.pop();
                }
            }
        }
        _ => {}
    }
}

/// 合并提示到 description(对齐 CPA mergeHint:已有相同提示不重复追加)
fn merge_hint(existing: &str, hint: &str) -> String {
    if existing.is_empty() {
        return hint.to_string();
    }
    if existing == hint
        || existing.starts_with(&format!("{} (", hint))
        || existing.contains(&format!("({})", hint))
    {
        return existing.to_string();
    }
    format!("{} ({})", existing, hint)
}

/// 向对象 description 追加提示
fn append_hint(map: &mut Map<String, Value>, hint: &str) {
    let existing = map
        .get("description")
        .and_then(|d| d.as_str())
        .unwrap_or("")
        .to_string();
    map.insert(
        "description".into(),
        Value::String(merge_hint(&existing, hint)),
    );
}

// --- Phase 1: 转换与提示 ---

/// 对齐 CPA inlineLocalRefs:定义容器被剥离前,内联本地 $ref
/// 每次展开获得独立副本,兄弟关键字覆盖被引用定义,环引用退化为类型化提示
pub(crate) fn inline_local_refs(schema: &Value) -> Value {
    fn contains_ref(v: &Value) -> bool {
        match v {
            Value::Object(m) => m.contains_key("$ref") || m.values().any(contains_ref),
            Value::Array(a) => a.iter().any(contains_ref),
            _ => false,
        }
    }
    if !contains_ref(schema) {
        return schema.clone();
    }
    resolve_local_refs(schema, schema, &mut Vec::new())
}

fn resolve_local_refs(root: &Value, value: &Value, active: &mut Vec<String>) -> Value {
    match value {
        Value::Array(arr) => Value::Array(
            arr.iter()
                .map(|item| resolve_local_refs(root, item, active))
                .collect(),
        ),
        Value::Object(map) => {
            if let Some(Value::String(ref_)) = map.get("$ref") {
                if let Some(ptr) = ref_.strip_prefix("#/") {
                    if let Some(target) = resolve_json_pointer(root, ptr) {
                        if active.contains(ref_) {
                            return cyclic_ref_fallback(map, target, ref_);
                        }
                        active.push(ref_.clone());
                        let resolved_target = resolve_local_refs(root, target, active);
                        active.pop();
                        if let Value::Object(target_map) = resolved_target {
                            let mut out = target_map.clone();
                            for (key, item) in map {
                                if key == "$ref" {
                                    continue;
                                }
                                out.insert(key.clone(), resolve_local_refs(root, item, active));
                            }
                            return Value::Object(out);
                        }
                    }
                }
            }
            let mut out = Map::with_capacity(map.len());
            for (key, item) in map {
                out.insert(key.clone(), resolve_local_refs(root, item, active));
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

/// JSON Pointer 解析(~0/~1 反转义)
fn resolve_json_pointer<'a>(root: &'a Value, pointer: &str) -> Option<&'a Value> {
    let mut current = root;
    for raw in pointer.split('/') {
        let part = raw.replace("~1", "/").replace("~0", "~");
        match current {
            Value::Object(map) => current = map.get(&part)?,
            Value::Array(arr) => {
                let idx: usize = part.parse().ok()?;
                current = arr.get(idx)?;
            }
            _ => return None,
        }
    }
    Some(current)
}

/// 环引用退化:保留 type/nullable/description,追加 "See: <name>" 提示
fn cyclic_ref_fallback(node: &Map<String, Value>, target: &Value, ref_: &str) -> Value {
    let mut out = Map::new();
    if let Value::Object(t) = target {
        for key in ["type", "nullable", "description"] {
            if let Some(v) = t.get(key) {
                out.insert(key.to_string(), v.clone());
            }
        }
    }
    for (key, v) in node {
        if key != "$ref" {
            out.insert(key.clone(), v.clone());
        }
    }
    let hint = format!("See: {}", ref_name(ref_));
    append_hint(&mut out, &hint);
    Value::Object(out)
}

/// 取 $ref 最后一段作为定义名
fn ref_name(ref_: &str) -> String {
    match ref_.rfind('/') {
        Some(i) if i + 1 < ref_.len() => ref_[i + 1..].replace("~1", "/").replace("~0", "~"),
        _ => ref_.to_string(),
    }
}

/// 对齐 CPA convertRefsToHints:未解析/外部 $ref 转 description
/// preserve_siblings=false 时整个节点替换为 {"type":"object","description":hint}
fn convert_refs_to_hints(schema: &mut Value, preserve_siblings: bool) {
    let mut path = Vec::new();
    walk_objects(schema, &mut path, &mut |_, map| {
        let Some(Value::String(ref_)) = map.get("$ref") else {
            return;
        };
        let hint = format!("See: {}", ref_name(ref_));
        if !preserve_siblings {
            let existing = map
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("")
                .to_string();
            let hint = if existing.is_empty() {
                hint
            } else {
                format!("{} ({})", existing, hint)
            };
            *map = Map::from_iter([
                ("type".to_string(), json!("object")),
                ("description".to_string(), Value::String(hint)),
            ]);
            return;
        }
        map.shift_remove("$ref");
        append_hint(map, &hint);
    });
}

/// const → 单元素 enum
fn convert_const_to_enum(schema: &mut Value) {
    let mut path = Vec::new();
    walk_objects(schema, &mut path, &mut |_, map| {
        let Some(val) = map.get("const") else {
            return;
        };
        if map.contains_key("enum") {
            return;
        }
        map.insert("enum".into(), json!([val.clone()]));
    });
}

/// enum 值全转 string(Gemini proto 要求);force_string_type 时 type 强制 string
fn convert_enum_values_to_strings(schema: &mut Value, force_string_type: bool) {
    let mut path = Vec::new();
    walk_objects(schema, &mut path, &mut |_, map| {
        let Some(Value::Array(arr)) = map.get("enum") else {
            return;
        };
        let strings: Vec<Value> = arr
            .iter()
            .map(|item| Value::String(value_display_string(item)))
            .collect();
        map.insert("enum".into(), Value::Array(strings));
        if force_string_type {
            map.insert("type".into(), json!("string"));
        }
    });
}

/// 对齐 Go item.String():数值不带多余引号,字符串原样,bool 小写
fn value_display_string(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Null => "null".to_string(),
        other => other.to_string(),
    }
}

/// 2..=10 个成员的 enum 追加 "Allowed: a, b" 提示
fn add_enum_hints(schema: &mut Value) {
    let mut path = Vec::new();
    walk_objects(schema, &mut path, &mut |_, map| {
        let Some(Value::Array(arr)) = map.get("enum") else {
            return;
        };
        if arr.len() <= 1 || arr.len() > 10 {
            return;
        }
        let vals: Vec<String> = arr
            .iter()
            .map(|item| {
                item.as_str()
                    .map(|s| s.to_string())
                    .unwrap_or_else(|| value_display_string(item))
            })
            .collect();
        append_hint(map, &format!("Allowed: {}", vals.join(", ")));
    });
}

/// 对齐 CPA dropIgnoredEnumsToHints:
/// antigravity(tool 语义)drop 全部 enum;单成员先补提示
fn drop_ignored_enums_to_hints(schema: &mut Value, opts: CleanOptions) {
    if !opts.antigravity_semantics {
        return;
    }
    let mut path = Vec::new();
    walk_objects(schema, &mut path, &mut |_, map| {
        let Some(Value::Array(arr)) = map.get("enum") else {
            return;
        };
        if arr.len() == 1 {
            let v = arr[0]
                .as_str()
                .map(|s| s.to_string())
                .unwrap_or_else(|| value_display_string(&arr[0]));
            append_hint(map, &format!("Allowed: {}", v));
        }
        map.shift_remove("enum");
    });
}

/// additionalProperties:false → "No extra properties allowed" 提示
fn add_additional_properties_hints(schema: &mut Value) {
    let mut path = Vec::new();
    walk_objects(schema, &mut path, &mut |_, map| {
        if map.get("additionalProperties") == Some(&Value::Bool(false)) {
            append_hint(map, "No extra properties allowed");
        }
    });
}

/// 对齐 CPA unsupportedConstraints(Claude VALIDATED 拒绝 default/examples)
const UNSUPPORTED_CONSTRAINTS: &[&str] = &[
    "minLength",
    "maxLength",
    "exclusiveMinimum",
    "exclusiveMaximum",
    "pattern",
    "minItems",
    "maxItems",
    "uniqueItems",
    "contains",
    "format",
    "default",
    "examples",
];

/// 约束关键字搬入 description(antigravity 额外搬 minimum/maximum/multipleOf)
fn move_constraints_to_description(schema: &mut Value, opts: CleanOptions) {
    let mut keywords: Vec<&str> = UNSUPPORTED_CONSTRAINTS.to_vec();
    if opts.antigravity_semantics {
        keywords.extend(["minimum", "maximum", "multipleOf"]);
    }
    let mut path = Vec::new();
    walk_objects(schema, &mut path, &mut |path, map| {
        if is_property_definition(path) {
            return;
        }
        for key in &keywords {
            let Some(val) = map.get(*key) else {
                continue;
            };
            // object/array 值按原始 JSON 搬入提示(对齐 CPA val.Raw)
            let hint = format!("{}: {}", key, value_display_string(val));
            append_hint(map, &hint);
        }
    });
}

/// not 关键字原样 JSON 搬入 description(仅 antigravity)
fn move_not_to_description(schema: &mut Value) {
    let mut path = Vec::new();
    walk_objects(schema, &mut path, &mut |path, map| {
        if is_property_definition(path) {
            return;
        }
        if let Some(val) = map.get("not") {
            append_hint(map, &format!("not: {}", val));
        }
    });
}

// --- Phase 2: 展平复杂结构 ---

/// 对齐 CPA mergeConditionals:then/else 的 properties 并入父节点(不覆盖已有键)
fn merge_conditionals(schema: &mut Value) {
    let mut path = Vec::new();
    walk_objects(schema, &mut path, &mut |path, map| {
        if is_property_definition(path) {
            return;
        }
        for key in ["then", "else"] {
            // 先克隆分支 properties,避免与父节点可变借用冲突
            let props_clone = match map.get(key) {
                Some(Value::Object(branch)) => match branch.get("properties") {
                    Some(Value::Object(props)) => props.clone(),
                    _ => continue,
                },
                _ => continue,
            };
            let parent_props = map.entry("properties").or_insert_with(|| json!({}));
            if let Value::Object(parent_map) = parent_props {
                for (k, v) in props_clone {
                    parent_map.entry(k).or_insert(v);
                }
            }
        }
    });
}

/// 对齐 CPA mergeAllOf:allOf 成员合并进父节点(required 并集,其余补缺不覆盖),随后删除 allOf
fn merge_all_of(schema: &mut Value) {
    let mut path = Vec::new();
    walk_objects(schema, &mut path, &mut |_, map| {
        let Some(Value::Array(items)) = map.get("allOf") else {
            return;
        };
        let items = items.clone();
        for item in &items {
            let Value::Object(item_map) = item else {
                continue;
            };
            for (field, value) in item_map {
                match field.as_str() {
                    "required" => {
                        let Value::Array(req) = value else {
                            continue;
                        };
                        let entry = map.entry("required").or_insert_with(|| json!([]));
                        if let Value::Array(cur) = entry {
                            for r in req {
                                if !cur.contains(r) {
                                    cur.push(r.clone());
                                }
                            }
                        }
                    }
                    // 条件适用性无法用上游 schema 表达,跳过
                    "if" | "then" | "else" | "allOf" => {}
                    _ => {
                        merge_missing_schema(map, field, value);
                    }
                }
            }
        }
        map.shift_remove("allOf");
    });
}

/// 对齐 CPA mergeMissingSchemaAtPath:递归补缺,父 schema 是权威定义,不替换已有值
fn merge_missing_schema(map: &mut Map<String, Value>, field: &str, incoming: &Value) {
    match map.get_mut(field) {
        None => {
            map.insert(field.to_string(), incoming.clone());
        }
        Some(Value::Object(existing)) => {
            if let Value::Object(incoming_map) = incoming {
                let keys: Vec<String> = incoming_map.keys().cloned().collect();
                for key in keys {
                    let value = incoming_map.get(&key).cloned().unwrap_or(Value::Null);
                    merge_missing_schema(existing, &key, &value);
                }
            }
        }
        _ => {}
    }
}

/// 对齐 CPA flattenAnyOfOneOf:选最强分支,null 分支转 nullable,
/// 父 description 并入,多类型追加 "Accepts: a | b"
fn flatten_any_of_one_of(schema: &mut Value) {
    let mut path = Vec::new();
    walk_objects(schema, &mut path, &mut |_, map| {
        for key in ["anyOf", "oneOf"] {
            let Some(Value::Array(arr)) = map.get(key) else {
                continue;
            };
            if arr.is_empty() {
                continue;
            }
            let arr = arr.clone();

            // 父节点已有 properties(如对象 schema 带 anyOf/oneOf 约束):
            // 不替换父节点,各分支 properties 并入父节点后删 union 关键字(对齐 CPA)
            if map.get("properties").is_some_and(|p| p.is_object()) {
                let mut has_null = false;
                for item in &arr {
                    if item.get("type").and_then(|t| t.as_str()) == Some("null") {
                        has_null = true;
                    }
                    let Some(Value::Object(branch_props)) = item.get("properties") else {
                        continue;
                    };
                    let branch_props = branch_props.clone();
                    let Value::Object(parent_props) = map.get_mut("properties").unwrap() else {
                        continue;
                    };
                    for (k, v) in branch_props {
                        merge_missing_schema(parent_props, &k, &v);
                    }
                }
                if has_null {
                    map.insert("nullable".into(), Value::Bool(true));
                }
                map.shift_remove(key);
                continue;
            }

            let parent_desc = map
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("")
                .to_string();

            let (best_idx, all_types) = select_best(&arr);
            let Some(Value::Object(mut selected)) = arr.into_iter().nth(best_idx) else {
                continue;
            };
            let has_null = all_types.iter().any(|t| t == "null");
            let best_is_null = selected.get("type").and_then(|t| t.as_str()) == Some("null");
            if has_null && !best_is_null {
                selected.insert("nullable".into(), Value::Bool(true));
            }

            let child_desc = selected
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("")
                .to_string();
            if !parent_desc.is_empty() {
                let merged = if child_desc.is_empty() {
                    parent_desc
                } else if child_desc == parent_desc {
                    child_desc
                } else {
                    format!("{} ({})", parent_desc, child_desc)
                };
                selected.insert("description".into(), Value::String(merged));
            }

            if all_types.len() > 1 {
                append_hint(
                    &mut selected,
                    &format!("Accepts: {}", all_types.join(" | ")),
                );
            }

            // 父节点整体替换为选中分支(对齐 CPA setRawAt parentPath)
            *map = selected;
        }
    });
}

/// 对齐 CPA selectBest:object=3, array=2, 具名非 null=1, 其余 0;同分取先
fn select_best(items: &[Value]) -> (usize, Vec<String>) {
    let mut best_score = -1i32;
    let mut best_idx = 0usize;
    let mut types = Vec::new();
    for (i, item) in items.iter().enumerate() {
        let obj = item.as_object();
        let t = obj
            .and_then(|m| m.get("type"))
            .and_then(|t| t.as_str())
            .unwrap_or("");
        let (score, t) = if t == "object" || obj.is_some_and(|m| m.contains_key("properties")) {
            (3, if t.is_empty() { "object" } else { t })
        } else if t == "array" || obj.is_some_and(|m| m.contains_key("items")) {
            (2, if t.is_empty() { "array" } else { t })
        } else if !t.is_empty() && t != "null" {
            (1, t)
        } else if t == "null" {
            (0, "null")
        } else {
            // 无类型分支不计入 types(对齐 CPA:空类型不再默认 "null")
            (0, "")
        };
        if !t.is_empty() {
            types.push(t.to_string());
        }
        if score > best_score {
            best_score = score;
            best_idx = i;
        }
    }
    (best_idx, types)
}

/// 对齐 CPA flattenTypeArrays:["string","null"] → "string"
/// 多类型追加 "Accepts:" 提示;含 null 时 antigravity 置 nullable:true,
/// gemini 路径则把该字段从父 required 移除并加 "(nullable)" 提示
fn flatten_type_arrays(schema: &mut Value, preserve_native_nullable: bool) {
    // 需要后置处理的 (对象节点, 字段名) 由闭包内直接改:required 移除需操作父节点,
    // walk 只给当前对象,所以先收集再二次处理
    let mut nullable_props: Vec<(Vec<String>, String)> = Vec::new();

    let mut path: Vec<String> = Vec::new();
    walk_objects(schema, &mut path, &mut |path, map| {
        let Some(Value::Array(types)) = map.get("type") else {
            return;
        };
        if types.is_empty() {
            return;
        }
        let types = types.clone();
        let mut has_null = false;
        let mut non_null: Vec<String> = Vec::new();
        for item in &types {
            if let Some(s) = item.as_str() {
                if s == "null" {
                    has_null = true;
                } else if !s.is_empty() {
                    non_null.push(s.to_string());
                }
            }
        }
        let first = non_null
            .first()
            .cloned()
            .unwrap_or_else(|| "string".to_string());
        map.insert("type".into(), Value::String(first));

        if non_null.len() > 1 {
            append_hint(map, &format!("Accepts: {}", non_null.join(" | ")));
        }

        if has_null {
            if preserve_native_nullable {
                map.insert("nullable".into(), Value::Bool(true));
                append_hint(map, "(nullable)");
            } else {
                // path 末尾是 type 所在对象;若该对象是 properties.<field> 的属性定义,
                // 记录 (对象路径, 字段名) 以便从 required 移除
                let n = path.len();
                if n >= 2 && path[n - 2] == "properties" {
                    let field = path[n - 1].clone();
                    let object_path: Vec<String> = path[..n - 2].to_vec();
                    append_hint(map, "(nullable)");
                    nullable_props.push((object_path, field));
                }
            }
        }
    });

    // 二次遍历:从对象 required 中移除已 nullable 化的字段
    if !nullable_props.is_empty() {
        let mut path: Vec<String> = Vec::new();
        walk_objects(schema, &mut path, &mut |path, map| {
            let Some(Value::Array(req)) = map.get("required") else {
                return;
            };
            let mut req = req.clone();
            let before = req.len();
            for (obj_path, field) in &nullable_props {
                if obj_path == path {
                    req.retain(|r| r.as_str() != Some(field.as_str()));
                }
            }
            if req.len() != before {
                if req.is_empty() {
                    map.shift_remove("required");
                } else {
                    map.insert("required".into(), Value::Array(req));
                }
            }
        });
    }
}

// --- Phase 3: 清理 ---

/// 对齐 CPA removeUnsupportedKeywords:删除不受支持的关键字与 x-* 扩展
fn remove_unsupported_keywords(schema: &mut Value, opts: CleanOptions) {
    let mut keywords: Vec<&str> = UNSUPPORTED_CONSTRAINTS.to_vec();
    if opts.antigravity_semantics {
        keywords.extend(["minimum", "maximum", "multipleOf"]);
    }
    keywords.extend([
        "$schema",
        "$defs",
        "definitions",
        "const",
        "$ref",
        "$id",
        "additionalProperties",
        "propertyNames",
        "patternProperties",
        "if",
        "then",
        "else",
        "$comment",
        "enumDescriptions",
        "enumTitles",
        "prefill",
        "deprecated",
        "encrypted",
    ]);
    if opts.antigravity_semantics {
        keywords.push("not");
    }

    let mut path = Vec::new();
    walk_objects(schema, &mut path, &mut |path, map| {
        if is_property_definition(path) {
            return;
        }
        for key in &keywords {
            map.shift_remove(*key);
        }
        // x-* 扩展字段
        let xkeys: Vec<String> = map
            .keys()
            .filter(|k| k.starts_with("x-"))
            .cloned()
            .collect();
        for key in xkeys {
            map.shift_remove(&key);
        }
    });
}

/// 删除指定关键字(属性名映射内的同名 key 保留)
fn remove_keywords(schema: &mut Value, keywords: &[&str]) {
    let mut path = Vec::new();
    walk_objects(schema, &mut path, &mut |path, map| {
        if is_property_definition(path) {
            return;
        }
        for key in keywords {
            map.shift_remove(*key);
        }
    });
}

/// 对齐 CPA removePlaceholderFields:删除 properties 下的占位 "_" 与占位 "reason"
/// 属性,并同步清理 required 中的对应项
fn remove_placeholder_fields(schema: &mut Value) {
    let mut path = Vec::new();
    walk_objects(schema, &mut path, &mut |_, map| {
        let removed: Vec<String> = {
            let Some(Value::Object(props)) = map.get_mut("properties") else {
                return;
            };
            let mut removed: Vec<String> = Vec::new();
            // "_" 占位属性(properties 下任意位置)直接删
            if props.contains_key("_") {
                props.shift_remove("_");
                removed.push("_".to_string());
            }
            // 占位 reason:properties 仅含 reason 且描述为占位文案
            let is_placeholder_reason = props.len() == 1
                && props
                    .get("reason")
                    .and_then(|r| r.get("description"))
                    .and_then(|d| d.as_str())
                    == Some(PLACEHOLDER_REASON_DESCRIPTION);
            if is_placeholder_reason {
                props.shift_remove("reason");
                removed.push("reason".to_string());
            }
            removed
        };
        if removed.is_empty() {
            return;
        }
        if let Some(Value::Array(req)) = map.get("required") {
            let mut req = req.clone();
            req.retain(|r| {
                r.as_str()
                    .map(|s| !removed.iter().any(|x| x == s))
                    .unwrap_or(true)
            });
            if req.is_empty() {
                map.shift_remove("required");
            } else {
                map.insert("required".into(), Value::Array(req));
            }
        }
    });
}

/// 对齐 CPA cleanupRequiredFields:required 仅保留 properties 中存在的键;
/// required 是数组但 properties 缺失/非对象时删除 required
fn cleanup_required_fields(schema: &mut Value) {
    let mut path = Vec::new();
    walk_objects(schema, &mut path, &mut |_, map| {
        if !map.get("required").is_some_and(|r| r.is_array()) {
            return;
        }
        if !map.get("properties").is_some_and(|p| p.is_object()) {
            map.shift_remove("required");
            return;
        }
        let (Some(Value::Array(req)), Some(Value::Object(props))) =
            (map.get("required"), map.get("properties"))
        else {
            return;
        };
        let valid: Vec<Value> = req
            .iter()
            .filter(|r| r.as_str().map(|s| props.contains_key(s)).unwrap_or(false))
            .cloned()
            .collect();
        if valid.len() != req.len() {
            if valid.is_empty() {
                map.shift_remove("required");
            } else {
                map.insert("required".into(), Value::Array(valid));
            }
        }
    });
}

// --- Phase 4: 占位(Claude VALIDATED) ---

/// 对齐 CPA addEmptySchemaPlaceholder:
/// 无 properties 或空 properties 的对象 schema → 补必填 reason 属性;
/// 有 properties 但无 required(且非顶层)→ 补必填 "_" 布尔属性
fn add_empty_schema_placeholder(schema: &mut Value) {
    fn apply(node: &mut Value, is_top_level: bool) {
        let Value::Object(map) = node else {
            return;
        };
        // 先递归子节点(自底向上近似 CPA sortByDepth)
        let keys: Vec<String> = map.keys().cloned().collect();
        for key in keys {
            if let Some(child) = map.get_mut(&key) {
                match child {
                    Value::Object(_) => apply(child, false),
                    Value::Array(arr) => {
                        for item in arr.iter_mut() {
                            apply(item, false);
                        }
                    }
                    _ => {}
                }
            }
        }

        let is_object = map.get("type").and_then(|t| t.as_str()) == Some("object");
        if !is_object {
            return;
        }
        let props = map.get("properties");
        // 对齐 CPA gjson Exists():仅字段缺失才算缺失;显式 null/非对象
        // 在 gjson 里 Exists()=true,CPA 同样不加占位,原样放行
        let props_missing = props.is_none();
        let props_empty = props
            .and_then(|p| p.as_object())
            .is_some_and(|p| p.is_empty());
        let has_required = map
            .get("required")
            .and_then(|r| r.as_array())
            .is_some_and(|r| !r.is_empty());

        if props_missing || props_empty {
            map.insert(
                "properties".into(),
                json!({
                    "reason": {
                        "type": "string",
                        "description": PLACEHOLDER_REASON_DESCRIPTION
                    }
                }),
            );
            map.insert("required".into(), json!(["reason"]));
            return;
        }

        // 对齐 CPA propsVal.IsObject():properties 非对象(含 null)不加 `_`
        if !has_required && !is_top_level {
            let Some(props_map) = map.get_mut("properties").and_then(|p| p.as_object_mut()) else {
                return;
            };
            props_map
                .entry("_".to_string())
                .or_insert_with(|| json!({"type": "boolean"}));
            map.insert("required".into(), json!(["_"]));
        }
    }
    apply(schema, true);
}

/// 归一化畸形 schema 节点(对齐 CPA normalizeMalformedSchemaObjects):
/// 1. 包裹缺 type/properties 的 bare property maps
/// 2. 剥离 property 的 boolean required,提升到父级 required 数组
fn normalize_malformed_schema_objects(schema: Value) -> Value {
    let Value::Object(root_map) = schema else {
        return schema;
    };

    // 跳过 API 请求文档(有 tools/contents/messages 等)
    if is_api_request_document(&root_map) {
        return Value::Object(root_map);
    }

    // 单键 {"schema": ...} 时解套、修复、再套回
    if root_map.len() == 1 {
        if let Some(Value::Object(inner)) = root_map.get("schema").cloned() {
            let (repaired, modified) = repair_schema_node(inner);
            if modified {
                let mut wrapped = Map::new();
                wrapped.insert("schema".to_string(), Value::Object(repaired));
                return Value::Object(wrapped);
            }
            return Value::Object(root_map);
        }
    }

    let (repaired, _modified) = repair_schema_node(root_map);
    Value::Object(repaired)
}

/// 检测是否为 API 请求文档(有 tools/contents/messages 等顶层键)
fn is_api_request_document(m: &Map<String, Value>) -> bool {
    if m.contains_key("tools")
        || m.contains_key("contents")
        || m.contains_key("messages")
        || m.contains_key("functionDeclarations")
        || m.contains_key("function_declarations")
    {
        return true;
    }
    if let Some(Value::Object(req)) = m.get("request") {
        if is_api_request_document(req) {
            return true;
        }
    }
    false
}

/// 递归修复 schema 节点
fn repair_schema_node(node: Map<String, Value>) -> (Map<String, Value>, bool) {
    let mut clone = node.clone();
    let mut modified = false;
    let mut skip_step2 = false; // Step 1 处理了 bare properties 后跳过 Step 2

    // 1. 收集 bare properties(非 schema 关键字的 map,且未声明为 primitive/array)
    if !is_non_object_declared_type(clone.get("type")) {
        let mut bare_props = Map::new();
        let keys_to_remove: Vec<_> = clone
            .iter()
            .filter_map(|(k, v)| {
                if v.is_object() && !is_known_schema_keyword_or_extension(k) {
                    Some(k.clone())
                } else {
                    None
                }
            })
            .collect();

        for k in &keys_to_remove {
            if let Some(v) = clone.remove(k) {
                bare_props.insert(k.clone(), v);
            }
        }

        if !bare_props.is_empty() {
            let (repaired_props, promoted_reqs, _) = repair_property_map(bare_props);
            // 合并到 properties
            let mut props = match clone.remove("properties") {
                Some(Value::Object(existing)) => existing,
                _ => Map::new(),
            };
            for (k, v) in repaired_props {
                props.insert(k, v);
            }
            clone.insert("properties".to_string(), Value::Object(props));
            if !clone.contains_key("type") {
                clone.insert("type".to_string(), Value::String("object".to_string()));
            }

            // 合并 required
            if !promoted_reqs.is_empty() {
                let existing = extract_string_array(clone.get("required"));
                let merged = merge_string_slices(&existing, &promoted_reqs);
                clone.insert(
                    "required".to_string(),
                    Value::Array(merged.into_iter().map(Value::String).collect()),
                );
            }
            modified = true;
            skip_step2 = true; // bare properties 已被 repair_property_map 递归修复,跳过 Step 2
        }
    }

    // 2. 递归修复 properties(仅当 Step 1 未处理时)
    if !skip_step2 {
        if let Some(Value::Object(props_val)) = clone.get("properties").cloned() {
            let (repaired, promoted, props_mod) = repair_property_map(props_val);
            clone.insert("properties".to_string(), Value::Object(repaired));
            if props_mod {
                modified = true;
            }
            if !promoted.is_empty() {
                let existing = extract_string_array(clone.get("required"));
                let merged = merge_string_slices(&existing, &promoted);
                clone.insert(
                    "required".to_string(),
                    Value::Array(merged.into_iter().map(Value::String).collect()),
                );
                modified = true;
            }
        }
    }

    // 3. 递归进 items/additionalProperties/patternProperties 等容器
    if let Some(Value::Object(items)) = clone.remove("items") {
        let (repaired, items_mod) = repair_schema_node(items);
        clone.insert("items".to_string(), Value::Object(repaired));
        if items_mod {
            modified = true;
        }
    } else if let Some(Value::Array(items_list)) = clone.remove("items") {
        let (repaired, list_mod) = repair_schema_list(items_list);
        clone.insert("items".to_string(), Value::Array(repaired));
        if list_mod {
            modified = true;
        }
    }

    if let Some(Value::Object(add_props)) = clone.get("additionalProperties").cloned() {
        let (repaired, add_mod) = repair_schema_node(add_props);
        clone.insert("additionalProperties".to_string(), Value::Object(repaired));
        if add_mod {
            modified = true;
        }
    }

    if let Some(Value::Object(pat_props)) = clone.remove("patternProperties") {
        let (repaired, _, pat_mod) = repair_property_map(pat_props);
        clone.insert("patternProperties".to_string(), Value::Object(repaired));
        if pat_mod {
            modified = true;
        }
    }

    for key in &[
        "if",
        "then",
        "else",
        "not",
        "contains",
        "propertyNames",
        "unevaluatedProperties",
        "unevaluatedItems",
        "contentSchema",
        "additionalItems",
    ] {
        if let Some(Value::Object(sub_val)) = clone.remove(*key) {
            let (repaired, sub_mod) = repair_schema_node(sub_val);
            clone.insert(key.to_string(), Value::Object(repaired));
            if sub_mod {
                modified = true;
            }
        }
    }

    for key in &["anyOf", "oneOf", "allOf", "prefixItems"] {
        if let Some(Value::Array(list_val)) = clone.remove(*key) {
            let (repaired, list_mod) = repair_schema_list(list_val);
            clone.insert(key.to_string(), Value::Array(repaired));
            if list_mod {
                modified = true;
            }
        }
    }

    for key in &["$defs", "definitions", "dependentSchemas"] {
        if let Some(Value::Object(defs_val)) = clone.remove(*key) {
            let mut repaired_defs = Map::new();
            let mut defs_modified = false;
            for (dk, dv) in defs_val {
                if let Value::Object(def_map) = dv {
                    let (repaired_def, def_mod) = repair_schema_node(def_map);
                    repaired_defs.insert(dk, Value::Object(repaired_def));
                    if def_mod {
                        defs_modified = true;
                        modified = true;
                    }
                } else {
                    repaired_defs.insert(dk, dv);
                }
            }
            clone.insert(key.to_string(), Value::Object(repaired_defs));
            if defs_modified {
                modified = true;
            }
        }
    }

    (clone, modified)
}

/// 修复 property map,提升 boolean required
fn repair_property_map(props: Map<String, Value>) -> (Map<String, Value>, Vec<String>, bool) {
    let mut out = Map::new();
    let mut promoted_reqs = Vec::new();
    let mut modified = false;

    for (k, v) in props {
        let Value::Object(mut child_map) = v else {
            out.insert(k, v);
            continue;
        };

        // 先提取 boolean required(递归前)
        if let Some(Value::Bool(req_bool)) = child_map.remove("required") {
            modified = true;
            if req_bool {
                promoted_reqs.push(k.clone());
            }
        }

        // 再递归修复子节点(处理嵌套 bare properties)
        let (repaired_child, child_mod) = repair_schema_node(child_map);
        if child_mod {
            modified = true;
        }

        out.insert(k, Value::Object(repaired_child));
    }

    promoted_reqs.sort();
    (out, promoted_reqs, modified)
}

fn repair_schema_list(list: Vec<Value>) -> (Vec<Value>, bool) {
    let mut repaired = Vec::new();
    let mut list_modified = false;
    for item in list {
        if let Value::Object(item_map) = item {
            let (repaired_item, item_mod) = repair_schema_node(item_map);
            repaired.push(Value::Object(repaired_item));
            if item_mod {
                list_modified = true;
            }
        } else {
            repaired.push(item);
        }
    }
    (repaired, list_modified)
}

fn is_known_schema_keyword_or_extension(key: &str) -> bool {
    if key.starts_with("x-") {
        return true;
    }
    matches!(
        key,
        "properties"
            | "patternProperties"
            | "additionalProperties"
            | "items"
            | "prefixItems"
            | "$defs"
            | "definitions"
            | "dependentSchemas"
            | "dependentRequired"
            | "dependencies"
            | "if"
            | "then"
            | "else"
            | "not"
            | "contains"
            | "propertyNames"
            | "unevaluatedProperties"
            | "unevaluatedItems"
            | "contentSchema"
            | "additionalItems"
            | "default"
            | "const"
            | "example"
            | "examples"
            | "discriminator"
            | "xml"
            | "externalDocs"
            | "enumDescriptions"
            | "enumTitles"
    )
}

fn is_non_object_declared_type(t: Option<&Value>) -> bool {
    match t {
        Some(Value::String(s)) => !s.is_empty() && s != "object",
        Some(Value::Array(arr)) => {
            !arr.is_empty() && !arr.iter().any(|item| item.as_str() == Some("object"))
        }
        _ => false,
    }
}

fn extract_string_array(val: Option<&Value>) -> Vec<String> {
    match val {
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|v| v.as_str().map(String::from))
            .collect(),
        _ => Vec::new(),
    }
}

fn merge_string_slices(existing: &[String], promoted: &[String]) -> Vec<String> {
    let mut seen = std::collections::HashSet::new();
    let mut res = Vec::new();
    for s in existing.iter().chain(promoted.iter()) {
        if !s.is_empty() && seen.insert(s.clone()) {
            res.push(s.clone());
        }
    }
    res
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn test_gemini_enum_values_to_strings_and_type_forced() {
        let schema = json!({"type": "integer", "enum": [1, 2, 3]});
        let out = clean_json_schema_for_gemini(&schema);
        assert_eq!(out["type"], "string");
        assert_eq!(out["enum"], json!(["1", "2", "3"]));
        let desc = out["description"].as_str().unwrap();
        assert!(desc.contains("Allowed: 1, 2, 3"), "desc={}", desc);
    }

    #[test]
    fn test_gemini_removes_metadata_and_unsupported() {
        let schema = json!({
            "title": "T", "$schema": "http://x", "additionalProperties": false,
            "encrypted": "secret-marker",
            "properties": {"p": {"type": "string", "format": "date", "nullable": true}}
        });
        let out = clean_json_schema_for_gemini(&schema);
        assert!(out.get("title").is_none());
        assert!(out.get("$schema").is_none());
        assert!(out.get("additionalProperties").is_none());
        assert!(out.get("encrypted").is_none());
        assert!(out["description"]
            .as_str()
            .unwrap()
            .contains("No extra properties allowed"));
        let p = &out["properties"]["p"];
        assert!(p.get("format").is_none());
        assert!(p.get("nullable").is_none());
        assert!(p["description"].as_str().unwrap().contains("format: date"));
    }

    #[test]
    fn test_property_named_like_keyword_preserved() {
        // 属性名叫 "type"/"format" 是作者命名,不可删
        let schema = json!({
            "type": "object",
            "properties": {"type": {"type": "string"}, "format": {"type": "string"}}
        });
        let out = clean_json_schema_for_gemini(&schema);
        assert!(out["properties"].get("type").is_some());
        assert!(out["properties"].get("format").is_some());
    }

    #[test]
    fn test_type_array_flatten_and_required_removal() {
        let schema = json!({
            "type": "object",
            "properties": {"a": {"type": ["string", "null"]}},
            "required": ["a"]
        });
        let out = clean_json_schema_for_gemini(&schema);
        assert_eq!(out["properties"]["a"]["type"], "string");
        assert!(out["properties"]["a"]["description"]
            .as_str()
            .unwrap()
            .contains("(nullable)"));
        assert!(out.get("required").is_none());
    }

    #[test]
    fn test_antigravity_inlines_local_ref() {
        let schema = json!({
            "type": "object",
            "properties": {"node": {"$ref": "#/$defs/Node"}},
            "$defs": {"Node": {"type": "object", "properties": {"v": {"type": "string"}}}}
        });
        let out = clean_json_schema_for_antigravity(&schema);
        assert_eq!(out["properties"]["node"]["type"], "object");
        assert_eq!(
            out["properties"]["node"]["properties"]["v"]["type"],
            "string"
        );
        assert!(out.get("$defs").is_none());
    }

    #[test]
    fn test_antigravity_adds_placeholder_for_empty_object() {
        let schema = json!({"type": "object", "properties": {}});
        let out = clean_json_schema_for_antigravity(&schema);
        assert_eq!(out["required"], json!(["reason"]));
        assert_eq!(out["properties"]["reason"]["type"], "string");
    }

    #[test]
    fn test_antigravity_drops_enums_with_hint() {
        let schema = json!({"type": "string", "enum": ["a", "b"]});
        let out = clean_json_schema_for_antigravity(&schema);
        assert!(out.get("enum").is_none());
        assert!(out["description"]
            .as_str()
            .unwrap()
            .contains("Allowed: a, b"));
        // antigravity 不强制 type=string 重写(类型本就 string)
        assert_eq!(out["type"], "string");
    }

    #[test]
    fn test_antigravity_passes_null_properties_through() {
        // 对齐 CPA gjson Exists():显式 null 视为存在,不加占位,原样放行
        let schema = json!({"type": "object", "properties": null});
        let phase0 = normalize_malformed_schema_objects(schema.clone());
        eprintln!(
            "phase0 = {}",
            serde_json::to_string_pretty(&phase0).unwrap()
        );
        let out = clean_json_schema_for_antigravity(&schema);
        eprintln!("out = {}", serde_json::to_string_pretty(&out).unwrap());
        assert!(out["properties"].is_null());
        assert!(out.get("required").is_none());
    }

    #[test]
    fn test_antigravity_placeholder_removed_by_gemini_cleaner() {
        // 带占位 reason 的 schema 走 gemini 清洗应被剥掉
        let schema = json!({
            "type": "object",
            "properties": {"reason": {"type": "string", "description": PLACEHOLDER_REASON_DESCRIPTION}},
            "required": ["reason"]
        });
        let out = clean_json_schema_for_gemini(&schema);
        assert!(out["properties"].get("reason").is_none());
        assert!(out.get("required").is_none());
    }

    #[test]
    fn test_antigravity_nested_tool_schema_adds_underscore() {
        // 顶层清洗跳过 `_`;套一层后工具 schema 根补占位(对齐 cleanNestedSchema)
        let schema = json!({
            "type": "object",
            "properties": {"flag": {"type": "string"}}
        });
        let top = clean_json_schema_for_antigravity(&schema);
        assert!(top.get("required").is_none());
        assert!(top["properties"].get("_").is_none());
        let nested = clean_nested_schema_for_antigravity(&schema);
        assert_eq!(nested["required"], json!(["_"]));
        assert_eq!(nested["properties"]["_"]["type"], "boolean");
        assert_eq!(nested["properties"]["flag"]["type"], "string");
    }

    #[test]
    fn test_anyof_flatten_picks_strongest() {
        let schema = json!({"anyOf": [{"type": "null"}, {"type": "object", "properties": {"x": {"type": "string"}}}]});
        let out = clean_json_schema_for_gemini(&schema);
        assert_eq!(out["type"], "object");
        assert!(out.get("anyOf").is_none());
        assert!(out["description"].as_str().unwrap().contains("Accepts:"));
    }

    #[test]
    fn test_gemini_keeps_additional_properties_hint_then_removes_key() {
        let schema = json!({"type": "object", "additionalProperties": {"type": "string"}});
        let out = clean_json_schema_for_gemini(&schema);
        // 非 false 的 additionalProperties 无提示,仅删键
        assert!(out.get("additionalProperties").is_none());
        assert!(out.get("description").is_none());
    }

    #[test]
    fn test_normalize_bare_properties_wraps_as_object() {
        // MCP 工具常见畸形:顶层直接写 property maps,缺 type/properties 包裹
        let malformed = json!({
            "name": {"type": "string"},
            "age": {"type": "number"}
        });
        let out = clean_json_schema_for_gemini(&malformed);
        assert_eq!(out["type"], "object");
        assert_eq!(out["properties"]["name"]["type"], "string");
        assert_eq!(out["properties"]["age"]["type"], "number");
    }

    #[test]
    fn test_normalize_promotes_boolean_required() {
        // property 的 boolean required 应提升到父级 required 数组
        let malformed = json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "required": true},
                "age": {"type": "number", "required": false},
                "email": {"type": "string"}
            }
        });
        let out = clean_json_schema_for_gemini(&malformed);
        assert_eq!(out["required"], json!(["name"]));
        assert!(out["properties"]["name"].get("required").is_none());
        assert!(out["properties"]["age"].get("required").is_none());
    }

    #[test]
    fn test_normalize_skips_api_request_document() {
        // 跳过 API 请求文档(有 tools/contents/messages)
        let request = json!({
            "tools": [],
            "name": {"type": "string"}
        });
        let out = clean_json_schema_for_gemini(&request);
        // 未修复,name 仍在顶层
        assert!(out.get("name").is_some());
        assert!(out.get("properties").is_none());
    }

    #[test]
    fn test_normalize_nested_bare_with_boolean_required() {
        // 最小复现:嵌套 bare property 的 boolean required 提升
        let malformed = json!({
            "address": {
                "city": {"type": "string", "required": true}
            }
        });
        let out = clean_json_schema_for_gemini(&malformed);
        assert_eq!(out["type"], "object");
        assert_eq!(out["properties"]["address"]["type"], "object");
        assert_eq!(
            out["properties"]["address"]["properties"]["city"]["type"],
            "string"
        );
        assert_eq!(out["properties"]["address"]["required"], json!(["city"]));
    }

    #[test]
    fn test_normalize_recursively_repairs_nested() {
        // 递归修复嵌套 properties
        let malformed = json!({
            "type": "object",
            "properties": {
                "user": {
                    "name": {"type": "string", "required": true},
                    "address": {
                        "city": {"type": "string", "required": true}
                    }
                }
            }
        });
        let out = clean_json_schema_for_gemini(&malformed);
        // user 是标准 properties,其内部 bare properties 被修复
        assert_eq!(out["properties"]["user"]["type"], "object");
        assert_eq!(out["properties"]["user"]["required"], json!(["name"]));
        assert_eq!(
            out["properties"]["user"]["properties"]["name"]["type"],
            "string"
        );

        // address 也是 bare property,被包裹成 object
        let address = &out["properties"]["user"]["properties"]["address"];
        assert_eq!(address["type"], "object");
        assert_eq!(address["properties"]["city"]["type"], "string");
        assert_eq!(address["required"], json!(["city"]));
    }

    #[test]
    fn test_anyof_required_only_branches_keep_parent_properties() {
        // 对齐 CPA issue 5219 #2:required-only 分支不得覆盖父节点 type/properties
        let input = json!({
            "type": "object",
            "anyOf": [
                {"required": ["left"]},
                {"required": ["right"]}
            ],
            "properties": {
                "left": {"type": "integer"},
                "right": {"type": "integer"}
            },
            "additionalProperties": false
        });
        let out = clean_json_schema_for_antigravity(&input);
        assert_eq!(out["type"], "object");
        assert!(out["properties"]["left"].is_object());
        assert!(out["properties"]["right"].is_object());
        assert!(out.get("anyOf").is_none());
    }

    #[test]
    fn test_contains_keyword_stripped_with_hint() {
        // 对齐 CPA issue 5219 #3:contains 剥离并搬入 description 提示
        let input = json!({
            "type": "object",
            "properties": {
                "tags": {
                    "type": "array",
                    "items": {"type": "string"},
                    "contains": {"enum": ["x"]}
                }
            },
            "required": ["tags"],
            "additionalProperties": false
        });
        for out in [
            clean_json_schema_for_antigravity(&input),
            clean_json_schema_for_gemini(&input),
        ] {
            let tags = &out["properties"]["tags"];
            assert!(tags.get("contains").is_none());
            let desc = tags["description"].as_str().unwrap_or("");
            assert!(desc.contains("contains"), "desc: {desc}");
        }
    }
}
