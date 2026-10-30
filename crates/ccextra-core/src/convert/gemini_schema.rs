// JSON Schema 深度清洗(对齐 CLIProxyAPI internal/util/gemini_schema.go)
//
// 两个入口:
// - clean_json_schema_for_gemini: Gemini 直连(enum 强制 string、去 nullable/title/占位字段)
// - clean_json_schema_for_antigravity: Antigravity(drop 全部 enum、
//   空对象 schema 补占位 required 属性,满足 Claude VALIDATED 模式)
//
// 所有清洗路径在剥离 $defs/definitions 前内联本地 $ref(CPA c558d320 / #6732),
// 并跳过 literal 关键字(default/example/examples/const/enum)内的伪 schema 键。
//
// 只接受单个 schema 值,禁止传整个请求文档(对齐 CPA 注释:
// 清洗按 key 名重写,作用于会话历史会误删工具参数)
// unsupported 关键字含 contentMediaType/contentEncoding(CPA a43026d7 / #6699)、
// readOnly/writeOnly(CPA 154d5144 / #6778)

use serde_json::{json, Map, Value};

/// 清洗选项(对齐 CPA jsonSchemaCleanOptions,仅保留入口用到的字段)
#[derive(Clone, Copy, Default)]
struct CleanOptions {
    /// Claude VALIDATED 模式:空对象 schema 补占位属性
    add_placeholder: bool,
    /// 非 VALIDATED Antigravity:仅剥 title(对齐 CPA removeToolTitle=!requirePlaceholder)
    remove_tool_title: bool,
    /// 工具 array 缺 items 时补 string items(对齐 CPA addMissingArrayItems / b5cde4ba)
    add_missing_array_items: bool,
    /// Antigravity 语义:内联 $ref、drop 全部 enum、not 转 description、nullable 原生保留
    antigravity_semantics: bool,
    /// Gemini:去 nullable/title 与占位字段
    remove_gemini_metadata: bool,
    /// enum 值转 string 后强制 type=string(仅 Gemini)
    force_enum_string_type: bool,
    /// Gemini 直连 parametersJsonSchema 载体:保留 additionalProperties 与
    /// 标准约束关键字(对齐 CPA CleanJSONSchemaForGeminiJSONSchema / b532db9c)
    preserve_json_schema_constraints: bool,
    /// 多分支 anyOf/oneOf 强制展平为最强单分支(对齐 CPA flattenMultiBranchUnions /
    /// CleanJSONSchemaForAntigravityClaudeTool / 5efe9758);仅 AG Claude 工具参数启用
    flatten_multi_branch_unions: bool,
}

/// 占位 reason 属性的描述文本(对齐 CPA placeholderReasonDescription)
const PLACEHOLDER_REASON_DESCRIPTION: &str = "Brief explanation of why you are calling this tool";

/// Gemini 工具 schema 清洗(对齐 CleanJSONSchemaForGeminiJSONSchema:
/// parametersJsonSchema 载体保留 additionalProperties 与标准约束)
pub fn clean_json_schema_for_gemini(schema: &Value) -> Value {
    clean_json_schema(
        schema,
        CleanOptions {
            add_missing_array_items: true,
            remove_gemini_metadata: true,
            force_enum_string_type: true,
            preserve_json_schema_constraints: true,
            ..Default::default()
        },
    )
}

/// Antigravity 工具 schema 清洗(对齐 CleanJSONSchemaForAntigravity = Tool + requirePlaceholder)
pub fn clean_json_schema_for_antigravity(schema: &Value) -> Value {
    clean_json_schema_for_antigravity_tool(schema, true)
}

/// 对齐 CPA CleanJSONSchemaForAntigravityTool(requirePlaceholder)。
pub fn clean_json_schema_for_antigravity_tool(schema: &Value, require_placeholder: bool) -> Value {
    clean_json_schema(
        schema,
        CleanOptions {
            add_placeholder: require_placeholder,
            remove_tool_title: !require_placeholder,
            add_missing_array_items: true,
            antigravity_semantics: true,
            ..Default::default()
        },
    )
}

/// 对齐 CPA cleanNestedSchema:套一层 `"schema"` 再清洗再 unwrap。
/// cleaner 顶层跳过 `_` 占位;套一层后工具 schema 根非顶层,全可选 properties 补 `_`。
pub fn clean_nested_schema_for_antigravity(schema: &Value) -> Value {
    clean_nested_schema_for_antigravity_tool(schema, true)
}

/// 对齐 CPA cleanNestedSchema + CleanJSONSchemaForAntigravityTool(requirePlaceholder)。
pub fn clean_nested_schema_for_antigravity_tool(
    schema: &Value,
    require_placeholder: bool,
) -> Value {
    let wrapped = json!({"schema": schema});
    let mut cleaned = clean_json_schema_for_antigravity_tool(&wrapped, require_placeholder);
    match cleaned.as_object_mut().and_then(|m| m.remove("schema")) {
        Some(inner) => inner,
        None => clean_json_schema_for_antigravity_tool(schema, require_placeholder),
    }
}

/// Antigravity Claude 工具 parameters 清洗(对齐 CPA CleanJSONSchemaForAntigravityClaudeTool /
/// 5efe9758):在 AG Tool 清洗基础上强制展平多分支 anyOf/oneOf。
pub fn clean_json_schema_for_antigravity_claude_tool(schema: &Value) -> Value {
    clean_json_schema(
        schema,
        CleanOptions {
            add_placeholder: true,
            add_missing_array_items: true,
            antigravity_semantics: true,
            flatten_multi_branch_unions: true,
            ..Default::default()
        },
    )
}

/// 对齐 CPA cleanNestedSchema + CleanJSONSchemaForAntigravityClaudeTool。
pub fn clean_nested_schema_for_antigravity_claude(schema: &Value) -> Value {
    let wrapped = json!({"schema": schema});
    let mut cleaned = clean_json_schema_for_antigravity_claude_tool(&wrapped);
    match cleaned.as_object_mut().and_then(|m| m.remove("schema")) {
        Some(inner) => inner,
        None => clean_json_schema_for_antigravity_claude_tool(schema),
    }
}

/// 主编排(对齐 cleanJSONSchema 四阶段)
fn clean_json_schema(schema: &Value, opts: CleanOptions) -> Value {
    let mut s = schema.clone();

    // Phase 0: 归一化畸形 schema(对齐 CPA normalizeMalformedSchemaObjects)
    s = normalize_malformed_schema_objects(s, opts.add_missing_array_items);

    // Phase 1: 转换与提示。所有 cleaner 随后剥离 $defs/definitions,本地 $ref
    // 必须在此对每条路径展开,否则嵌套字段会丢(对齐 CPA c558d320 / #6732)。
    s = inline_local_refs_for_clean(&s);
    convert_refs_to_hints(&mut s, opts.antigravity_semantics);
    convert_const_to_enum(&mut s);
    convert_enum_values_to_strings(&mut s, opts.force_enum_string_type);
    add_enum_hints(&mut s);
    drop_ignored_enums_to_hints(&mut s, opts);
    // 保留模式下 additionalProperties 与约束原样保留(对齐 CPA b532db9c)
    if !opts.preserve_json_schema_constraints {
        add_additional_properties_hints(&mut s);
        move_constraints_to_description(&mut s, opts);
    }
    if opts.antigravity_semantics {
        move_not_to_description(&mut s);
    }

    // Phase 2: 展平复杂结构
    merge_conditionals(&mut s);
    merge_all_of(&mut s);
    flatten_any_of_one_of(
        &mut s,
        opts.preserve_json_schema_constraints,
        opts.flatten_multi_branch_unions,
    );
    flatten_type_arrays(&mut s, opts.antigravity_semantics);

    // Phase 3: 清理
    remove_unsupported_keywords(&mut s, opts);
    if opts.remove_gemini_metadata {
        remove_keywords(&mut s, &["nullable", "title"]);
        remove_placeholder_fields(&mut s);
    } else if opts.remove_tool_title {
        remove_keywords(&mut s, &["title"]);
    }
    cleanup_required_fields(&mut s);
    // Gemini protobuf 要求声明 properties 的节点 type 为 OBJECT(对齐 CPA e1e3a811 / Issue #6394)。
    // 须先于 sanitize_array_items:节点同时有 properties + type:array + items 时先定 object,
    // 再由 array sanitizer 剥掉残留 items(对齐 CPA c558d320 顺序)。
    sanitize_object_properties(&mut s);
    // Gemini protobuf 要求声明 items 的节点 type 为 ARRAY(对齐 CPA ffe6ad3c / Issue #6011)。
    // add_missing: 内联后仍缺 items 的 array 补占位(Phase 0 不能在 $ref 节点上预加,否则会盖掉定义里的真 items)。
    sanitize_array_items(&mut s, opts.add_missing_array_items);

    // Phase 4: 空对象 schema 占位(Claude VALIDATED)
    if opts.add_placeholder {
        add_empty_schema_placeholder(&mut s);
    }

    s
}

// --- 遍历与判定辅助 ---

/// 对齐 CPA schemaNameMapKeywords:值是"名字→子 schema"映射的关键字
fn is_name_map_keyword(key: &str) -> bool {
    // 对齐 CPA schemaNameMapKeywords(c558d320 起含 Draft-07 dependencies)
    matches!(
        key,
        "properties"
            | "patternProperties"
            | "dependentSchemas"
            | "dependencies"
            | "$defs"
            | "definitions"
    )
}

/// literal 关键字承载客户端数据而非子 schema(对齐 CPA literalValueKeywords / c558d320)。
/// 清洗与 $ref 内联须原样拷贝其值;内部长得像 schema 关键字的键是数据。
fn is_literal_value_keyword(key: &str) -> bool {
    matches!(key, "default" | "example" | "examples" | "const" | "enum")
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
                // literal 值是客户端数据,内部伪 schema 键不清洗;name-map 下同名键是属性名仍下钻
                // (对齐 CPA WalkSchema / c558d320)
                if is_literal_value_keyword(&key) && !is_property_definition(path) {
                    continue;
                }
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

/// 内联体积预算(对齐 CPA maxInlinedSchemaBytes):共享定义每引用各拷一份,超预算退化为类型化提示。
const MAX_INLINED_SCHEMA_BYTES: usize = 256 * 1024;

/// 退化引用拷贝目标 description 的上限(对齐 CPA maxFallbackDescriptionBytes)。
const MAX_FALLBACK_DESCRIPTION_BYTES: usize = 512;

/// 公开内联:保留 $defs/definitions,供 xAI 等非 clean 路径使用(对齐 CPA InlineLocalRefs)。
///
/// `None` = schema 不含任何 `$ref` 键(免 clone)。`Some` 可能与输入相等(字面量 `$ref`,
/// 如属性名或 default 实例数据),调用方需判断"确实发生内联"时须自行与输入比较。
pub(crate) fn inline_local_refs(schema: &Value) -> Option<Value> {
    inline_local_refs_rooted(schema, false)
}

/// 清洗路径内联(对齐 CPA inlineLocalRefsForClean / c558d320)。
/// 单键 `{"schema": ...}` 载体时对内层解析,避免相对 carrier 根查找全 miss。
fn inline_local_refs_for_clean(schema: &Value) -> Value {
    if !contains_ref(schema) {
        return schema.clone();
    }
    if let Value::Object(root_map) = schema {
        if root_map.len() == 1 {
            if let Some(inner) = root_map.get("schema") {
                if inner.is_object() {
                    let resolved = resolve_schema_root(inner, &mut RefResolveState::new(), true);
                    return Value::Object(Map::from_iter([("schema".to_string(), resolved)]));
                }
            }
        }
    }
    inline_local_refs_rooted(schema, true).expect("contains_ref 已在函数入口确认")
}

/// `None` = schema 不含 `$ref`;`Some` 可能与输入相等(字面量 `$ref`),语义同 [`inline_local_refs`]。
fn inline_local_refs_rooted(schema: &Value, for_clean: bool) -> Option<Value> {
    if !contains_ref(schema) {
        return None;
    }
    Some(resolve_schema_root(
        schema,
        &mut RefResolveState::new(),
        for_clean,
    ))
}

fn contains_ref(v: &Value) -> bool {
    match v {
        Value::Object(m) => m.contains_key("$ref") || m.values().any(contains_ref),
        Value::Array(a) => a.iter().any(contains_ref),
        _ => false,
    }
}

/// 单次内联剩余预算(对齐 CPA refResolveState)。
struct RefResolveState {
    budget: usize,
}

impl RefResolveState {
    fn new() -> Self {
        Self {
            budget: MAX_INLINED_SCHEMA_BYTES,
        }
    }
}

fn sorted_keys(map: &Map<String, Value>) -> Vec<String> {
    let mut keys: Vec<String> = map.keys().cloned().collect();
    keys.sort();
    keys
}

/// 先解析 schema 本体再解析定义容器,避免预算花在随后会被剥掉的定义拷贝上。
fn resolve_schema_root(root: &Value, state: &mut RefResolveState, for_clean: bool) -> Value {
    let Value::Object(root_map) = root else {
        return resolve_local_refs(root, root, &mut Vec::new(), state, false, for_clean);
    };

    let mut out = Map::with_capacity(root_map.len());
    for key in sorted_keys(root_map) {
        let item = &root_map[&key];
        match key.as_str() {
            "$defs" | "definitions" => {
                if !for_clean {
                    // 占位,本体解析完后再填
                    out.insert(key, Value::Null);
                }
            }
            "$ref" => {
                out.insert(key, item.clone());
            }
            _ => {
                out.insert(
                    key.clone(),
                    resolve_schema_entry(root, &key, item, &mut Vec::new(), state, for_clean),
                );
            }
        }
    }

    // 根级 $ref 展开后兄弟关键字覆盖目标(对齐 resolveLocalRefs)
    if let Some(Value::String(ref_)) = root_map.get("$ref") {
        if let Some(ptr) = ref_.strip_prefix("#/") {
            if let Some(target) = resolve_json_pointer(root, ptr) {
                let size = target_expansion_size(target, for_clean);
                if size <= state.budget {
                    state.budget -= size;
                    let mut active = vec![ref_.clone()];
                    let resolved_target =
                        resolve_local_refs(root, target, &mut active, state, false, for_clean);
                    if let Value::Object(target_map) = resolved_target {
                        let mut merged = Map::with_capacity(target_map.len() + out.len());
                        for k in sorted_keys(&target_map) {
                            merged.insert(k.clone(), target_map[&k].clone());
                        }
                        for k in sorted_keys(&out) {
                            if k != "$ref" {
                                merged.insert(k.clone(), out[&k].clone());
                            }
                        }
                        out = merged;
                    } else {
                        out = resolve_ref_fallback(
                            root,
                            &out,
                            target,
                            ref_,
                            &mut Vec::new(),
                            state,
                            for_clean,
                        );
                    }
                } else {
                    out = resolve_ref_fallback(
                        root,
                        &out,
                        target,
                        ref_,
                        &mut Vec::new(),
                        state,
                        for_clean,
                    );
                }
                out.shift_remove("$ref");
            }
        }
    }

    if !for_clean {
        for key in ["$defs", "definitions"] {
            if let Some(item) = root_map.get(key) {
                out.insert(
                    key.to_string(),
                    resolve_schema_entry(root, key, item, &mut Vec::new(), state, false),
                );
            }
        }
    }

    Value::Object(out)
}

fn target_expansion_size(value: &Value, for_clean: bool) -> usize {
    if !for_clean {
        return json_value_size(value);
    }
    clean_expansion_size(value, false)
}

fn clean_expansion_size(value: &Value, in_name_map: bool) -> usize {
    match value {
        Value::Null => 4,
        Value::Bool(_) => 5,
        Value::Number(n) => n.to_string().len(),
        Value::String(s) => json_string_encoded_size(s),
        Value::Array(arr) => {
            let mut size = 2;
            for item in arr {
                size += clean_expansion_size(item, false) + 1;
                if size > MAX_INLINED_SCHEMA_BYTES + 1 {
                    return MAX_INLINED_SCHEMA_BYTES + 1;
                }
            }
            size
        }
        Value::Object(map) => {
            let mut size = 2;
            for (key, item) in map {
                if !in_name_map && (key == "$defs" || key == "definitions") {
                    continue;
                }
                let mut child_in_name_map = false;
                if !in_name_map {
                    if is_literal_value_keyword(key) {
                        size += json_string_encoded_size(key) + 1 + json_value_size(item) + 1;
                        if size > MAX_INLINED_SCHEMA_BYTES + 1 {
                            return MAX_INLINED_SCHEMA_BYTES + 1;
                        }
                        continue;
                    }
                    if is_name_map_keyword(key) {
                        child_in_name_map = true;
                    }
                }
                size += json_string_encoded_size(key)
                    + 1
                    + clean_expansion_size(item, child_in_name_map)
                    + 1;
                if size > MAX_INLINED_SCHEMA_BYTES + 1 {
                    return MAX_INLINED_SCHEMA_BYTES + 1;
                }
            }
            size
        }
    }
}

/// 估算编码体积(含 Go encoding/json 默认 HTML 转义),用于内联预算。
fn json_value_size(value: &Value) -> usize {
    match value {
        Value::Null => 4,
        Value::Bool(_) => 5,
        Value::Number(n) => n.to_string().len(),
        Value::String(s) => json_string_encoded_size(s),
        Value::Array(arr) => {
            let mut size = 2;
            for item in arr {
                size += json_value_size(item) + 1;
            }
            size
        }
        Value::Object(map) => {
            let mut size = 2;
            for (key, item) in map {
                size += json_string_encoded_size(key) + 1 + json_value_size(item) + 1;
            }
            size
        }
    }
}

/// JSON 字符串编码长度(含引号),对齐 Go encoding/json HTML escape。
fn json_string_encoded_size(s: &str) -> usize {
    let bytes = s.as_bytes();
    let mut size = 2;
    let mut i = 0;
    while i < bytes.len() {
        let c = bytes[i];
        match c {
            b'\n' | b'\r' | b'\t' => size += 2,
            c if c < 0x20 => size += 6,
            b'"' | b'\\' => size += 2,
            b'<' | b'>' | b'&' => size += 6,
            c if c < 0x80 => size += 1,
            0xe2 if i + 2 < bytes.len()
                && bytes[i + 1] == 0x80
                && (bytes[i + 2] == 0xa8 || bytes[i + 2] == 0xa9) =>
            {
                size += 6;
                i += 2;
            }
            _ => size += 1,
        }
        i += 1;
    }
    size
}

fn resolve_local_refs(
    root: &Value,
    value: &Value,
    active: &mut Vec<String>,
    state: &mut RefResolveState,
    in_name_map: bool,
    for_clean: bool,
) -> Value {
    match value {
        Value::Array(arr) => Value::Array(
            arr.iter()
                .map(|item| resolve_local_refs(root, item, active, state, false, for_clean))
                .collect(),
        ),
        Value::Object(map) => {
            if !in_name_map {
                if let Some(Value::String(ref_)) = map.get("$ref") {
                    if let Some(ptr) = ref_.strip_prefix("#/") {
                        if let Some(target) = resolve_json_pointer(root, ptr) {
                            if active.contains(ref_) {
                                return Value::Object(resolve_ref_fallback(
                                    root, map, target, ref_, active, state, for_clean,
                                ));
                            }
                            let size = target_expansion_size(target, for_clean);
                            if size > state.budget {
                                return Value::Object(resolve_ref_fallback(
                                    root, map, target, ref_, active, state, for_clean,
                                ));
                            }
                            state.budget -= size;
                            active.push(ref_.clone());
                            let resolved_target =
                                resolve_local_refs(root, target, active, state, false, for_clean);
                            active.pop();
                            if let Value::Object(target_map) = resolved_target {
                                let mut out = Map::with_capacity(target_map.len() + map.len());
                                for key in sorted_keys(&target_map) {
                                    if for_clean && (key == "$defs" || key == "definitions") {
                                        continue;
                                    }
                                    out.insert(key.clone(), target_map[&key].clone());
                                }
                                for key in sorted_keys(map) {
                                    if key == "$ref" {
                                        continue;
                                    }
                                    if for_clean && (key == "$defs" || key == "definitions") {
                                        continue;
                                    }
                                    out.insert(
                                        key.clone(),
                                        resolve_schema_entry(
                                            root, &key, &map[&key], active, state, for_clean,
                                        ),
                                    );
                                }
                                return Value::Object(out);
                            }
                        }
                    }
                }
            }

            let mut out = Map::with_capacity(map.len());
            for key in sorted_keys(map) {
                if for_clean && !in_name_map && (key == "$defs" || key == "definitions") {
                    continue;
                }
                out.insert(
                    key.clone(),
                    resolve_schema_entry(root, &key, &map[&key], active, state, for_clean),
                );
            }
            Value::Object(out)
        }
        other => other.clone(),
    }
}

/// 环/超预算退化:仍解析兄弟子 schema,literal 原样(对齐 CPA resolveRefFallback)。
fn resolve_ref_fallback(
    root: &Value,
    node: &Map<String, Value>,
    target: &Value,
    ref_: &str,
    active: &mut Vec<String>,
    state: &mut RefResolveState,
    for_clean: bool,
) -> Map<String, Value> {
    let mut out = cyclic_ref_fallback(node, target, ref_);
    // 退化拷贝的目标 description 计入预算;超限只留短 hint
    if let Value::Object(target_map) = target {
        if let Some(Value::String(description)) = target_map.get("description") {
            if !description.is_empty()
                && description.len() <= MAX_FALLBACK_DESCRIPTION_BYTES
                && !node.contains_key("description")
            {
                let cost = json_string_encoded_size(description);
                if cost > state.budget {
                    out.insert(
                        "description".to_string(),
                        Value::String(format!("See: {}", ref_name(ref_))),
                    );
                } else {
                    state.budget -= cost;
                }
            }
        }
    }
    for key in sorted_keys(node) {
        if key == "$ref" || key == "description" {
            continue;
        }
        if for_clean && (key == "$defs" || key == "definitions") {
            out.shift_remove(&key);
            continue;
        }
        out.insert(
            key.clone(),
            resolve_schema_entry(root, &key, &node[&key], active, state, for_clean),
        );
    }
    out
}

/// 解析单个 map 条目:literal 深拷贝不解析;name-map 下同名键是属性名仍作 schema。
fn resolve_schema_entry(
    root: &Value,
    key: &str,
    item: &Value,
    active: &mut Vec<String>,
    state: &mut RefResolveState,
    for_clean: bool,
) -> Value {
    if is_literal_value_keyword(key) {
        return item.clone();
    }
    if is_name_map_keyword(key) {
        if let Value::Object(name_map) = item {
            let mut out = Map::with_capacity(name_map.len());
            for name in sorted_keys(name_map) {
                out.insert(
                    name.clone(),
                    resolve_local_refs(root, &name_map[&name], active, state, false, for_clean),
                );
            }
            return Value::Object(out);
        }
    }
    resolve_local_refs(root, item, active, state, false, for_clean)
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

/// 环引用退化:保留 type/nullable/description(超长 description 省略),追加 `"See: <name>"` 提示
fn cyclic_ref_fallback(
    node: &Map<String, Value>,
    target: &Value,
    ref_: &str,
) -> Map<String, Value> {
    let mut out = Map::new();
    if let Value::Object(t) = target {
        for key in ["type", "nullable", "description"] {
            if let Some(v) = t.get(key) {
                if key == "description" {
                    if let Value::String(d) = v {
                        if d.len() > MAX_FALLBACK_DESCRIPTION_BYTES {
                            continue;
                        }
                    }
                }
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
    out
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
    walk_objects(schema, &mut path, &mut |path, map| {
        // 属性名本身叫 "$ref" 时跳过(对齐 CPA isPropertyDefinition / c558d320)
        if is_property_definition(path) {
            return;
        }
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
    walk_objects(schema, &mut path, &mut |path, map| {
        if is_property_definition(path) {
            return;
        }
        let Some(val) = map.get("const").cloned() else {
            return;
        };
        if val.is_string() {
            let values = map
                .get("enum")
                .and_then(Value::as_array)
                .map(|existing| {
                    existing
                        .iter()
                        .filter(|item| *item == &val)
                        .cloned()
                        .collect::<Vec<_>>()
                })
                .unwrap_or_else(|| vec![val]);
            map.insert("enum".into(), Value::Array(values));
            map.entry("type").or_insert_with(|| json!("string"));
            return;
        }
        if map.contains_key("enum") {
            return;
        }
        map.insert("enum".into(), json!([val]));
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

/// 对齐 CPA flattenAnyOfOneOf(a0fd8448):多分支 anyOf/oneOf 保留;
/// 单分支 / `[T,null]` 仍投影; native Schema 载体将 oneOf→anyOf;
/// required-only 分支用父 properties 具化后留在 union 里。
fn flatten_any_of_one_of(schema: &mut Value, preserve_one_of: bool, flatten_multi_branch: bool) {
    flatten_any_of_one_of_at(
        schema,
        &mut Vec::new(),
        preserve_one_of,
        flatten_multi_branch,
    );
}

/// 子节点优先(对齐 CPA sortByDepth 深路径先处理),再处理当前节点的 union。
fn flatten_any_of_one_of_at(
    value: &mut Value,
    path: &mut Vec<String>,
    preserve_one_of: bool,
    flatten_multi_branch: bool,
) {
    match value {
        Value::Object(map) => {
            let keys: Vec<String> = map.keys().cloned().collect();
            for key in keys {
                // literal 值是客户端数据,内部 anyOf/oneOf 不展平(对齐 CPA findPathsByFields / c558d320)
                if is_literal_value_keyword(&key) && !is_property_definition(path) {
                    continue;
                }
                if let Some(child) = map.get_mut(&key) {
                    if matches!(child, Value::Object(_) | Value::Array(_)) {
                        path.push(key);
                        flatten_any_of_one_of_at(
                            child,
                            path,
                            preserve_one_of,
                            flatten_multi_branch,
                        );
                        path.pop();
                    }
                }
            }
            if !is_property_definition(path) {
                process_union_keywords(map, preserve_one_of, flatten_multi_branch);
            }
        }
        Value::Array(arr) => {
            for (i, item) in arr.iter_mut().enumerate() {
                if matches!(item, Value::Object(_) | Value::Array(_)) {
                    path.push(i.to_string());
                    flatten_any_of_one_of_at(item, path, preserve_one_of, flatten_multi_branch);
                    path.pop();
                }
            }
        }
        _ => {}
    }
}

fn is_required_only_union_branch(item: &Value) -> bool {
    let Some(obj) = item.as_object() else {
        return false;
    };
    if !obj.contains_key("required") {
        return false;
    }
    for key in [
        "type",
        "properties",
        "items",
        "enum",
        "anyOf",
        "oneOf",
        "allOf",
    ] {
        if obj.contains_key(key) {
            return false;
        }
    }
    true
}

fn materialize_required_only_branch(
    item: &Value,
    parent_props: &Map<String, Value>,
) -> Option<Value> {
    let obj = item.as_object()?;
    let reqs = obj.get("required")?.as_array()?;
    if reqs.is_empty() {
        return None;
    }
    let mut out = obj.clone();
    let mut props = Map::new();
    for req in reqs {
        let name = req.as_str()?;
        let prop = parent_props.get(name)?.clone();
        props.insert(name.to_string(), prop);
    }
    out.insert("properties".into(), Value::Object(props));
    out.insert("type".into(), Value::String("object".to_string()));
    Some(Value::Object(out))
}

fn is_null_only_branch(item: &Value) -> bool {
    let Some(obj) = item.as_object() else {
        return false;
    };
    if obj.contains_key("properties") || obj.contains_key("items") {
        return false;
    }
    match obj.get("type") {
        Some(Value::String(t)) => t == "null",
        Some(Value::Array(arr)) if !arr.is_empty() => {
            arr.iter().all(|e| e.as_str() == Some("null"))
        }
        _ => false,
    }
}

/// 对齐 CPA partitionUnionBranches:剥 null / false,true→{},required-only 具化。
fn partition_union_branches(
    items: &[Value],
    parent_props: Option<&Map<String, Value>>,
) -> (Vec<Value>, bool, bool) {
    let mut non_null = Vec::new();
    let mut has_null = false;
    let mut has_filtered = false;
    for item in items {
        match item {
            Value::Bool(true) => {
                non_null.push(json!({}));
                has_filtered = true;
            }
            Value::Bool(false)
            | Value::Null
            | Value::Number(_)
            | Value::String(_)
            | Value::Array(_) => {
                has_filtered = true;
            }
            Value::Object(_) if is_null_only_branch(item) => {
                has_null = true;
            }
            Value::Object(_) if is_required_only_union_branch(item) => {
                if let Some(props) = parent_props {
                    if let Some(materialized) = materialize_required_only_branch(item, props) {
                        non_null.push(materialized);
                        has_filtered = true;
                        continue;
                    }
                }
                has_filtered = true;
            }
            Value::Object(_) => {
                non_null.push(item.clone());
            }
        }
    }
    (non_null, has_null, has_filtered)
}

fn merge_parent_description_into(selected: &mut Map<String, Value>, parent_desc: &str) {
    if parent_desc.is_empty() {
        return;
    }
    let child_desc = selected
        .get("description")
        .and_then(|d| d.as_str())
        .unwrap_or("")
        .to_string();
    let merged = if child_desc.is_empty() {
        parent_desc.to_string()
    } else if child_desc == parent_desc {
        child_desc
    } else {
        format!("{} ({})", parent_desc, child_desc)
    };
    selected.insert("description".into(), Value::String(merged));
}

fn process_union_keywords(
    map: &mut Map<String, Value>,
    preserve_one_of: bool,
    flatten_multi_branch: bool,
) {
    // oneOf 先于 anyOf(对齐 CPA paths 收集顺序),以便 sibling 时把 oneOf 收成 hint
    for key in ["oneOf", "anyOf"] {
        let Some(Value::Array(arr)) = map.get(key) else {
            continue;
        };
        if arr.is_empty() {
            continue;
        }
        let arr = arr.clone();

        // native Schema 同时有 anyOf+oneOf:保留 anyOf,oneOf 搬 description hint
        // flatten_multi_branch 时跳过(对齐 CPA 5efe9758)
        if key == "oneOf"
            && !flatten_multi_branch
            && !preserve_one_of
            && map.get("anyOf").is_some_and(|v| v.is_array())
        {
            append_hint(map, &format!("oneOf: {}", Value::Array(arr)));
            map.shift_remove(key);
            continue;
        }

        let parent_props = map.get("properties").and_then(|p| p.as_object());
        let (non_null_items, has_null, has_filtered) = partition_union_branches(&arr, parent_props);
        // 上游已剥 null 并写 nullable:true 时继承(对齐 CPA 5efe9758)
        let nullable = has_null
            || map
                .get("nullable")
                .and_then(|v| v.as_bool())
                .unwrap_or(false);

        // 多分支保留; flatten_multi_branch 时落到最强单分支展平(AG Claude)
        if non_null_items.len() >= 2 && !flatten_multi_branch {
            let target_key = if key == "oneOf" && !preserve_one_of {
                "anyOf"
            } else {
                key
            };
            if target_key != key || has_null || has_filtered {
                map.insert(target_key.into(), Value::Array(non_null_items));
                if target_key != key {
                    map.shift_remove(key);
                }
            }
            if nullable {
                map.insert("nullable".into(), Value::Bool(true));
                append_hint(map, "(nullable)");
            }
            continue;
        }

        // 父已有 properties:不整体替换父节点;合并分支 properties 后删 union
        if map.get("properties").is_some_and(|p| p.is_object()) {
            for item in &non_null_items {
                let Some(Value::Object(branch_props)) = item.get("properties") else {
                    continue;
                };
                let branch_props = branch_props.clone();
                let Value::Object(parent_props_mut) = map.get_mut("properties").unwrap() else {
                    continue;
                };
                for (k, v) in branch_props {
                    merge_missing_schema(parent_props_mut, &k, &v);
                }
            }
            if nullable {
                map.insert("nullable".into(), Value::Bool(true));
            }
            if !map.contains_key("type") {
                map.insert("type".into(), Value::String("object".to_string()));
            }
            map.shift_remove(key);
            continue;
        }

        if non_null_items.is_empty() {
            if !nullable {
                map.shift_remove(key);
                continue;
            }
            let parent_desc = map
                .get("description")
                .and_then(|d| d.as_str())
                .unwrap_or("")
                .to_string();
            let mut selected = Map::new();
            selected.insert("type".into(), Value::String("null".to_string()));
            if !parent_desc.is_empty() {
                selected.insert("description".into(), Value::String(parent_desc));
            }
            *map = selected;
            continue;
        }

        // 单非空分支,或 flatten_multi_branch 下多分支:投影为最强分支
        let parent_desc = map
            .get("description")
            .and_then(|d| d.as_str())
            .unwrap_or("")
            .to_string();

        let mut candidate_items = arr.clone();
        if has_filtered || non_null_items.len() >= 2 {
            candidate_items = non_null_items.clone();
            if has_null {
                candidate_items.push(json!({"type": "null"}));
            }
        }
        let (_, all_types) = select_best(&candidate_items);

        let selected_value = if non_null_items.len() >= 2 {
            let (best_idx, _) = select_best(&non_null_items);
            non_null_items
                .get(best_idx)
                .cloned()
                .unwrap_or_else(|| non_null_items[0].clone())
        } else {
            non_null_items[0].clone()
        };
        let Value::Object(mut selected) = selected_value else {
            continue;
        };
        if nullable {
            selected.insert("nullable".into(), Value::Bool(true));
        }
        merge_parent_description_into(&mut selected, &parent_desc);
        if all_types.len() > 1 {
            append_hint(
                &mut selected,
                &format!("Accepts: {}", all_types.join(" | ")),
            );
        } else if nullable {
            append_hint(&mut selected, "(nullable)");
        }
        *map = selected;
    }
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
        // 节点已声明 items 时优先 array,避免 union 展平后留下非 array type(对齐 CPA ffe6ad3c)
        // 节点已声明 properties 时优先 object(对齐 CPA e1e3a811)
        let first = if map.contains_key("items") && non_null.iter().any(|t| t == "array") {
            "array".to_string()
        } else if map.contains_key("properties") && non_null.iter().any(|t| t == "object") {
            "object".to_string()
        } else {
            non_null
                .first()
                .cloned()
                .unwrap_or_else(|| "string".to_string())
        };
        map.insert("type".into(), Value::String(first.clone()));

        if first != "array" && map.contains_key("items") {
            map.shift_remove("items");
        }

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

/// 声明 items 的节点必须 type=array(对齐 CPA sanitizeArrayItems,ffe6ad3c / Issue #6011)。
/// type 缺失推断 array;type 显式非 array 时剥 items。
/// add_missing: 内联后仍缺 items 的 array 补 `{"type":"string"}`(对齐 CPA c558d320)。
/// 后序遍历:先处理子节点,父节点剥 items 时子树已处理完。
/// 属性名本身叫 "items" 时跳过(is_property_definition);literal 值不下钻。
fn sanitize_array_items(schema: &mut Value, add_missing: bool) {
    fn visit(value: &mut Value, path: &mut Vec<String>, add_missing: bool) {
        let keys: Vec<String> = match value {
            Value::Object(map) => map.keys().cloned().collect(),
            Value::Array(arr) => (0..arr.len()).map(|i| i.to_string()).collect(),
            _ => return,
        };
        for key in keys {
            if matches!(value, Value::Object(_))
                && is_literal_value_keyword(&key)
                && !is_property_definition(path)
            {
                continue;
            }
            let child = match value {
                Value::Object(map) => map.get_mut(&key),
                Value::Array(arr) => key.parse::<usize>().ok().and_then(|i| arr.get_mut(i)),
                _ => None,
            };
            if let Some(child) = child {
                if matches!(child, Value::Object(_) | Value::Array(_)) {
                    path.push(key);
                    visit(child, path, add_missing);
                    path.pop();
                }
            }
        }
        let Value::Object(map) = value else {
            return;
        };
        if is_property_definition(path) {
            return;
        }
        if map.contains_key("items") {
            // gjson String():缺失/null/"" 都是空串,CPA 据此补 array;其它非 "array" 剥 items
            // (对齐 CPA 2eb8dd11:比较大小写不敏感,大写 ARRAY 保留 items)
            if schema_type_string(map.get("type")).is_empty() {
                map.insert("type".into(), Value::String("array".to_string()));
            } else if !schema_type_string(map.get("type")).eq_ignore_ascii_case("array") {
                map.shift_remove("items");
            }
        } else if add_missing
            && is_array_declared_type(map.get("type"))
            && !map.contains_key("$ref")
        {
            // 内联后仍缺 items 的 array 补占位;仍挂 $ref 的跳过以免盖掉定义
            map.insert("items".into(), json!({"type": "string"}));
        }
    }

    visit(schema, &mut Vec::new(), add_missing);
}

/// 对齐 CPA sanitizeObjectProperties (e1e3a811 / Issue #6394):
/// 节点若声明 "properties"(且其值为对象),强制其 "type" 为 "object"。
/// Gemini protobuf 校验器对 properties 与 required 执行严格断言($type == Type.OBJECT);
/// 若 type 缺失或非 object,归一化为 object 保障 nested 属性与 required 有效。
fn sanitize_object_properties(schema: &mut Value) {
    fn visit(value: &mut Value, path: &mut Vec<String>) {
        let keys: Vec<String> = match value {
            Value::Object(map) => map.keys().cloned().collect(),
            Value::Array(arr) => (0..arr.len()).map(|i| i.to_string()).collect(),
            _ => return,
        };
        for key in keys {
            if matches!(value, Value::Object(_))
                && is_literal_value_keyword(&key)
                && !is_property_definition(path)
            {
                continue;
            }
            let child = match value {
                Value::Object(map) => map.get_mut(&key),
                Value::Array(arr) => key.parse::<usize>().ok().and_then(|i| arr.get_mut(i)),
                _ => None,
            };
            if let Some(child) = child {
                if matches!(child, Value::Object(_) | Value::Array(_)) {
                    path.push(key);
                    visit(child, path);
                    path.pop();
                }
            }
        }
        let Value::Object(map) = value else {
            return;
        };
        if is_property_definition(path) {
            return;
        }
        let Some(Value::Object(_)) = map.get("properties") else {
            return;
        };
        if !schema_type_string(map.get("type")).eq_ignore_ascii_case("object") {
            map.insert("type".into(), Value::String("object".to_string()));
        }
    }

    visit(schema, &mut Vec::new());
}

// --- Phase 3: 清理 ---

/// 对齐 CPA removeUnsupportedKeywords:删除不受支持的关键字与 x-* 扩展
fn remove_unsupported_keywords(schema: &mut Value, opts: CleanOptions) {
    let mut keywords: Vec<&str> = Vec::new();
    // 保留模式下约束与 additionalProperties 原样保留(对齐 CPA b532db9c:
    // preserveStandardConstraints 时 constraintKeywords 为空,additionalProperties 跳过删除)
    if !opts.preserve_json_schema_constraints {
        keywords.extend(UNSUPPORTED_CONSTRAINTS);
        if opts.antigravity_semantics {
            keywords.extend(["minimum", "maximum", "multipleOf"]);
        }
        keywords.push("additionalProperties");
    }
    keywords.extend([
        "$schema",
        "$defs",
        "definitions",
        "const",
        "$ref",
        "$id",
        "id",
        "$anchor",
        "$vocabulary",
        "$dynamicRef",
        "$dynamicAnchor",
        "propertyNames",
        "patternProperties",
        // 对齐 CPA 9126772d / Issue #6539: Gemini/Antigravity 拒识 dependent* 关键字
        "dependentRequired",
        "dependentSchemas",
        "dependencies",
        "if",
        "then",
        "else",
        "$comment",
        "enumDescriptions",
        "enumTitles",
        "prefill",
        "deprecated",
        "encrypted",
        // 对齐 CPA e9ba5c50 / Issue #6639: OpenAPI discriminator 元数据关键字
        "discriminator",
        // 对齐 CPA c93978c4:补充剥离不支持关键字
        "additionalItems",
        "unevaluatedProperties",
        "unevaluatedItems",
        "contentSchema",
        // 对齐 CPA a43026d7 / Issue #6699: JSON Schema Content Vocabulary 注解
        "contentMediaType",
        "contentEncoding",
        // 对齐 CPA 154d5144 / Issue #6778: JSON Schema 访问注解(Firestore/MCP 常见)
        "readOnly",
        "writeOnly",
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
    fn apply(node: &mut Value, path: &mut Vec<String>, is_top_level: bool) {
        let Value::Object(map) = node else {
            return;
        };
        // 先递归子节点(自底向上近似 CPA sortByDepth);literal 内不下钻
        // (对齐 CPA findPaths/WalkSchema / c558d320)
        let keys: Vec<String> = map.keys().cloned().collect();
        for key in keys {
            if is_literal_value_keyword(&key) && !is_property_definition(path) {
                continue;
            }
            if let Some(child) = map.get_mut(&key) {
                match child {
                    Value::Object(_) => {
                        path.push(key.clone());
                        apply(child, path, false);
                        path.pop();
                    }
                    Value::Array(arr) => {
                        path.push(key.clone());
                        for (i, item) in arr.iter_mut().enumerate() {
                            path.push(i.to_string());
                            apply(item, path, false);
                            path.pop();
                        }
                        path.pop();
                    }
                    _ => {}
                }
            }
        }

        let is_object = map.get("type").and_then(|t| t.as_str()) == Some("object");
        if !is_object {
            return;
        }
        // 对齐 CPA addEmptySchemaPlaceholder:父节点已是 anyOf/oneOf 多分支 union 时
        // 不注入 reason/_ 占位(分支自身仍可递归补占位)
        if map.get("anyOf").is_some_and(|v| v.is_array())
            || map.get("oneOf").is_some_and(|v| v.is_array())
        {
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
    apply(schema, &mut Vec::new(), true);
}

/// 归一化畸形 schema 节点(对齐 CPA normalizeMalformedSchemaObjects):
/// 1. 包裹缺 type/properties 的 bare property maps
/// 2. 剥离 property 的 boolean required,提升到父级 required 数组
/// 3. 工具 array 缺 items 时补 string items(对齐 CPA addMissingArrayItems)
fn normalize_malformed_schema_objects(schema: Value, add_missing_array_items: bool) -> Value {
    // 根节点布尔 true → 空对象 schema(对齐 CPA c93978c4;false 原样返回)
    if schema == Value::Bool(true) {
        return json!({});
    }
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
            let (repaired, modified) = repair_schema_node(inner, add_missing_array_items);
            if modified {
                let mut wrapped = Map::new();
                wrapped.insert("schema".to_string(), Value::Object(repaired));
                return Value::Object(wrapped);
            }
            return Value::Object(root_map);
        }
        // {"schema": true} → {"schema": {}}(对齐 CPA c93978c4)
        if root_map.get("schema") == Some(&Value::Bool(true)) {
            return json!({"schema": {}});
        }
    }

    let (repaired, _modified) = repair_schema_node(root_map, add_missing_array_items);
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
fn repair_schema_node(
    node: Map<String, Value>,
    add_missing_array_items: bool,
) -> (Map<String, Value>, bool) {
    let mut clone = node;
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
            let (repaired_props, promoted_reqs, _) =
                repair_property_map(bare_props, add_missing_array_items);
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
            // 对齐 CPA e1e3a811:含 properties 但无 type 时显式置 type="object"
            if !clone.contains_key("type") {
                clone.insert("type".to_string(), Value::String("object".to_string()));
                modified = true;
            }
            let (repaired, promoted, props_mod) =
                repair_property_map(props_val, add_missing_array_items);
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

    // Gemini/Antigravity 拒无 items 的工具 array schema,也拒 items 配非 array type
    // (对齐 CPA b5cde4ba + ffe6ad3c)。显式非 array type 的 items 留给 sanitize_array_items 剥离。
    // 仍挂未解析 $ref 的节点不加占位 items / 不合成 type,否则内联时会变成兄弟键盖掉定义(CPA c558d320)。
    // 对齐 CPA clone["$ref"] == nil:键缺失或值为 JSON null 都视为无 $ref
    if add_missing_array_items && clone.get("$ref").map_or(true, Value::is_null) {
        if is_array_declared_type(clone.get("type")) && !clone.contains_key("items") {
            clone.insert("items".to_string(), json!({"type": "string"}));
            modified = true;
        } else if clone.contains_key("items") && schema_type_string(clone.get("type")).is_empty() {
            // 对齐 CPA:type == nil || type == ""(含 JSON null 与 "")
            clone.insert("type".to_string(), Value::String("array".to_string()));
            modified = true;
        }
    }

    // 3. 递归进 items/additionalProperties/patternProperties 等容器
    // items 布尔 true → 空对象 schema(对齐 CPA c93978c4)
    if clone.get("items") == Some(&Value::Bool(true)) {
        clone.insert("items".to_string(), json!({}));
        modified = true;
    }
    if let Some(Value::Object(items)) = clone.remove("items") {
        let (repaired, items_mod) = repair_schema_node(items, add_missing_array_items);
        clone.insert("items".to_string(), Value::Object(repaired));
        if items_mod {
            modified = true;
        }
    } else if let Some(Value::Array(items_list)) = clone.remove("items") {
        let (repaired, list_mod) = repair_schema_list(items_list, add_missing_array_items);
        clone.insert("items".to_string(), Value::Array(repaired));
        if list_mod {
            modified = true;
        }
    }

    if let Some(Value::Object(add_props)) = clone.get("additionalProperties").cloned() {
        let (repaired, add_mod) = repair_schema_node(add_props, add_missing_array_items);
        clone.insert("additionalProperties".to_string(), Value::Object(repaired));
        if add_mod {
            modified = true;
        }
    }

    if let Some(Value::Object(pat_props)) = clone.remove("patternProperties") {
        let (repaired, _, pat_mod) = repair_property_map(pat_props, add_missing_array_items);
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
        // 布尔 true 子 schema → 空对象(对齐 CPA c93978c4)
        if clone.get(*key) == Some(&Value::Bool(true)) {
            clone.insert(key.to_string(), json!({}));
            modified = true;
            continue;
        }
        if let Some(Value::Object(sub_val)) = clone.remove(*key) {
            let (repaired, sub_mod) = repair_schema_node(sub_val, add_missing_array_items);
            clone.insert(key.to_string(), Value::Object(repaired));
            if sub_mod {
                modified = true;
            }
        }
    }

    for key in &["anyOf", "oneOf", "allOf", "prefixItems"] {
        if let Some(Value::Array(list_val)) = clone.remove(*key) {
            let (repaired, list_mod) = repair_schema_list(list_val, add_missing_array_items);
            clone.insert(key.to_string(), Value::Array(repaired));
            if list_mod {
                modified = true;
            }
        }
    }

    // dependencies 同为定义遍历关键字(对齐 CPA c93978c4)
    for key in &["$defs", "definitions", "dependentSchemas", "dependencies"] {
        if let Some(Value::Object(defs_val)) = clone.remove(*key) {
            let mut repaired_defs = Map::new();
            let mut defs_modified = false;
            for (dk, dv) in defs_val {
                if let Value::Object(def_map) = dv {
                    let (repaired_def, def_mod) =
                        repair_schema_node(def_map, add_missing_array_items);
                    repaired_defs.insert(dk, Value::Object(repaired_def));
                    if def_mod {
                        defs_modified = true;
                        modified = true;
                    }
                } else if dv == Value::Bool(true) {
                    // 布尔 true 定义 → 空对象(对齐 CPA c93978c4)
                    repaired_defs.insert(dk, json!({}));
                    defs_modified = true;
                    modified = true;
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
fn repair_property_map(
    props: Map<String, Value>,
    add_missing_array_items: bool,
) -> (Map<String, Value>, Vec<String>, bool) {
    let mut out = Map::new();
    let mut promoted_reqs = Vec::new();
    let mut modified = false;

    for (k, v) in props {
        let Value::Object(mut child_map) = v else {
            // 布尔 true 属性值 → 空对象(对齐 CPA c93978c4)
            if v == Value::Bool(true) {
                out.insert(k, json!({}));
                modified = true;
                continue;
            }
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
        let (repaired_child, child_mod) = repair_schema_node(child_map, add_missing_array_items);
        if child_mod {
            modified = true;
        }

        out.insert(k, Value::Object(repaired_child));
    }

    promoted_reqs.sort();
    (out, promoted_reqs, modified)
}

fn repair_schema_list(list: Vec<Value>, add_missing_array_items: bool) -> (Vec<Value>, bool) {
    let mut repaired = Vec::new();
    let mut list_modified = false;
    for item in list {
        if let Value::Object(item_map) = item {
            let (repaired_item, item_mod) = repair_schema_node(item_map, add_missing_array_items);
            repaired.push(Value::Object(repaired_item));
            if item_mod {
                list_modified = true;
            }
        } else if item == Value::Bool(true) {
            // 布尔 true 分支 → 空对象(对齐 CPA c93978c4)
            repaired.push(json!({}));
            list_modified = true;
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
        // 对齐 CPA 2eb8dd11:object 判定大小写不敏感
        Some(Value::String(s)) => !s.is_empty() && !s.eq_ignore_ascii_case("object"),
        Some(Value::Array(arr)) => {
            !arr.is_empty()
                && !arr.iter().any(|item| {
                    item.as_str()
                        .is_some_and(|s| s.eq_ignore_ascii_case("object"))
                })
        }
        _ => false,
    }
}

/// 对齐 CPA isArrayDeclaredType:type 为 "array" 或 type 列表含 "array"
/// 对齐 gjson Result.String():缺失、null、"" 为空串;数组取 Raw(如 "[]")
fn schema_type_string(t: Option<&Value>) -> String {
    match t {
        None | Some(Value::Null) => String::new(),
        Some(Value::String(s)) => s.clone(),
        Some(other) => other.to_string(),
    }
}

fn is_array_declared_type(t: Option<&Value>) -> bool {
    match t {
        // 对齐 CPA 2eb8dd11:array 判定大小写不敏感
        Some(Value::String(s)) => s.eq_ignore_ascii_case("array"),
        Some(Value::Array(arr)) => arr.iter().any(|item| {
            item.as_str()
                .is_some_and(|s| s.eq_ignore_ascii_case("array"))
        }),
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
        // 对齐 CPA b532db9c:Gemini 直连保留 additionalProperties,不再加提示
        assert_eq!(out["additionalProperties"], json!(false));
        assert!(out.get("encrypted").is_none());
        assert!(out.get("description").is_none());
        let p = &out["properties"]["p"];
        // format 属标准约束,保留(对齐 CPA b532db9c)
        assert_eq!(p["format"], "date");
        assert!(p.get("nullable").is_none());
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
    fn test_schema_identifier_keywords_stripped() {
        // 对齐 CPA f668ac41(Issue #5888):draft-04 "id" 与 2019-09/2020-12
        // 标识关键字从 schema 节点剥离,属性名映射里的 "id" 属性保留
        let schema = json!({
            "id": "http://example.com/root.json",
            "$anchor": "rootAnchor",
            "$vocabulary": {"https://json-schema.org/draft/2020-12/vocab/core": true},
            "type": "object",
            "properties": {
                "kind": {
                    "type": "string",
                    "enum": ["short", "video"],
                    "id": "ContentType",
                    "$anchor": "contentTypeAnchor",
                    "$dynamicAnchor": "dynAnchor",
                    "$dynamicRef": "#dynAnchor",
                    "description": "Kind"
                },
                "id": {
                    "type": "string",
                    "description": "Property legitimately named id should survive"
                }
            },
            "required": ["kind"]
        });
        for out in [
            clean_json_schema_for_gemini(&schema),
            clean_json_schema_for_antigravity(&schema),
        ] {
            // 根节点标识关键字剥离
            assert!(out.get("id").is_none(), "root id kept: {out}");
            assert!(out.get("$anchor").is_none(), "root $anchor kept: {out}");
            assert!(
                out.get("$vocabulary").is_none(),
                "root $vocabulary kept: {out}"
            );
            // 属性 schema 内的标识关键字剥离
            assert!(out["properties"]["kind"].get("id").is_none());
            assert!(out["properties"]["kind"].get("$anchor").is_none());
            assert!(out["properties"]["kind"].get("$dynamicAnchor").is_none());
            assert!(out["properties"]["kind"].get("$dynamicRef").is_none());
            // 属性名 "id" 本身保留
            assert!(
                out["properties"].get("id").is_some(),
                "property id dropped: {out}"
            );
            assert_eq!(out["properties"]["id"]["type"], "string");
        }
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
    fn test_antigravity_string_const_keeps_string_type() {
        let schema = json!({"const": "read"});
        let out = clean_json_schema_for_antigravity(&schema);
        assert_eq!(out["type"], "string");
        assert!(out.get("enum").is_none());
        assert!(out["description"]
            .as_str()
            .unwrap()
            .contains("Allowed: read"));
    }

    #[test]
    fn test_gemini_string_const_intersects_existing_enum() {
        let schema = json!({"const": "read", "enum": ["read", "write"]});
        let out = clean_json_schema_for_gemini(&schema);
        assert_eq!(out["enum"], json!(["read"]));
        assert_eq!(out["type"], "string");
    }

    #[test]
    fn test_gemini_property_named_const_is_not_treated_as_schema_keyword() {
        let schema = json!({
            "type": "object",
            "properties": {"const": {"const": "browser"}}
        });
        let out = clean_json_schema_for_gemini(&schema);
        assert!(out["properties"].get("enum").is_none());
        assert_eq!(out["properties"]["const"]["enum"], json!(["browser"]));
        assert_eq!(out["properties"]["const"]["type"], "string");
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
    fn test_antigravity_tool_skips_placeholder_when_disabled() {
        // 对齐 CPA requirePlaceholder=false(非 claude / 非 gemini-3-pro 族)
        let schema = json!({"type": "object", "properties": {}, "title": "T"});
        let out = clean_json_schema_for_antigravity_tool(&schema, false);
        assert!(out.get("required").is_none(), "{out}");
        assert!(out
            .get("properties")
            .and_then(|p| p.get("reason"))
            .is_none());
        assert!(out.get("title").is_none(), "{out}");
        let nested = clean_nested_schema_for_antigravity_tool(
            &json!({"type": "object", "properties": {"flag": {"type": "string"}}}),
            false,
        );
        assert!(nested.get("required").is_none(), "{nested}");
        assert!(nested["properties"].get("_").is_none());
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
        let phase0 = normalize_malformed_schema_objects(schema.clone(), true);
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
    fn test_anyof_single_non_null_with_null_still_projects() {
        // [T, null] 仍投影为 T;(nullable)/Accepts 提示保留。多分支则不再 flatten。
        let schema = json!({"anyOf": [{"type": "null"}, {"type": "object", "properties": {"x": {"type": "string"}}}]});
        let out = clean_json_schema_for_gemini(&schema);
        assert_eq!(out["type"], "object");
        assert!(out.get("anyOf").is_none());
        assert!(out["description"].as_str().unwrap().contains("Accepts:"));
    }

    #[test]
    fn test_gemini_keeps_additional_properties_schema_valued() {
        let schema = json!({"type": "object", "additionalProperties": {"type": "string"}});
        let out = clean_json_schema_for_gemini(&schema);
        // 对齐 CPA b532db9c:schema 形态的 additionalProperties 原样保留
        assert_eq!(out["additionalProperties"]["type"], "string");
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
    fn test_anyof_required_only_branches_materialized_and_preserved() {
        // 对齐 CPA a0fd8448:required-only 分支用父 properties 具化后保留在 anyOf
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
        let branches = out["anyOf"].as_array().expect("anyOf preserved");
        assert_eq!(branches.len(), 2);
        assert_eq!(branches[0]["type"], "object");
        assert_eq!(branches[0]["properties"]["left"]["type"], "integer");
        assert_eq!(branches[0]["required"], json!(["left"]));
        assert_eq!(branches[1]["properties"]["right"]["type"], "integer");
        assert_eq!(branches[1]["required"], json!(["right"]));
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
        // Antigravity 语义不变:contains 剥离搬提示
        let out = clean_json_schema_for_antigravity(&input);
        let tags = &out["properties"]["tags"];
        assert!(tags.get("contains").is_none());
        let desc = tags["description"].as_str().unwrap_or("");
        assert!(desc.contains("contains"), "desc: {desc}");
        // Gemini 直连保留标准约束(对齐 CPA b532db9c)
        let out = clean_json_schema_for_gemini(&input);
        let tags = &out["properties"]["tags"];
        assert_eq!(tags["contains"]["enum"], json!(["x"]));
        assert!(tags.get("description").is_none());
    }

    #[test]
    fn test_gemini_preserves_constraints_and_additional_properties() {
        // 对齐 CPA b532db9c(Issue #5959):parametersJsonSchema 载体保留
        // additionalProperties 与 pattern/minLength/maxLength 等标准约束
        let input = json!({
            "$schema": "https://json-schema.org/draft/2020-12/schema",
            "title": "SubmitTool",
            "type": "object",
            "additionalProperties": false,
            "properties": {
                "recipient": {
                    "type": "string",
                    "pattern": "^(alice|bob)$",
                    "minLength": 3,
                    "maxLength": 10
                },
                "nested": {
                    "type": "object",
                    "additionalProperties": false,
                    "properties": {"tag": {"type": "string", "pattern": "^[a-z]+$"}}
                },
                "items_list": {
                    "type": "array",
                    "items": {"type": "string", "pattern": "^[0-9]+$"}
                }
            },
            "required": ["recipient"]
        });
        let out = clean_json_schema_for_gemini(&input);
        assert_eq!(out["additionalProperties"], json!(false));
        assert_eq!(out["properties"]["recipient"]["pattern"], "^(alice|bob)$");
        assert_eq!(out["properties"]["recipient"]["minLength"], 3);
        assert_eq!(out["properties"]["recipient"]["maxLength"], 10);
        assert_eq!(
            out["properties"]["nested"]["additionalProperties"],
            json!(false)
        );
        assert_eq!(
            out["properties"]["nested"]["properties"]["tag"]["pattern"],
            "^[a-z]+$"
        );
        assert_eq!(
            out["properties"]["items_list"]["items"]["pattern"],
            "^[0-9]+$"
        );
        // 元数据仍剥离
        assert!(out.get("$schema").is_none());
        assert!(out.get("title").is_none());
        // 无约束提示
        assert!(out.get("description").is_none());
        assert!(out["properties"]["recipient"].get("description").is_none());
    }

    #[test]
    fn test_multibranch_anyof_preserved() {
        // 对齐 CPA a0fd8448:多分支 anyOf 不再 select_best 展平
        let input = json!({
            "type": "object",
            "properties": {
                "item": {
                    "anyOf": [
                        {"type": "string"},
                        {
                            "type": "object",
                            "properties": {"image": {"type": "string"}},
                            "required": ["image"]
                        },
                        {
                            "type": "object",
                            "properties": {"audio": {"type": "string"}},
                            "required": ["audio"]
                        }
                    ]
                }
            },
            "required": ["item"]
        });
        let ag = clean_json_schema_for_antigravity(&input);
        let branches = ag["properties"]["item"]["anyOf"].as_array().unwrap();
        assert_eq!(branches.len(), 3);
        assert!(branches[2]["properties"].get("audio").is_some());

        let gem = clean_json_schema_for_gemini(&input);
        assert_eq!(
            gem["properties"]["item"]["anyOf"].as_array().unwrap().len(),
            3
        );
    }

    #[test]
    fn test_antigravity_claude_flattens_multibranch_anyof() {
        // 对齐 CPA 5efe9758:AG Claude 工具清洗展平多分支 → 最强 object 分支
        let input = json!({
            "type": "object",
            "properties": {
                "item": {
                    "anyOf": [
                        {"type": "string"},
                        {
                            "type": "object",
                            "properties": {"image": {"type": "string"}},
                            "required": ["image"]
                        },
                        {
                            "type": "object",
                            "properties": {"audio": {"type": "string"}},
                            "required": ["audio"]
                        }
                    ]
                }
            },
            "required": ["item"]
        });
        let claude = clean_json_schema_for_antigravity_claude_tool(&input);
        let item = &claude["properties"]["item"];
        assert!(item.get("anyOf").is_none(), "{item}");
        assert!(item.get("oneOf").is_none(), "{item}");
        assert_eq!(item["type"], "object");
        assert!(
            item["properties"].get("image").is_some() || item["properties"].get("audio").is_some(),
            "{item}"
        );
        let desc = item
            .get("description")
            .and_then(|d| d.as_str())
            .unwrap_or("");
        assert!(desc.contains("Accepts:"), "{desc}");
        let again = clean_json_schema_for_antigravity_claude_tool(&claude);
        assert!(again["properties"]["item"].get("anyOf").is_none());
    }

    #[test]
    fn test_antigravity_claude_flattens_oneof_and_inherits_nullable() {
        // 对齐 CPA 5efe9758:oneOf 多分支展平;已有 nullable 继承
        let input = json!({
            "type": "object",
            "properties": {
                "value": {
                    "oneOf": [
                        {"type": "string"},
                        {"type": "integer"}
                    ],
                    "nullable": true
                }
            }
        });
        let out = clean_json_schema_for_antigravity_claude_tool(&input);
        let value = &out["properties"]["value"];
        assert!(
            value.get("oneOf").is_none() && value.get("anyOf").is_none(),
            "{value}"
        );
        assert_eq!(value["type"], "string");
        assert_eq!(value["nullable"], true);
        let desc = value
            .get("description")
            .and_then(|d| d.as_str())
            .unwrap_or("");
        assert!(desc.contains("Accepts:"), "{desc}");

        let ag = clean_json_schema_for_antigravity(&input);
        assert_eq!(
            ag["properties"]["value"]["anyOf"].as_array().unwrap().len(),
            2
        );
    }

    #[test]
    fn test_oneof_normalized_to_anyof_on_antigravity_preserved_on_gemini() {
        // AG/native:!preserve → oneOf→anyOf; Gemini JSON Schema:保留 oneOf
        let input = json!({
            "type": "object",
            "properties": {
                "item": {
                    "oneOf": [
                        {"type": "string"},
                        {
                            "type": "object",
                            "properties": {"image": {"type": "string"}},
                            "required": ["image"]
                        },
                        {
                            "type": "object",
                            "properties": {"audio": {"type": "string"}},
                            "required": ["audio"]
                        }
                    ]
                }
            },
            "required": ["item"]
        });
        let ag = clean_json_schema_for_antigravity(&input);
        assert!(ag["properties"]["item"].get("oneOf").is_none());
        assert_eq!(
            ag["properties"]["item"]["anyOf"].as_array().unwrap().len(),
            3
        );

        let gem = clean_json_schema_for_gemini(&input);
        assert_eq!(
            gem["properties"]["item"]["oneOf"].as_array().unwrap().len(),
            3
        );
        assert!(gem["properties"]["item"].get("anyOf").is_none());
    }

    #[test]
    fn test_nullable_multibranch_anyof_sets_hint() {
        let input = json!({
            "type": "object",
            "properties": {
                "item": {
                    "anyOf": [
                        {"type": "string"},
                        {
                            "type": "object",
                            "properties": {"image": {"type": "string"}},
                            "required": ["image"]
                        },
                        {
                            "type": "object",
                            "properties": {"audio": {"type": "string"}},
                            "required": ["audio"]
                        },
                        {"type": "null"}
                    ]
                }
            }
        });
        let ag = clean_json_schema_for_antigravity(&input);
        let item = &ag["properties"]["item"];
        assert_eq!(item["anyOf"].as_array().unwrap().len(), 3);
        assert_eq!(item["nullable"], true);
        assert!(item["description"].as_str().unwrap().contains("(nullable)"));

        let gem = clean_json_schema_for_gemini(&input);
        let item = &gem["properties"]["item"];
        assert_eq!(item["anyOf"].as_array().unwrap().len(), 3);
        assert!(item.get("nullable").is_none()); // remove_gemini_metadata
        assert!(item["description"].as_str().unwrap().contains("(nullable)"));
    }

    #[test]
    fn test_sibling_oneof_hinted_away_on_antigravity() {
        let input = json!({
            "type": "object",
            "properties": {
                "value": {
                    "anyOf": [{"type": "string"}, {"type": "integer"}],
                    "oneOf": [{"type": "string"}, {"type": "boolean"}]
                }
            }
        });
        let ag = clean_json_schema_for_antigravity(&input);
        let value = &ag["properties"]["value"];
        assert!(value.get("oneOf").is_none());
        assert_eq!(value["anyOf"].as_array().unwrap().len(), 2);
        assert!(value["description"].as_str().unwrap().contains("oneOf:"));

        let gem = clean_json_schema_for_gemini(&input);
        let value = &gem["properties"]["value"];
        assert_eq!(value["anyOf"].as_array().unwrap().len(), 2);
        assert_eq!(value["oneOf"].as_array().unwrap().len(), 2);
    }

    #[test]
    fn test_false_union_branch_stripped() {
        let single =
            json!({"type":"object","properties":{"value":{"anyOf":[false,{"type":"string"}]}}});
        let out = clean_json_schema_for_antigravity(&single);
        assert!(out["properties"]["value"].get("anyOf").is_none());
        assert_eq!(out["properties"]["value"]["type"], "string");

        let multi = json!({"type":"object","properties":{"value":{"oneOf":[false,{"type":"string"},{"type":"integer"}]}}});
        let out = clean_json_schema_for_antigravity(&multi);
        assert!(out["properties"]["value"].get("oneOf").is_none());
        assert_eq!(
            out["properties"]["value"]["anyOf"]
                .as_array()
                .unwrap()
                .len(),
            2
        );
    }

    #[test]
    fn test_nested_nullable_union_unwraps_to_inner() {
        for nested in [
            json!({"type":"object","properties":{"item":{"anyOf":[{"type":"null"},{"anyOf":[{"type":"string"},{"type":"integer"}]}]}}}),
            json!({"type":"object","properties":{"item":{"anyOf":[{"anyOf":[{"type":"string"},{"type":"integer"}]},{"type":"null"}]}}}),
        ] {
            let once = clean_json_schema_for_antigravity(&nested);
            let item = &once["properties"]["item"];
            assert_eq!(item["anyOf"].as_array().unwrap().len(), 2);
            assert_eq!(item["nullable"], true);
            assert!(item["description"].as_str().unwrap().contains("(nullable)"));
            let twice = clean_json_schema_for_antigravity(&once);
            assert_eq!(once, twice);
        }
    }

    #[test]
    fn test_parent_props_multibranch_union_not_merged_away() {
        let input = json!({
            "type": "object",
            "properties": {},
            "anyOf": [
                {
                    "type": "object",
                    "properties": {"image": {"type": "string"}},
                    "required": ["image"]
                },
                {
                    "type": "object",
                    "properties": {"audio": {"type": "string"}},
                    "required": ["audio"]
                }
            ]
        });
        for out in [
            clean_json_schema_for_antigravity(&input),
            clean_json_schema_for_gemini(&input),
        ] {
            assert_eq!(out["anyOf"].as_array().unwrap().len(), 2);
            assert!(out["anyOf"][1]["properties"].get("audio").is_some());
        }
    }

    #[test]
    fn test_boolean_true_subschemas_normalized_to_empty_object() {
        // 对齐 CPA c93978c4:布尔 true 子 schema 归一化为 {}
        let input = json!({
            "type": "object",
            "properties": {
                "open": true,
                "closed": {"type": "string"},
                "list": {"type": "array", "items": true},
                "branchy": {"anyOf": [true, {"type": "string"}]}
            },
            "$defs": {"free": true, "shaped": {"type": "number"}},
            "dependencies": {"dep": true}
        });
        for out in [
            clean_json_schema_for_gemini(&input),
            clean_json_schema_for_antigravity(&input),
        ] {
            assert_eq!(out["properties"]["open"], json!({}));
            assert_eq!(out["properties"]["closed"]["type"], "string");
            assert_eq!(out["properties"]["list"]["items"], json!({}));
            // 对齐 CPA a0fd8448:true→{} 后与 string 构成多分支,anyOf 保留
            let branchy = &out["properties"]["branchy"];
            let branches = branchy["anyOf"].as_array().expect("anyOf preserved");
            assert_eq!(branches.len(), 2);
            assert_eq!(branches[0], json!({}));
            assert_eq!(branches[1]["type"], "string");
            // $defs / dependencies 归一化发生在剥离前,最终输出均剥离
            assert!(out.get("$defs").is_none());
            assert!(out.get("dependencies").is_none());
        }
    }

    #[test]
    fn test_root_boolean_true_normalized() {
        // 根节点与包裹层布尔 true → {}
        assert_eq!(clean_json_schema_for_gemini(&json!(true)), json!({}));
        assert_eq!(
            clean_json_schema_for_gemini(&json!({"schema": true})),
            json!({"schema": {}})
        );
        // false 原样返回
        assert_eq!(clean_json_schema_for_gemini(&json!(false)), json!(false));
    }

    #[test]
    fn test_new_unsupported_keywords_stripped() {
        // 对齐 CPA c93978c4:additionalItems/unevaluated*/contentSchema 剥离
        let input = json!({
            "type": "object",
            "properties": {
                "p": {
                    "type": "array",
                    "additionalItems": {"type": "string"},
                    "unevaluatedProperties": {"type": "string"},
                    "unevaluatedItems": {"type": "string"},
                    "contentSchema": {"type": "string"}
                }
            }
        });
        for out in [
            clean_json_schema_for_gemini(&input),
            clean_json_schema_for_antigravity(&input),
        ] {
            let p = &out["properties"]["p"];
            assert!(p.get("additionalItems").is_none());
            assert!(p.get("unevaluatedProperties").is_none());
            assert!(p.get("unevaluatedItems").is_none());
            assert!(p.get("contentSchema").is_none());
        }
    }

    #[test]
    fn test_dependent_keywords_stripped_issue_6539() {
        // 对齐 CPA 9126772d / TestCleanJSONSchema_RemovesDependentKeywords_Issue6539:
        // dependentRequired / dependentSchemas / dependencies 作为 schema 关键字剥离,
        // properties 下同名合法属性名保留。
        let input = json!({
            "type": "object",
            "properties": {
                "baseline_url": {"type": "string"},
                "candidate_url": {"type": "string"},
                "options": {
                    "type": "object",
                    "properties": {
                        "threshold": {"type": "number"},
                        "diff_color": {"type": "string"}
                    },
                    "dependentRequired": {
                        "threshold": ["diff_color"]
                    },
                    "dependentSchemas": {
                        "threshold": {
                            "properties": {
                                "diff_color": {"type": "string"}
                            }
                        }
                    },
                    "dependencies": {
                        "threshold": ["diff_color"]
                    }
                },
                "dependentRequired": {
                    "type": "string",
                    "description": "Legitimate property named dependentRequired"
                },
                "dependentSchemas": {
                    "type": "object",
                    "properties": {
                        "nested": {"type": "string"}
                    },
                    "dependentRequired": {
                        "nested": ["other"]
                    }
                },
                "dependencies": {
                    "type": "string",
                    "description": "Legitimate property named dependencies"
                }
            },
            "dependentRequired": {
                "baseline_url": ["candidate_url"]
            },
            "dependentSchemas": {
                "baseline_url": {
                    "required": ["candidate_url"]
                }
            },
            "dependencies": {
                "baseline_url": ["candidate_url"]
            }
        });

        for out in [
            clean_json_schema_for_gemini(&input),
            clean_json_schema_for_antigravity(&input),
        ] {
            for kw in ["dependentRequired", "dependentSchemas", "dependencies"] {
                assert!(out.get(kw).is_none(), "root {kw} was not removed: {out}");
                assert!(
                    out["properties"]["options"].get(kw).is_none(),
                    "nested properties.options.{kw} was not removed: {out}"
                );
            }
            assert!(
                out["properties"]["dependentSchemas"]
                    .get("dependentRequired")
                    .is_none(),
                "properties.dependentSchemas.dependentRequired was not removed: {out}"
            );
            for prop in ["dependentRequired", "dependentSchemas", "dependencies"] {
                assert!(
                    out["properties"].get(prop).is_some(),
                    "legitimate property {prop} was removed: {out}"
                );
            }
        }
    }

    #[test]
    fn test_tool_arrays_missing_items_get_string_schema() {
        // 对齐 CPA b5cde4ba / TestCleanJSONSchema_ToolArraysMissingItems:
        // Gemini 与 Antigravity 工具 schema 缺 items 的 array 补 string items。
        let input = json!({
            "type": "object",
            "properties": {
                "params": {"type": "array"},
                "values": {"type": ["array", "null"], "description": "no items"},
                "existing": {"type": "array", "items": {"type": "number"}}
            }
        });
        for out in [
            clean_json_schema_for_antigravity(&input),
            clean_json_schema_for_gemini(&input),
        ] {
            assert_eq!(out["properties"]["params"]["items"]["type"], "string");
            assert_eq!(out["properties"]["values"]["items"]["type"], "string");
            assert_eq!(out["properties"]["existing"]["items"]["type"], "number");
        }
        for out in [
            clean_json_schema_for_antigravity(&json!({"type": "array"})),
            clean_json_schema_for_gemini(&json!({"type": "array"})),
        ] {
            assert_eq!(out["items"]["type"], "string");
        }
    }

    #[test]
    fn test_items_require_array_type() {
        // 对齐 CPA ffe6ad3c / TestCleanJSONSchema_ArrayItemsRequireArrayType_Issue6011
        let missing_type = json!({
            "type": "object",
            "properties": {
                "revision_reasons": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "evidence_reference": {
                                "description": "references to evidence",
                                "items": {"type": "string"}
                            }
                        }
                    }
                }
            }
        });
        let union_type = json!({
            "type": "object",
            "properties": {
                "revision_reasons": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "evidence_reference": {
                                "type": ["string", "array"],
                                "items": {"type": "string"}
                            }
                        }
                    }
                }
            }
        });
        let non_array = json!({
            "type": "object",
            "properties": {
                "label": {"type": "string", "items": {"type": "string"}},
                "config": {
                    "type": "object",
                    "properties": {"key": {"type": "string"}},
                    "items": {"type": "string"}
                }
            }
        });
        let named_items = json!({
            "type": "object",
            "properties": {"items": {"type": "string", "description": "a field named items"}}
        });
        let root_missing = json!({"items": {"type": "string"}});

        for clean in [
            clean_json_schema_for_gemini,
            clean_json_schema_for_antigravity,
        ] {
            let out = clean(&missing_type);
            let target =
                &out["properties"]["revision_reasons"]["items"]["properties"]["evidence_reference"];
            assert_eq!(target["type"], "array");
            assert_eq!(target["items"]["type"], "string");

            let out = clean(&union_type);
            assert_eq!(
                out["properties"]["revision_reasons"]["items"]["properties"]["evidence_reference"]
                    ["type"],
                "array"
            );

            let out = clean(&non_array);
            assert!(out["properties"]["label"].get("items").is_none());
            assert_eq!(out["properties"]["label"]["type"], "string");
            assert!(out["properties"]["config"].get("items").is_none());

            let out = clean(&named_items);
            assert_eq!(out["properties"]["items"]["type"], "string");
            assert!(out["properties"]["items"].get("items").is_none());

            let out = clean(&root_missing);
            assert_eq!(out["type"], "array");
            assert_eq!(out["items"]["type"], "string");

            let again = clean(&out);
            assert_eq!(again, out);

            // gjson String() 对 null 与 "" 都返回空串,CPA 补 array 而非剥 items
            for input in [
                json!({"type": null, "items": {"type": "string"}}),
                json!({"type": "", "items": {"type": "string"}}),
            ] {
                let out = clean(&input);
                assert_eq!(out["type"], "array");
                assert_eq!(out["items"]["type"], "string");
            }
            // type: [] 的 gjson String() 是 "[]",非空,剥 items,type 保持空数组
            let empty_list = clean(&json!({"type": [], "items": {"type": "string"}}));
            assert!(empty_list.get("items").is_none());
            assert_eq!(empty_list["type"], json!([]));
        }
    }

    #[test]
    fn test_items_preserved_for_uppercase_array_type() {
        // 对齐 CPA 2eb8dd11 / TestSanitizeArrayItems_PreservesItemsForUppercaseArrayType:
        // 大写 ARRAY 声明保留 items,不再误剥
        let input = json!({
            "type": "OBJECT",
            "properties": {
                "summary": {"type": "STRING"},
                "brands": {
                    "type": "ARRAY",
                    "items": {"type": "STRING"}
                },
                "catalog": {
                    "type": "OBJECT",
                    "properties": {
                        "items": {
                            "type": "ARRAY",
                            "items": {
                                "type": "OBJECT",
                                "properties": {"id": {"type": "STRING"}}
                            }
                        }
                    }
                }
            }
        });
        for clean in [
            clean_json_schema_for_gemini,
            clean_json_schema_for_antigravity,
        ] {
            let out = clean(&input);
            assert!(
                out["properties"]["brands"]["items"].is_object(),
                "brands.items stripped: {out}"
            );
            assert_eq!(out["properties"]["brands"]["type"], "ARRAY");
            assert!(
                out["properties"]["catalog"]["properties"]["items"]["items"].is_object(),
                "nested items stripped: {out}"
            );
        }
    }

    #[test]
    fn test_flatten_type_arrays_properties_prefer_object() {
        // 对齐 CPA e1e3a811: type union 含 properties 时优先 object
        let mut schema = json!({
            "type": ["string", "object"],
            "properties": {
                "name": {"type": "string"}
            }
        });
        flatten_type_arrays(&mut schema, false);
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"].is_object());

        // 无 properties 时保持原逻辑(首个非 null)
        let mut schema2 = json!({
            "type": ["string", "object"]
        });
        flatten_type_arrays(&mut schema2, false);
        assert_eq!(schema2["type"], "string");
    }

    #[test]
    fn test_clean_json_schema_enforces_object_type_for_properties() {
        // 对齐 CPA e1e3a811 / TestCleanJSONSchema_EnforcesObjectTypeForProperties_Issue6394
        let input_union = json!({
            "type": "object",
            "properties": {
                "requestBody": {
                    "type": "object",
                    "properties": {
                        "delivery": {
                            "type": "object",
                            "properties": {
                                "endpoint": {
                                    "type": ["string", "object"],
                                    "properties": {"traces": {"type": "string"}},
                                    "required": ["traces"]
                                }
                            },
                            "required": ["endpoint"]
                        }
                    },
                    "required": ["delivery"]
                }
            },
            "required": ["requestBody"]
        });

        let input_primitive = json!({
            "type": "object",
            "properties": {
                "endpoint": {
                    "type": "string",
                    "properties": {"traces": {"type": "string"}},
                    "required": ["traces"]
                }
            }
        });

        let input_missing_type = json!({
            "type": "object",
            "properties": {
                "endpoint": {
                    "properties": {"traces": {"type": "string"}},
                    "required": ["traces"]
                }
            }
        });

        for clean in [
            clean_json_schema_for_gemini,
            clean_json_schema_for_antigravity,
        ] {
            let out1 = clean(&input_union);
            assert_eq!(
                out1["properties"]["requestBody"]["properties"]["delivery"]["properties"]
                    ["endpoint"]["type"],
                "object"
            );
            assert!(
                out1["properties"]["requestBody"]["properties"]["delivery"]["properties"]
                    ["endpoint"]["properties"]["traces"]
                    .is_object()
            );

            let out2 = clean(&input_primitive);
            assert_eq!(out2["properties"]["endpoint"]["type"], "object");
            assert!(out2["properties"]["endpoint"]["properties"]["traces"].is_object());

            let out3 = clean(&input_missing_type);
            assert_eq!(out3["properties"]["endpoint"]["type"], "object");
            assert!(out3["properties"]["endpoint"]["properties"]["traces"].is_object());
        }
    }

    #[test]
    fn test_removes_discriminator_keyword_issue_6639() {
        // 对齐 CPA e9ba5c50 / Issue #6639: schema 关键字 discriminator 剥离,
        // properties.discriminator 合法属性名保留;多分支 anyOf/oneOf 在非
        // AG-Claude flatten 路径上仍保留。
        let schema = json!({
            "type": "object",
            "properties": {
                "target": {
                    "anyOf": [
                        {"type": "object", "properties": {"kind": {"const": "a"}, "url": {"type": "string"}}},
                        {"type": "object", "properties": {"kind": {"const": "b"}, "path": {"type": "string"}}}
                    ],
                    "discriminator": {"propertyName": "kind"}
                },
                "deliverables": {
                    "type": "array",
                    "items": {
                        "anyOf": [
                            {"type": "object", "properties": {"kind": {"const": "file"}, "path": {"type": "string"}}},
                            {"type": "object", "properties": {"kind": {"const": "link"}, "url": {"type": "string"}}}
                        ],
                        "discriminator": {"propertyName": "kind"}
                    }
                },
                "polymorphicObject": {
                    "type": "object",
                    "properties": {
                        "kind": {"type": "string"}
                    },
                    "oneOf": [
                        {"properties": {"a": {"type": "string"}}},
                        {"properties": {"b": {"type": "number"}}}
                    ],
                    "discriminator": {
                        "propertyName": "kind",
                        "mapping": {"a": "#/definitions/A", "b": "#/definitions/B"}
                    }
                },
                "discriminator": {
                    "type": "string",
                    "description": "Legitimate property named discriminator"
                },
                "nestedDiscriminatorProp": {
                    "type": "object",
                    "properties": {
                        "discriminator": {
                            "type": "object",
                            "properties": {
                                "kind": {"type": "string"}
                            },
                            "discriminator": {"propertyName": "kind"}
                        }
                    }
                }
            },
            "discriminator": {"propertyName": "target"}
        });

        for name in [
            "Gemini",
            "Antigravity",
            "AntigravityTool",
            "AntigravityClaudeTool",
        ] {
            let out = match name {
                "Gemini" => clean_json_schema_for_gemini(&schema),
                "Antigravity" => clean_json_schema_for_antigravity(&schema),
                "AntigravityTool" => clean_json_schema_for_antigravity_tool(&schema, false),
                "AntigravityClaudeTool" => clean_json_schema_for_antigravity_claude_tool(&schema),
                _ => unreachable!(),
            };
            assert!(
                out.get("discriminator").is_none(),
                "[{name}] root discriminator kept: {out}"
            );
            assert!(
                out["properties"]["target"].get("discriminator").is_none(),
                "[{name}] target.discriminator kept: {out}"
            );
            assert!(
                out["properties"]["deliverables"]["items"]
                    .get("discriminator")
                    .is_none(),
                "[{name}] deliverables.items.discriminator kept: {out}"
            );
            assert!(
                out["properties"]["polymorphicObject"]
                    .get("discriminator")
                    .is_none(),
                "[{name}] polymorphicObject.discriminator kept: {out}"
            );
            assert!(
                out["properties"]["nestedDiscriminatorProp"]["properties"]["discriminator"]
                    .get("discriminator")
                    .is_none(),
                "[{name}] nested keyword discriminator kept: {out}"
            );

            // AG Claude 工具路径强制展平多分支,不要求 anyOf 仍为 2
            if name != "AntigravityClaudeTool" {
                assert_eq!(
                    out["properties"]["target"]["anyOf"]
                        .as_array()
                        .map(|a| a.len()),
                    Some(2),
                    "[{name}] target.anyOf branches: {out}"
                );
                assert_eq!(
                    out["properties"]["deliverables"]["items"]["anyOf"]
                        .as_array()
                        .map(|a| a.len()),
                    Some(2),
                    "[{name}] deliverables.items.anyOf branches: {out}"
                );
            }

            assert!(
                out["properties"].get("discriminator").is_some(),
                "[{name}] property discriminator dropped: {out}"
            );
            assert!(
                out["properties"]["nestedDiscriminatorProp"]["properties"]["discriminator"]
                    ["properties"]
                    .get("kind")
                    .is_some(),
                "[{name}] nested property discriminator dropped: {out}"
            );
        }
    }

    #[test]
    fn test_removes_content_media_type_keyword_issue_6699() {
        // 对齐 CPA a43026d7 / Issue #6699: contentMediaType/contentEncoding 注解剥离,
        // properties 同名合法属性保留。
        let schema = json!({
            "type": "object",
            "contentMediaType": "application/json",
            "contentEncoding": "utf-8",
            "properties": {
                "value": {
                    "anyOf": [
                        { "type": "object", "properties": {} },
                        {
                            "type": "string",
                            "contentMediaType": "application/json",
                            "contentEncoding": "base64"
                        }
                    ]
                },
                "directPayload": {
                    "type": "string",
                    "contentMediaType": "application/json",
                    "contentEncoding": "base64"
                },
                "contentMediaType": {
                    "type": "string",
                    "contentMediaType": "text/plain",
                    "description": "Legitimate property named contentMediaType"
                },
                "contentEncoding": {
                    "type": "string",
                    "contentEncoding": "base64",
                    "description": "Legitimate property named contentEncoding"
                }
            }
        });

        for name in [
            "Gemini",
            "Antigravity",
            "AntigravityTool",
            "AntigravityClaudeTool",
        ] {
            let out = match name {
                "Gemini" => clean_json_schema_for_gemini(&schema),
                "Antigravity" => clean_json_schema_for_antigravity(&schema),
                "AntigravityTool" => clean_json_schema_for_antigravity_tool(&schema, false),
                "AntigravityClaudeTool" => clean_json_schema_for_antigravity_claude_tool(&schema),
                _ => unreachable!(),
            };
            assert!(
                out.get("contentMediaType").is_none(),
                "[{name}] root contentMediaType kept: {out}"
            );
            assert!(
                out.get("contentEncoding").is_none(),
                "[{name}] root contentEncoding kept: {out}"
            );
            assert!(
                out["properties"]["value"]["anyOf"][1]
                    .get("contentMediaType")
                    .is_none(),
                "[{name}] anyOf[1].contentMediaType kept: {out}"
            );
            assert!(
                out["properties"]["value"]["anyOf"][1]
                    .get("contentEncoding")
                    .is_none(),
                "[{name}] anyOf[1].contentEncoding kept: {out}"
            );
            assert!(
                out["properties"]["directPayload"]
                    .get("contentMediaType")
                    .is_none(),
                "[{name}] directPayload.contentMediaType kept: {out}"
            );
            assert!(
                out["properties"]["directPayload"]
                    .get("contentEncoding")
                    .is_none(),
                "[{name}] directPayload.contentEncoding kept: {out}"
            );
            assert!(
                out["properties"]["contentMediaType"]
                    .get("contentMediaType")
                    .is_none(),
                "[{name}] nested keyword contentMediaType kept: {out}"
            );
            assert!(
                out["properties"]["contentEncoding"]
                    .get("contentEncoding")
                    .is_none(),
                "[{name}] nested keyword contentEncoding kept: {out}"
            );

            if name != "AntigravityClaudeTool" {
                assert_eq!(
                    out["properties"]["value"]["anyOf"]
                        .as_array()
                        .map(|a| a.len()),
                    Some(2),
                    "[{name}] value.anyOf branches: {out}"
                );
                assert_eq!(
                    out["properties"]["value"]["anyOf"][1]["type"], "string",
                    "[{name}] value.anyOf[1].type: {out}"
                );
            }

            assert!(
                out["properties"].get("contentMediaType").is_some(),
                "[{name}] property contentMediaType dropped: {out}"
            );
            assert!(
                out["properties"].get("contentEncoding").is_some(),
                "[{name}] property contentEncoding dropped: {out}"
            );
        }
    }
    #[test]
    fn test_removes_readonly_writeonly_keyword_issue_6778() {
        // 对齐 CPA 154d5144 / Issue #6778: readOnly/writeOnly 注解剥离,
        // properties 同名合法属性保留。
        let schema = json!({
            "type": "object",
            "$schema": "http://json-schema.org/draft-04/schema#",
            "readOnly": false,
            "writeOnly": false,
            "properties": {
                "parent": { "type": "string", "description": "Parent resource name" },
                "database": {
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                            "description": "Database resource name",
                            "readOnly": true
                        },
                        "uid": { "type": "string", "readOnly": true },
                        "createTime": { "type": "string", "readOnly": true },
                        "etag": { "type": "string", "writeOnly": true },
                        "locationId": { "type": "string" }
                    }
                },
                "itemsList": {
                    "type": "array",
                    "items": {
                        "type": "string",
                        "readOnly": true,
                        "writeOnly": false
                    }
                },
                "unionField": {
                    "anyOf": [
                        { "type": "string", "readOnly": true },
                        { "type": "integer", "writeOnly": true }
                    ]
                },
                "readOnly": {
                    "type": "boolean",
                    "readOnly": true,
                    "description": "Legitimate property named readOnly"
                },
                "writeOnly": {
                    "type": "boolean",
                    "writeOnly": true,
                    "description": "Legitimate property named writeOnly"
                }
            },
            "required": ["parent"]
        });

        for name in [
            "Gemini",
            "Antigravity",
            "AntigravityTool",
            "AntigravityClaudeTool",
        ] {
            let out = match name {
                "Gemini" => clean_json_schema_for_gemini(&schema),
                "Antigravity" => clean_json_schema_for_antigravity(&schema),
                "AntigravityTool" => clean_json_schema_for_antigravity_tool(&schema, false),
                "AntigravityClaudeTool" => clean_json_schema_for_antigravity_claude_tool(&schema),
                _ => unreachable!(),
            };
            assert!(
                out.get("readOnly").is_none(),
                "[{name}] root readOnly kept: {out}"
            );
            assert!(
                out.get("writeOnly").is_none(),
                "[{name}] root writeOnly kept: {out}"
            );
            assert!(
                out["properties"]["database"]["properties"]["name"]
                    .get("readOnly")
                    .is_none(),
                "[{name}] database.name.readOnly kept: {out}"
            );
            assert!(
                out["properties"]["database"]["properties"]["uid"]
                    .get("readOnly")
                    .is_none(),
                "[{name}] database.uid.readOnly kept: {out}"
            );
            assert!(
                out["properties"]["database"]["properties"]["etag"]
                    .get("writeOnly")
                    .is_none(),
                "[{name}] database.etag.writeOnly kept: {out}"
            );
            assert!(
                out["properties"]["itemsList"]["items"]
                    .get("readOnly")
                    .is_none(),
                "[{name}] itemsList.items.readOnly kept: {out}"
            );
            assert!(
                out["properties"]["itemsList"]["items"]
                    .get("writeOnly")
                    .is_none(),
                "[{name}] itemsList.items.writeOnly kept: {out}"
            );
            assert!(
                out["properties"]["unionField"]["anyOf"][0]
                    .get("readOnly")
                    .is_none(),
                "[{name}] unionField.anyOf[0].readOnly kept: {out}"
            );
            assert!(
                out["properties"]["unionField"]["anyOf"][1]
                    .get("writeOnly")
                    .is_none(),
                "[{name}] unionField.anyOf[1].writeOnly kept: {out}"
            );
            assert!(
                out["properties"]["readOnly"].get("readOnly").is_none(),
                "[{name}] nested keyword readOnly kept: {out}"
            );
            assert!(
                out["properties"]["writeOnly"].get("writeOnly").is_none(),
                "[{name}] nested keyword writeOnly kept: {out}"
            );

            assert_eq!(
                out["properties"]["database"]["properties"]["name"]["type"], "string",
                "[{name}] database.name.type lost: {out}"
            );
            assert_eq!(
                out["properties"]["database"]["properties"]["name"]["description"],
                "Database resource name",
                "[{name}] database.name.description lost: {out}"
            );
            assert!(
                out["properties"].get("readOnly").is_some(),
                "[{name}] property readOnly dropped: {out}"
            );
            assert_eq!(
                out["properties"]["readOnly"]["type"], "boolean",
                "[{name}] property readOnly type lost: {out}"
            );
            assert_eq!(
                out["properties"]["readOnly"]["description"], "Legitimate property named readOnly",
                "[{name}] property readOnly description lost: {out}"
            );
            assert!(
                out["properties"].get("writeOnly").is_some(),
                "[{name}] property writeOnly dropped: {out}"
            );
            assert_eq!(
                out["properties"]["writeOnly"]["type"], "boolean",
                "[{name}] property writeOnly type lost: {out}"
            );
            assert_eq!(
                out["properties"]["writeOnly"]["description"],
                "Legitimate property named writeOnly",
                "[{name}] property writeOnly description lost: {out}"
            );
        }
    }

    // --- CPA c558d320 / Issue #6732: 全路径内联、$ref 预算、literal 原样 ---

    #[test]
    fn test_issue6732_gemini_cleaners_inline_local_defs() {
        let input = json!({
            "type": "object",
            "properties": {"person": {"$ref": "#/$defs/Person"}},
            "required": ["person"],
            "$defs": {
                "Person": {
                    "type": "object",
                    "properties": {"name": {"type": "string"}},
                    "required": ["name"]
                }
            }
        });
        let out = clean_json_schema_for_gemini(&input);
        assert_eq!(
            out["properties"]["person"]["properties"]["name"]["type"], "string",
            "{out}"
        );
        assert!(out.get("$defs").is_none(), "{out}");
        let s = out.to_string();
        assert!(!s.contains("See: Person"), "{out}");
    }

    #[test]
    fn test_issue6732_gemini_cleaners_inline_property_reuse() {
        let input = json!({
            "type": "object",
            "properties": {
                "billing": {"type": "object", "properties": {"zip": {"type": "string"}}},
                "shipping": {"$ref": "#/properties/billing"}
            }
        });
        let out = clean_json_schema_for_gemini(&input);
        assert_eq!(
            out["properties"]["shipping"]["properties"]["zip"]["type"], "string",
            "{out}"
        );
    }

    #[test]
    fn test_issue6732_gemini_cleaners_reuse_same_definition_twice() {
        let input = json!({
            "type": "object",
            "properties": {
                "billing": {"$ref": "#/$defs/Address"},
                "shipping": {"$ref": "#/$defs/Address"}
            },
            "$defs": {"Address": {"type": "object", "properties": {"zip": {"type": "string"}}}}
        });
        let out = clean_json_schema_for_gemini(&input);
        for field in ["billing", "shipping"] {
            assert_eq!(
                out["properties"][field]["properties"]["zip"]["type"], "string",
                "{field} lost: {out}"
            );
        }
    }

    #[test]
    fn test_issue6732_wrapped_schema_resolves_against_inner_root() {
        let inner = json!({
            "type": "object",
            "properties": {"person": {"$ref": "#/$defs/Person"}},
            "required": ["person"],
            "$defs": {
                "Person": {
                    "type": "object",
                    "properties": {"name": {"type": "string"}},
                    "required": ["name"]
                }
            }
        });
        let wrapped = json!({"schema": inner});
        let out = clean_json_schema_for_antigravity_tool(&wrapped, false);
        assert_eq!(
            out["schema"]["properties"]["person"]["properties"]["name"]["type"], "string",
            "{out}"
        );
        assert!(out["schema"].get("$defs").is_none(), "{out}");
        assert!(!out.to_string().contains("See: Person"), "{out}");
    }

    #[test]
    fn test_issue6732_default_literal_preserved_on_json_schema_route() {
        let input = json!({
            "type": "object",
            "properties": {
                "report": {
                    "type": "object",
                    "default": {
                        "title": "Report",
                        "format": "pdf",
                        "id": 7,
                        "x-owner": "ops"
                    }
                }
            }
        });
        let out = clean_json_schema_for_gemini(&input);
        let def = &out["properties"]["report"]["default"];
        assert_eq!(def["title"], "Report", "{out}");
        assert_eq!(def["format"], "pdf", "{out}");
        assert_eq!(def["id"], 7, "{out}");
        assert_eq!(def["x-owner"], "ops", "{out}");
    }

    #[test]
    fn test_issue6732_example_literals_preserved() {
        let input = json!({
            "type": "object",
            "properties": {
                "a": {
                    "type": "string",
                    "example": {"title": "Hi", "x-owner": "ops", "id": 3},
                    "examples": [{"title": "Hi2", "x-owner": "ops", "id": 4}]
                }
            }
        });
        let out = clean_json_schema_for_gemini(&input);
        let example = &out["properties"]["a"]["example"];
        assert_eq!(example["title"], "Hi", "{out}");
        assert_eq!(example["x-owner"], "ops", "{out}");
        assert_eq!(example["id"], 3, "{out}");
        let item = &out["properties"]["a"]["examples"][0];
        assert_eq!(item["title"], "Hi2", "{out}");
        assert_eq!(item["x-owner"], "ops", "{out}");
        assert_eq!(item["id"], 4, "{out}");
    }

    #[test]
    fn test_issue6732_inline_local_refs_leaves_literal_refs() {
        let input = json!({
            "type": "object",
            "properties": {
                "a": {
                    "type": "object",
                    "default": {"$ref": "#/$defs/Person"}
                }
            },
            "$defs": {
                "Person": {
                    "type": "object",
                    "properties": {"name": {"type": "string"}}
                }
            }
        });
        let got = inline_local_refs(&input).unwrap();
        assert_eq!(
            got["properties"]["a"]["default"]["$ref"], "#/$defs/Person",
            "literal $ref inside default was expanded: {got}"
        );
        assert!(
            got["properties"]["a"]["default"]
                .get("properties")
                .is_none(),
            "literal $ref inside default was expanded: {got}"
        );
        assert!(
            got["$defs"]["Person"]["properties"]["name"].is_object(),
            "InlineLocalRefs must not strip definitions: {got}"
        );
    }

    #[test]
    fn test_issue6732_properties_named_default_and_type_remain_schemas() {
        let input = json!({
            "type": "object",
            "properties": {
                "default": {"$ref": "#/$defs/Person"},
                "type": {"type": "string", "title": "Kind"}
            },
            "$defs": {
                "Person": {
                    "type": "object",
                    "properties": {"name": {"type": "string"}}
                }
            }
        });
        let out = clean_json_schema_for_gemini(&input);
        assert_eq!(
            out["properties"]["default"]["properties"]["name"]["type"], "string",
            "{out}"
        );
        assert!(
            out["properties"]["type"].get("title").is_none(),
            "property named type title should still be cleaned: {out}"
        );
    }

    #[test]
    fn test_issue6732_expansion_budget_bounds_shared_defs() {
        // diamond D12 → 2^13 叶子;超预算须退化且输出有界,外层仍展开
        let mut defs = serde_json::Map::new();
        defs.insert(
            "leaf0".to_string(),
            json!({
                "type": "string",
                "description": "leaf leaf leaf leaf leaf leaf leaf leaf leaf leaf"
            }),
        );
        for i in 0..13 {
            let target = if i == 0 {
                "leaf0".to_string()
            } else {
                format!("D{}", i - 1)
            };
            defs.insert(
                format!("D{i}"),
                json!({
                    "type": "object",
                    "properties": {
                        "a": {"$ref": format!("#/$defs/{target}")},
                        "b": {"$ref": format!("#/$defs/{target}")}
                    }
                }),
            );
        }
        let input = json!({
            "type": "object",
            "properties": {"leaf": {"$ref": "#/$defs/D12"}},
            "$defs": defs
        });
        let out = clean_json_schema_for_gemini(&input);
        let s = out.to_string();
        assert!(
            s.len() < 512 * 1024,
            "diamond expansion unbounded: {} bytes",
            s.len()
        );
        assert!(out["properties"].get("leaf").is_some(), "{out}");
        assert!(
            out["properties"]["leaf"]["properties"]["a"]["properties"]
                .get("a")
                .is_some(),
            "outer levels should still expand: {out}"
        );
        assert!(out.get("$defs").is_none(), "{out}");
    }

    #[test]
    fn test_issue6732_unused_definitions_do_not_consume_expansion_budget() {
        let big = "x".repeat(280 * 1024);
        let input = json!({
            "type": "object",
            "properties": {"a": {"$ref": "#/$defs/S"}},
            "$defs": {
                "Unused": {"type": "string", "description": big},
                "S": {"type": "string"}
            }
        });
        let out = clean_json_schema_for_gemini(&input);
        assert_eq!(out["properties"]["a"]["type"], "string", "{out}");
        assert!(!out.to_string().contains("See: S"), "{out}");
    }

    #[test]
    fn test_issue6732_budget_fallback_does_not_copy_large_descriptions() {
        let big = "x".repeat(280 * 1024);
        let input = json!({
            "type": "object",
            "properties": {
                "a": {"$ref": "#/$defs/Big"},
                "b": {"$ref": "#/$defs/Big"},
                "c": {"$ref": "#/$defs/Big"},
                "d": {"$ref": "#/$defs/Big"},
                "e": {"$ref": "#/$defs/Big"},
                "f": {"$ref": "#/$defs/Big"},
                "g": {"$ref": "#/$defs/Big"},
                "h": {"$ref": "#/$defs/Big"}
            },
            "$defs": {
                "Big": {
                    "type": "object",
                    "description": big,
                    "properties": {"name": {"type": "string"}}
                }
            }
        });
        let out = clean_json_schema_for_gemini(&input);
        let s = out.to_string();
        assert!(
            s.len() < 512 * 1024,
            "large desc fan-out: {} bytes",
            s.len()
        );
    }

    #[test]
    fn test_issue6732_array_ref_without_target_items_gets_placeholder() {
        let input = json!({
            "type": "object",
            "properties": {
                "rows": {"type": "array", "$ref": "#/$defs/Common"}
            },
            "$defs": {"Common": {"description": "Rows"}}
        });
        let first = clean_json_schema_for_antigravity_tool(&input, false);
        assert_eq!(
            first["properties"]["rows"]["items"]["type"], "string",
            "{first}"
        );
        let second = clean_json_schema_for_antigravity_tool(&first, false);
        assert_eq!(first, second, "not idempotent:\n{first}\n{second}");
    }

    #[test]
    fn test_issue6732_array_ref_with_explicit_type_keeps_target_items() {
        let input = json!({
            "type": "object",
            "properties": {
                "rows": {
                    "type": "array",
                    "$ref": "#/$defs/Rows"
                }
            },
            "$defs": {
                "Rows": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {"name": {"type": "string"}}
                    }
                }
            }
        });
        let out = clean_json_schema_for_gemini(&input);
        assert_eq!(
            out["properties"]["rows"]["items"]["properties"]["name"]["type"], "string",
            "target items clobbered: {out}"
        );
    }

    #[test]
    fn test_issue6732_synthesized_type_does_not_override_ref_target() {
        let input = json!({
            "$ref": "#/$defs/O",
            "items": {"type": "integer"},
            "$defs": {
                "O": {"type": "object", "properties": {"x": {"type": "string"}}}
            }
        });
        let first = clean_json_schema_for_gemini(&input);
        assert_eq!(first["type"], "object", "{first}");
        assert!(first.get("items").is_none(), "{first}");
        assert_eq!(first["properties"]["x"]["type"], "string", "{first}");
        let second = clean_json_schema_for_gemini(&first);
        assert_eq!(first, second, "not idempotent");
    }

    #[test]
    fn test_issue6732_property_named_ref_remains_property() {
        let input = json!({
            "type": "object",
            "properties": {"$ref": {"type": "string"}}
        });
        let out = clean_json_schema_for_gemini(&input);
        assert_eq!(out["properties"]["$ref"]["type"], "string", "{out}");
        assert!(!out.to_string().contains("See:"), "{out}");
    }

    #[test]
    fn test_issue6732_property_named_const_remains_property() {
        let input = json!({
            "type": "object",
            "properties": {"const": {"type": "string"}}
        });
        let out = clean_json_schema_for_gemini(&input);
        assert_eq!(out["properties"]["const"]["type"], "string", "{out}");
        assert!(out.get("enum").is_none(), "{out}");
    }

    #[test]
    fn test_issue6732_inline_local_refs_definition_names_keep_schema_semantics() {
        let input = json!({
            "$defs": {
                "S": {"type": "string"},
                "properties": {"type": "object", "default": {"$ref": "#/$defs/S"}},
                "default": {
                    "type": "object",
                    "properties": {"x": {"$ref": "#/$defs/S"}}
                }
            }
        });
        let got = inline_local_refs(&input).unwrap();
        assert_eq!(
            got["$defs"]["properties"]["default"]["$ref"], "#/$defs/S",
            "literal default inside def named properties expanded: {got}"
        );
        assert_eq!(
            got["$defs"]["default"]["properties"]["x"]["type"], "string",
            "def named default not resolved: {got}"
        );
    }

    #[test]
    fn test_issue6732_dependencies_name_map_keeps_schema_semantics() {
        let input = json!({
            "type": "object",
            "dependencies": {
                "default": {"properties": {"x": {"$ref": "#/definitions/S"}}}
            },
            "definitions": {"S": {"type": "string"}}
        });
        let got = inline_local_refs(&input).unwrap();
        assert_eq!(
            got["dependencies"]["default"]["properties"]["x"]["type"], "string",
            "{got}"
        );
        assert!(
            got["dependencies"]["default"]["properties"]["x"]
                .get("$ref")
                .is_none(),
            "{got}"
        );
    }

    #[test]
    fn test_issue6732_target_nested_definitions_exempt_from_budget() {
        let big = "x".repeat(280 * 1024);
        let person = json!({
            "type": "object",
            "properties": {"name": {"type": "string"}},
            "$defs": {"Unused": {"type": "string", "description": big}}
        });
        let property_ref = json!({
            "type": "object",
            "properties": {"person": {"$ref": "#/$defs/Person"}},
            "$defs": {"Person": person.clone()}
        });
        let root_ref = json!({
            "$ref": "#/$defs/Person",
            "$defs": {"Person": person}
        });
        for (name, input) in [("propertyRef", property_ref), ("rootRef", root_ref)] {
            let out = clean_json_schema_for_gemini(&input);
            let name_path = if name == "rootRef" {
                &out["properties"]["name"]["type"]
            } else {
                &out["properties"]["person"]["properties"]["name"]["type"]
            };
            assert_eq!(name_path, "string", "{name}: {out}");
            assert!(out.get("$defs").is_none(), "{name}: {out}");
        }
    }

    #[test]
    fn test_issue6732_budget_fallback_still_resolves_siblings() {
        let big = "x".repeat(280 * 1024);
        let input = json!({
            "type": "object",
            "properties": {
                "a": {
                    "$ref": "#/$defs/Big",
                    "properties": {"x": {"$ref": "#/$defs/S"}}
                }
            },
            "$defs": {
                "Big": {"type": "object", "description": big},
                "S": {"type": "string"}
            }
        });
        let out = clean_json_schema_for_gemini(&input);
        assert_eq!(
            out["properties"]["a"]["properties"]["x"]["type"], "string",
            "{out}"
        );
        assert!(!out.to_string().contains("\"$ref\""), "{out}");
    }

    #[test]
    fn test_issue6732_ref_sibling_name_map_references_still_expand() {
        let input = json!({
            "type": "object",
            "properties": {
                "a": {
                    "$ref": "#/$defs/Base",
                    "properties": {"x": {"$ref": "#/$defs/S"}}
                }
            },
            "$defs": {
                "Base": {"type": "object"},
                "S": {"type": "string"}
            }
        });
        let out = clean_json_schema_for_gemini(&input);
        assert_eq!(
            out["properties"]["a"]["properties"]["x"]["type"], "string",
            "{out}"
        );
    }

    #[test]
    fn test_issue6732_small_target_fan_out_stays_bounded() {
        let mut props = serde_json::Map::new();
        for i in 0..2000 {
            props.insert(format!("p{i}"), json!({"$ref": "#/$defs/S"}));
        }
        let input = json!({
            "type": "object",
            "properties": props,
            "$defs": {
                "S": {"type": "string", "description": "x".repeat(512)}
            }
        });
        let out = clean_json_schema_for_gemini(&input);
        let n = out.to_string().len();
        assert!(n < 512 * 1024, "small-target fan-out = {n}");
    }

    /// 节点同时有 properties + type:array + items:object sanitizer 先跑后应剥 items
    /// (对齐 CPA c558d320 sanitizeObjectProperties 先于 sanitizeArrayItems)。
    #[test]
    fn test_c558d320_sanitize_object_before_array_strips_leftover_items() {
        let input = json!({
            "type": "object",
            "properties": {
                "weird": {
                    "type": "array",
                    "properties": {"name": {"type": "string"}},
                    "items": {"type": "string"}
                }
            }
        });
        let out = clean_json_schema_for_gemini(&input);
        let weird = &out["properties"]["weird"];
        assert_eq!(weird["type"], "object", "{out}");
        assert!(
            weird.get("items").is_none(),
            "leftover items after object sanitize: {out}"
        );
        assert_eq!(weird["properties"]["name"]["type"], "string", "{out}");
    }

    /// AG 路径保留 singular example;占位器不得递归进 example 塞 `_` / required。
    #[test]
    fn test_c558d320_ag_placeholder_skips_example_literal() {
        let input = json!({
            "type": "object",
            "properties": {
                "payload": {
                    "type": "object",
                    "properties": {"id": {"type": "string"}},
                    "required": ["id"],
                    "example": {
                        "type": "object",
                        "properties": {"demo": {"type": "string"}}
                    }
                }
            },
            "required": ["payload"]
        });
        let out = clean_json_schema_for_antigravity(&input);
        let example = &out["properties"]["payload"]["example"];
        assert!(
            example.get("_").is_none()
                && example
                    .get("required")
                    .and_then(|r| r.as_array())
                    .map(|a| !a.iter().any(|v| v == "_"))
                    .unwrap_or(true),
            "placeholder injected into example literal: {out}"
        );
        // example 对象本身若被当 schema 清洗,还会被塞 properties.reason;二者都不应出现
        assert!(
            example.pointer("/properties/_").is_none(),
            "underscore inside example.properties: {out}"
        );
        assert_eq!(
            example["properties"]["demo"]["type"], "string",
            "example body should stay intact: {out}"
        );
    }

    /// AG 占位:带 anyOf 的 object union 节点不得再注入 `_`/`reason`(对齐 CPA c558d320)。
    #[test]
    fn test_c558d320_ag_placeholder_skips_anyof_union_node() {
        let input = json!({
            "type": "object",
            "properties": {
                "choice": {
                    "type": "object",
                    "anyOf": [
                        {
                            "type": "object",
                            "properties": {"a": {"type": "string"}},
                            "required": ["a"]
                        },
                        {
                            "type": "object",
                            "properties": {"b": {"type": "integer"}},
                            "required": ["b"]
                        }
                    ],
                    "properties": {
                        "reason": {"type": "string"}
                    }
                }
            },
            "required": ["choice"]
        });
        let out = clean_json_schema_for_antigravity(&input);
        let choice = &out["properties"]["choice"];
        assert!(
            choice.get("anyOf").is_some(),
            "AG Tool 应保留多分支 anyOf: {out}"
        );
        assert!(
            choice.pointer("/properties/_").is_none(),
            "union 节点不应注入 properties._: {out}"
        );
        assert!(
            choice
                .get("required")
                .and_then(|r| r.as_array())
                .map(|a| !a.iter().any(|v| v == "_"))
                .unwrap_or(true),
            "union 节点不应注入 required:[_]: {out}"
        );
        // 原有 reason 属性保留;不得被占位 reason-only properties 整表替换
        assert_eq!(
            choice["properties"]["reason"]["type"], "string",
            "原有 reason 属性应保留: {out}"
        );
        assert_ne!(
            choice.get("required"),
            Some(&json!(["reason"])),
            "不应把 union 节点改成占位 required:[reason]: {out}"
        );
    }

    /// Gemini 保留约束:default 内 anyOf 不得被 flatten。
    #[test]
    fn test_c558d320_gemini_flatten_skips_default_literal_anyof() {
        let input = json!({
            "type": "object",
            "properties": {
                "report": {
                    "type": "object",
                    "default": {"anyOf": [{"type": "string"}]}
                }
            }
        });
        let out = clean_json_schema_for_gemini(&input);
        let def = &out["properties"]["report"]["default"];
        assert!(
            def.get("anyOf").is_some(),
            "default.anyOf was flattened away: {out}"
        );
        assert_eq!(def["anyOf"][0]["type"], "string", "{out}");
        assert!(
            def.get("type").is_none() || def["type"] != "string",
            "default should not be projected to type:string: {out}"
        );
    }

    /// properties 下名为 $ref 且值为 string 的畸形 schema 不得转 hint。
    #[test]
    fn test_c558d320_property_named_ref_string_value_not_hinted() {
        let input = json!({
            "type": "object",
            "properties": {
                "$ref": "https://example.com/schemas/thing.json"
            }
        });
        let out = clean_json_schema_for_gemini(&input);
        // normalize 可能把 string property 包一层;关键是不出现 See: hint
        assert!(!out.to_string().contains("See:"), "{out}");
        assert!(
            out["properties"].get("$ref").is_some(),
            "property named $ref disappeared: {out}"
        );
    }

    /// $ref 键存在但值为 null 时仍允许补 items(对齐 CPA clone[$ref]==nil)。
    #[test]
    fn test_c558d320_null_ref_still_allows_array_items_placeholder() {
        let input = json!({
            "type": "object",
            "properties": {
                "rows": {
                    "type": "array",
                    "$ref": null
                }
            }
        });
        let out = clean_json_schema_for_gemini(&input);
        assert_eq!(
            out["properties"]["rows"]["items"]["type"], "string",
            "null $ref should not block items placeholder: {out}"
        );
    }

    #[test]
    fn test_issue6732_clean_inlining_strips_definitions_from_intermediate() {
        let person = json!({
            "type": "object",
            "properties": {"name": {"type": "string"}},
            "$defs": {
                "Unused": {
                    "type": "string",
                    "description": "x".repeat(280 * 1024)
                }
            }
        });
        let input = json!({
            "type": "object",
            "properties": {
                "p0": {"$ref": "#/$defs/Person"},
                "p1": {"$ref": "#/$defs/Person"}
            },
            "$defs": {"Person": person}
        });
        let got = inline_local_refs_for_clean(&input);
        let n = got.to_string().len();
        assert!(n < 64 * 1024, "clean inlining intermediate = {n}");
        assert!(got.get("$defs").is_none(), "{got}");
        assert!(got.pointer("/properties/p0/$defs").is_none(), "{got}");
        assert_eq!(
            got["properties"]["p1"]["properties"]["name"]["type"], "string",
            "{got}"
        );
    }

    #[test]
    fn test_issue6732_omitted_fallback_description_not_charged() {
        let input = json!({
            "type": "object",
            "properties": {
                "a": {"$ref": "#/$defs/A"},
                "b": {"$ref": "#/$defs/B"}
            },
            "$defs": {
                "A": {
                    "type": "string",
                    "description": "x".repeat(262142)
                },
                "B": {
                    "type": "object",
                    "properties": {"name": {"type": "string"}}
                }
            }
        });
        let out = clean_json_schema_for_gemini(&input);
        assert_eq!(
            out["properties"]["b"]["properties"]["name"]["type"], "string",
            "{out}"
        );
    }

    #[test]
    fn test_issue6732_escaped_description_fan_out_stays_bounded() {
        let mut props = serde_json::Map::new();
        for i in 0..8 {
            props.insert(format!("p{i}"), json!({"$ref": "#/$defs/Big"}));
        }
        let input = json!({
            "type": "object",
            "properties": props,
            "$defs": {
                "Big": {
                    "type": "object",
                    "description": "<".repeat(32 * 1024),
                    "properties": {"name": {"type": "string"}}
                }
            }
        });
        let out = clean_json_schema_for_gemini(&input);
        let n = out.to_string().len();
        assert!(n < 512 * 1024, "escaped-description fan-out = {n}");
    }

    #[test]
    fn test_issue6732_nested_unused_definitions_graph_does_not_consume_budget() {
        let mut nested_defs = serde_json::Map::new();
        nested_defs.insert(
            "Leaf".to_string(),
            json!({
                "type": "string",
                "description": "x".repeat(1024)
            }),
        );
        for i in 0..256 {
            nested_defs.insert(
                format!("U{i}"),
                json!({"$ref": "#/properties/a/$defs/Leaf"}),
            );
        }
        let input = json!({
            "type": "object",
            "properties": {
                "person": {"$ref": "#/$defs/Person"},
                "a": {
                    "type": "object",
                    "$defs": nested_defs
                }
            },
            "required": ["person"],
            "$defs": {
                "Person": {
                    "type": "object",
                    "properties": {"name": {"type": "string"}},
                    "required": ["name"]
                }
            }
        });
        let out = clean_json_schema_for_gemini(&input);
        assert_eq!(
            out["properties"]["person"]["properties"]["name"]["type"], "string",
            "{out}"
        );
        assert!(out.get("$defs").is_none(), "{out}");
        assert!(out.pointer("/properties/a/$defs").is_none(), "{out}");
    }
}
