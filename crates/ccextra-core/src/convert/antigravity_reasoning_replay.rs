//! Antigravity / Gemini thoughtSignature reasoning replay（对齐 CPA
//! `antigravity_reasoning_replay.go`，含缺失 functionCall 插回 G5）。
//!
//! - 缓存条目形状：`thought_signature` / `function_call_part`，均带 `contextHash`
//! - 注入：在已有 `request.contents` part 上补 `thoughtSignature`；稳定 tool_use id
//!   （`ccx_gemini_…`）据账本换回原生 `functionCall.id` / `functionResponse.id/name`
//! - 兜底：找不回原生 id → 合成 id；首个 functionCall 补哨兵；回放破坏配对 → 退回原始请求
//! - 累积：从响应 `candidates[0].content.parts` 收集可回放项；仅 terminal（有 finishReason）才可提交

use serde_json::{json, Value};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

use super::antigravity_tools::antigravity_upstream_tool_name_to_client;
use super::signature::GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR;
use super::tool_id::{
    canonical_json, gemini_claude_tool_use_id, is_gemini_claude_tool_use_id, write_canonical,
};

/// 对齐 CPA `minAntigravityThoughtSignatureReplayLen`
pub const MIN_THOUGHT_SIGNATURE_LEN: usize = 16;

/// 单 entry 条目上限（对齐 CPA MaxItemsPerEntry）
pub const MAX_ITEMS_PER_ENTRY: usize = 4096;
/// 单 entry 字节上限（对齐 CPA MaxBytesPerEntry）
pub const MAX_BYTES_PER_ENTRY: usize = 16 << 20;

/// 是否为可入库的 native thoughtSignature（非空且非 bypass 哨兵）。
pub fn has_native_thought_signature(signature: &str) -> bool {
    let signature = signature.trim();
    !signature.is_empty() && signature != GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR
}

/// part 上的签名（对齐 CPA antigravityNativePartThoughtSignature :2750-2757：依次取
/// thoughtSignature / thought_signature / extra_content.google.thought_signature 中首个非空）。
fn part_thought_signature(part: &Value) -> Option<&str> {
    [
        part.get("thoughtSignature"),
        part.get("thought_signature"),
        part.pointer("/extra_content/google/thought_signature"),
    ]
    .into_iter()
    .flatten()
    .filter_map(|v| v.as_str())
    .map(str::trim)
    .find(|s| !s.is_empty())
}

/// 删掉 part 上三处签名字段（对齐 CPA 删 thoughtSignature / thought_signature /
/// extra_content.google.thought_signature）。
fn strip_part_thought_signatures(part: &mut Value) {
    if let Some(obj) = part.as_object_mut() {
        obj.remove("thoughtSignature");
        obj.remove("thought_signature");
    }
    if let Some(google) = part
        .get_mut("extra_content")
        .and_then(|e| e.get_mut("google"))
        .and_then(|g| g.as_object_mut())
    {
        google.remove("thought_signature");
    }
}

/// 规范化单条 replay item；非法返回 None。
pub fn normalize_ag_replay_item(item: &Value) -> Option<Value> {
    let ty = item
        .get("type")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    match ty {
        "thought_signature" => normalize_thought_signature_item(item),
        "function_call_part" => normalize_function_call_part_item(item),
        _ => None,
    }
}

/// 批量规范化；空或全非法返回 None。
pub fn normalize_ag_replay_items(items: &[Value]) -> Option<Vec<Value>> {
    if items.len() > MAX_ITEMS_PER_ENTRY {
        return None;
    }
    let mut out = Vec::with_capacity(items.len());
    let mut total = 0usize;
    for item in items {
        let Some(normalized) = normalize_ag_replay_item(item) else {
            continue;
        };
        total = total.saturating_add(normalized.to_string().len());
        if total > MAX_BYTES_PER_ENTRY {
            return None;
        }
        out.push(normalized);
    }
    if out.is_empty() {
        None
    } else {
        Some(out)
    }
}

fn normalize_thought_signature_item(item: &Value) -> Option<Value> {
    let sig = item
        .get("thoughtSignature")
        .or_else(|| item.get("thought_signature"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    if sig.is_empty()
        || sig == GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR
        || sig.len() < MIN_THOUGHT_SIGNATURE_LEN
    {
        return None;
    }
    let mut out = json!({"type": "thought_signature", "thoughtSignature": sig});
    copy_optional_number(item, &mut out, "contentIndex");
    copy_optional_number(item, &mut out, "partIndex");
    if let Some(kind) = item.get("targetKind").and_then(|v| v.as_str()) {
        if kind == "text" || kind == "thought" {
            out["targetKind"] = json!(kind);
        }
    }
    if let Some(hash) = item
        .get("targetHash")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        out["targetHash"] = json!(hash);
    }
    copy_optional_number(item, &mut out, "targetOccurrence");
    if let Some(ch) = item
        .get("contextHash")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        out["contextHash"] = json!(ch);
    }
    Some(out)
}

fn normalize_function_call_part_item(item: &Value) -> Option<Value> {
    let mut call_id = item
        .get("call_id")
        .or_else(|| item.get("id"))
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let mut name = item
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim()
        .to_string();
    let mut args = item.get("args").cloned();
    if name.is_empty() || args.is_none() {
        if let Some(fc) = item.get("functionCall") {
            if call_id.is_empty() {
                call_id = fc
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string();
            }
            if name.is_empty() {
                name = fc
                    .get("name")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string();
            }
            if args.is_none() {
                args = fc.get("args").cloned();
            }
        }
    }
    let args = args?;
    if name.is_empty() {
        return None;
    }
    let mut out = json!({"type": "function_call_part", "name": name, "args": args});
    if !call_id.is_empty() {
        out["call_id"] = json!(call_id);
    }
    if let Some(sig) = item
        .get("thoughtSignature")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty() && *s != GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR)
    {
        out["thoughtSignature"] = json!(sig);
    }
    copy_optional_number(item, &mut out, "contentIndex");
    copy_optional_number(item, &mut out, "partIndex");
    copy_optional_number(item, &mut out, "targetOccurrence");
    if let Some(ch) = item
        .get("contextHash")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        out["contextHash"] = json!(ch);
    }
    Some(out)
}

fn copy_optional_number(src: &Value, dst: &mut Value, key: &str) {
    if let Some(n) = src.get(key).and_then(|v| v.as_i64()) {
        dst[key] = json!(n);
    }
}

fn part_fingerprint(part: &Value) -> Option<(String, String)> {
    if part.get("functionCall").is_some() || part.get("functionResponse").is_some() {
        return None;
    }
    let text = part.get("text")?.as_str()?;
    let kind = if part.get("thought").and_then(|v| v.as_bool()) == Some(true) {
        "thought"
    } else {
        "text"
    };
    let mut hasher = Sha256::new();
    hasher.update(kind.as_bytes());
    hasher.update([0u8]);
    hasher.update(text.as_bytes());
    Some((kind.to_string(), hex::encode(hasher.finalize())))
}

fn args_equal(a: &Value, b: &Value) -> bool {
    canonical_json(a) == canonical_json(b)
}

fn contents_mut(request: &mut Value) -> Option<&mut Vec<Value>> {
    request.get_mut("contents").and_then(|v| v.as_array_mut())
}

fn contents_ref(request: &Value) -> Option<&Vec<Value>> {
    request.get("contents").and_then(|v| v.as_array())
}

fn is_model_role(content: &Value) -> bool {
    content
        .get("role")
        .and_then(|v| v.as_str())
        .map(|r| r.trim().eq_ignore_ascii_case("model"))
        .unwrap_or(false)
}

fn content_parts(content: &Value) -> &[Value] {
    content
        .get("parts")
        .and_then(|v| v.as_array())
        .map(Vec::as_slice)
        .unwrap_or_default()
}

fn str_field<'a>(v: &'a Value, key: &str) -> &'a str {
    v.get(key).and_then(|v| v.as_str()).unwrap_or("").trim()
}

fn item_call_id(item: &Value) -> &str {
    let id = str_field(item, "call_id");
    if id.is_empty() {
        str_field(item, "id")
    } else {
        id
    }
}

/// 去掉签名字段后的 part 规范 JSON(对齐 CPA contextFingerprints 删
/// thoughtSignature / thought_signature / extra_content.google.thought_signature)。
fn canonical_part_without_signatures(part: &Value) -> String {
    let Some(map) = part.as_object() else {
        return canonical_json(part);
    };
    let mut keys: Vec<&String> = map
        .keys()
        .filter(|k| k.as_str() != "thoughtSignature" && k.as_str() != "thought_signature")
        .collect();
    keys.sort();
    let mut out = String::from("{");
    for (i, k) in keys.into_iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push_str(&serde_json::to_string(k).unwrap_or_default());
        out.push(':');
        let child = &map[k];
        if k == "extra_content" && child.pointer("/google/thought_signature").is_some() {
            let mut stripped = child.clone();
            if let Some(google) = stripped.get_mut("google").and_then(|g| g.as_object_mut()) {
                google.remove("thought_signature");
            }
            write_canonical(&stripped, &mut out);
        } else {
            write_canonical(child, &mut out);
        }
    }
    out.push('}');
    out
}

/// 请求上下文前缀指纹（对齐 CPA antigravityReplayContextFingerprints）：
/// systemInstruction / tools / toolConfig 规范 JSON，再逐条 content（role + 去签名 parts）
/// 增量 SHA-256；`at(k)` = contents[..k] 之前的前缀摘要。按内容边界缓存 hasher 状态，
/// 身份回写后只需从改动点截断重算。
pub(crate) struct ContextFingerprints {
    hashers: Vec<Sha256>,
    wrote: Vec<bool>,
    sums: Vec<String>,
}

impl ContextFingerprints {
    pub(crate) fn new(request: &Value) -> Self {
        let mut hasher = Sha256::new();
        let mut wrote = false;
        for key in ["systemInstruction", "tools", "toolConfig"] {
            if let Some(value) = request.get(key) {
                hasher.update(b"request.");
                hasher.update(key.as_bytes());
                hasher.update([0u8]);
                hasher.update(canonical_json(value).as_bytes());
                hasher.update([0u8]);
                wrote = true;
            }
        }
        let sum = if wrote {
            hex::encode(hasher.clone().finalize())
        } else {
            String::new()
        };
        Self {
            hashers: vec![hasher],
            wrote: vec![wrote],
            sums: vec![sum],
        }
    }

    /// contents[..before] 的前缀指纹；越界返回空串（对齐 CPA at()）。
    pub(crate) fn at(&mut self, contents: &[Value], before: usize) -> String {
        if before > contents.len() {
            return String::new();
        }
        while self.hashers.len() <= before {
            let ci = self.hashers.len() - 1;
            let mut hasher = self.hashers[ci].clone();
            let content = &contents[ci];
            let role = content
                .get("role")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_lowercase();
            hasher.update(role.as_bytes());
            hasher.update([0u8]);
            for part in content_parts(content) {
                hasher.update(canonical_part_without_signatures(part).as_bytes());
                hasher.update([0u8]);
            }
            // 写过角色分隔符即视为有字节（对齐 CPA writeByte 置 wroteBytes）
            self.sums.push(hex::encode(hasher.clone().finalize()));
            self.hashers.push(hasher);
            self.wrote.push(true);
        }
        if self.wrote[before] {
            self.sums[before].clone()
        } else {
            String::new()
        }
    }

    /// contents[ci] 起被改写：丢弃覆盖它的前缀缓存（at(k) 只依赖 contents[..k]）。
    fn invalidate_from(&mut self, ci: usize) {
        let keep = ci + 1;
        if self.hashers.len() > keep {
            self.hashers.truncate(keep);
            self.wrote.truncate(keep);
            self.sums.truncate(keep);
        }
    }
}

/// 对齐 CPA contextMatches：条目无 contextHash 视为匹配。
fn context_matches(
    fp: &mut ContextFingerprints,
    contents: &[Value],
    item: &Value,
    content_index: usize,
) -> bool {
    let expected = str_field(item, "contextHash");
    expected.is_empty() || expected == fp.at(contents, content_index)
}

fn locate_thought_signature_part(
    contents: &[Value],
    fp: &mut ContextFingerprints,
    item: &Value,
) -> Option<(usize, usize)> {
    let content_index = item.get("contentIndex").and_then(|v| v.as_u64())? as usize;
    if content_index >= contents.len() {
        return None;
    }
    let content = &contents[content_index];
    if !is_model_role(content) {
        return None;
    }
    let parts = content_parts(content);
    let target_kind = str_field(item, "targetKind");
    let target_hash = str_field(item, "targetHash");

    let mut part_index: Option<usize> = None;
    if !target_hash.is_empty() {
        // targetHash 已钉住 part 字节，签名只校验自身完整性 → 不看 contextHash
        // （对齐 CPA thoughtSignaturePartIndex :1085-1094）
        if let Some(wanted) = item.get("targetOccurrence").and_then(|v| v.as_i64()) {
            let mut occurrence = 0i64;
            for (idx, part) in parts.iter().enumerate() {
                let Some((kind, fingerprint)) = part_fingerprint(part) else {
                    continue;
                };
                if fingerprint != target_hash {
                    continue;
                }
                if !target_kind.is_empty() && kind != target_kind {
                    continue;
                }
                if occurrence == wanted {
                    part_index = Some(idx);
                    break;
                }
                occurrence += 1;
            }
        } else {
            if let Some(candidate) = item.get("partIndex").and_then(|v| v.as_u64()) {
                let candidate = candidate as usize;
                if candidate < parts.len() {
                    if let Some((kind, fingerprint)) = part_fingerprint(&parts[candidate]) {
                        if fingerprint == target_hash
                            && (target_kind.is_empty() || kind == target_kind)
                        {
                            part_index = Some(candidate);
                        }
                    }
                }
            }
            if part_index.is_none() {
                for (idx, part) in parts.iter().enumerate() {
                    let Some((kind, fingerprint)) = part_fingerprint(part) else {
                        continue;
                    };
                    if fingerprint == target_hash && (target_kind.is_empty() || kind == target_kind)
                    {
                        part_index = Some(idx);
                        break;
                    }
                }
            }
        }
    } else {
        // 无 targetHash：只有 contextHash 一致时按位置猜才安全（对齐 CPA :1141-1146）
        if !context_matches(fp, contents, item, content_index) {
            return None;
        }
        if let Some(candidate) = item.get("partIndex").and_then(|v| v.as_u64()) {
            let candidate = candidate as usize;
            if candidate < parts.len() && part_fingerprint(&parts[candidate]).is_some() {
                part_index = Some(candidate);
            }
        }
        if part_index.is_none() {
            for idx in (0..parts.len()).rev() {
                if part_fingerprint(&parts[idx]).is_some() {
                    part_index = Some(idx);
                    break;
                }
            }
        }
    }
    part_index.map(|pi| (content_index, pi))
}

fn function_call_at(contents: &[Value], ci: usize, pi: usize) -> Option<&Value> {
    contents
        .get(ci)
        .map(content_parts)
        .and_then(|parts| parts.get(pi))
        .and_then(|p| p.get("functionCall"))
}

/// 回放比对用的工具上下文：原始 Claude 请求（工具 schema 来源）+ 上游短名 → 原名映射。
/// 对齐 CPA `antigravityReplayToolSchemasFromRequests(opts.OriginalRequest, req.Payload)`
/// 的输入（仅 Claude 源格式才有）；缺省（全 None）即 CPA 的 nil toolSchemas。
#[derive(Debug, Default, Clone, Copy)]
pub struct AgReplayToolContext<'a> {
    pub original_request: Option<&'a Value>,
    pub short_to_original: Option<&'a HashMap<String, String>>,
}

/// 每次回放构建一次的工具表（CPA toolSchemas 的等价物）。
struct ReplayToolTable<'a> {
    ctx: AgReplayToolContext<'a>,
    /// 客户端工具名 → JSON schema（对齐 CPA：input_schema / parameters / parametersJsonSchema
    /// 取首个对象；同名先到先得）
    schemas: HashMap<String, Value>,
}

impl<'a> ReplayToolTable<'a> {
    fn new(ctx: AgReplayToolContext<'a>) -> Self {
        let mut schemas = HashMap::new();
        let tools = ctx
            .original_request
            .and_then(|r| r.get("tools"))
            .and_then(|t| t.as_array());
        for tool in tools.into_iter().flatten() {
            let mut candidates = vec![tool];
            if let Some(function) = tool.get("function") {
                candidates.push(function);
            }
            for candidate in candidates {
                let name = str_field(candidate, "name");
                if name.is_empty() {
                    continue;
                }
                let Some(schema) = ["input_schema", "parameters", "parametersJsonSchema"]
                    .iter()
                    .find_map(|k| candidate.get(*k).filter(|v| v.is_object()))
                else {
                    continue;
                };
                schemas
                    .entry(name.to_string())
                    .or_insert_with(|| schema.clone());
            }
        }
        Self { ctx, schemas }
    }

    /// 上游 functionCall 名 → 客户端工具名（与 gemini_response 响应侧还原同一规则）。
    fn client_name(&self, upstream: &str) -> String {
        let restored = antigravity_upstream_tool_name_to_client(upstream);
        self.ctx
            .short_to_original
            .and_then(|m| m.get(&restored))
            .cloned()
            .unwrap_or(restored)
    }

    /// 对齐 CPA `toolSchemas[name]`（CPA 以原名与上游 sanitized 名双键；这里按上游名、
    /// 再按还原后的客户端名查）。
    fn schema(&self, upstream: &str) -> Option<&Value> {
        self.schemas
            .get(upstream)
            .or_else(|| self.schemas.get(&self.client_name(upstream)))
    }
}

/// 对齐 CPA antigravityReplayJSONValue：字符串 args 解析其内容。
fn replay_json_value(value: &Value) -> Option<Value> {
    match value {
        Value::String(s) => {
            if s.trim().is_empty() {
                return None;
            }
            serde_json::from_str(s).ok()
        }
        other => Some(other.clone()),
    }
}

/// 对齐 CPA antigravityNormalizeReplayToolValue：去掉等于 schema `default` 的属性（递归
/// properties / items）。数值相等按 canonical_json（整数值浮点按整数，近似 Go DeepEqual
/// 对 float64 的语义）。
fn normalize_replay_tool_value(value: &Value, schema: Option<&Value>) -> Value {
    let schema_object = schema.and_then(|s| s.as_object());
    match value {
        Value::Object(map) => {
            let properties = schema_object
                .and_then(|s| s.get("properties"))
                .and_then(|p| p.as_object());
            let mut normalized = serde_json::Map::with_capacity(map.len());
            for (key, child) in map {
                let child_schema = properties.and_then(|p| p.get(key));
                let normalized_child = normalize_replay_tool_value(child, child_schema);
                if let Some(default_value) = child_schema
                    .filter(|s| s.is_object())
                    .and_then(|s| s.get("default"))
                {
                    let normalized_default =
                        normalize_replay_tool_value(default_value, child_schema);
                    if args_equal(&normalized_child, &normalized_default) {
                        continue;
                    }
                }
                normalized.insert(key.clone(), normalized_child);
            }
            Value::Object(normalized)
        }
        Value::Array(items) => {
            let item_schema = schema_object.and_then(|s| s.get("items"));
            Value::Array(
                items
                    .iter()
                    .map(|child| normalize_replay_tool_value(child, item_schema))
                    .collect(),
            )
        }
        other => other.clone(),
    }
}

/// 名称 + args 一致（对齐 CPA antigravityFunctionCallMatchesReplayItem：先规范 JSON 全等，
/// 有工具 schema 时再按 default 归一后比较）。
fn function_call_matches_item(fc: &Value, item: &Value, tools: &ReplayToolTable) -> bool {
    let name = str_field(item, "name");
    if name.is_empty() || str_field(fc, "name") != name {
        return false;
    }
    let (Some(current), Some(native)) = (fc.get("args"), item.get("args")) else {
        return false;
    };
    if args_equal(current, native) {
        return true;
    }
    let Some(schema) = tools.schema(name) else {
        return false;
    };
    let (Some(current_value), Some(native_value)) =
        (replay_json_value(current), replay_json_value(native))
    else {
        return false;
    };
    args_equal(
        &normalize_replay_tool_value(&current_value, Some(schema)),
        &normalize_replay_tool_value(&native_value, Some(schema)),
    )
}

/// 首个 `functionCall.id == id` 的位置（对齐 CPA functionCallsByID 取首个）。
fn first_function_call_with_id(contents: &[Value], id: &str) -> Option<(usize, usize)> {
    if id.is_empty() {
        return None;
    }
    for (ci, content) in contents.iter().enumerate() {
        for (pi, part) in content_parts(content).iter().enumerate() {
            if let Some(fc) = part.get("functionCall") {
                if str_field(fc, "id") == id {
                    return Some((ci, pi));
                }
            }
        }
    }
    None
}

/// 对齐 CPA functionCallPartLocationForReplayWithSchemas。
fn locate_function_call_part(
    contents: &[Value],
    fp: &mut ContextFingerprints,
    item: &Value,
    stable_id: &str,
    tools: &ReplayToolTable,
) -> Option<(usize, usize)> {
    let name = str_field(item, "name");
    item.get("args")?;
    if name.is_empty() {
        return None;
    }
    let call_id = item_call_id(item);

    // 1. 按原生 id / 稳定 id 精确定位：上下文一致才算命中；上下文漂移交给 provenance 路径
    let mut candidates = vec![call_id];
    if !stable_id.is_empty() && stable_id != call_id {
        candidates.push(stable_id);
    }
    for candidate in candidates {
        if candidate.is_empty() {
            continue;
        }
        let Some((ci, pi)) = first_function_call_with_id(contents, candidate) else {
            continue;
        };
        if !context_matches(fp, contents, item, ci) {
            return None;
        }
        let fc = function_call_at(contents, ci, pi)?;
        return function_call_matches_item(fc, item, tools).then_some((ci, pi));
    }

    let skip_part = |fc: &Value| {
        let id = str_field(fc, "id");
        is_gemini_claude_tool_use_id(id) && id != stable_id
    };

    // 2. 带 targetOccurrence：在缓存 contentIndex 内按同名同参出现次序定位
    if let Some(wanted) = item.get("targetOccurrence").and_then(|v| v.as_i64()) {
        let ci = item.get("contentIndex").and_then(|v| v.as_u64())? as usize;
        if ci >= contents.len() || !context_matches(fp, contents, item, ci) {
            return None;
        }
        let mut occurrence = 0i64;
        for (pi, part) in content_parts(&contents[ci]).iter().enumerate() {
            let Some(fc) = part.get("functionCall") else {
                continue;
            };
            if skip_part(fc) || !function_call_matches_item(fc, item, tools) {
                continue;
            }
            if occurrence == wanted {
                return Some((ci, pi));
            }
            occurrence += 1;
        }
        return None;
    }

    // 3. 全局扫描：仅上下文一致的 content，且恰好一个候选（对齐 CPA len(matches) == 1）
    let mut found: Option<(usize, usize)> = None;
    for ci in 0..contents.len() {
        if !context_matches(fp, contents, item, ci) {
            continue;
        }
        for (pi, part) in content_parts(&contents[ci]).iter().enumerate() {
            let Some(fc) = part.get("functionCall") else {
                continue;
            };
            if skip_part(fc) || !function_call_matches_item(fc, item, tools) {
                continue;
            }
            if found.is_some() {
                return None;
            }
            found = Some((ci, pi));
        }
    }
    found
}

/// 稳定 id 精确命中但上下文漂移：身份已由 id 证明，仍可回写（对齐 CPA
/// functionCallProvenanceLocation + :2156-2162）。
fn function_call_provenance_location(
    contents: &[Value],
    item: &Value,
    stable_id: &str,
    tools: &ReplayToolTable,
) -> Option<(usize, usize)> {
    if stable_id.is_empty() || stable_id == item_call_id(item) {
        return None;
    }
    let (ci, pi) = first_function_call_with_id(contents, stable_id)?;
    let fc = function_call_at(contents, ci, pi)?;
    function_call_matches_item(fc, item, tools).then_some((ci, pi))
}

fn remove_thought_signature_from_other_parts(
    contents: &mut [Value],
    content_index: usize,
    signature: &str,
    keep_part: usize,
) {
    let Some(parts) = contents
        .get_mut(content_index)
        .and_then(|c| c.get_mut("parts"))
        .and_then(|v| v.as_array_mut())
    else {
        return;
    };
    for (idx, part) in parts.iter_mut().enumerate() {
        if idx == keep_part {
            continue;
        }
        if part_thought_signature(part) == Some(signature) {
            strip_part_thought_signatures(part);
        }
    }
}

fn part_mut(contents: &mut [Value], ci: usize, pi: usize) -> Option<&mut Value> {
    contents
        .get_mut(ci)
        .and_then(|c| c.get_mut("parts"))
        .and_then(|p| p.as_array_mut())
        .and_then(|arr| arr.get_mut(pi))
}

fn set_part_thought_signature(part: &mut Value, signature: &str) -> bool {
    if let Some(existing) = part_thought_signature(part) {
        if has_native_thought_signature(existing) {
            return false;
        }
    }
    if let Some(obj) = part.as_object_mut() {
        obj.insert("thoughtSignature".into(), json!(signature));
        obj.remove("thought_signature");
        true
    } else {
        false
    }
}

/// 写签名到 (ci, pi)：已有 native 签名跳过；同回合其它 part 上的同一签名先摘掉。
fn apply_signature_at(contents: &mut [Value], ci: usize, pi: usize, signature: &str) -> bool {
    if signature.is_empty() {
        return false;
    }
    let already_native = contents
        .get(ci)
        .map(content_parts)
        .and_then(|parts| parts.get(pi))
        .and_then(part_thought_signature)
        .map(has_native_thought_signature)
        .unwrap_or(false);
    if already_native {
        return false;
    }
    remove_thought_signature_from_other_parts(contents, ci, signature, pi);
    part_mut(contents, ci, pi)
        .map(|part| set_part_thought_signature(part, signature))
        .unwrap_or(false)
}

/// 身份回写分支的签名写法（对齐 CPA restoreAntigravityNativeFunctionCallReplay :2114-2118）：
/// 先无条件删掉 part 上的 thoughtSignature / thought_signature /
/// extra_content.google.thought_signature（含已有 native 签名），账本签名非空时再摘掉同回合
/// 其它 part 上的同一签名并写入。返回 part 是否变化。
fn overwrite_signature_at(contents: &mut [Value], ci: usize, pi: usize, signature: &str) -> bool {
    let Some(before) = part_mut(contents, ci, pi).map(|p| p.clone()) else {
        return false;
    };
    if let Some(part) = part_mut(contents, ci, pi) {
        strip_part_thought_signatures(part);
    }
    if !signature.is_empty() {
        remove_thought_signature_from_other_parts(contents, ci, signature, pi);
        if let Some(obj) = part_mut(contents, ci, pi).and_then(|p| p.as_object_mut()) {
            obj.insert("thoughtSignature".into(), json!(signature));
        }
    }
    part_mut(contents, ci, pi).is_some_and(|p| *p != before)
}

/// 对齐 CPA antigravityFunctionResponsesCanRestoreID。
fn function_responses_can_restore_id(
    contents: &[Value],
    current_id: &str,
    native_name: &str,
) -> bool {
    if current_id.is_empty() {
        return true;
    }
    contents.iter().all(|content| {
        content_parts(content).iter().all(|part| {
            let Some(fr) = part.get("functionResponse") else {
                return true;
            };
            if str_field(fr, "id") != current_id {
                return true;
            }
            let name = str_field(fr, "name");
            name.is_empty() || name == "unknown" || name == native_name
        })
    })
}

enum FunctionCallReplay {
    Unchanged,
    Signature,
    /// 身份（id/name/args）被改写；值为最早改动的 content 下标
    Identity(usize),
}

/// 回写原生 functionCall 身份与签名（对齐 CPA restoreAntigravityNativeFunctionCallReplay）。
///
/// `allow_legacy_id_restore`（对齐 CPA :2153）：精确/按序/全局定位命中且该工具名在
/// 请求工具表（toolSchemas）里时为 true——旧式 `{name}-{counter}` 等非原生 id 也换回原生
/// id（连同 functionResponse）；工具表里没有该名、或走 provenance 路径时为 false，只补签名。
fn restore_native_function_call(
    contents: &mut [Value],
    ci: usize,
    pi: usize,
    item: &Value,
    allow_legacy_id_restore: bool,
) -> FunctionCallReplay {
    let Some(current) = function_call_at(contents, ci, pi) else {
        return FunctionCallReplay::Unchanged;
    };
    let current_id = str_field(current, "id").to_string();
    let current_canonical = canonical_json(current);
    let native_id = item_call_id(item).to_string();
    let native_name = str_field(item, "name").to_string();
    let signature = str_field(item, "thoughtSignature").to_string();
    let Some(args) = item.get("args").cloned() else {
        return FunctionCallReplay::Unchanged;
    };
    // 对齐 CPA antigravityNativeFunctionCallJSON：字符串 args 能解析成 JSON 就写回解析值，
    // 否则保留原字符串（CPA 用宽松的 gjson.Parse，畸形 JSON 时会写出非法 raw；这里严格解析，
    // 失败一律保留字符串）。
    let args = match args {
        Value::String(s) => serde_json::from_str::<Value>(&s).unwrap_or(Value::String(s)),
        other => other,
    };

    let restore_identity = current_id == native_id
        || is_gemini_claude_tool_use_id(&current_id)
        || allow_legacy_id_restore;
    if !restore_identity {
        return if apply_signature_at(contents, ci, pi, &signature) {
            FunctionCallReplay::Signature
        } else {
            FunctionCallReplay::Unchanged
        };
    }
    if current_id != native_id
        && !function_responses_can_restore_id(contents, &current_id, &native_name)
    {
        return FunctionCallReplay::Unchanged;
    }
    let mut native_call = json!({"name": native_name});
    let id = if native_id.is_empty() {
        current_id.as_str()
    } else {
        native_id.as_str()
    };
    if !id.is_empty() {
        native_call["id"] = json!(id);
    }
    native_call["args"] = args;

    let mut first_changed: Option<usize> = None;
    if canonical_json(&native_call) != current_canonical {
        if let Some(part) = part_mut(contents, ci, pi) {
            part["functionCall"] = native_call;
            first_changed = Some(ci);
        }
    }
    let signed = overwrite_signature_at(contents, ci, pi, &signature);
    if !current_id.is_empty() && !native_id.is_empty() && current_id != native_id {
        for (rci, content) in contents.iter_mut().enumerate() {
            let Some(parts) = content.get_mut("parts").and_then(|p| p.as_array_mut()) else {
                continue;
            };
            for part in parts.iter_mut() {
                let Some(fr) = part
                    .get_mut("functionResponse")
                    .and_then(|v| v.as_object_mut())
                else {
                    continue;
                };
                if fr.get("id").and_then(|v| v.as_str()).unwrap_or("").trim() != current_id {
                    continue;
                }
                fr.insert("id".into(), json!(native_id));
                fr.insert("name".into(), json!(native_name));
                first_changed = Some(first_changed.map_or(rci, |c| c.min(rci)));
            }
        }
    }
    match first_changed {
        Some(min_ci) => FunctionCallReplay::Identity(min_ci),
        None if signed => FunctionCallReplay::Signature,
        None => FunctionCallReplay::Unchanged,
    }
}

/// CPA filterAntigravityReasoningReplayItemsForRequestWithIndex :812-821 的 no-op 判定：
/// `needsNativeRestore = currentID != nativeID || canonical(args) 不同`；不需回写且
/// （账本签名为空 || part.thoughtSignature 已是 native）→ 该条不合格。
fn function_call_replay_is_noop(contents: &[Value], ci: usize, pi: usize, item: &Value) -> bool {
    let Some(part) = contents.get(ci).map(content_parts).and_then(|p| p.get(pi)) else {
        return false;
    };
    let Some(fc) = part.get("functionCall") else {
        return false;
    };
    let same_args = match (fc.get("args"), item.get("args")) {
        (Some(current), Some(native)) => args_equal(current, native),
        (None, None) => true,
        _ => false,
    };
    let needs_native_restore = str_field(fc, "id") != item_call_id(item) || !same_args;
    if needs_native_restore {
        return false;
    }
    str_field(item, "thoughtSignature").is_empty()
        || has_native_thought_signature(str_field(part, "thoughtSignature"))
}

/// 首个 `functionResponse.id == id` 所在 content 下标（对齐 CPA functionResponseContentByID
/// 取首个）。
fn first_function_response_content_with_id(contents: &[Value], id: &str) -> Option<usize> {
    if id.is_empty() {
        return None;
    }
    contents.iter().position(|content| {
        content_parts(content).iter().any(|part| {
            part.get("functionResponse")
                .is_some_and(|fr| str_field(fr, "id") == id)
        })
    })
}

/// 构造插回的 functionCall part（对齐 CPA insert/append 辅助：`args.Value()` 原样写回，
/// 字符串 args 不解析）。`thought_sig` 为空则不写签名。
fn inserted_function_call_part(
    name: &str,
    call_id: &str,
    args: &Value,
    thought_sig: &str,
) -> Value {
    let mut fc = json!({"name": name});
    if !call_id.is_empty() {
        fc["id"] = json!(call_id);
    }
    fc["args"] = args.clone();
    let mut part = json!({"functionCall": fc});
    if !thought_sig.is_empty() {
        part["thoughtSignature"] = json!(thought_sig);
    }
    part
}

/// 对齐 CPA restoreAntigravityFunctionResponseReplayIdentity :920-943：插回后把
/// `functionResponse.id == current_id` 的全部 response 改成原生 id / name。
fn restore_function_response_identity(
    contents: &mut [Value],
    current_id: &str,
    native_id: &str,
    native_name: &str,
) {
    if current_id.is_empty()
        || native_id.is_empty()
        || native_name.is_empty()
        || current_id == native_id
    {
        return;
    }
    for content in contents.iter_mut() {
        let Some(parts) = content.get_mut("parts").and_then(|p| p.as_array_mut()) else {
            continue;
        };
        for part in parts.iter_mut() {
            let Some(fr) = part
                .get_mut("functionResponse")
                .and_then(|v| v.as_object_mut())
            else {
                continue;
            };
            if fr.get("id").and_then(|v| v.as_str()).unwrap_or("").trim() != current_id {
                continue;
            }
            fr.insert("id".into(), json!(native_id));
            fr.insert("name".into(), json!(native_name));
        }
    }
}

/// G5：历史里缺失的 functionCall 插回（对齐 CPA filter :827-848 的准入 +
/// mergeAntigravityFunctionCallPartReplayWithSchemas :2163-2195）。仅当定位 / provenance 都
/// 未命中时调用。准入：账本有原生 call_id；原生 id 与稳定 id 都不在任何 functionCall 上
/// （在 = 客户端改过该 call，绝不插第二份）；能按 [原生 id, 稳定 id] 找到 functionResponse；
/// 且 FR 所在下标的 contextHash 一致，或 FR 前一个 content 是 model 且其下标 contextHash 一致。
/// 插法：前一个 model 回合一致 → 追加到其 parts 末尾（签名空且该回合尚无 functionCall 时补
/// 哨兵）；否则 FR 下标一致 → 在 FR 前插一个只含该 call 的 model 回合（签名空补哨兵）。
/// 成功后把 FR 的 id / name 换回原生。返回最早改动的 content 下标。
fn insert_missing_function_call(
    contents: &mut Vec<Value>,
    fp: &mut ContextFingerprints,
    item: &Value,
    stable_id: &str,
) -> Option<usize> {
    let name = str_field(item, "name").to_string();
    let args = item.get("args")?.clone();
    let call_id = item_call_id(item).to_string();
    // CPA :2196-2200：无原生 call_id 只接受精确语义命中，不插回
    if name.is_empty() || call_id.is_empty() {
        return None;
    }
    if first_function_call_with_id(contents, &call_id).is_some()
        || first_function_call_with_id(contents, stable_id).is_some()
    {
        return None;
    }
    let mut candidates = vec![call_id.clone()];
    if !stable_id.is_empty() && stable_id != call_id {
        candidates.push(stable_id.to_string());
    }
    let (fr_index, response_id) = candidates.into_iter().find_map(|candidate| {
        first_function_response_content_with_id(contents, &candidate).map(|ci| (ci, candidate))
    })?;
    let sig = str_field(item, "thoughtSignature").to_string();

    // 1. 并行 call：FR 前一个 model 回合上下文一致 → 追加（CPA :2179-2184 / :1228）
    if fr_index > 0
        && is_model_role(&contents[fr_index - 1])
        && context_matches(fp, contents, item, fr_index - 1)
    {
        let model_index = fr_index - 1;
        if let Some(parts) = contents[model_index]
            .get_mut("parts")
            .and_then(|p| p.as_array_mut())
        {
            let thought_sig =
                if sig.is_empty() && !parts.iter().any(|p| p.get("functionCall").is_some()) {
                    GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR.to_string()
                } else {
                    sig.clone()
                };
            parts.push(inserted_function_call_part(
                &name,
                &call_id,
                &args,
                &thought_sig,
            ));
            restore_function_response_identity(contents, &response_id, &call_id, &name);
            tracing::debug!(
                call_id = %call_id,
                content_index = model_index,
                "AG replay: 缺失 functionCall 追加回前一 model 回合"
            );
            return Some(model_index);
        }
    }
    // 2. FR 下标上下文一致 → 在 FR 前插 model 回合（CPA :2185-2189 / :1188）
    if context_matches(fp, contents, item, fr_index) {
        let thought_sig = if sig.is_empty() {
            GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR.to_string()
        } else {
            sig
        };
        contents.insert(
            fr_index,
            json!({
                "role": "model",
                "parts": [inserted_function_call_part(&name, &call_id, &args, &thought_sig)],
            }),
        );
        restore_function_response_identity(contents, &response_id, &call_id, &name);
        tracing::debug!(
            call_id = %call_id,
            content_index = fr_index,
            "AG replay: 缺失 functionCall 插回为 FR 前的 model 回合"
        );
        return Some(fr_index);
    }
    // CPA 之后还有按缓存 contentIndex/partIndex 的位置写入兜底；准入（filter）已要求上面
    // 两个上下文之一一致，故只在前一 model 回合缺 parts 数组时可达，这里不移植。
    None
}

fn apply_function_call_item(
    contents: &mut Vec<Value>,
    fp: &mut ContextFingerprints,
    item: &Value,
    tools: &ReplayToolTable,
) -> FunctionCallReplay {
    let name = str_field(item, "name");
    let stable_id = gemini_claude_tool_use_id(item_call_id(item), name, item.get("args"));
    if let Some((ci, pi)) = locate_function_call_part(contents, fp, item, &stable_id, tools) {
        // 对齐 CPA eligibility filter :812-821（顺序路径不合格即跳过，:414-416；批量路径
        // sameNativeCall 同条件 no-op，:613-616）：id 与 args 都已是原生，且账本无签名或 part
        // 已带 native 签名 → 不动
        if function_call_replay_is_noop(contents, ci, pi, item) {
            return FunctionCallReplay::Unchanged;
        }
        // 对齐 CPA :2153 `_, allowLegacyIDRestore := toolSchemas[name]`
        let allow_legacy = tools.schema(name).is_some();
        return restore_native_function_call(contents, ci, pi, item, allow_legacy);
    }
    // provenance 路径：CPA :2161 传 allowLegacyIDRestore=false
    if let Some((ci, pi)) = function_call_provenance_location(contents, item, &stable_id, tools) {
        return restore_native_function_call(contents, ci, pi, item, false);
    }
    // G5 缺失 functionCall 插回（CPA :2163-2195）
    match insert_missing_function_call(contents, fp, item, &stable_id) {
        Some(min_ci) => FunctionCallReplay::Identity(min_ci),
        None => FunctionCallReplay::Unchanged,
    }
}

/// 将缓存 items 注入到 Antigravity `request` 对象（含 `contents`）。
/// 逐条顺序回放（对齐 CPA sequential 路径：逐条 eligibility filter，不合格即跳过）：签名回填 +
/// 稳定 id → 原生 id 回写 + 缺失 functionCall 插回（G5）。返回是否发生变更。
pub fn apply_ag_replay_items(request: &mut Value, items: &[Value]) -> bool {
    apply_ag_replay_items_with_tools(request, items, AgReplayToolContext::default())
}

/// 同 [`apply_ag_replay_items`]，带工具上下文（schema default 归一 / 旧式 id 身份回写）。
pub fn apply_ag_replay_items_with_tools(
    request: &mut Value,
    items: &[Value],
    tools: AgReplayToolContext,
) -> bool {
    let tools = ReplayToolTable::new(tools);
    let Some(normalized) = normalize_ag_replay_items(items) else {
        return false;
    };
    if contents_ref(request).is_none() {
        return false;
    }
    let mut fp = ContextFingerprints::new(request);
    let Some(contents) = contents_mut(request) else {
        return false;
    };
    let mut changed = false;
    for item in &normalized {
        match item.get("type").and_then(|v| v.as_str()).unwrap_or("") {
            "thought_signature" => {
                let sig = str_field(item, "thoughtSignature");
                if sig.is_empty() {
                    continue;
                }
                let Some((ci, pi)) = locate_thought_signature_part(contents, &mut fp, item) else {
                    continue;
                };
                if apply_signature_at(contents, ci, pi, sig) {
                    changed = true;
                }
            }
            "function_call_part" => match apply_function_call_item(contents, &mut fp, item, &tools)
            {
                FunctionCallReplay::Unchanged => {}
                FunctionCallReplay::Signature => changed = true,
                FunctionCallReplay::Identity(min_ci) => {
                    // 身份回写改变上下文指纹：后续条目按改写后的 payload 重算（对齐 CPA 重建 index）
                    fp.invalidate_from(min_ci);
                    changed = true;
                }
            },
            _ => {}
        }
    }
    changed
}

// ---- G6 兜底（对齐 CPA prepareAntigravityGeminiReasoningReplayPayload）----

/// 降级 id 前缀（对齐 CPA antigravitySyntheticToolCallID 的 `call_` + sha256 前 6 字节）。
fn synthetic_tool_call_id(reserved_id: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(b"antigravity-degraded-tool-call\0");
    hasher.update(reserved_id.as_bytes());
    format!("call_{}", hex::encode(&hasher.finalize()[..6]))
}

/// 账本找不回原生 id 的稳定 id → 确定性合成 id；被降级 call 上的 native 签名换哨兵
/// （回合首个）或删除（其余）。对齐 CPA degradeAntigravityClaudeToolProvenanceIDs。
pub fn degrade_unresolved_tool_provenance_ids(request: &mut Value) -> usize {
    let Some(contents) = contents_mut(request) else {
        return 0;
    };
    let mut count = 0usize;
    for content in contents.iter_mut() {
        let Some(parts) = content.get_mut("parts").and_then(|p| p.as_array_mut()) else {
            continue;
        };
        let mut seen_call = false;
        for part in parts.iter_mut() {
            if let Some(fc) = part.get("functionCall") {
                let first = !seen_call;
                seen_call = true;
                let id = str_field(fc, "id").to_string();
                if !is_gemini_claude_tool_use_id(&id) {
                    continue;
                }
                let Some(fc_obj) = part.get_mut("functionCall").and_then(|v| v.as_object_mut())
                else {
                    continue;
                };
                fc_obj.insert("id".into(), json!(synthetic_tool_call_id(&id)));
                if part_thought_signature(part)
                    .map(has_native_thought_signature)
                    .unwrap_or(false)
                {
                    if let Some(obj) = part.as_object_mut() {
                        obj.remove("thought_signature");
                        if first {
                            obj.insert(
                                "thoughtSignature".into(),
                                json!(GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR),
                            );
                        } else {
                            obj.remove("thoughtSignature");
                        }
                    }
                }
                count += 1;
                continue;
            }
            if let Some(fr) = part
                .get_mut("functionResponse")
                .and_then(|v| v.as_object_mut())
            {
                let id = fr
                    .get("id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string();
                if is_gemini_claude_tool_use_id(&id) {
                    fr.insert("id".into(), json!(synthetic_tool_call_id(&id)));
                    count += 1;
                }
            }
        }
    }
    count
}

/// 每个 model 回合首个 functionCall 无签名时补哨兵（对齐 CPA
/// antigravityRepairUnsignedFirstFunctionCalls）。返回补了几处。
pub fn repair_unsigned_first_function_calls(request: &mut Value) -> usize {
    let Some(contents) = contents_mut(request) else {
        return 0;
    };
    let mut repaired = 0usize;
    for content in contents.iter_mut() {
        if !is_model_role(content) {
            continue;
        }
        let Some(parts) = content.get_mut("parts").and_then(|p| p.as_array_mut()) else {
            continue;
        };
        let Some(part) = parts.iter_mut().find(|p| p.get("functionCall").is_some()) else {
            continue;
        };
        let has_signature = part_thought_signature(part).is_some()
            || part
                .pointer("/extra_content/google/thought_signature")
                .and_then(|v| v.as_str())
                .is_some_and(|s| !s.trim().is_empty());
        if !has_signature {
            if let Some(obj) = part.as_object_mut() {
                obj.insert(
                    "thoughtSignature".into(),
                    json!(GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR),
                );
                repaired += 1;
            }
        }
    }
    repaired
}

/// functionCall / functionResponse 配对校验（对齐 CPA signature.ValidateGeminiFunctionCallPairing）。
pub fn gemini_function_call_pairing_valid(request: &Value) -> bool {
    gemini_function_call_pairing_error(request).is_none()
}

/// 配对校验，失败返回 CPA 同款诊断文案（CPA 校验的是外层 AG body，路径前缀为
/// `request.contents`；这里收的是 `request` 子对象，前缀照 CPA 写死）。
pub fn gemini_function_call_pairing_error(request: &Value) -> Option<String> {
    const PATH: &str = "request.contents";
    let contents = contents_ref(request)?;
    let mut pending: Vec<(String, String, String)> = Vec::new(); // (id, name, path)
    for (i, content) in contents.iter().enumerate() {
        let parts = content_parts(content);
        if parts.is_empty() {
            if !pending.is_empty() {
                return Some(format!(
                    "{PATH}[{i}]: content appears before {} pending functionResponse part(s)",
                    pending.len()
                ));
            }
            continue;
        }
        let mut calls: Vec<(String, String, String)> = Vec::new();
        let mut responses: Vec<(&Value, String)> = Vec::new();
        for (j, part) in parts.iter().enumerate() {
            let part_path = format!("{PATH}[{i}].parts[{j}]");
            if let Some(fc) = part.get("functionCall") {
                let name = fc.get("name").and_then(|v| v.as_str()).unwrap_or("");
                if name.is_empty() {
                    return Some(format!("{part_path}: missing functionCall.name"));
                }
                let id = fc.get("id").and_then(|v| v.as_str()).unwrap_or("");
                calls.push((id.to_string(), name.to_string(), part_path.clone()));
            }
            if let Some(fr) = part.get("functionResponse") {
                responses.push((fr, part_path));
            }
        }
        if !calls.is_empty() && !responses.is_empty() {
            return Some(format!(
                "{PATH}[{i}]: functionCall and functionResponse parts must not be interleaved in the same content"
            ));
        }
        if !calls.is_empty() {
            if !pending.is_empty() {
                return Some(format!(
                    "{PATH}[{i}]: functionCall appears before {} pending functionResponse part(s)",
                    pending.len()
                ));
            }
            pending = calls;
            continue;
        }
        if responses.is_empty() {
            if !pending.is_empty() && is_model_role(content) {
                return Some(format!(
                    "{PATH}[{i}]: model content appears before {} pending functionResponse part(s)",
                    pending.len()
                ));
            }
            continue;
        }
        if pending.is_empty() {
            return Some(format!(
                "{PATH}[{i}]: functionResponse without preceding functionCall"
            ));
        }
        if responses.len() != pending.len() {
            return Some(format!(
                "{PATH}[{i}]: functionResponse count {} does not match pending functionCall count {}",
                responses.len(),
                pending.len()
            ));
        }
        for ((fr, part_path), (call_id, call_name, call_path)) in responses.iter().zip(&pending) {
            let id = fr.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let name = fr.get("name").and_then(|v| v.as_str()).unwrap_or("");
            if !call_id.is_empty() && id.is_empty() {
                return Some(format!(
                    "{part_path}: missing functionResponse.id for {call_path}"
                ));
            }
            if !call_id.is_empty() && id != call_id {
                return Some(format!(
                    "{part_path}: functionResponse.id {id:?} does not match functionCall.id {call_id:?} at {call_path}"
                ));
            }
            if name.is_empty() {
                return Some(format!("{part_path}: missing functionResponse.name"));
            }
            if !call_name.is_empty() && name != call_name {
                return Some(format!(
                    "{part_path}: functionResponse.name {name:?} does not match functionCall.name {call_name:?} at {call_path}"
                ));
            }
        }
        pending.clear();
    }
    None
}

/// 对齐 CPA repairAntigravityGeminiFunctionResponseNames：functionResponse 名缺失或为
/// `unknown` 时按同 id 的 functionCall 补名。
fn repair_function_response_names(contents: &mut [Value]) {
    let mut call_id_to_name: HashMap<String, String> = HashMap::new();
    for content in contents.iter() {
        for part in content_parts(content) {
            if let Some(fc) = part.get("functionCall") {
                let (id, name) = (str_field(fc, "id"), str_field(fc, "name"));
                if !id.is_empty() && !name.is_empty() && name != "unknown" {
                    call_id_to_name.insert(id.to_string(), name.to_string());
                }
            }
        }
    }
    if call_id_to_name.is_empty() {
        return;
    }
    for content in contents.iter_mut() {
        let Some(parts) = content.get_mut("parts").and_then(|p| p.as_array_mut()) else {
            continue;
        };
        for part in parts.iter_mut() {
            let Some(fr) = part.get_mut("functionResponse") else {
                continue;
            };
            let (id, name) = (str_field(fr, "id"), str_field(fr, "name"));
            if id.is_empty() || (!name.is_empty() && name != "unknown") {
                continue;
            }
            if let Some(real) = call_id_to_name.get(id).cloned() {
                if fr.is_object() {
                    fr["name"] = json!(real);
                }
            }
        }
    }
}

/// 对齐 CPA normalizeAntigravityGeminiFunctionResponseRoles（antigravity_executor.go:545）：
/// 先修 functionResponse 名；紧跟 functionCall 回合的 response 回合按 call 顺序重排
/// response parts（其余 part 追加在后）；纯 functionResponse 回合 role 改为 `model`。
/// 改写的是外发 payload（CPA 在 sanitize 与 prepare 中都把结果作为后续 payload）。
pub fn normalize_function_response_roles(request: &mut Value) {
    let Some(contents) = contents_mut(request) else {
        return;
    };
    repair_function_response_names(contents);
    let gstr = |v: &Value, k: &str| v.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
    let mut pending: Vec<(String, String)> = Vec::new();
    for content in contents.iter_mut() {
        // 只读扫描：只记 call / response 身份与 part 下标，不复制 part；
        // 仅需重排或改 role 的 content 才原地改写（未改动的 content 不克隆、不重建）。
        let (new_order, set_model_role) = {
            let Some(parts) = content.get("parts").and_then(|p| p.as_array()) else {
                pending.clear();
                continue;
            };
            if parts.is_empty() {
                pending.clear();
                continue;
            }
            let mut calls: Vec<(String, String)> = Vec::new();
            let mut responses: Vec<(usize, String, String)> = Vec::new();
            let mut other_indices: Vec<usize> = Vec::new();
            for (idx, part) in parts.iter().enumerate() {
                if let Some(fc) = part.get("functionCall") {
                    calls.push((gstr(fc, "id"), gstr(fc, "name")));
                } else if let Some(fr) = part.get("functionResponse") {
                    responses.push((idx, gstr(fr, "id"), gstr(fr, "name")));
                } else {
                    other_indices.push(idx);
                }
            }
            let has_other_part = !other_indices.is_empty();
            if !calls.is_empty() && responses.is_empty() {
                pending = calls;
                continue;
            }
            if responses.is_empty() {
                if has_other_part {
                    pending.clear();
                }
                continue;
            }
            if !calls.is_empty() {
                pending.clear();
                continue;
            }
            let mut new_order: Option<Vec<usize>> = None;
            if !pending.is_empty() {
                let mut used = vec![false; responses.len()];
                let mut ordered: Vec<usize> = Vec::with_capacity(parts.len());
                for (call_id, call_name) in &pending {
                    for (ri, (pi, rid, rname)) in responses.iter().enumerate() {
                        if used[ri] {
                            continue;
                        }
                        if (!call_id.is_empty() && rid == call_id)
                            || (call_id.is_empty() && !call_name.is_empty() && rname == call_name)
                        {
                            used[ri] = true;
                            ordered.push(*pi);
                            break;
                        }
                    }
                }
                for (ri, (pi, _, _)) in responses.iter().enumerate() {
                    if !used[ri] {
                        ordered.push(*pi);
                    }
                }
                ordered.extend(other_indices.iter().copied());
                // 顺序变了才改写（值相同的 part 互换也照旧重建，输出字节不变）
                if ordered.iter().enumerate().any(|(i, &pi)| i != pi) {
                    new_order = Some(ordered);
                }
            }
            pending.clear();
            let set_model_role =
                !has_other_part && content.get("role").and_then(|r| r.as_str()) != Some("model");
            (new_order, set_model_role)
        };
        if let Some(order) = new_order {
            if let Some(parts) = content.get_mut("parts").and_then(|p| p.as_array_mut()) {
                let mut old: Vec<Option<Value>> =
                    std::mem::take(parts).into_iter().map(Some).collect();
                *parts = order.into_iter().filter_map(|i| old[i].take()).collect();
            }
        }
        if set_model_role {
            if let Some(obj) = content.as_object_mut() {
                obj.insert("role".into(), json!("model"));
            }
        }
    }
}

/// CPA 本地 400 文案前缀（prepareAntigravityGeminiReasoningReplayPayload 的 statusErr）。
pub const AG_INVALID_FUNCTION_CALL_HISTORY_PREFIX: &str =
    "antigravity executor: invalid Gemini function call history: ";

/// 请求侧回放全流程结果（供 server 层决定日志与账本清理）。
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct AgReplayPrepareReport {
    /// 账本条目改写了 payload
    pub applied: bool,
    /// 找不回原生 id 而降级的稳定 id 个数
    pub degraded: usize,
    /// 回合首个 functionCall 补哨兵个数
    pub repaired: usize,
    /// 回放破坏配对 → 已退回原始 payload，调用方应清空账本
    pub rolled_back: bool,
    /// 最终配对无效且不满足回退条件 → 调用方应回本地 400（完整 CPA 文案），不清账本
    pub pairing_error: Option<String>,
}

/// 对齐 CPA 外发前处理链（调用方须仅对 `antigravity_uses_reasoning_replay` 模型调用，
/// 与 CPA 门控一致；与有无会话 / 账本 / 稳定 id 无关）：
/// 0. sanitize 阶段（antigravity_executor.go:493-498）：SanitizeGeminiRequestThoughtSignatures
///    + functionResponse 角色归一；
/// 1. 应用账本（可空）→ 归一；2. 历史含稳定 id → 合成 id 降级 → 归一；
/// 3. 回合首个 functionCall 补哨兵；4. 最终配对无效：账本确有改写且原始配对有效 → 整体退回
///    原始 payload（`rolled_back`）；否则 → `pairing_error`（本地 400，对齐 CPA statusErr）；
/// 5. 成功 / 回退时补边界 user 回合（对齐 CPA prepare 之后的
///    ensureAntigravityGeminiBoundaryUserContent：剥尾部纯 thought → 首 user → 尾 user；
///    角色归一可能让尾部 functionResponse 回合变成 model）。
#[cfg(test)]
pub fn prepare_ag_replay_request(
    request: &mut Value,
    items: Option<&[Value]>,
) -> AgReplayPrepareReport {
    prepare_ag_replay_request_with_tools(request, items, AgReplayToolContext::default())
}

/// 同 [`prepare_ag_replay_request`]，带工具上下文（见 [`AgReplayToolContext`]）。
pub fn prepare_ag_replay_request_with_tools(
    request: &mut Value,
    items: Option<&[Value]>,
    tools: AgReplayToolContext,
) -> AgReplayPrepareReport {
    let items = items.filter(|i| !i.is_empty());
    if let Some(contents) = contents_mut(request) {
        super::signature::sanitize_gemini_request_thought_signatures(contents);
    }
    normalize_function_response_roles(request);
    // 回退只在账本确有改写时发生；无账本时不必留原始副本
    let original = items.map(|_| request.clone());
    let mut report = AgReplayPrepareReport::default();
    if let Some(items) = items {
        report.applied = apply_ag_replay_items_with_tools(request, items, tools);
    }
    normalize_function_response_roles(request);
    if request_has_tool_provenance_id(request) {
        report.degraded = degrade_unresolved_tool_provenance_ids(request);
        normalize_function_response_roles(request);
    }
    report.repaired = repair_unsigned_first_function_calls(request);
    if let Some(err) = gemini_function_call_pairing_error(request) {
        let original_valid = original
            .as_ref()
            .is_some_and(gemini_function_call_pairing_valid);
        if let Some(original) = original {
            *request = original;
        }
        if report.applied && original_valid {
            report.rolled_back = true;
        } else {
            // 对齐 CPA：每个 AG Gemini 族请求都校验配对（与有无会话 / 账本无关）。历史本身
            // 坏了时这个 400 每轮都会重现（历史不变），直到用户回退（rewind）或 /clear。
            report.pairing_error = Some(format!("{AG_INVALID_FUNCTION_CALL_HISTORY_PREFIX}{err}"));
            return report;
        }
    }
    if let Some(contents) = contents_mut(request) {
        super::gemini::ensure_antigravity_gemini_boundary_user_content(contents);
    }
    report
}

fn request_has_tool_provenance_id(request: &Value) -> bool {
    contents_ref(request).is_some_and(|contents| {
        contents.iter().any(|content| {
            content_parts(content).iter().any(|part| {
                ["functionCall", "functionResponse"].iter().any(|k| {
                    part.get(*k)
                        .map(|v| is_gemini_claude_tool_use_id(str_field(v, "id")))
                        .unwrap_or(false)
                })
            })
        })
    })
}

/// Claude 请求 `system` 的账本分道（对齐 CPA antigravityClaudeReplaySystemLane：
/// 去 cache_control 后 sha256 前 16 字节 hex）。偏差：额外剥 Claude Code 计费归属行。
/// 该行只在 first-party 签名路径（带 `cch=` / `cc_prev_req` / `cc_prompt_id`）逐请求变化；
/// 经自定义 base URL 走代理时只有 `cc_version=VER.FFF; cc_entrypoint=…`（FFF 取自首条
/// user 消息），会话内稳定。仍剥：一旦出现签名字段也不至于每轮换账本，且升级 / compact
/// 后由 contextHash 与稳定 id 把关，不依赖该行分道。无 system 返回空串。
pub fn claude_replay_system_lane(claude_request: &Value) -> String {
    let Some(system) = claude_request.get("system") else {
        return String::new();
    };
    fn normalize(v: &Value) -> Value {
        match v {
            Value::Object(map) => Value::Object(
                map.iter()
                    .filter(|(k, _)| !k.trim().eq_ignore_ascii_case("cache_control"))
                    .map(|(k, child)| {
                        let child = if k == "text" {
                            match child {
                                Value::String(s) => json!(super::strip_attribution_line(s)),
                                other => normalize(other),
                            }
                        } else {
                            normalize(child)
                        };
                        (k.clone(), child)
                    })
                    .collect(),
            ),
            Value::Array(arr) => Value::Array(arr.iter().map(normalize).collect()),
            other => other.clone(),
        }
    }
    let normalized = match system {
        Value::String(s) => json!(super::strip_attribution_line(s)),
        other => normalize(other),
    };
    let digest = Sha256::digest(canonical_json(&normalized).as_bytes());
    hex::encode(&digest[..16])
}

fn function_call_occurrence_key(fc: &Value) -> String {
    let name = fc.get("name").and_then(|v| v.as_str()).unwrap_or("").trim();
    if name.is_empty() {
        return String::new();
    }
    // 缺失 args 视为 {}（与稳定 id 派生、账本条目、下一轮 converter 补的 {} 一致）
    let args = fc
        .get("args")
        .map(canonical_json)
        .unwrap_or_else(|| "{}".to_string());
    format!("{name}\0{args}")
}

/// 对流式/非流式响应的可回放项累积器（纯逻辑，无 IO）。
#[derive(Debug, Default)]
pub struct AgReplayAccumulator {
    items: Vec<Value>,
    seen_signatures: std::collections::HashSet<String>,
    seen_fc: std::collections::HashSet<String>,
    content_index: i64,
    next_part_index: i64,
    pending_signatures: Vec<(String, String)>, // (sig, target_kind)
    visible_text: String,
    thought_text: String,
    visible_part_index: i64,
    thought_part_index: i64,
    last_response_kind: String,
    segment_occurrences: std::collections::HashMap<String, i64>,
    function_call_occurrences: std::collections::HashMap<String, i64>,
    /// 响应所在 content 之前的上下文指纹（对齐 CPA responseContextHash）
    response_context_hash: String,
    pub terminal: bool,
    pub overflow: bool,
    item_bytes: usize,
}

fn stamp_context_hash(mut item: Value, context_hash: &str) -> Value {
    if !context_hash.is_empty() {
        item["contextHash"] = json!(context_hash);
    }
    item
}

impl AgReplayAccumulator {
    /// 从请求 contents 种子历史条目（对齐 CPA reasoningReplayItemsFromRequest：
    /// 全部 functionCall + 带 native 签名的文本），并确定下一 model content 槽位。
    pub fn new_from_request(request: &Value) -> Self {
        let mut acc = Self {
            content_index: -1,
            next_part_index: 0,
            visible_part_index: -1,
            thought_part_index: -1,
            ..Self::default()
        };
        let Some(contents) = contents_ref(request) else {
            return acc;
        };
        let mut fp = ContextFingerprints::new(request);
        // pending model content（对齐 CPA pendingModelContentIndex）：
        // 末尾是不含 functionResponse 的 model 回合（prefill）时续写该回合，
        // part 序号从其已有 parts 之后开始；否则落在末尾之后。
        let (content_index, base_part_index) = pending_model_content_index(contents);
        acc.content_index = content_index as i64;
        acc.next_part_index = base_part_index as i64;
        acc.response_context_hash = fp.at(contents, content_index);
        for (ci, content) in contents.iter().enumerate() {
            let parts = content_parts(content);
            if !is_model_role(content) || parts.is_empty() {
                continue;
            }
            let context_hash = fp.at(contents, ci);
            let mut fc_occurrences: std::collections::HashMap<String, i64> = Default::default();
            for (pi, part) in parts.iter().enumerate() {
                let sig = part_thought_signature(part)
                    .filter(|s| has_native_thought_signature(s))
                    .unwrap_or("");
                if let Some(fc) = part.get("functionCall") {
                    let key = function_call_occurrence_key(fc);
                    let occurrence = *fc_occurrences.get(&key).unwrap_or(&0);
                    if !key.is_empty() {
                        fc_occurrences.insert(key, occurrence + 1);
                    }
                    if let Some(item) =
                        build_function_call_part_item(ci as i64, pi as i64, occurrence, fc, sig)
                    {
                        if !sig.is_empty() {
                            acc.seen_signatures.insert(sig.to_string());
                        }
                        acc.append_item(stamp_context_hash(item, &context_hash));
                    }
                    continue;
                }
                if sig.is_empty() {
                    continue;
                }
                // 签名挂在无文本 part 上时指向前一 part（对齐 CPA :2370-2377）
                let mut target_index = pi;
                let mut fingerprint = part_fingerprint(part);
                if fingerprint.is_none() && pi > 0 {
                    target_index = pi - 1;
                    fingerprint = part_fingerprint(&parts[target_index]);
                }
                let Some((kind, hash)) = fingerprint else {
                    continue;
                };
                let occurrence = parts[..target_index]
                    .iter()
                    .filter_map(part_fingerprint)
                    .filter(|(k, h)| *k == kind && *h == hash)
                    .count();
                acc.seen_signatures.insert(sig.to_string());
                let item = json!({
                    "type": "thought_signature",
                    "thoughtSignature": sig,
                    "contentIndex": ci,
                    "partIndex": target_index,
                    "targetKind": kind,
                    "targetHash": hash,
                    "targetOccurrence": occurrence,
                });
                acc.append_item(stamp_context_hash(item, &context_hash));
            }
        }
        // 续写回合已有 parts 计入出现次序（对齐 CPA :2304-2317）
        if let Some(pending) = contents.get(content_index) {
            for part in content_parts(pending) {
                if let Some(fc) = part.get("functionCall") {
                    let key = function_call_occurrence_key(fc);
                    if !key.is_empty() {
                        *acc.function_call_occurrences.entry(key).or_insert(0) += 1;
                    }
                    continue;
                }
                if let Some((kind, hash)) = part_fingerprint(part) {
                    *acc.segment_occurrences
                        .entry(format!("{kind}\0{hash}"))
                        .or_insert(0) += 1;
                }
            }
        }
        acc
    }

    fn append_item(&mut self, item: Value) {
        let bytes = item.to_string().len();
        if self.items.len() + 1 > MAX_ITEMS_PER_ENTRY
            || self.item_bytes.saturating_add(bytes) > MAX_BYTES_PER_ENTRY
        {
            self.overflow = true;
            return;
        }
        self.item_bytes += bytes;
        self.items.push(item);
    }

    /// 观察一帧上游响应 JSON（流式 chunk 或非流完整 body）。
    /// 支持 `response.candidates` 与顶层 `candidates`。
    pub fn observe_response(&mut self, payload: &Value) {
        let response = payload.get("response").unwrap_or(payload);
        let candidates = response.get("candidates").and_then(|v| v.as_array());
        let Some(candidates) = candidates else {
            return;
        };
        let Some(first) = candidates.first() else {
            return;
        };
        if first
            .get("finishReason")
            .and_then(|v| v.as_str())
            .map(|s| !s.trim().is_empty())
            .unwrap_or(false)
        {
            self.terminal = true;
        }
        let Some(parts) = first.pointer("/content/parts").and_then(|v| v.as_array()) else {
            return;
        };
        for part in parts {
            self.observe_part(part);
        }
    }

    fn observe_part(&mut self, part: &Value) {
        let pi = self.next_part_index;
        self.next_part_index += 1;
        let mut signature = part_thought_signature(part)
            .filter(|s| has_native_thought_signature(s))
            .unwrap_or("")
            .to_string();

        if let Some(fc) = part.get("functionCall") {
            if self.last_response_kind == "text" || self.last_response_kind == "thought" {
                self.flush_pending_for_kind(&self.last_response_kind.clone());
            }
            if signature.is_empty() {
                // 取最后一个 unbound pending
                if let Some(idx) = self
                    .pending_signatures
                    .iter()
                    .rposition(|(_, kind)| kind.is_empty())
                {
                    signature = self.pending_signatures.remove(idx).0;
                }
            } else {
                self.pending_signatures.retain(|(_, kind)| !kind.is_empty());
            }
            let name = fc.get("name").and_then(|v| v.as_str()).unwrap_or("");
            // 缺失 args 视为 {}（同 function_call_occurrence_key）
            let args_raw = fc
                .get("args")
                .map(canonical_json)
                .unwrap_or_else(|| "{}".to_string());
            let call_id = fc.get("id").and_then(|v| v.as_str()).unwrap_or("");
            let key = format!("{name}\0{args_raw}\0{call_id}");
            let dedupe = if signature.is_empty() {
                format!("{key}\0part:{pi}")
            } else {
                format!("{key}\0{signature}")
            };
            if self.seen_fc.contains(&dedupe) {
                return;
            }
            self.seen_fc.insert(dedupe);
            let occurrence_key = function_call_occurrence_key(fc);
            let occurrence = *self
                .function_call_occurrences
                .get(&occurrence_key)
                .unwrap_or(&0);
            if !occurrence_key.is_empty() {
                self.function_call_occurrences
                    .insert(occurrence_key, occurrence + 1);
            }
            if let Some(item) =
                build_function_call_part_item(self.content_index, pi, occurrence, fc, &signature)
            {
                if !signature.is_empty() {
                    self.seen_signatures.insert(signature.clone());
                }
                let item = stamp_context_hash(item, &self.response_context_hash);
                self.append_item(item);
            }
            self.last_response_kind = "function_call".into();
            return;
        }

        let thought = part.get("thought").and_then(|v| v.as_bool()) == Some(true);
        let text = part.get("text").and_then(|v| v.as_str()).unwrap_or("");
        let has_text = !text.is_empty();
        let signature_only = !signature.is_empty() && !has_text;

        if signature_only && self.last_response_kind == "function_call" {
            if !self.seen_signatures.contains(&signature) {
                self.attach_detached_signature_to_last_function_call(&signature);
                self.seen_signatures.insert(signature);
            }
            return;
        }

        // 对齐 CPA :2507-2510：thought 标记先定 targetKind（无文本的 thought 签名 part 也算 thought）
        let mut target_kind = if thought {
            "thought".to_string()
        } else {
            String::new()
        };
        if has_text {
            if target_kind != "thought" {
                target_kind = "text".into();
            }
            if !signature.is_empty() {
                // 对齐 CPA :2521-2543：带签名的文本到达时，丢掉「未绑定前缀」pending——
                // 无 kind 的，或同 kind 但该段缓冲仍空的；若丢掉的正是本签名则撤销 seen，
                // 让本 part 重新接收。同 kind 还剩其它签名 → 先按当前缓冲落账。
                let visible_empty = self.visible_text.is_empty();
                let thought_empty = self.thought_text.is_empty();
                let mut remaining = Vec::new();
                for (sig, kind) in std::mem::take(&mut self.pending_signatures) {
                    let mut unbound_prefix = kind.is_empty();
                    if kind == target_kind {
                        unbound_prefix = (target_kind == "text" && visible_empty)
                            || (target_kind == "thought" && thought_empty);
                    }
                    if unbound_prefix {
                        if sig == signature {
                            self.seen_signatures.remove(&signature);
                        }
                        continue;
                    }
                    remaining.push((sig, kind));
                }
                self.pending_signatures = remaining;
                if self
                    .pending_signatures
                    .iter()
                    .any(|(sig, kind)| *kind == target_kind && *sig != signature)
                {
                    self.flush_pending_for_kind(&target_kind.clone());
                }
            }
            if self.last_response_kind != target_kind
                && (self.last_response_kind == "text" || self.last_response_kind == "thought")
            {
                self.flush_pending_for_kind(&self.last_response_kind.clone());
            }
            if target_kind == "thought" {
                if self.thought_text.is_empty() {
                    self.thought_part_index = pi;
                }
                self.thought_text.push_str(text);
            } else {
                if self.visible_text.is_empty() {
                    self.visible_part_index = pi;
                }
                self.visible_text.push_str(text);
            }
            self.last_response_kind = target_kind.clone();
        }

        if !signature.is_empty() && !self.seen_signatures.contains(&signature) {
            if target_kind.is_empty() {
                target_kind = self.last_response_kind.clone();
            }
            // 对齐 CPA :2565-2567：纯签名 part 落在同 kind 但该段缓冲已空（刚落账 / 尚无文本）
            // → 无可绑定文本，记为已见并丢弃，避免误绑到下一段文本
            let unmatched_detached_carrier = signature_only
                && self.last_response_kind == target_kind
                && ((target_kind == "text" && self.visible_text.is_empty())
                    || (target_kind == "thought" && self.thought_text.is_empty()));
            if unmatched_detached_carrier {
                self.seen_signatures.insert(signature);
                return;
            }
            // 对齐 CPA :2568：pending + 已入账 + 本条超条目上限，或字节将超上限 → overflow，不接收
            if self.pending_signatures.len() + self.items.len() + 1 > MAX_ITEMS_PER_ENTRY
                || self.item_bytes.saturating_add(signature.len()) > MAX_BYTES_PER_ENTRY
            {
                self.overflow = true;
                self.seen_signatures.insert(signature);
                return;
            }
            self.pending_signatures
                .push((signature.clone(), target_kind.clone()));
            self.seen_signatures.insert(signature);
            if signature_only || has_text {
                if target_kind == "text" && !self.visible_text.is_empty() {
                    self.flush_pending_for_kind("text");
                } else if target_kind == "thought" && !self.thought_text.is_empty() {
                    self.flush_pending_for_kind("thought");
                }
            }
        }
    }

    fn attach_detached_signature_to_last_function_call(&mut self, signature: &str) {
        for item in self.items.iter_mut().rev() {
            if item.get("type").and_then(|v| v.as_str()) != Some("function_call_part") {
                continue;
            }
            // 对齐 CPA attachDetachedSignatureToLastFunctionCall :2401-2426：只看最后一个
            // function_call_part，已有签名即停（不再往前找更早的 call）；写入受字节上限约束
            if !str_field(item, "thoughtSignature").is_empty() {
                return;
            }
            let before = item.to_string().len();
            item["thoughtSignature"] = json!(signature);
            let after = item.to_string().len();
            if self.item_bytes.saturating_add(after.saturating_sub(before)) > MAX_BYTES_PER_ENTRY {
                if let Some(obj) = item.as_object_mut() {
                    obj.remove("thoughtSignature");
                }
                self.overflow = true;
                return;
            }
            self.item_bytes += after.saturating_sub(before);
            return;
        }
    }

    fn flush_pending_for_kind(&mut self, target_kind: &str) {
        if target_kind != "text" && target_kind != "thought" {
            return;
        }
        let (text, part_index) = if target_kind == "thought" {
            (self.thought_text.clone(), self.thought_part_index)
        } else {
            (self.visible_text.clone(), self.visible_part_index)
        };
        // 对齐 CPA :2633-2670：文本为空时 targetHash 为空，pending 全部保留，缓冲照常复位
        let mut target_hash = String::new();
        let mut occurrence = 0i64;
        if !text.is_empty() {
            let mut hasher = Sha256::new();
            hasher.update(target_kind.as_bytes());
            hasher.update([0u8]);
            hasher.update(text.as_bytes());
            target_hash = hex::encode(hasher.finalize());
            let occ_key = format!("{target_kind}\0{target_hash}");
            occurrence = *self.segment_occurrences.get(&occ_key).unwrap_or(&0);
            self.segment_occurrences.insert(occ_key, occurrence + 1);
        }

        let mut remaining = Vec::new();
        for (sig, kind) in std::mem::take(&mut self.pending_signatures) {
            if kind != target_kind || target_hash.is_empty() {
                remaining.push((sig, kind));
                continue;
            }
            let item = json!({
                "type": "thought_signature",
                "thoughtSignature": sig,
                "contentIndex": self.content_index,
                "partIndex": part_index,
                "targetKind": target_kind,
                "targetHash": target_hash,
                "targetOccurrence": occurrence,
            });
            let item = stamp_context_hash(item, &self.response_context_hash);
            self.append_item(item);
        }
        self.pending_signatures = remaining;
        if target_kind == "thought" {
            self.thought_text.clear();
            self.thought_part_index = -1;
        } else {
            self.visible_text.clear();
            self.visible_part_index = -1;
        }
    }

    fn flush_all_pending(&mut self) {
        for (_sig, kind) in self.pending_signatures.iter_mut() {
            if !kind.is_empty() {
                continue;
            }
            if self.last_response_kind == "text" && !self.visible_text.is_empty() {
                *kind = "text".into();
            } else if self.last_response_kind == "thought" && !self.thought_text.is_empty() {
                *kind = "thought".into();
            } else if !self.visible_text.is_empty() {
                *kind = "text".into();
            } else if !self.thought_text.is_empty() {
                *kind = "thought".into();
            }
        }
        self.flush_pending_for_kind("thought");
        self.flush_pending_for_kind("text");
        self.pending_signatures.clear();
    }

    /// 若 terminal 且未 overflow，返回可提交的规范化 items；
    /// terminal 但空/overflow → Some(empty) 表示应清空缓存；
    /// 非 terminal → None（不提交）。
    pub fn take_commit_items(&mut self) -> Option<Vec<Value>> {
        if !self.terminal {
            return None;
        }
        if self.overflow {
            return Some(Vec::new());
        }
        self.flush_all_pending();
        if self.overflow {
            return Some(Vec::new());
        }
        if self.items.is_empty() {
            return Some(Vec::new());
        }
        Some(normalize_ag_replay_items(&self.items).unwrap_or_default())
    }

    pub fn items(&self) -> &[Value] {
        &self.items
    }
}

fn pending_model_content_index(contents: &[Value]) -> (usize, usize) {
    if let Some(last) = contents.last() {
        if is_model_role(last) {
            let parts = last
                .get("parts")
                .and_then(|v| v.as_array())
                .map(Vec::as_slice)
                .unwrap_or_default();
            if !parts.iter().any(|p| p.get("functionResponse").is_some()) {
                return (contents.len() - 1, parts.len());
            }
        }
    }
    (contents.len(), 0)
}

fn build_function_call_part_item(
    content_index: i64,
    part_index: i64,
    target_occurrence: i64,
    fc: &Value,
    signature: &str,
) -> Option<Value> {
    let name = fc.get("name").and_then(|v| v.as_str())?.trim();
    if name.is_empty() {
        return None;
    }
    // 缺失 args 视为 {}（与稳定 id 派生、响应侧发给 Claude 的 input 一致）
    let args = fc.get("args").cloned().unwrap_or_else(|| json!({}));
    let mut item = json!({
        "type": "function_call_part",
        "contentIndex": content_index,
        "partIndex": part_index,
        "targetOccurrence": target_occurrence,
        "name": name,
        "args": args,
    });
    if let Some(id) = fc
        .get("id")
        .and_then(|v| v.as_str())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        item["call_id"] = json!(id);
    }
    if !signature.is_empty() {
        item["thoughtSignature"] = json!(signature);
    }
    Some(item)
}

/// 从完整非流响应构造可提交 items（内部用 accumulator）。
pub fn collect_ag_replay_from_response(
    request: &Value,
    response_body: &Value,
) -> Option<Vec<Value>> {
    let mut acc = AgReplayAccumulator::new_from_request(request);
    acc.observe_response(response_body);
    acc.take_commit_items()
}

/// 上游错误 body 是否触发 AG 账本清理（逐字对齐 CPA
/// clearAntigravityReasoningReplayOnInvalidSignature 的文案探测：strings.ToLower 后含
/// thoughtsignature / thought_signature / signature 任一即可）。状态码门控（仅 400）在调用侧。
/// 仅供 AG replay 账本清理使用。
pub fn is_antigravity_thought_signature_invalid(body: &[u8]) -> bool {
    let lower = String::from_utf8_lossy(body).to_lowercase();
    lower.contains("thoughtsignature")
        || lower.contains("thought_signature")
        || lower.contains("signature")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn normalize_rejects_short_and_bypass_signature() {
        assert!(normalize_ag_replay_item(&json!({
            "type": "thought_signature",
            "thoughtSignature": "short"
        }))
        .is_none());
        assert!(normalize_ag_replay_item(&json!({
            "type": "thought_signature",
            "thoughtSignature": GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR
        }))
        .is_none());
        let ok = normalize_ag_replay_item(&json!({
            "type": "thought_signature",
            "thoughtSignature": "native-signature-ok-16",
            "contentIndex": 1,
            "partIndex": 0,
            "targetKind": "text",
        }))
        .unwrap();
        assert_eq!(ok["thoughtSignature"], "native-signature-ok-16");
        assert_eq!(ok["contentIndex"], 1);
    }

    #[test]
    fn normalize_function_call_part() {
        let ok = normalize_ag_replay_item(&json!({
            "type": "function_call_part",
            "name": "Read",
            "call_id": "id1",
            "args": {"file_path": "/a"},
            "thoughtSignature": "sig-first-long-enough"
        }))
        .unwrap();
        assert_eq!(ok["name"], "Read");
        assert_eq!(ok["call_id"], "id1");
        assert_eq!(ok["thoughtSignature"], "sig-first-long-enough");
    }

    #[test]
    fn apply_merges_signature_into_existing_function_call() {
        let items = vec![json!({
            "type": "function_call_part",
            "contentIndex": 1,
            "partIndex": 0,
            "name": "Read",
            "call_id": "id1",
            "args": {"file_path": "/a"},
            "thoughtSignature": "sig-first-long-enough"
        })];
        let mut request = json!({
            "contents": [
                {"role": "user", "parts": [{"text": "hi"}]},
                {"role": "model", "parts": [{"functionCall": {"id": "id1", "name": "Read", "args": {"file_path": "/a"}}}]},
                {"role": "user", "parts": [{"functionResponse": {"id": "id1", "name": "Read", "response": {"result": "ok"}}}]}
            ]
        });
        assert!(apply_ag_replay_items(&mut request, &items));
        assert_eq!(
            request["contents"][1]["parts"][0]["thoughtSignature"],
            "sig-first-long-enough"
        );
    }

    #[test]
    fn apply_attaches_text_signature_by_target_hash() {
        let text = "answer-one";
        let mut hasher = Sha256::new();
        hasher.update(b"text");
        hasher.update([0u8]);
        hasher.update(text.as_bytes());
        let hash = hex::encode(hasher.finalize());
        let items = vec![json!({
            "type": "thought_signature",
            "thoughtSignature": "native-signature-turn-one-123456",
            "contentIndex": 1,
            "partIndex": 0,
            "targetKind": "text",
            "targetHash": hash,
            "targetOccurrence": 0,
        })];
        let mut request = json!({
            "contents": [
                {"role": "user", "parts": [{"text": "turn one"}]},
                {"role": "model", "parts": [{"text": "answer-one"}]},
                {"role": "user", "parts": [{"text": "turn two"}]}
            ]
        });
        assert!(apply_ag_replay_items(&mut request, &items));
        assert_eq!(
            request["contents"][1]["parts"][0]["thoughtSignature"],
            "native-signature-turn-one-123456"
        );
    }

    #[test]
    fn apply_skips_stale_positional_signature_without_matching_part() {
        // contentIndex 越界 → 不改
        let items = vec![json!({
            "type": "thought_signature",
            "thoughtSignature": "stale-thought-sig-ok12",
            "contentIndex": 8,
            "partIndex": 3,
        })];
        let mut request = json!({
            "contents": [
                {"role": "user", "parts": [{"text": "hi"}]},
                {"role": "model", "parts": [{"text": "visible answer"}]},
                {"role": "user", "parts": [{"text": "next"}]}
            ]
        });
        assert!(!apply_ag_replay_items(&mut request, &items));
        assert!(request["contents"][1]["parts"][0]
            .get("thoughtSignature")
            .is_none());
    }

    #[test]
    fn apply_inserts_missing_function_call_without_context_hash() {
        // 对齐 CPA G5：无 contextHash 视为上下文一致，FR 前插回带账本签名的 model 回合
        let items = vec![json!({
            "type": "function_call_part",
            "contentIndex": 1,
            "partIndex": 0,
            "name": "Read",
            "call_id": "id1",
            "args": {"file_path": "/a"},
            "thoughtSignature": "sig-first-long-enough"
        })];
        let mut request = json!({
            "contents": [
                {"role": "user", "parts": [{"text": "hi"}]},
                {"role": "user", "parts": [{"functionResponse": {"id": "id1", "name": "Read", "response": {"result": "ok"}}}]}
            ]
        });
        assert!(apply_ag_replay_items(&mut request, &items));
        assert_eq!(
            request["contents"][1],
            json!({"role": "model", "parts": [{
                "functionCall": {"id": "id1", "name": "Read", "args": {"file_path": "/a"}},
                "thoughtSignature": "sig-first-long-enough"
            }]})
        );
        assert_eq!(request["contents"].as_array().unwrap().len(), 3);
        assert!(gemini_function_call_pairing_valid(&request));
    }

    #[test]
    fn accumulator_collects_function_calls_with_signature() {
        let request = json!({"contents": [{"role": "user", "parts": [{"text": "hi"}]}]});
        let mut acc = AgReplayAccumulator::new_from_request(&request);
        assert_eq!(acc.content_index, 1);
        acc.observe_response(&json!({
            "response": {"candidates": [{"content": {"parts": [
                {"thoughtSignature": "sig-first-long-enough", "functionCall": {"name": "Read", "args": {"file_path": "/a"}, "id": "id1"}}
            ]}}]}
        }));
        acc.observe_response(&json!({
            "response": {"candidates": [{"content": {"parts": [
                {"functionCall": {"name": "Read", "args": {"file_path": "/b"}, "id": "id2"}}
            ]}, "finishReason": "STOP"}]}
        }));
        let items = acc.take_commit_items().unwrap();
        assert_eq!(items.len(), 2);
        assert_eq!(items[0]["partIndex"], 0);
        assert_eq!(items[0]["thoughtSignature"], "sig-first-long-enough");
        assert_eq!(items[1]["partIndex"], 1);
    }

    #[test]
    fn accumulator_text_signature_chain_roundtrip() {
        let sig1 = "native-signature-turn-one-123456";
        let request1 = json!({"contents": [{"role": "user", "parts": [{"text": "turn one"}]}]});
        let mut acc1 = AgReplayAccumulator::new_from_request(&request1);
        acc1.observe_response(&json!({
            "response": {"candidates": [{"content": {"parts": [{"text": "answer-"}]}}]}
        }));
        acc1.observe_response(&json!({
            "response": {"candidates": [{"content": {"parts": [{"text": "one"}]}}]}
        }));
        acc1.observe_response(&json!({
            "response": {"candidates": [{"content": {"parts": [
                {"text": "", "thoughtSignature": sig1}
            ]}, "finishReason": "STOP"}]}
        }));
        let items = acc1.take_commit_items().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(items[0]["type"], "thought_signature");

        let mut request2 = json!({
            "contents": [
                {"role": "user", "parts": [{"text": "turn one"}]},
                {"role": "model", "parts": [{"text": "answer-one"}]},
                {"role": "user", "parts": [{"text": "turn two"}]}
            ]
        });
        assert!(apply_ag_replay_items(&mut request2, &items));
        assert_eq!(
            request2["contents"][1]["parts"][0]["thoughtSignature"],
            sig1
        );
    }

    #[test]
    fn accumulator_does_not_commit_partial() {
        let request = json!({"contents": [{"role": "user", "parts": [{"text": "turn"}]}]});
        let mut acc = AgReplayAccumulator::new_from_request(&request);
        acc.observe_response(&json!({
            "response": {"candidates": [{"content": {"parts": [
                {"text": "partial"},
                {"text": "", "thoughtSignature": "partial-signature-123456789"}
            ]}}]}
        }));
        assert!(acc.take_commit_items().is_none());
    }

    #[test]
    fn accumulator_terminal_empty_yields_clear() {
        let request = json!({"contents": [{"role": "user", "parts": [{"text": "new"}]}]});
        let mut acc = AgReplayAccumulator::new_from_request(&request);
        acc.observe_response(&json!({
            "response": {"candidates": [{"content": {"parts": [
                {"text": "answer without signature"}
            ]}, "finishReason": "STOP"}]}
        }));
        let items = acc.take_commit_items().unwrap();
        assert!(items.is_empty());
    }
    #[test]
    fn detects_invalid_thought_signature_errors() {
        assert!(is_antigravity_thought_signature_invalid(
            br#"{"error":{"message":"Invalid thoughtSignature"}}"#
        ));
        assert!(is_antigravity_thought_signature_invalid(
            br#"{"error":{"message":"thought_signature validation failed"}}"#
        ));
        assert!(!is_antigravity_thought_signature_invalid(
            br#"{"error":{"message":"quota exceeded"}}"#
        ));
        // 对齐 CPA：含 "signature" 即命中（不再要求 invalid/failed/error），大小写不敏感
        assert!(is_antigravity_thought_signature_invalid(
            b"Corrupted SIGNATURE in history"
        ));
        assert!(is_antigravity_thought_signature_invalid(
            br#"{"message":"thoughtSignature expired"}"#
        ));
    }

    #[test]
    fn normalize_rejects_when_over_max_bytes() {
        // 单条序列化后超过 MAX_BYTES_PER_ENTRY → 整批拒绝
        let sig = "b".repeat(MAX_BYTES_PER_ENTRY);
        let items = vec![json!({
            "type": "thought_signature",
            "thoughtSignature": sig,
            "contentIndex": 1,
            "partIndex": 0,
        })];
        assert!(normalize_ag_replay_items(&items).is_none());
    }

    #[test]
    fn normalize_accepts_under_max_bytes() {
        let sig = "native-signature-ok-16chars";
        let items = vec![json!({
            "type": "thought_signature",
            "thoughtSignature": sig,
            "contentIndex": 1,
            "partIndex": 0,
            "targetKind": "text",
        })];
        let out = normalize_ag_replay_items(&items).expect("under limit");
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn accumulator_byte_overflow_yields_clear_on_terminal() {
        let request = json!({"contents": [{"role": "user", "parts": [{"text": "hi"}]}]});
        let mut acc = AgReplayAccumulator::new_from_request(&request);
        // 超大签名使 append_item 触发 overflow；terminal 后应清空（Some(empty)）
        let huge = "s".repeat(MAX_BYTES_PER_ENTRY);
        acc.observe_response(&json!({
            "response": {"candidates": [{"content": {"parts": [
                {"thoughtSignature": huge, "functionCall": {"name": "Read", "args": {"p": "/a"}, "id": "id1"}}
            ]}, "finishReason": "STOP"}]}
        }));
        assert!(acc.overflow);
        let items = acc.take_commit_items().expect("terminal");
        assert!(
            items.is_empty(),
            "overflow must clear ledger, not store partial"
        );
    }

    fn dup_call_request(copies: usize) -> Value {
        let mut parts = Vec::new();
        for _ in 0..copies {
            parts.push(json!({"functionCall": {"name": "Read", "args": {"file_path": "/a"}}}));
        }
        json!({"contents": [
            {"role": "user", "parts": [{"text": "hi"}]},
            {"role": "model", "parts": parts},
        ]})
    }

    /// 对齐 CPA locate 全局兜底 len(matches) == 1：位置未命中时，同名同参多候选不注入。
    #[test]
    fn apply_global_scan_requires_unique_function_call_match() {
        // contentIndex 指向 user 回合 → 精确命中失败，走全局扫描
        let items = vec![json!({
            "type": "function_call_part",
            "contentIndex": 0,
            "partIndex": 0,
            "name": "Read",
            "args": {"file_path": "/a"},
            "thoughtSignature": "sig-first-long-enough"
        })];

        let mut dup = dup_call_request(2);
        assert!(!apply_ag_replay_items(&mut dup, &items));
        for pi in 0..2 {
            assert!(dup["contents"][1]["parts"][pi]
                .get("thoughtSignature")
                .is_none());
        }

        let mut single = dup_call_request(1);
        assert!(apply_ag_replay_items(&mut single, &items));
        assert_eq!(
            single["contents"][1]["parts"][0]["thoughtSignature"],
            "sig-first-long-enough"
        );
    }

    /// 带 targetOccurrence 时在缓存 content 内按出现次序精确命中，不受全局唯一性约束
    /// （对齐 CPA；B 起去掉了无 targetOccurrence 的 contentIndex/partIndex 直配）。
    #[test]
    fn apply_precise_index_match_still_applies_with_duplicates() {
        let items = vec![json!({
            "type": "function_call_part",
            "contentIndex": 1,
            "partIndex": 1,
            "targetOccurrence": 1,
            "name": "Read",
            "args": {"file_path": "/a"},
            "thoughtSignature": "sig-first-long-enough"
        })];
        let mut dup = dup_call_request(2);
        assert!(apply_ag_replay_items(&mut dup, &items));
        assert!(dup["contents"][1]["parts"][0]
            .get("thoughtSignature")
            .is_none());
        assert_eq!(
            dup["contents"][1]["parts"][1]["thoughtSignature"],
            "sig-first-long-enough"
        );
    }

    /// 对齐 CPA pendingModelContentIndex：末尾 model prefill 续写该回合。
    #[test]
    fn accumulator_pending_index_continues_trailing_model_prefill() {
        let request = json!({"contents": [
            {"role": "user", "parts": [{"text": "hi"}]},
            {"role": "model", "parts": [{"text": "prefill"}]}
        ]});
        let mut acc = AgReplayAccumulator::new_from_request(&request);
        assert_eq!(acc.content_index, 1);
        assert_eq!(acc.next_part_index, 1);
        acc.observe_response(&json!({
            "response": {"candidates": [{"content": {"parts": [
                {"thoughtSignature": "sig-first-long-enough", "functionCall": {"name": "Read", "args": {"p": "/a"}, "id": "id1"}}
            ]}, "finishReason": "STOP"}]}
        }));
        let items = acc.take_commit_items().unwrap();
        assert_eq!(items[0]["contentIndex"], 1);
        assert_eq!(items[0]["partIndex"], 1);

        // 续写后的下一轮：合并回同一 model 回合的 call 能按精确位置命中
        let mut next = json!({"contents": [
            {"role": "user", "parts": [{"text": "hi"}]},
            {"role": "model", "parts": [
                {"text": "prefill"},
                {"functionCall": {"id": "id1", "name": "Read", "args": {"p": "/a"}}}
            ]},
            {"role": "user", "parts": [{"functionResponse": {"id": "id1", "name": "Read", "response": {"result": "ok"}}}]}
        ]});
        assert!(apply_ag_replay_items(&mut next, &items));
        assert_eq!(
            next["contents"][1]["parts"][1]["thoughtSignature"],
            "sig-first-long-enough"
        );
    }

    #[test]
    fn accumulator_pending_index_after_user_or_function_response_turn() {
        let user_last = json!({"contents": [
            {"role": "user", "parts": [{"text": "hi"}]},
            {"role": "model", "parts": [{"text": "a"}]},
            {"role": "user", "parts": [{"text": "next"}]}
        ]});
        let acc = AgReplayAccumulator::new_from_request(&user_last);
        assert_eq!((acc.content_index, acc.next_part_index), (3, 0));

        // model 回合带 functionResponse 不算 prefill
        let model_fr = json!({"contents": [
            {"role": "user", "parts": [{"text": "hi"}]},
            {"role": "model", "parts": [{"functionResponse": {"name": "Read", "response": {"result": "ok"}}}]}
        ]});
        let acc = AgReplayAccumulator::new_from_request(&model_fr);
        assert_eq!((acc.content_index, acc.next_part_index), (2, 0));

        let empty = json!({"contents": []});
        let acc = AgReplayAccumulator::new_from_request(&empty);
        assert_eq!((acc.content_index, acc.next_part_index), (0, 0));
    }

    // ---- B：稳定 id / contextHash / 兜底 ----

    use crate::convert::tool_id::{gemini_claude_tool_use_id, is_gemini_claude_tool_use_id};

    const SIG_A: &str = "native-signature-for-call-a-0001";
    const SIG_TEXT: &str = "native-signature-for-text-000001";

    fn base_request() -> Value {
        json!({
            "systemInstruction": {"role": "user", "parts": [{"text": "sys"}]},
            "tools": [{"functionDeclarations": [{"name": "Read", "parameters": {"type": "object"}}]}],
            "contents": [{"role": "user", "parts": [{"text": "hi"}]}]
        })
    }

    /// 首轮：AG 回原生 id + 签名的 Read，加一个无签名的并行 Read。
    fn first_turn_items(request: &Value) -> Vec<Value> {
        let mut acc = AgReplayAccumulator::new_from_request(request);
        acc.observe_response(&json!({"response": {"candidates": [{"content": {"parts": [
            {"thoughtSignature": SIG_A, "functionCall": {"id": "native-a", "name": "Read", "args": {"p": "/a"}}},
            {"functionCall": {"id": "native-b", "name": "Read", "args": {"p": "/b"}}}
        ]}, "finishReason": "STOP"}]}}));
        acc.take_commit_items().unwrap()
    }

    /// 第二轮：Claude 历史里是稳定 id（converter 给每个 functionCall 挂哨兵）。
    fn second_turn_request(base: &Value, ids: [&str; 2]) -> Value {
        let mut req = base.clone();
        let contents = req["contents"].as_array_mut().unwrap();
        contents.push(json!({"role": "model", "parts": [
            {"thoughtSignature": GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR, "functionCall": {"id": ids[0], "name": "Read", "args": {"p": "/a"}}},
            {"thoughtSignature": GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR, "functionCall": {"id": ids[1], "name": "Read", "args": {"p": "/b"}}}
        ]}));
        contents.push(json!({"role": "user", "parts": [
            {"functionResponse": {"id": ids[0], "name": "Read", "response": {"result": "a"}}},
            {"functionResponse": {"id": ids[1], "name": "Read", "response": {"result": "b"}}}
        ]}));
        req
    }

    /// 期望值 = CPA sanitize 阶段输出（SanitizeGeminiRequestThoughtSignatures + 角色归一）。
    fn cpa_sanitize(request: &mut Value) {
        if let Some(contents) = contents_mut(request) {
            super::super::signature::sanitize_gemini_request_thought_signatures(contents);
        }
        normalize_function_response_roles(request);
    }

    fn stable(id: &str, p: &str) -> String {
        gemini_claude_tool_use_id(id, "Read", Some(&json!({"p": p})))
    }

    #[test]
    fn stable_id_round_trip_restores_native_ids_and_signature() {
        let base = base_request();
        let items = first_turn_items(&base);
        assert!(items.iter().all(|i| i.get("contextHash").is_some()));
        let (sa, sb) = (stable("native-a", "/a"), stable("native-b", "/b"));
        let mut req = second_turn_request(&base, [&sa, &sb]);
        let report = prepare_ag_replay_request(&mut req, Some(&items));
        assert!(report.applied && !report.rolled_back);
        assert_eq!(report.degraded, 0);
        let model = &req["contents"][1]["parts"];
        assert_eq!(model[0]["functionCall"]["id"], "native-a");
        assert_eq!(model[0]["thoughtSignature"], SIG_A);
        assert_eq!(model[1]["functionCall"]["id"], "native-b");
        // 对齐 CPA：兄弟 call 上 converter 的哨兵被 sanitize 删掉（sibling calls must be
        // unsigned）；账本无签名的身份回写不再补写
        assert!(model[1].get("thoughtSignature").is_none());
        let user = &req["contents"][2]["parts"];
        assert_eq!(user[0]["functionResponse"]["id"], "native-a");
        assert_eq!(user[0]["functionResponse"]["name"], "Read");
        assert_eq!(user[1]["functionResponse"]["id"], "native-b");
        assert!(gemini_function_call_pairing_valid(&req));

        // 第三轮：账本从发出去的请求种子，稳定 id 仍可还原（含无签名 call）
        let mut acc = AgReplayAccumulator::new_from_request(&req);
        acc.observe_response(&json!({"response": {"candidates": [{"content": {"parts": [
            {"text": "done"}
        ]}, "finishReason": "STOP"}]}}));
        let items3 = acc.take_commit_items().unwrap();
        let mut req3 = second_turn_request(&base, [&sa, &sb]);
        req3["contents"].as_array_mut().unwrap().extend([
            json!({"role": "model", "parts": [{"text": "done"}]}),
            json!({"role": "user", "parts": [{"text": "more"}]}),
        ]);
        let report = prepare_ag_replay_request(&mut req3, Some(&items3));
        assert!(report.applied && report.degraded == 0);
        assert_eq!(
            req3["contents"][1]["parts"][1]["functionCall"]["id"],
            "native-b"
        );
        assert_eq!(
            req3["contents"][2]["parts"][1]["functionResponse"]["id"],
            "native-b"
        );
        assert_eq!(req3["contents"][1]["parts"][0]["thoughtSignature"], SIG_A);
    }

    /// 旧式 `{name}-{counter}` id（原生 id 为空的条目）：只补签名，不改 id / name。
    #[test]
    fn legacy_counter_ids_keep_identity_and_get_signature() {
        let base = base_request();
        let mut acc = AgReplayAccumulator::new_from_request(&base);
        acc.observe_response(&json!({"response": {"candidates": [{"content": {"parts": [
            {"thoughtSignature": SIG_A, "functionCall": {"name": "Read", "args": {"p": "/a"}}},
            {"functionCall": {"name": "Read", "args": {"p": "/b"}}}
        ]}, "finishReason": "STOP"}]}}));
        let items = acc.take_commit_items().unwrap();
        let mut req = second_turn_request(&base, ["Read-1", "Read-2"]);
        let mut before = req.clone();
        // 纯 functionResponse 回合 role → model（对齐 CPA 角色归一）
        cpa_sanitize(&mut before);
        let report = prepare_ag_replay_request(&mut req, Some(&items));
        assert!(report.applied);
        assert_eq!(report.degraded, 0);
        assert_eq!(req["contents"][1]["parts"][0]["thoughtSignature"], SIG_A);
        for (ci, pi) in [(1, 0), (1, 1)] {
            assert_eq!(
                req["contents"][ci]["parts"][pi]["functionCall"],
                before["contents"][ci]["parts"][pi]["functionCall"]
            );
        }
        assert_eq!(req["contents"][2], before["contents"][2]);

        // 无账本时旧式 id 请求只做角色归一
        let mut untouched = second_turn_request(&base, ["Read-1", "Read-2"]);
        let report = prepare_ag_replay_request(&mut untouched, None);
        assert_eq!(report, AgReplayPrepareReport::default());
        let mut expected = second_turn_request(&base, ["Read-1", "Read-2"]);
        cpa_sanitize(&mut expected);
        assert_eq!(untouched, expected);
        assert_eq!(untouched["contents"][2]["role"], "model");
    }

    /// 账本 miss（重启 / 换 lane）：稳定 id → 确定性合成 id，call/response 仍配对，首 call 有哨兵。
    #[test]
    fn unrecoverable_stable_id_degrades_to_synthetic_id() {
        let base = base_request();
        let (sa, sb) = (stable("native-a", "/a"), stable("native-b", "/b"));
        let mut req = second_turn_request(&base, [&sa, &sb]);
        // 模拟客户端带了某个 native 签名（不应原样发给合成 id）
        req["contents"][1]["parts"][0]["thoughtSignature"] = json!(SIG_A);
        req["contents"][1]["parts"][1]["thoughtSignature"] = json!(SIG_TEXT);
        let report = prepare_ag_replay_request(&mut req, None);
        assert_eq!(report.degraded, 4);
        assert!(!report.applied && !report.rolled_back);
        let fc_a = req["contents"][1]["parts"][0]["functionCall"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        let fc_b = req["contents"][1]["parts"][1]["functionCall"]["id"]
            .as_str()
            .unwrap()
            .to_string();
        assert!(fc_a.starts_with("call_") && fc_b.starts_with("call_") && fc_a != fc_b);
        assert!(!is_gemini_claude_tool_use_id(&fc_a));
        assert_eq!(
            req["contents"][2]["parts"][0]["functionResponse"]["id"],
            fc_a.as_str()
        );
        assert_eq!(
            req["contents"][2]["parts"][1]["functionResponse"]["id"],
            fc_b.as_str()
        );
        assert_eq!(
            req["contents"][1]["parts"][0]["thoughtSignature"],
            GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR
        );
        assert!(req["contents"][1]["parts"][1]
            .get("thoughtSignature")
            .is_none());
        assert!(gemini_function_call_pairing_valid(&req));

        // 账本有条目但对不上（args 被客户端改过）→ 同样降级，不把签名挂到改过的 call
        let items = first_turn_items(&base);
        let mut edited = second_turn_request(&base, [&sa, &sb]);
        edited["contents"][1]["parts"][0]["functionCall"]["args"] = json!({"p": "/edited"});
        let report = prepare_ag_replay_request(&mut edited, Some(&items));
        assert_eq!(report.degraded, 2);
        let part = &edited["contents"][1]["parts"][0];
        assert!(part["functionCall"]["id"]
            .as_str()
            .unwrap()
            .starts_with("call_"));
        assert_ne!(part["thoughtSignature"], SIG_A);
        assert_eq!(
            edited["contents"][1]["parts"][1]["functionCall"]["id"],
            "native-b"
        );
    }

    fn text_hash(text: &str) -> String {
        part_fingerprint(&json!({"text": text})).unwrap().1
    }

    #[test]
    fn context_fingerprint_is_stable_and_sensitive() {
        let a = base_request();
        let mut fa = ContextFingerprints::new(&a);
        let mut fb = ContextFingerprints::new(&a.clone());
        let ca = a["contents"].as_array().unwrap().clone();
        assert_eq!(fa.at(&ca, 1), fb.at(&ca, 1));
        assert!(!fa.at(&ca, 0).is_empty());

        // 键序不同、签名不同 → 同一指纹
        let reordered: Value = serde_json::from_str(
            r#"{"contents":[{"parts":[{"text":"hi","thoughtSignature":"x"}],"role":"user"}],
                "tools":[{"functionDeclarations":[{"parameters":{"type":"object"},"name":"Read"}]}],
                "systemInstruction":{"parts":[{"text":"sys"}],"role":"user"}}"#,
        )
        .unwrap();
        let cr = reordered["contents"].as_array().unwrap().clone();
        assert_eq!(
            ContextFingerprints::new(&reordered).at(&cr, 1),
            fa.at(&ca, 1)
        );

        for mutate in [
            |r: &mut Value| r["systemInstruction"]["parts"][0]["text"] = json!("sys2"),
            |r: &mut Value| r["tools"][0]["functionDeclarations"][0]["name"] = json!("Write"),
            |r: &mut Value| r["toolConfig"] = json!({"functionCallingConfig": {"mode": "ANY"}}),
            |r: &mut Value| r["contents"][0]["parts"][0]["text"] = json!("edited"),
        ] {
            let mut changed = a.clone();
            mutate(&mut changed);
            let cc = changed["contents"].as_array().unwrap().clone();
            assert_ne!(ContextFingerprints::new(&changed).at(&cc, 1), fa.at(&ca, 1));
        }
    }

    /// 同一 Claude 请求经两次 Antigravity 转换（含 schema 清洗、工具声明）指纹一致。
    #[test]
    fn context_fingerprint_stable_across_identical_conversions() {
        let claude = json!({
            "model": "gemini-3-flash",
            "system": [{"type": "text", "text": "be brief"}],
            "tools": [{"name": "Read", "description": "r", "input_schema": {
                "type": "object", "properties": {"p": {"type": "string", "default": "x"}},
                "additionalProperties": false, "$schema": "http://json-schema.org/draft-07/schema#"
            }}, {"name": "Noop", "input_schema": {"type": "object"}}],
            "messages": [{"role": "user", "content": "hi"}]
        });
        let convert = |c: &Value| {
            let (body, _) = crate::convert::convert_to_antigravity(c, "gemini-3-flash", Some("p"));
            body["request"].clone()
        };
        let (r1, r2) = (convert(&claude), convert(&claude));
        let (c1, c2) = (
            r1["contents"].as_array().unwrap().clone(),
            r2["contents"].as_array().unwrap().clone(),
        );
        assert_eq!(
            ContextFingerprints::new(&r1).at(&c1, c1.len()),
            ContextFingerprints::new(&r2).at(&c2, c2.len())
        );
        let mut other = claude.clone();
        other["tools"][0]["input_schema"]["properties"]["q"] = json!({"type": "string"});
        let r3 = convert(&other);
        let c3 = r3["contents"].as_array().unwrap().clone();
        assert_ne!(
            ContextFingerprints::new(&r3).at(&c3, c3.len()),
            ContextFingerprints::new(&r1).at(&c1, c1.len())
        );
    }

    /// 上下文漂移：按位置猜的文本签名、无原生 id 的 functionCall 签名都不写；
    /// 带 targetHash 的文本签名、稳定 id 精确命中的 functionCall（provenance）照写。
    #[test]
    fn context_hash_gates_positional_and_legacy_function_call_signatures() {
        let base = base_request();
        let ctx = {
            let mut fp = ContextFingerprints::new(&base);
            fp.at(base["contents"].as_array().unwrap(), 1)
        };
        let sa = stable("native-a", "/a");
        let items = vec![
            json!({"type": "thought_signature", "thoughtSignature": SIG_TEXT,
                   "contentIndex": 1, "partIndex": 0, "contextHash": ctx}),
            json!({"type": "thought_signature", "thoughtSignature": "native-signature-hashed-text-01",
                   "contentIndex": 3, "partIndex": 0, "targetKind": "text",
                   "targetHash": text_hash("answer"), "targetOccurrence": 0, "contextHash": ctx}),
            json!({"type": "function_call_part", "contentIndex": 1, "partIndex": 1,
                   "targetOccurrence": 0, "name": "Read", "args": {"p": "/legacy"},
                   "thoughtSignature": "native-signature-legacy-call-01", "contextHash": ctx}),
            json!({"type": "function_call_part", "contentIndex": 1, "partIndex": 2,
                   "targetOccurrence": 0, "name": "Read", "call_id": "native-a", "args": {"p": "/a"},
                   "thoughtSignature": SIG_A, "contextHash": ctx}),
        ];
        let history = |system: &str| {
            let mut req = base.clone();
            req["systemInstruction"]["parts"][0]["text"] = json!(system);
            req["contents"].as_array_mut().unwrap().extend([
                json!({"role": "model", "parts": [
                    {"text": "visible"},
                    {"thoughtSignature": GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR, "functionCall": {"id": "Read-7", "name": "Read", "args": {"p": "/legacy"}}},
                    {"functionCall": {"id": sa, "name": "Read", "args": {"p": "/a"}}}
                ]}),
                json!({"role": "user", "parts": [
                    {"functionResponse": {"id": "Read-7", "name": "Read", "response": {"result": "x"}}},
                    {"functionResponse": {"id": sa, "name": "Read", "response": {"result": "y"}}}
                ]}),
                json!({"role": "model", "parts": [{"text": "answer"}]}),
                json!({"role": "user", "parts": [{"text": "next"}]}),
            ]);
            req
        };

        // 同上下文：全部命中
        let mut same = history("sys");
        assert!(apply_ag_replay_items(&mut same, &items));
        assert_eq!(
            same["contents"][1]["parts"][0]["thoughtSignature"],
            SIG_TEXT
        );
        assert_eq!(
            same["contents"][1]["parts"][1]["thoughtSignature"],
            "native-signature-legacy-call-01"
        );
        assert_eq!(same["contents"][1]["parts"][2]["thoughtSignature"], SIG_A);
        assert_eq!(
            same["contents"][3]["parts"][0]["thoughtSignature"],
            "native-signature-hashed-text-01"
        );

        // system 改了：位置猜 + 旧式 call 不写；targetHash 文本与稳定 id call 照写
        let mut drifted = history("sys edited");
        assert!(apply_ag_replay_items(&mut drifted, &items));
        assert!(drifted["contents"][1]["parts"][0]
            .get("thoughtSignature")
            .is_none());
        assert_eq!(
            drifted["contents"][1]["parts"][1]["thoughtSignature"],
            GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR
        );
        assert_eq!(
            drifted["contents"][1]["parts"][2]["thoughtSignature"],
            SIG_A
        );
        assert_eq!(
            drifted["contents"][1]["parts"][2]["functionCall"]["id"],
            "native-a"
        );
        assert_eq!(
            drifted["contents"][2]["parts"][1]["functionResponse"]["id"],
            "native-a"
        );
        assert_eq!(
            drifted["contents"][3]["parts"][0]["thoughtSignature"],
            "native-signature-hashed-text-01"
        );
    }

    /// 回放破坏配对（畸形历史复用同一稳定 id）→ 退回原始 payload，交由调用方清账本。
    #[test]
    fn replay_breaking_pairing_rolls_back_to_original() {
        let base = base_request();
        let items = first_turn_items(&base);
        let sa = stable("native-a", "/a");
        let mut req = base.clone();
        req["contents"].as_array_mut().unwrap().extend([
            json!({"role": "model", "parts": [
                {"functionCall": {"id": sa, "name": "Read", "args": {"p": "/a"}}},
                {"functionCall": {"id": sa, "name": "Read", "args": {"p": "/a"}}}
            ]}),
            json!({"role": "user", "parts": [
                {"functionResponse": {"id": sa, "name": "Read", "response": {"result": "a"}}},
                {"functionResponse": {"id": sa, "name": "Read", "response": {"result": "a"}}}
            ]}),
        ]);
        assert!(gemini_function_call_pairing_valid(&req));
        let mut original = req.clone();
        cpa_sanitize(&mut original);
        let report = prepare_ag_replay_request(&mut req, Some(&items));
        assert!(report.applied && report.rolled_back);
        assert_eq!(req, original);
    }

    #[test]
    fn pairing_validator_matches_cpa_rules() {
        let ok = json!({"contents": [
            {"role": "user", "parts": [{"text": "hi"}]},
            {"role": "model", "parts": [{"functionCall": {"id": "a", "name": "R", "args": {}}}]},
            {"role": "user", "parts": [{"text": "reminder"}]},
            {"role": "user", "parts": [{"functionResponse": {"id": "a", "name": "R", "response": {}}}]},
            {"role": "model", "parts": [{"functionCall": {"id": "b", "name": "R", "args": {}}}]}
        ]});
        assert!(gemini_function_call_pairing_valid(&ok));
        for bad in [
            json!({"contents": [{"role": "user", "parts": [{"functionResponse": {"id": "a", "name": "R"}}]}]}),
            json!({"contents": [
                {"role": "model", "parts": [{"functionCall": {"id": "a", "name": "R"}}]},
                {"role": "user", "parts": [{"functionResponse": {"id": "b", "name": "R"}}]}]}),
            json!({"contents": [
                {"role": "model", "parts": [{"functionCall": {"id": "a", "name": "R"}}]},
                {"role": "user", "parts": [{"functionResponse": {"id": "a", "name": "W"}}]}]}),
            json!({"contents": [
                {"role": "model", "parts": [{"functionCall": {"id": "a", "name": "R"}}]},
                {"role": "model", "parts": [{"text": "x"}]}]}),
        ] {
            assert!(!gemini_function_call_pairing_valid(&bad), "{bad}");
        }
    }

    #[test]
    fn repair_adds_sentinel_only_to_unsigned_first_function_call() {
        let mut req = json!({"contents": [
            {"role": "model", "parts": [
                {"text": "t"},
                {"functionCall": {"id": "a", "name": "R", "args": {}}},
                {"functionCall": {"id": "b", "name": "R", "args": {}}}
            ]},
            {"role": "model", "parts": [
                {"thoughtSignature": SIG_A, "functionCall": {"id": "c", "name": "R", "args": {}}}
            ]},
            {"role": "user", "parts": [{"functionCall": {"id": "d", "name": "R", "args": {}}}]}
        ]});
        assert_eq!(repair_unsigned_first_function_calls(&mut req), 1);
        assert_eq!(
            req["contents"][0]["parts"][1]["thoughtSignature"],
            GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR
        );
        assert!(req["contents"][0]["parts"][2]
            .get("thoughtSignature")
            .is_none());
        assert_eq!(req["contents"][1]["parts"][0]["thoughtSignature"], SIG_A);
        assert!(req["contents"][2]["parts"][0]
            .get("thoughtSignature")
            .is_none());
    }

    #[test]
    fn system_lane_ignores_cache_control_and_billing_line() {
        let a = json!({"system": [
            {"type": "text", "text": "x-anthropic-billing-header: cc_version=1; cch=111;"},
            {"type": "text", "text": "You are helpful.", "cache_control": {"type": "ephemeral"}}
        ]});
        let b = json!({"system": [
            {"type": "text", "text": "x-anthropic-billing-header: cc_version=1; cch=222;"},
            {"type": "text", "text": "You are helpful."}
        ]});
        let c = json!({"system": [{"type": "text", "text": "You are terse."}]});
        let lane = claude_replay_system_lane(&a);
        assert_eq!(lane.len(), 32);
        assert_eq!(lane, claude_replay_system_lane(&b));
        assert_ne!(lane, claude_replay_system_lane(&c));
        assert_eq!(claude_replay_system_lane(&json!({"messages": []})), "");
    }

    // ---- 7 项修复的行为测试 ----

    fn claude_tools(tools: Value) -> Value {
        json!({"model": "gemini-3-flash", "tools": tools, "messages": []})
    }

    fn read_tool() -> Value {
        claude_tools(json!([{"name": "Read", "input_schema": {
            "type": "object", "properties": {"p": {"type": "string"}}, "required": ["p"]
        }}]))
    }

    fn ctx(original: &Value) -> AgReplayToolContext<'_> {
        AgReplayToolContext {
            original_request: Some(original),
            short_to_original: None,
        }
    }

    /// 第 1 项：回放路径激活 + 原始与回放后配对都无效 → CPA 同款 400 文案；请求退回原始。
    #[test]
    fn invalid_pairing_with_ledger_yields_cpa_400_message() {
        let base = base_request();
        let items = first_turn_items(&base);
        let (sa, sb) = (stable("native-a", "/a"), stable("native-b", "/b"));
        let mut req = second_turn_request(&base, [&sa, &sb]);
        // 删掉一个 functionResponse：count 1 vs pending 2
        req["contents"][2]["parts"].as_array_mut().unwrap().pop();
        let mut original = req.clone();
        cpa_sanitize(&mut original);
        let report = prepare_ag_replay_request(&mut req, Some(&items));
        assert!(!report.rolled_back);
        assert_eq!(
            report.pairing_error.as_deref(),
            Some(
                "antigravity executor: invalid Gemini function call history: \
                 request.contents[2]: functionResponse count 1 does not match pending functionCall count 2"
            )
        );
        assert_eq!(req, original);

        // 无账本但历史含稳定 id（回放路径仍激活）→ 同样 400
        let mut req = original.clone();
        let report = prepare_ag_replay_request(&mut req, None);
        assert!(report.pairing_error.is_some());
    }

    /// 门控对齐 CPA：无账本、无稳定 id 的无效历史同样 400（CPA 只按模型门控）。
    #[test]
    fn invalid_pairing_without_replay_also_400_like_cpa() {
        let base = base_request();
        let mut req = second_turn_request(&base, ["Read-1", "Read-2"]);
        req["contents"][2]["parts"].as_array_mut().unwrap().pop();
        let report = prepare_ag_replay_request(&mut req, None);
        assert!(!report.applied && !report.rolled_back);
        assert_eq!(
            report.pairing_error.as_deref(),
            Some(
                "antigravity executor: invalid Gemini function call history: \
                 request.contents[2]: functionResponse count 1 does not match pending functionCall count 2"
            )
        );
    }

    /// 第 1 项：诊断文案与 CPA ValidateGeminiFunctionCallPairing 逐字一致（%q → Debug 引号）。
    #[test]
    fn pairing_error_messages_match_cpa() {
        let cases = [
            (
                json!({"contents": [
                    {"role": "model", "parts": [{"functionCall": {"id": "a", "name": "R"}}]},
                    {"role": "user", "parts": [{"functionResponse": {"id": "b", "name": "R"}}]}]}),
                "request.contents[1].parts[0]: functionResponse.id \"b\" does not match functionCall.id \"a\" at request.contents[0].parts[0]",
            ),
            (
                json!({"contents": [{"role": "user", "parts": [{"functionResponse": {"id": "a", "name": "R"}}]}]}),
                "request.contents[0]: functionResponse without preceding functionCall",
            ),
            (
                json!({"contents": [
                    {"role": "model", "parts": [{"functionCall": {"id": "a", "name": "R"}}]},
                    {"role": "model", "parts": [{"text": "x"}]}]}),
                "request.contents[1]: model content appears before 1 pending functionResponse part(s)",
            ),
            (
                json!({"contents": [
                    {"role": "model", "parts": [{"functionCall": {"id": "a", "name": "R"}}]},
                    {"role": "user", "parts": [{"functionResponse": {"name": "R"}}]}]}),
                "request.contents[1].parts[0]: missing functionResponse.id for request.contents[0].parts[0]",
            ),
        ];
        for (req, want) in cases {
            assert_eq!(
                gemini_function_call_pairing_error(&req).as_deref(),
                Some(want)
            );
        }
    }

    /// 第 2 项：旧式 id + 工具表有该工具 → 换回原生 id（含 functionResponse）+ 签名（CPA :2153）。
    #[test]
    fn legacy_id_restores_identity_when_tool_schema_exists() {
        let base = base_request();
        let items = first_turn_items(&base);
        let original = read_tool();
        let mut req = second_turn_request(&base, ["Read-1", "Read-2"]);
        let report = prepare_ag_replay_request_with_tools(&mut req, Some(&items), ctx(&original));
        assert!(report.applied && report.pairing_error.is_none());
        let model = &req["contents"][1]["parts"];
        assert_eq!(model[0]["functionCall"]["id"], "native-a");
        assert_eq!(model[0]["thoughtSignature"], SIG_A);
        assert_eq!(model[1]["functionCall"]["id"], "native-b");
        let user = &req["contents"][2]["parts"];
        assert_eq!(user[0]["functionResponse"]["id"], "native-a");
        assert_eq!(user[1]["functionResponse"]["id"], "native-b");
        assert!(gemini_function_call_pairing_valid(&req));
    }

    /// 第 2 项：旧式 id + 工具表无该工具名 → 不改身份，只补签名。
    #[test]
    fn legacy_id_keeps_identity_without_tool_schema() {
        let base = base_request();
        let items = first_turn_items(&base);
        let original = claude_tools(json!([{"name": "Write", "input_schema": {"type": "object"}}]));
        let mut req = second_turn_request(&base, ["Read-1", "Read-2"]);
        let mut before = req.clone();
        cpa_sanitize(&mut before);
        let report = prepare_ag_replay_request_with_tools(&mut req, Some(&items), ctx(&original));
        assert!(report.applied);
        assert_eq!(
            req["contents"][1]["parts"][0]["functionCall"]["id"],
            "Read-1"
        );
        assert_eq!(req["contents"][1]["parts"][0]["thoughtSignature"], SIG_A);
        assert_eq!(req["contents"][2], before["contents"][2]);
    }

    fn bash_tool() -> Value {
        claude_tools(json!([{"name": "Bash", "input_schema": {
            "type": "object",
            "properties": {"command": {"type": "string"}, "timeout": {"type": "integer", "default": 120}},
            "required": ["command"],
            "additionalProperties": false
        }}]))
    }

    /// 首轮 Bash 原生 call → 第二轮客户端以 `client_args` 回传（稳定 id 由原生 args 派生）。
    fn bash_round_trip(native_args: Value, client_args: Value) -> (Vec<Value>, Value) {
        let base = base_request();
        let mut acc = AgReplayAccumulator::new_from_request(&base);
        acc.observe_response(&json!({"response": {"candidates": [{"content": {"parts": [
            {"thoughtSignature": SIG_A, "functionCall": {"id": "native-x", "name": "Bash", "args": native_args}}
        ]}, "finishReason": "STOP"}]}}));
        let items = acc.take_commit_items().unwrap();
        let sid = gemini_claude_tool_use_id("native-x", "Bash", Some(&native_args));
        let mut req = base.clone();
        req["contents"].as_array_mut().unwrap().extend([
            json!({"role": "model", "parts": [{"thoughtSignature": GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR,
                "functionCall": {"id": sid, "name": "Bash", "args": client_args}}]}),
            json!({"role": "user", "parts": [{"functionResponse": {"id": sid, "name": "Bash", "response": {}}}]}),
        ]);
        (items, req)
    }

    /// 对齐 CPA：不拿清洗后的原生 args 比对。响应侧剥掉 `reason` 后客户端回传的 args 与账本
    /// 原生 args 不等（schema 也无 default 可归一）→ 不命中，稳定 id 降级为合成 id。
    #[test]
    fn cleaned_client_args_do_not_match_like_cpa() {
        let native = json!({"command": "ls", "reason": "list files"});
        let original = bash_tool();
        let (items, mut req) = bash_round_trip(native, json!({"command": "ls"}));
        let report = prepare_ag_replay_request_with_tools(&mut req, Some(&items), ctx(&original));
        assert!(!report.applied);
        assert_eq!(report.degraded, 2);
        let id = req["contents"][1]["parts"][0]["functionCall"]["id"]
            .as_str()
            .unwrap();
        assert!(id.starts_with("call_"));
        assert_eq!(
            req["contents"][1]["parts"][0]["functionCall"]["args"],
            json!({"command": "ls"})
        );
    }

    /// 第 3 项：schema default 归一（对齐 CPA antigravityNormalizeReplayToolValue）：
    /// 客户端省略等于 default 的字段仍命中；数值 120 与 120.0 视为相等。
    #[test]
    fn schema_default_normalization_matches_omitted_defaults() {
        let original = bash_tool();
        let native = json!({"command": "ls", "timeout": 120});
        let (items, mut req) = bash_round_trip(native.clone(), json!({"command": "ls"}));
        let report = prepare_ag_replay_request_with_tools(&mut req, Some(&items), ctx(&original));
        assert!(report.applied && report.degraded == 0);
        assert_eq!(
            req["contents"][1]["parts"][0]["functionCall"]["args"],
            native
        );

        let (items, mut req) = bash_round_trip(native, json!({"command": "ls", "timeout": 120.0}));
        assert!(
            prepare_ag_replay_request_with_tools(&mut req, Some(&items), ctx(&original)).applied
        );

        // 非 default 值不同 → 不命中
        let (items, mut req) = bash_round_trip(
            json!({"command": "ls", "timeout": 5}),
            json!({"command": "ls"}),
        );
        let report = prepare_ag_replay_request_with_tools(&mut req, Some(&items), ctx(&original));
        assert!(!report.applied);
        assert_eq!(report.degraded, 2);
    }

    /// 第 6 项：字符串 args 回写：可解析 → 写回解析后的对象；不可解析 → 保留原字符串（对齐 CPA
    /// antigravityNativeFunctionCallJSON）。
    #[test]
    fn restore_parses_string_args_like_cpa() {
        for (raw, want) in [
            (json!("{\"p\":\"/s\"}"), json!({"p": "/s"})),
            (json!("not json"), json!("not json")),
        ] {
            let sid = gemini_claude_tool_use_id("native-s", "Read", Some(&raw));
            let mut contents = vec![
                json!({"role": "model", "parts": [{"functionCall": {"id": sid, "name": "Read", "args": {"p": "/s"}}}]}),
            ];
            let item = json!({"type": "function_call_part", "call_id": "native-s", "name": "Read",
                "args": raw, "thoughtSignature": SIG_A});
            let out = restore_native_function_call(&mut contents, 0, 0, &item, false);
            assert!(matches!(out, FunctionCallReplay::Identity(0)));
            assert_eq!(
                contents[0]["parts"][0]["functionCall"],
                json!({"id": "native-s", "name": "Read", "args": want})
            );
        }
    }

    /// 第 6 项：字符串 args 经 schema 路径（解析字符串内容）定位命中并回写对象。
    #[test]
    fn string_native_args_match_via_schema_and_write_back_object() {
        let base = base_request();
        let raw = json!("{\"p\":\"/s\"}");
        let sid = gemini_claude_tool_use_id("native-s", "Read", Some(&raw));
        let items = vec![
            json!({"type": "function_call_part", "call_id": "native-s", "name": "Read",
            "args": raw, "thoughtSignature": SIG_A, "contentIndex": 1, "partIndex": 0, "targetOccurrence": 0}),
        ];
        let mut req = base.clone();
        req["contents"].as_array_mut().unwrap().extend([
            json!({"role": "model", "parts": [{"functionCall": {"id": sid, "name": "Read", "args": {"p": "/s"}}}]}),
            json!({"role": "user", "parts": [{"functionResponse": {"id": sid, "name": "Read", "response": {}}}]}),
        ]);
        let original = read_tool();
        let report = prepare_ag_replay_request_with_tools(&mut req, Some(&items), ctx(&original));
        assert!(report.applied && report.degraded == 0);
        assert_eq!(
            req["contents"][1]["parts"][0]["functionCall"],
            json!({"id": "native-s", "name": "Read", "args": {"p": "/s"}})
        );
    }

    /// 第 5 项：pending 签名接收前检查条目 / 字节上限（对齐 CPA :2568）。
    #[test]
    fn pending_signature_respects_entry_limits() {
        let base = base_request();
        let text_part = json!({"response": {"candidates": [{"content": {"parts": [
            {"text": "hello", "thoughtSignature": SIG_TEXT}
        ]}}]}});

        let mut ok = AgReplayAccumulator::new_from_request(&base);
        ok.observe_response(&text_part);
        assert!(!ok.overflow);
        assert_eq!(ok.items().len(), 1);

        let mut by_items = AgReplayAccumulator::new_from_request(&base);
        by_items.items = vec![json!({}); MAX_ITEMS_PER_ENTRY];
        by_items.observe_response(&text_part);
        assert!(by_items.overflow);
        assert!(by_items.pending_signatures.is_empty());

        let mut by_bytes = AgReplayAccumulator::new_from_request(&base);
        by_bytes.item_bytes = MAX_BYTES_PER_ENTRY - 4;
        by_bytes.observe_response(&text_part);
        assert!(by_bytes.overflow);
        assert!(by_bytes.pending_signatures.is_empty());
        assert!(by_bytes.items().is_empty());
    }

    // ---- 角色归一（对齐 CPA normalizeAntigravityGeminiFunctionResponseRoles）----

    /// 移植 CPA TestNormalizeAntigravityGeminiFunctionResponseRolesOrdersParallelResponses。
    #[test]
    fn normalize_roles_orders_parallel_responses() {
        let mut req = json!({"contents": [
            {"role": "model", "parts": [
                {"functionCall": {"id": "call-1", "name": "read", "args": {"file": "one"}}},
                {"functionCall": {"id": "call-2", "name": "read", "args": {"file": "two"}}}]},
            {"role": " Model ", "parts": [
                {"functionResponse": {"id": "call-2", "name": "read", "response": {"result": "two"}}},
                {"functionResponse": {"id": "call-1", "name": "read", "response": {"result": "one"}}}]}
        ]});
        normalize_function_response_roles(&mut req);
        assert_eq!(req["contents"][1]["role"], "model");
        assert_eq!(
            req["contents"][1]["parts"][0]["functionResponse"]["id"],
            "call-1"
        );
        assert_eq!(
            req["contents"][1]["parts"][1]["functionResponse"]["id"],
            "call-2"
        );
        assert!(gemini_function_call_pairing_valid(&req));
    }

    /// 移植 CPA ...OrdersMixedParallelResponses：混合回合保留 user，response 前置、其余追加。
    #[test]
    fn normalize_roles_orders_mixed_parallel_responses() {
        let mut req = json!({"contents": [
            {"role": "model", "parts": [
                {"functionCall": {"id": "call-1", "name": "read", "args": {}}},
                {"functionCall": {"id": "call-2", "name": "read", "args": {}}}]},
            {"role": "user", "parts": [
                {"text": "results follow"},
                {"functionResponse": {"id": "call-2", "name": "read", "response": {}}},
                {"text": "continue"},
                {"functionResponse": {"id": "call-1", "name": "read", "response": {}}}]}
        ]});
        assert!(!gemini_function_call_pairing_valid(&req));
        normalize_function_response_roles(&mut req);
        let parts = &req["contents"][1]["parts"];
        assert_eq!(req["contents"][1]["role"], "user");
        assert_eq!(parts[0]["functionResponse"]["id"], "call-1");
        assert_eq!(parts[1]["functionResponse"]["id"], "call-2");
        assert_eq!(parts[2]["text"], "results follow");
        assert_eq!(parts[3]["text"], "continue");
        assert!(gemini_function_call_pairing_valid(&req));
    }

    /// 移植 CPA ...DoesNotCrossEmptyContentBoundary；另覆盖 functionResponse 补名。
    #[test]
    fn normalize_roles_respects_empty_boundary_and_repairs_names() {
        for boundary in [
            json!({"role": "user", "parts": []}),
            json!({"role": "user"}),
            json!({"role": "user", "parts": null}),
        ] {
            let mut req = json!({"contents": [
                {"role": "model", "parts": [
                    {"functionCall": {"id": "call-1", "name": "read", "args": {}}},
                    {"functionCall": {"id": "call-2", "name": "read", "args": {}}}]},
                boundary,
                {"role": "user", "parts": [
                    {"functionResponse": {"id": "call-2", "name": "read", "response": {}}},
                    {"functionResponse": {"id": "call-1", "name": "read", "response": {}}}]}
            ]});
            normalize_function_response_roles(&mut req);
            assert_eq!(req["contents"][2]["role"], "model");
            assert_eq!(
                req["contents"][2]["parts"][0]["functionResponse"]["id"],
                "call-2"
            );
            assert!(!gemini_function_call_pairing_valid(&req));
        }
        let mut req = json!({"contents": [
            {"role": "model", "parts": [{"functionCall": {"id": "c", "name": "Read", "args": {}}}]},
            {"role": "user", "parts": [{"functionResponse": {"id": "c", "name": "unknown", "response": {}}}]}
        ]});
        normalize_function_response_roles(&mut req);
        assert_eq!(
            req["contents"][1]["parts"][0]["functionResponse"]["name"],
            "Read"
        );
        assert!(gemini_function_call_pairing_valid(&req));
    }

    /// 角色归一让尾部「functionResponse 嵌媒体」回合变成 model → 补空 user（对齐 CPA
    /// prepare 之后的 EnsureGeminiTrailingUserContent）；纯 functionResponse 不补。
    #[test]
    fn prepare_appends_trailing_user_after_media_function_response() {
        let fr_media = json!({"functionResponse": {"id": "c", "name": "Read", "response": {},
            "parts": [{"inlineData": {"mimeType": "image/png", "data": "AA=="}}]}});
        let mut req = json!({"contents": [
            {"role": "user", "parts": [{"text": "hi"}]},
            {"role": "model", "parts": [{"thoughtSignature": GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR,
                "functionCall": {"id": "c", "name": "Read", "args": {}}}]},
            {"role": "user", "parts": [fr_media]}
        ]});
        let report = prepare_ag_replay_request(&mut req, None);
        assert!(report.pairing_error.is_none());
        let contents = req["contents"].as_array().unwrap();
        assert_eq!(contents.len(), 4);
        assert_eq!(contents[2]["role"], "model");
        assert_eq!(
            contents[3],
            json!({"role": "user", "parts": [{"text": ""}]})
        );

        let mut plain = json!({"contents": [
            {"role": "user", "parts": [{"text": "hi"}]},
            {"role": "model", "parts": [{"functionCall": {"id": "c", "name": "Read", "args": {}}}]},
            {"role": "user", "parts": [{"functionResponse": {"id": "c", "name": "Read", "response": {}}}]}
        ]});
        prepare_ag_replay_request(&mut plain, None);
        assert_eq!(plain["contents"].as_array().unwrap().len(), 3);
    }

    /// 放宽门控后，典型 Claude Code 历史经 ccextra 转换不应被本地 400：并行调用（结果乱序）、
    /// is_error 结果、多轮、无参工具、tool_result 与文本混排、中途 system 提醒、中断的工具调用。
    #[test]
    fn typical_claude_code_histories_pass_widened_validation() {
        let tools = json!([
            {"name": "Read", "input_schema": {"type": "object", "properties": {"file_path": {"type": "string"}}, "required": ["file_path"]}},
            {"name": "Bash", "input_schema": {"type": "object", "properties": {"command": {"type": "string"}}, "required": ["command"]}},
            {"name": "EnterPlanMode", "input_schema": {"type": "object", "properties": {}}}
        ]);
        let messages = json!([
            {"role": "user", "content": [{"type": "text", "text": "<system-reminder>ctx</system-reminder>"}, {"type": "text", "text": "fix the bug"}]},
            {"role": "assistant", "content": [
                {"type": "text", "text": "Reading both files."},
                {"type": "tool_use", "id": "toolu_a", "name": "Read", "input": {"file_path": "/a"}},
                {"type": "tool_use", "id": "toolu_b", "name": "Read", "input": {"file_path": "/b"}}]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_b", "content": "B"},
                {"type": "tool_result", "tool_use_id": "toolu_a", "content": [{"type": "text", "text": "A"}]},
                {"type": "text", "text": "<system-reminder>todo list</system-reminder>"}]},
            {"role": "assistant", "content": [{"type": "tool_use", "id": "toolu_c", "name": "Bash", "input": {"command": "false"}}]},
            {"role": "user", "content": [{"type": "tool_result", "tool_use_id": "toolu_c", "content": "exit 1", "is_error": true}]},
            {"role": "system", "content": "mid-session notice"},
            {"role": "assistant", "content": [{"type": "tool_use", "id": "toolu_d", "name": "EnterPlanMode", "input": {}}]},
            {"role": "user", "content": [
                {"type": "tool_result", "tool_use_id": "toolu_d", "content": "The user doesn't want to proceed with this tool use.", "is_error": true},
                {"type": "text", "text": "[Request interrupted by user for tool use]"}]},
            {"role": "assistant", "content": [{"type": "text", "text": "Stopped."}]},
            {"role": "user", "content": "continue please"}
        ]);
        for model in ["gemini-3-flash", "gemini-3.1-pro-high"] {
            let body = json!({"model": model, "max_tokens": 1024, "system": "sys", "tools": tools, "messages": messages});
            let (ag, _) = super::super::antigravity::convert_to_antigravity_with(
                &body,
                model,
                None,
                &[],
                None,
            );
            let mut request = ag["request"].clone();
            let report = prepare_ag_replay_request(&mut request, None);
            assert!(
                report.pairing_error.is_none(),
                "{model}: {:?}\n{request:#}",
                report.pairing_error
            );
            assert!(gemini_function_call_pairing_valid(&request));
            let count = |key: &str| {
                request["contents"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .flat_map(|c| content_parts(c).iter())
                    .filter(|p| p.get(key).is_some())
                    .count()
            };
            assert_eq!(
                (count("functionCall"), count("functionResponse")),
                (4, 4),
                "{model}"
            );
        }
    }

    // ---- 身份回写覆盖签名（对齐 CPA restoreAntigravityNativeFunctionCallReplay）----

    /// 身份回写分支无条件删掉 part 上已有签名（含 native）再写账本签名；账本无签名时
    /// 删掉不补。非身份分支仍保留已有 native 签名（CPA 同样跳过）。
    #[test]
    fn identity_restore_overwrites_existing_native_signature() {
        let base = base_request();
        let items = first_turn_items(&base);
        let (sa, sb) = (stable("native-a", "/a"), stable("native-b", "/b"));
        let mut req = second_turn_request(&base, [&sa, &sb]);
        req["contents"][1]["parts"][0]["thoughtSignature"] = json!("client-old-native-sig-aaaa");
        req["contents"][1]["parts"][1]["thoughtSignature"] = json!("client-old-native-sig-bbbb");
        req["contents"][1]["parts"][1]["extra_content"] =
            json!({"google": {"thought_signature": "client-old-extra-sig-bb"}});
        assert!(apply_ag_replay_items(&mut req, &items));
        let parts = &req["contents"][1]["parts"];
        assert_eq!(parts[0]["functionCall"]["id"], "native-a");
        assert_eq!(parts[0]["thoughtSignature"], SIG_A);
        assert_eq!(parts[1]["functionCall"]["id"], "native-b");
        assert!(parts[1].get("thoughtSignature").is_none());
        assert!(parts[1]["extra_content"]["google"]
            .get("thought_signature")
            .is_none());
    }

    /// 对齐 CPA eligibility filter :812-821 / 批量 sameNativeCall :613-616：id 与 args 已是原生
    /// 且（账本无签名 || part 已带 native 签名）→ 该条不合格，不覆盖也不删签名；part 上只有
    /// 哨兵时仍合格，换成账本签名。
    #[test]
    fn same_native_call_with_native_signature_is_noop() {
        let base = base_request();
        let items = first_turn_items(&base);
        let (sa, sb) = (stable("native-a", "/a"), stable("native-b", "/b"));
        let mut req = second_turn_request(&base, [&sa, &sb]);
        assert!(apply_ag_replay_items(&mut req, &items));
        assert_eq!(
            req["contents"][1]["parts"][0]["functionCall"]["id"],
            "native-a"
        );
        assert_eq!(
            req["contents"][1]["parts"][1]["functionCall"]["id"],
            "native-b"
        );

        // 账本有签名、part 已有（不同的）native 签名 → 保留 part 原签名
        req["contents"][1]["parts"][0]["thoughtSignature"] = json!("client-old-native-sig-cccc");
        // 账本无签名（native-b 条目）、part 有 native 签名 → 不删
        req["contents"][1]["parts"][1]["thoughtSignature"] = json!("client-old-native-sig-dddd");
        let before = req.clone();
        assert!(!apply_ag_replay_items(&mut req, &items));
        assert_eq!(req, before);

        // part 只有哨兵（非 native）→ 合格，走身份分支覆盖为账本签名
        req["contents"][1]["parts"][0]["thoughtSignature"] =
            json!(GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR);
        assert!(apply_ag_replay_items(&mut req, &items));
        assert_eq!(req["contents"][1]["parts"][0]["thoughtSignature"], SIG_A);
        assert_eq!(
            req["contents"][1]["parts"][1]["thoughtSignature"],
            "client-old-native-sig-dddd"
        );
    }

    // ---- prepare 内的 sanitize（对齐 CPA sanitizeAntigravityGeminiRequestSignatures）----

    /// prepare 先跑 SanitizeGeminiRequestThoughtSignatures：兄弟 call 哨兵 / 文本 part 外来签名 /
    /// functionResponse 签名被删，首 call 外来签名换哨兵；兼容 Gemini 签名保留。
    #[test]
    fn prepare_runs_cpa_signature_sanitize_first() {
        let gemini_sig = super::super::signature::fixtures::gemini_native_signature(&[1, 2, 3]);
        let mut req = json!({"contents": [
            {"role": "user", "parts": [{"text": "hi"}]},
            {"role": "model", "parts": [
                {"text": "thinking", "thought": true, "thoughtSignature": gemini_sig},
                {"text": "visible", "thoughtSignature": "foreign-claude-signature-xyz"},
                {"functionCall": {"id": "a", "name": "Read", "args": {}}, "thoughtSignature": "foreign-first-call-sig"},
                {"functionCall": {"id": "b", "name": "Read", "args": {}}, "thoughtSignature": GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR}
            ]},
            {"role": "user", "parts": [
                {"functionResponse": {"id": "a", "name": "Read", "response": {}}, "thoughtSignature": "fr-sig"},
                {"functionResponse": {"id": "b", "name": "Read", "response": {}}}
            ]}
        ]});
        let report = prepare_ag_replay_request(&mut req, None);
        assert!(report.pairing_error.is_none());
        let parts = &req["contents"][1]["parts"];
        assert_eq!(parts[0]["thoughtSignature"], gemini_sig.as_str());
        assert!(parts[1].get("thoughtSignature").is_none());
        assert_eq!(
            parts[2]["thoughtSignature"],
            GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR
        );
        assert!(parts[3].get("thoughtSignature").is_none());
        assert!(req["contents"][2]["parts"][0]
            .get("thoughtSignature")
            .is_none());
        // sanitize 补的哨兵不计入 repair（repair 只看 sanitize 之后仍无签名的首 call）
        assert_eq!(report.repaired, 0);
    }

    // ---- 流式累积器签名绑定（对齐 CPA observeResponsePayload :2521-2543 / :2565-2567）----
    // 期望值均由 CPA @3f0cef01 的 accumulator 对同一流跑出（逐字段一致，含 contextHash /
    // targetHash）。

    const CPA_PROBE_CONTEXT_HASH: &str =
        "8af1c24b05ed09dbad4d311be806121143c99f129bdcd489dab6ab7a9307ac7b";

    fn observe_stream_parts(parts: &[Value]) -> Vec<Value> {
        let request = json!({"contents": [{"role": "user", "parts": [{"text": "hi"}]}]});
        let mut acc = AgReplayAccumulator::new_from_request(&request);
        for part in parts {
            acc.observe_response(&json!({"response": {"candidates": [
                {"content": {"role": "model", "parts": [part]}}
            ]}}));
        }
        acc.observe_response(&json!({"response": {"candidates": [{"finishReason": "STOP"}]}}));
        acc.take_commit_items().expect("terminal")
    }

    fn cpa_text_sig_item(sig: &str, part_index: i64, kind: &str, hash: &str) -> Value {
        json!({
            "type": "thought_signature", "thoughtSignature": sig,
            "contentIndex": 1, "partIndex": part_index,
            "targetKind": kind, "targetHash": hash, "targetOccurrence": 0,
            "contextHash": CPA_PROBE_CONTEXT_HASH,
        })
    }

    /// thought 文本 → 空文本纯签名 part → functionCall：签名绑 thought 段，call 不带签名。
    #[test]
    fn stream_signature_only_part_between_thought_and_call_matches_cpa() {
        let items = observe_stream_parts(&[
            json!({"thought": true, "text": "plan"}),
            json!({"text": "", "thoughtSignature": "SIG_AAAAAAAAAAAAAAAA1"}),
            json!({"functionCall": {"id": "c1", "name": "Read", "args": {"p": "/a"}}}),
        ]);
        assert_eq!(
            items,
            vec![
                cpa_text_sig_item(
                    "SIG_AAAAAAAAAAAAAAAA1",
                    0,
                    "thought",
                    "a8ed2d255b54c51121d31ca685885b6e6f45c934ace68a42dfae56f42f2a9224",
                ),
                json!({
                    "type": "function_call_part", "name": "Read", "args": {"p": "/a"},
                    "call_id": "c1", "contentIndex": 1, "partIndex": 2, "targetOccurrence": 0,
                    "contextHash": CPA_PROBE_CONTEXT_HASH,
                }),
            ]
        );
    }

    /// 文本分块、签名夹在中间的空 part：签名绑前一段 "Hel"，后一段 "lo" 不带签名。
    #[test]
    fn stream_signature_in_middle_empty_part_binds_preceding_text_like_cpa() {
        let items = observe_stream_parts(&[
            json!({"text": "Hel"}),
            json!({"text": "", "thoughtSignature": "SIG_BBBBBBBBBBBBBBBB1"}),
            json!({"text": "lo"}),
        ]);
        assert_eq!(
            items,
            vec![cpa_text_sig_item(
                "SIG_BBBBBBBBBBBBBBBB1",
                0,
                "text",
                "e61f99d4bc650a6b036b8e102128847eef955b0936014a8c4bfd0f02e85c728e",
            )]
        );
    }

    /// :2565-2567 unmatchedDetachedCarrier：段刚落账后再来的纯签名 part 无文本可绑 → 丢弃，
    /// 不得绑到下一段 "B"。
    #[test]
    fn stream_unmatched_detached_carrier_is_discarded_like_cpa() {
        let items = observe_stream_parts(&[
            json!({"text": "A"}),
            json!({"text": "", "thoughtSignature": "SIG_CCCCCCCCCCCCCCCC1"}),
            json!({"text": "", "thoughtSignature": "SIG_CCCCCCCCCCCCCCCC2"}),
            json!({"text": "B"}),
        ]);
        assert_eq!(
            items,
            vec![cpa_text_sig_item(
                "SIG_CCCCCCCCCCCCCCCC1",
                0,
                "text",
                "4dbd9e5f0d691d2a58bd08f582864447133d4b26e653b4fcb34bd01d604075b3",
            )]
        );
    }

    /// :2521-2543 带签名文本丢弃未绑定前缀 pending：开头的无 kind 纯签名不会在收尾时被绑到
    /// 后续文本 " there"。
    #[test]
    fn stream_signed_text_drops_unbound_prefix_pending_like_cpa() {
        let items = observe_stream_parts(&[
            json!({"text": "", "thoughtSignature": "SIG_DDDDDDDDDDDDDDDD1"}),
            json!({"text": "Hi", "thoughtSignature": "SIG_DDDDDDDDDDDDDDDD2"}),
            json!({"text": " there"}),
        ]);
        assert_eq!(
            items,
            vec![cpa_text_sig_item(
                "SIG_DDDDDDDDDDDDDDDD2",
                1,
                "text",
                "bf79fcfbd51816c48f08ccd9788e4e107f3c719e8c784993f7b2b38183001598",
            )]
        );
    }

    // ---- G5 缺失 functionCall 插回（对齐 CPA mergeAntigravityFunctionCallPartReplayWithSchemas
    // :2163-2195 + filter :827-848）。期望 contents 由 CPA @3f0cef01 对同一输入跑出
    // （sanitize → applyAntigravityReasoningReplayItems → 角色归一）。----

    fn orphan_fr_content(sa: &str, sb: &str) -> Value {
        json!({"role": "user", "parts": [
            {"functionResponse": {"id": sa, "name": "Read", "response": {"result": "a"}}},
            {"functionResponse": {"id": sb, "name": "Read", "response": {"result": "b"}}}
        ]})
    }

    fn cpa_restored_fr_content() -> Value {
        json!({"role": "model", "parts": [
            {"functionResponse": {"id": "native-a", "name": "Read", "response": {"result": "a"}}},
            {"functionResponse": {"id": "native-b", "name": "Read", "response": {"result": "b"}}}
        ]})
    }

    fn restored_calls() -> [Value; 2] {
        [
            json!({"functionCall": {"id": "native-a", "name": "Read", "args": {"p": "/a"}}, "thoughtSignature": SIG_A}),
            json!({"functionCall": {"id": "native-b", "name": "Read", "args": {"p": "/b"}}}),
        ]
    }

    /// model 回合整段缺失：首条在 FR 前插 model 回合，第二条追加到刚插入的回合；FR 换回原生
    /// id；prepare 后配对有效（无账本时同一请求是 CPA 本地 400）。
    #[test]
    fn orphan_function_response_with_ledger_reinserts_model_turn_like_cpa() {
        let base = base_request();
        let items = first_turn_items(&base);
        let (sa, sb) = (stable("native-a", "/a"), stable("native-b", "/b"));
        let mut req = base.clone();
        req["contents"]
            .as_array_mut()
            .unwrap()
            .push(orphan_fr_content(&sa, &sb));

        let mut applied = req.clone();
        cpa_sanitize(&mut applied);
        assert!(!gemini_function_call_pairing_valid(&applied));
        assert!(apply_ag_replay_items(&mut applied, &items));
        normalize_function_response_roles(&mut applied);
        let [ca, cb] = restored_calls();
        assert_eq!(
            applied["contents"],
            json!([
                {"role": "user", "parts": [{"text": "hi"}]},
                {"role": "model", "parts": [ca, cb]},
                cpa_restored_fr_content(),
            ])
        );

        let mut prepared = req.clone();
        let report = prepare_ag_replay_request(&mut prepared, Some(&items));
        assert!(report.applied);
        assert!(!report.rolled_back);
        assert!(report.pairing_error.is_none(), "{:?}", report.pairing_error);
        assert_eq!(report.degraded, 0);
        assert!(gemini_function_call_pairing_valid(&prepared));
    }

    /// model 回合还在但 call 丢了：两条都追加到前一个 model 回合末尾（已有文本 part 保留）。
    #[test]
    fn orphan_function_response_appends_to_preceding_model_turn_like_cpa() {
        let base = base_request();
        let items = first_turn_items(&base);
        let (sa, sb) = (stable("native-a", "/a"), stable("native-b", "/b"));
        let mut req = base.clone();
        let contents = req["contents"].as_array_mut().unwrap();
        contents.push(json!({"role": "model", "parts": [{"text": "ok"}]}));
        contents.push(orphan_fr_content(&sa, &sb));

        cpa_sanitize(&mut req);
        assert!(apply_ag_replay_items(&mut req, &items));
        normalize_function_response_roles(&mut req);
        let [ca, cb] = restored_calls();
        assert_eq!(
            req["contents"],
            json!([
                {"role": "user", "parts": [{"text": "hi"}]},
                {"role": "model", "parts": [{"text": "ok"}, ca, cb]},
                cpa_restored_fr_content(),
            ])
        );
        assert!(gemini_function_call_pairing_valid(&req));
    }

    /// 无账本 / 上下文不一致 / 账本条目无原生 call_id → 不插回，仍是 CPA 本地 400 文案。
    #[test]
    fn orphan_function_response_without_admission_stays_cpa_400() {
        let base = base_request();
        let items = first_turn_items(&base);
        let (sa, sb) = (stable("native-a", "/a"), stable("native-b", "/b"));
        let mut req = base.clone();
        req["contents"]
            .as_array_mut()
            .unwrap()
            .push(orphan_fr_content(&sa, &sb));
        let expected = format!(
            "{AG_INVALID_FUNCTION_CALL_HISTORY_PREFIX}request.contents[1]: functionResponse without preceding functionCall"
        );

        // 无账本
        let mut no_ledger = req.clone();
        let report = prepare_ag_replay_request(&mut no_ledger, None);
        assert_eq!(report.pairing_error.as_deref(), Some(expected.as_str()));

        // 上下文漂移（system 变了）：账本命中 FR 但 contextHash 不一致 → 不插
        let mut drifted = req.clone();
        drifted["systemInstruction"]["parts"][0]["text"] = json!("other sys");
        let report = prepare_ag_replay_request(&mut drifted, Some(&items));
        assert!(!report.applied);
        assert_eq!(report.pairing_error.as_deref(), Some(expected.as_str()));

        // 条目无原生 call_id（CPA :2196-2200 只接受精确语义命中）
        let no_id_items: Vec<Value> = items
            .iter()
            .map(|item| {
                let mut item = item.clone();
                if let Some(obj) = item.as_object_mut() {
                    obj.remove("call_id");
                }
                item
            })
            .collect();
        let mut no_id = req.clone();
        let report = prepare_ag_replay_request(&mut no_id, Some(&no_id_items));
        assert!(!report.applied);
        assert!(report.pairing_error.is_some());
    }

    /// 原生 / 稳定 id 已在某个 functionCall 上（客户端改过该 call）→ 绝不再插第二份。
    #[test]
    fn existing_call_with_same_id_blocks_reinsert() {
        let base = base_request();
        let items = first_turn_items(&base);
        let sa = stable("native-a", "/a");
        let mut contents = vec![
            json!({"role": "user", "parts": [{"text": "hi"}]}),
            json!({"role": "model", "parts": [
                {"functionCall": {"id": sa, "name": "Read", "args": {"p": "/changed"}}}
            ]}),
            json!({"role": "user", "parts": [
                {"functionResponse": {"id": sa, "name": "Read", "response": {}}}
            ]}),
        ];
        let mut fp = ContextFingerprints::new(&base);
        let item_a = items.iter().find(|i| i["call_id"] == "native-a").unwrap();
        assert!(insert_missing_function_call(&mut contents, &mut fp, item_a, &sa).is_none());
        assert_eq!(contents.len(), 3);
    }

    // ---- 缺失 args 的出现次序键 / extra_content 签名路径 ----

    /// 缺失 args 与显式 `{}` 共用出现次序计数（与稳定 id / 账本 / converter 的 `{}` 一致），
    /// 同回合混用时 targetOccurrence 依次为 0、1；dedupe 键同样视为 `{}`。
    #[test]
    fn missing_args_share_occurrence_key_with_empty_object() {
        assert_eq!(
            function_call_occurrence_key(&json!({"name": "Read"})),
            function_call_occurrence_key(&json!({"name": "Read", "args": {}}))
        );
        let request = json!({"contents": [{"role": "user", "parts": [{"text": "hi"}]}]});
        let mut acc = AgReplayAccumulator::new_from_request(&request);
        acc.observe_response(&json!({"response": {"candidates": [{"content": {"parts": [
            {"functionCall": {"id": "x1", "name": "Read"}},
            {"functionCall": {"id": "x2", "name": "Read", "args": {}}}
        ]}, "finishReason": "STOP"}]}}));
        let items = acc.take_commit_items().unwrap();
        let occurrences: Vec<i64> = items
            .iter()
            .map(|i| i["targetOccurrence"].as_i64().unwrap())
            .collect();
        assert_eq!(occurrences, vec![0, 1]);
        assert!(items.iter().all(|i| i["args"] == json!({})));
    }

    /// 对齐 CPA antigravityNativePartThoughtSignature：三条路径取首个非空，含
    /// extra_content.google.thought_signature；空的 thoughtSignature 不挡后续路径。
    #[test]
    fn part_signature_reads_extra_content_and_skips_empty_paths() {
        let sig = "native-signature-extra-content-01";
        assert_eq!(
            part_thought_signature(
                &json!({"extra_content": {"google": {"thought_signature": sig}}})
            ),
            Some(sig)
        );
        assert_eq!(
            part_thought_signature(&json!({"thoughtSignature": " ", "thought_signature": sig})),
            Some(sig)
        );
        let request = json!({"contents": [{"role": "user", "parts": [{"text": "hi"}]}]});
        let mut acc = AgReplayAccumulator::new_from_request(&request);
        acc.observe_response(&json!({"response": {"candidates": [{"content": {"parts": [
            {"functionCall": {"id": "e1", "name": "Read", "args": {}},
             "extra_content": {"google": {"thought_signature": sig}}}
        ]}, "finishReason": "STOP"}]}}));
        let items = acc.take_commit_items().unwrap();
        assert_eq!(items[0]["thoughtSignature"], sig);

        // 同回合其它 part 上的同一签名（含 extra_content 路径）被摘掉
        let mut contents = vec![json!({"role": "model", "parts": [
            {"text": "t", "extra_content": {"google": {"thought_signature": sig}}},
            {"functionCall": {"id": "e1", "name": "Read", "args": {}}}
        ]})];
        assert!(apply_signature_at(&mut contents, 0, 1, sig));
        assert!(contents[0]["parts"][0]["extra_content"]["google"]
            .get("thought_signature")
            .is_none());
        assert_eq!(contents[0]["parts"][1]["thoughtSignature"], sig);
    }

    /// 角色归一的 no-op 输入原样返回：已按 call 顺序排好且 role 已是 model 的
    /// functionResponse 回合、带其它 part 的 user 回合、普通文本回合都不变。
    #[test]
    fn normalize_function_response_roles_noop_inputs_unchanged() {
        let original = json!({"contents": [
            {"role": "user", "parts": [{"text": "hi"}]},
            {"role": "model", "parts": [
                {"functionCall": {"id": "a", "name": "Read", "args": {"p": "/a"}}},
                {"functionCall": {"id": "b", "name": "Read", "args": {"p": "/b"}}}
            ]},
            {"role": "model", "parts": [
                {"functionResponse": {"id": "a", "name": "Read", "response": {"result": "1"}}},
                {"functionResponse": {"id": "b", "name": "Read", "response": {"result": "2"}}}
            ]},
            {"role": "model", "parts": [
                {"functionCall": {"id": "c", "name": "Bash", "args": {}}}
            ]},
            {"role": "user", "parts": [
                {"functionResponse": {"id": "c", "name": "Bash", "response": {"result": "ok"}}},
                {"text": "reminder"}
            ]},
            {"role": "model", "parts": [{"text": "done"}]}
        ]});
        let mut request = original.clone();
        normalize_function_response_roles(&mut request);
        assert_eq!(request, original);
        // 幂等：改写过一次的结果再跑一遍不变
        let mut reordered = json!({"contents": [
            {"role": "model", "parts": [
                {"functionCall": {"id": "a", "name": "Read", "args": {}}},
                {"functionCall": {"id": "b", "name": "Read", "args": {}}}
            ]},
            {"role": "user", "parts": [
                {"functionResponse": {"id": "b", "name": "Read", "response": {}}},
                {"functionResponse": {"id": "a", "name": "Read", "response": {}}}
            ]}
        ]});
        normalize_function_response_roles(&mut reordered);
        assert_eq!(reordered["contents"][1]["role"], "model");
        assert_eq!(
            reordered["contents"][1]["parts"][0]["functionResponse"]["id"],
            "a"
        );
        let once = reordered.clone();
        normalize_function_response_roles(&mut reordered);
        assert_eq!(reordered, once);
    }
}
