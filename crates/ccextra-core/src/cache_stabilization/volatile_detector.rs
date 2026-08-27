//! PR-E5：易变内容检测与剥离。
//!
//! 扫描入站 LLM 请求体，查找当它们出现在缓存前缀（cached prefix）中时
//! 已知会破坏 prompt-cache 命中的子串——即 system prompt、tool 定义、
//! 历史消息所在的缓存前缀：
//!
//!   1. **ISO-8601 时间戳**（`YYYY-MM-DDTHH:MM:SS...`）——几乎总是
//!      每个请求都新鲜渲染，因此任何包含它的前缀命中缓存都是偶然的。
//!      1b. **仅日期**（`YYYY-MM-DD` 或 `YYYY/MM/DD`）——不含时间部分
//!      的 10 字节日历日期。常见于 system prompt（"Today's date is
//!      2026/06/13"）。每天都会变化。
//!   2. **UUID v4**（`xxxxxxxx-xxxx-4xxx-yxxx-xxxxxxxxxxxx`）——位置 14 的
//!      version-4 半字节将调用方每次请求生成的 UUID 与随机十六进制字符串
//!      区分开来（构建哈希通常是 v0，固定标识符在请求之间根本不会变化）。
//!   3. **以 ID 命名的 JSON 字段**——键名匹配已知易变字段名之一的键
//!      （`request_id`、`trace_id`、`session_id`、`correlation_id`）。
//!      即使普通文本字段中嵌入的非空 UUID 也会被 (1)/(2) 捕获；
//!      此规则捕获易变子串扫描会漏掉的值（例如整数 trace ID、自定义
//!      slug 格式）。
//!   4. **内联 ID 前缀**——自由文本（tool 描述、system prompt）中以已知
//!      易变前缀（`req-`、`msg_`、`call_`）开头、后接包含至少一个数字的
//!      每请求唯一字母数字后缀的子串。与针对 JSON *键* 名的规则 (3) 不同，
//!      此规则针对嵌入字符串 *内容* 中的 ID token。
//!
//! # 变更策略
//!
//! **检测**函数（`detect_volatile_content`、`scan_string`）只读——
//! 它们接收 `&Value` 且从不修改。
//! **改写**函数（[`normalize_client_dateline`]）把 "Today's date is …"
//! 指纹句还原为 ASCII 撇号 + 连字符日期（对齐 sub2api anthropicfp）。
//! 占位符式剥离（`strip_volatile_from_prefix`）已下线并移除：
//! 替换改变上游可见语义，退出。
//!
//! # 检测策略
//!
//! - **不使用正则。** Realignment 构建约束策略禁止用正则做解析
//!   （它会隐藏意图并拖慢冷启动）。每个模式都通过显式的字节位置检查来识别。
//! - **每个请求最多捕获 10 条。** 嘈杂的客户负载（设想：粘贴进 system
//!   prompt 的 CSV）否则可能产生数百条警告，淹没日志。上限是保守的；
//!   实际上前 1-3 条结果才是客户会采取行动的。
//! - **样本截断到 80 字符。** 我们记录一小段切片，以便客户定位违规内容，
//!   但绝不记录大批量客户数据。
//! - **路径作用域。** OpenAI 与 Anthropic 的 JSON 请求体结构不同；
//!   [`ApiKind`] 枚举选择正确的遍历器。Bedrock / Vertex 等会放在
//!   Phase E 的后续工作中——本 PR 保持接口面紧凑。

use serde_json::Value;

/// 易变遍历器的简化二值视图：Anthropic 一次遍历，所有 OpenAI 形态共享另一次。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiKind {
    Anthropic,
    OpenAi,
}

impl std::fmt::Display for ApiKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiKind::Anthropic => write!(f, "anthropic"),
            ApiKind::OpenAi => write!(f, "openai"),
        }
    }
}

/// 每个请求最多报告的结果条数。参见模块文档中的理由说明。
pub const MAX_FINDINGS_PER_REQUEST: usize = 10;

/// 每条结果我们记录的 `sample` 最大字节数。取一小段摘录，
/// 让运维人员能定位违规内容，同时不暴露大批量客户数据。
pub const SAMPLE_TRUNCATE_BYTES: usize = 80;

/// 约定上每请求唯一的 JSON 字段名。对键的 *子串* 进行不区分大小写的匹配
/// ——名为 `"x_request_id"` 或 `"meta.session_id"` 的键也会被捕获。
const ID_FIELD_NEEDLES: &[&str] = &["request_id", "trace_id", "session_id", "correlation_id"];

/// 自由文本中常作为每请求标识符出现的内联 ID 前缀。当这些前缀后接一个
/// 包含至少一个数字（表示随机性）的后缀时，整个 token 被视为易变。
const INLINE_ID_PREFIXES: &[&str] = &["req-", "msg_", "call_"];

/// 内联 ID 前缀之后的最小后缀长度，只有达到该长度 token 才被视为易变。
/// 太短会在稳定术语上误报（例如 "req-1a" 前缀后只有 2 个字符）。
const MIN_INLINE_ID_SUFFIX: usize = 3;

/// 我们找到的易变内容种类。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VolatileKind {
    /// ISO-8601 时间戳形态：位置 4=`-`、7=`-`、10=`T`、13=:`、16=:`。
    Timestamp,
    /// 仅日期：不含时间部分的 10 字节 `YYYY-MM-DD` 或 `YYYY/MM/DD`。
    /// 每天变化；在 system prompt 中易变。
    DateOnly,
    /// UUID v4 形态：36 字符、十六进制、连字符位于 8/13/18/23，位置 14
    /// 为 version 半字节 `4`。
    Uuid,
    /// JSON 键名包含其中一个约定每请求 ID 指针（`request_id`、
    /// `trace_id`、`session_id`、`correlation_id`）。
    IdField,
    /// 自由文本中以已知前缀（`req-`、`msg_`、`call_`）开头、后接包含至少
    /// 一个数字的每请求唯一字母数字后缀的内联 ID token。
    InlineId,
}

impl VolatileKind {
    /// 用于结构化日志的稳定字符串表示。仪表盘中的检测规则依赖此值过滤；
    /// 没有弃用说明就不要更改这些字符串。
    pub fn as_str(self) -> &'static str {
        match self {
            VolatileKind::Timestamp => "iso8601_timestamp",
            VolatileKind::DateOnly => "date_only",
            VolatileKind::Uuid => "uuid_v4",
            VolatileKind::IdField => "id_field",
            VolatileKind::InlineId => "inline_id",
        }
    }
}

/// 一条易变内容发现。
///
/// `location` 是 JSON-pointer 风格的路径，客户可将警告映射回其请求形态中
/// 的确切字段（例如 `system[2].text`、`messages[0].content[1].text`、
/// `tools[0].input_schema.properties.session_id`）。sample 是截断后的摘录；
/// 由 [`SAMPLE_TRUNCATE_BYTES`] 限制其长度。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolatileFinding {
    pub kind: VolatileKind,
    pub location: String,
    pub sample: String,
}

/// 公开的检测入口点。
///
/// 针对给定的 API 形态遍历已解析的请求体，返回最多
/// [`MAX_FINDINGS_PER_REQUEST`] 条结果。调用方负责传入 *已解析* 的请求体
/// ——在此热路径上重新解析会使 JSON 成本翻倍。
pub fn detect_volatile_content(body: &Value, kind: ApiKind) -> Vec<VolatileFinding> {
    let mut findings: Vec<VolatileFinding> = Vec::new();
    match kind {
        ApiKind::Anthropic => walk_anthropic(body, &mut findings),
        ApiKind::OpenAi => walk_openai(body, &mut findings),
    }
    findings
}

/// 每条结果发出一次 `tracing::warn!`，使用稳定的结构化形态。
/// 运维/客户在其日志搜索中消费 `event="volatile_content_detected"`，
/// 以发现会破坏缓存的内容。
pub fn emit_volatile_warnings(findings: &[VolatileFinding], request_id: &str) {
    for finding in findings {
        tracing::warn!(
            event = "volatile_content_detected",
            request_id = %request_id,
            kind = finding.kind.as_str(),
            location = %finding.location,
            sample = %finding.sample,
            "volatile content in cached prefix will bust prompt-cache hits; \
             move per-request IDs/timestamps to message metadata or post-prefix \
             fields"
        );
    }
}

// ─── Anthropic walker ──────────────────────────────────────────────────

fn walk_anthropic(body: &Value, out: &mut Vec<VolatileFinding>) {
    if !out.is_empty() && out.len() >= MAX_FINDINGS_PER_REQUEST {
        return;
    }
    // 只扫描缓存的 PREFIX 区域：system + tools。
    // messages[] 故意不扫描：cache prefix 仅由 system + tools 派生，
    // messages 内的易变内容不破坏 prefix，扫描只会产生噪音
    // （例如历史 tool_result 时间戳/UUID）。
    // system: string | array of content blocks
    if let Some(system) = body.get("system") {
        scan_value_for_strings(system, "system", out);
    }
    // tools[].description + tools[].input_schema
    if let Some(Value::Array(tools)) = body.get("tools") {
        for (i, tool) in tools.iter().enumerate() {
            if out.len() >= MAX_FINDINGS_PER_REQUEST {
                return;
            }
            if let Some(Value::String(desc)) = tool.get("description") {
                let loc = format!("tools[{i}].description");
                scan_string(desc, &loc, out);
            }
            if let Some(schema) = tool.get("input_schema") {
                let loc = format!("tools[{i}].input_schema");
                scan_value_recursive(schema, &loc, out);
            }
        }
    }
}

// ─── OpenAI walker ─────────────────────────────────────────────────────

fn is_responses_direct_tool(tool: &Value) -> bool {
    tool.get("parameters").is_some()
        || (tool.get("type").is_some() && tool.get("input_schema").is_none())
}

fn walk_openai(body: &Value, out: &mut Vec<VolatileFinding>) {
    // 只扫描缓存的 PREFIX 区域：instructions（Responses 的 system prompt）
    // + tools。messages[] / input[] 中的条目故意不扫描：
    // cache prefix 仅由 instructions + tools 派生，扫描只会产生噪音
    // （历史 tool_result 时间戳/UUID）。
    if let Some(Value::String(instructions)) = body.get("instructions") {
        scan_string(instructions, "instructions", out);
    } else if let Some(instructions) = body.get("instructions") {
        scan_value_for_strings(instructions, "instructions", out);
    }
    // tools[].function.description + tools[].function.parameters
    // Responses 直接 tool 形态：tools[].description + tools[].parameters
    if let Some(Value::Array(tools)) = body.get("tools") {
        for (i, tool) in tools.iter().enumerate() {
            if out.len() >= MAX_FINDINGS_PER_REQUEST {
                return;
            }
            if let Some(function) = tool.get("function") {
                if let Some(Value::String(desc)) = function.get("description") {
                    let loc = format!("tools[{i}].function.description");
                    scan_string(desc, &loc, out);
                }
                if let Some(params) = function.get("parameters") {
                    let loc = format!("tools[{i}].function.parameters");
                    scan_value_recursive(params, &loc, out);
                }
            } else if is_responses_direct_tool(tool) {
                if let Some(Value::String(desc)) = tool.get("description") {
                    let loc = format!("tools[{i}].description");
                    scan_string(desc, &loc, out);
                }
                if let Some(params) = tool.get("parameters") {
                    let loc = format!("tools[{i}].parameters");
                    scan_value_recursive(params, &loc, out);
                }
            }
        }
    }
}

// ─── Generic walkers ───────────────────────────────────────────────────

/// 扫描一个可能是字符串、内容块数组或其他形态的 [`Value`]。
/// 字符串会被扫描以查找易变子串；对象/块数组会被递归。
fn scan_value_for_strings(v: &Value, location: &str, out: &mut Vec<VolatileFinding>) {
    if out.len() >= MAX_FINDINGS_PER_REQUEST {
        return;
    }
    match v {
        Value::String(s) => scan_string(s, location, out),
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                if out.len() >= MAX_FINDINGS_PER_REQUEST {
                    return;
                }
                let nested = format!("{location}[{i}]");
                scan_value_recursive(item, &nested, out);
            }
        }
        Value::Object(_) => scan_value_recursive(v, location, out),
        _ => {}
    }
}

/// 递归遍历 [`Value`]，既用于字符串内容扫描，也用于以 ID 命名的键检测。
/// 这是唯一会检查键的遍历器：tool input_schema / function parameters /
/// 嵌套内容块都会流经这里。
fn scan_value_recursive(v: &Value, location: &str, out: &mut Vec<VolatileFinding>) {
    if out.len() >= MAX_FINDINGS_PER_REQUEST {
        return;
    }
    match v {
        Value::String(s) => scan_string(s, location, out),
        Value::Array(items) => {
            for (i, item) in items.iter().enumerate() {
                if out.len() >= MAX_FINDINGS_PER_REQUEST {
                    return;
                }
                let nested = format!("{location}[{i}]");
                scan_value_recursive(item, &nested, out);
            }
        }
        Value::Object(map) => {
            for (k, sub) in map.iter() {
                if out.len() >= MAX_FINDINGS_PER_REQUEST {
                    return;
                }
                if is_id_named_key(k) && !is_value_empty(sub) {
                    out.push(VolatileFinding {
                        kind: VolatileKind::IdField,
                        location: format!("{location}.{k}"),
                        sample: truncate_sample(&value_to_sample(sub)),
                    });
                    if out.len() >= MAX_FINDINGS_PER_REQUEST {
                        return;
                    }
                }
                let nested = format!("{location}.{k}");
                scan_value_recursive(sub, &nested, out);
            }
        }
        _ => {}
    }
}

/// 扫描字符串中的 ISO-8601 时间戳和 UUID v4 子串。
/// 同一字符串中的多处出现各自产生一条结果，直到达到全局上限。
fn scan_string(s: &str, location: &str, out: &mut Vec<VolatileFinding>) {
    let bytes = s.as_bytes();
    let len = bytes.len();
    // ISO-8601 最小窗口：`YYYY-MM-DDTHH:MM:SS` = 19 字节。
    // UUID v4 窗口：36 字节。
    // 按字节索引遍历；两个检查都是纯字节位置查找。
    let mut i = 0usize;
    while i < len {
        if out.len() >= MAX_FINDINGS_PER_REQUEST {
            return;
        }
        // 先尝试 ISO-8601（窗口更短，当字符串在 UUID 中途结束时漏配更少）。
        if i + 19 <= len && looks_like_iso8601(&bytes[i..i + 19]) {
            let end = (i + 19).min(len);
            out.push(VolatileFinding {
                kind: VolatileKind::Timestamp,
                location: location.to_string(),
                sample: truncate_sample(&s[i..end]),
            });
            i += 19;
            continue;
        }
        // 仅日期：YYYY-MM-DD 或 YYYY/MM/DD（10 字节）。
        // 在 ISO-8601 之后检查，以避免对完整时间戳的日期前缀进行双重匹配。
        if looks_like_date_only(s, i) {
            let end = (i + 10).min(len);
            out.push(VolatileFinding {
                kind: VolatileKind::DateOnly,
                location: location.to_string(),
                sample: truncate_sample(&s[i..end]),
            });
            i += 10;
            continue;
        }
        if i + 36 <= len && looks_like_uuid_v4(&bytes[i..i + 36]) {
            out.push(VolatileFinding {
                kind: VolatileKind::Uuid,
                location: location.to_string(),
                sample: truncate_sample(&s[i..i + 36]),
            });
            i += 36;
            continue;
        }
        // 检查内联 ID 前缀（req-, msg_, call_）
        if let Some(end) = find_inline_id_at(s, i) {
            out.push(VolatileFinding {
                kind: VolatileKind::InlineId,
                location: location.to_string(),
                sample: truncate_sample(&s[i..end]),
            });
            i = end;
            continue;
        }
        i += 1;
    }
}

/// 19 字节窗口是否为 ISO-8601 时间戳前缀？
/// 位置 0..4：4 个 ASCII 数字（年）。
/// 位置 4：`-`。
/// 位置 5..7：2 个 ASCII 数字（月）。
/// 位置 7：`-`。
/// 位置 8..10：2 个 ASCII 数字（日）。
/// 位置 10：`T` 或 `t` 或 ` `（空格——RFC 3339 §5.6 允许）。
/// 位置 11..13：2 个 ASCII 数字（时）。
/// 位置 13：`:`。
/// 位置 14..16：2 个 ASCII 数字（分）。
/// 位置 16：`:`。
/// 位置 17..19：2 个 ASCII 数字（秒）。
fn looks_like_iso8601(window: &[u8]) -> bool {
    if window.len() < 19 {
        return false;
    }
    let digits_in =
        |range: std::ops::Range<usize>| -> bool { window[range].iter().all(u8::is_ascii_digit) };
    digits_in(0..4)
        && window[4] == b'-'
        && digits_in(5..7)
        && window[7] == b'-'
        && digits_in(8..10)
        && (window[10] == b'T' || window[10] == b't' || window[10] == b' ')
        && digits_in(11..13)
        && window[13] == b':'
        && digits_in(14..16)
        && window[16] == b':'
        && digits_in(17..19)
}

/// 10 字节窗口是否为仅日期值（`YYYY-MM-DD` 或 `YYYY/MM/DD`）？
///
/// 必不能被 `T`/`t` 跟随（那表示完整 ISO-8601 时间戳，由
/// [`looks_like_iso8601`] 处理）。位置 11-12 上空格后跟两位数字也表明是
/// 空格分隔的日期时间（`2026-06-13 10:30:00`），由 19 字节检测器处理。
///
/// 基本合理性：月份 01-12，日期 01-31。
fn looks_like_date_only(s: &str, pos: usize) -> bool {
    let bytes = s.as_bytes();
    if pos + 10 > bytes.len() {
        return false;
    }
    let w = &bytes[pos..pos + 10];
    let is_digit = |i: usize| w[i].is_ascii_digit();
    // YYYY?S?MM?S?DD，其中 S 为 `-` 或 `/`
    if !(is_digit(0) && is_digit(1) && is_digit(2) && is_digit(3)) {
        return false;
    }
    let sep = w[4];
    if sep != b'-' && sep != b'/' {
        return false;
    }
    if !(is_digit(5) && is_digit(6)) {
        return false;
    }
    if w[7] != sep {
        return false; // 分隔符需一致
    }
    if !(is_digit(8) && is_digit(9)) {
        return false;
    }
    // 合理性：月份 01-12
    let month: u8 = (w[5] - b'0') * 10 + (w[6] - b'0');
    if !(1..=12).contains(&month) {
        return false;
    }
    // 合理性：日期 01-31
    let day: u8 = (w[8] - b'0') * 10 + (w[9] - b'0');
    if !(1..=31).contains(&day) {
        return false;
    }
    // 反向断言：若后跟 T/t，则这是完整 ISO-8601 时间戳
    // （由 19 字节检测器处理）。
    let after = pos + 10;
    if after < bytes.len() {
        let c = bytes[after];
        if c == b'T' || c == b't' {
            return false;
        }
        // 空格分隔的日期时间：`2026-06-13 10:30:00`
        if c == b' '
            && after + 3 <= bytes.len()
            && bytes[after + 1].is_ascii_digit()
            && bytes[after + 2].is_ascii_digit()
        {
            return false;
        }
    }
    true
}

/// 36 字节窗口是否为 UUID v4？
/// 连字符位于 8、13、18、23。
/// 位置 14 = `4`（version 半字节）。
/// 位置 19 属于 `{8, 9, a, b, A, B}`（按 RFC 4122 §4.4 的 variant 半字节）。
/// 其余所有位置：ASCII 十六进制。
fn looks_like_uuid_v4(window: &[u8]) -> bool {
    if window.len() < 36 {
        return false;
    }
    if window[8] != b'-' || window[13] != b'-' || window[18] != b'-' || window[23] != b'-' {
        return false;
    }
    if window[14] != b'4' {
        return false;
    }
    match window[19] {
        b'8' | b'9' | b'a' | b'b' | b'A' | b'B' => {}
        _ => return false,
    }
    for (i, &c) in window.iter().enumerate().take(36) {
        if i == 8 || i == 13 || i == 18 || i == 23 {
            continue;
        }
        if !c.is_ascii_hexdigit() {
            return false;
        }
    }
    true
}

/// 该字节是否内联 ID 后缀中允许的字符？
/// ASCII 字母数字、连字符和下划线是典型的 ID 字符。
fn is_inline_id_char(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'-' || c == b'_'
}

/// 检查字符串 `s` 在位置 `start` 处是否有一个内联 ID token。
/// 若找到易变内联 ID，则返回结束位置（不含）；否则返回 `None`。
///
/// 易变内联 ID 为：已知前缀之一（`req-`、`msg_`、`call_`）后接至少
/// [`MIN_INLINE_ID_SUFFIX`] 个 ID 字符的后缀，且该后缀包含至少一个
/// ASCII 数字（随机 ID 总是带数字；"req-body" 等稳定词则没有）。
fn find_inline_id_at(s: &str, start: usize) -> Option<usize> {
    // 确保 start 位于 UTF-8 字符边界上
    if !s.is_char_boundary(start) {
        return None;
    }
    let rest = &s[start..];
    for prefix in INLINE_ID_PREFIXES {
        if rest.len() < prefix.len() + MIN_INLINE_ID_SUFFIX {
            continue;
        }
        if !rest.starts_with(prefix) {
            continue;
        }
        // 越过前缀，扫描 ID 字符
        let mut end = start + prefix.len();
        let bytes = s.as_bytes();
        let mut has_digit = false;
        while end < bytes.len() && is_inline_id_char(bytes[end]) {
            if bytes[end].is_ascii_digit() {
                has_digit = true;
            }
            end += 1;
        }
        let suffix_len = end - start - prefix.len();
        if suffix_len >= MIN_INLINE_ID_SUFFIX && has_digit {
            return Some(end);
        }
    }
    None
}

/// JSON 键名是否匹配其中一个约定每请求 ID 指针？不区分大小写的子串匹配。
fn is_id_named_key(key: &str) -> bool {
    let lowered = key.to_ascii_lowercase();
    ID_FIELD_NEEDLES
        .iter()
        .any(|needle| lowered.contains(needle))
}

/// 将空字符串、空数组/对象以及 null 视为"无值"，
/// 这样 ID 字段规则不会对 *声明* 了 `request_id` 字段但传入 `""` 的
/// schema 误报。
fn is_value_empty(v: &Value) -> bool {
    match v {
        Value::Null => true,
        Value::String(s) => s.is_empty(),
        Value::Array(a) => a.is_empty(),
        Value::Object(m) => m.is_empty(),
        _ => false,
    }
}

/// 将 JSON 值呈现为短样本字符串。字符串原样透传（随后截断）；
/// 其他基本类型走 `to_string`。对象/数组呈现为紧凑 JSON。
fn value_to_sample(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => "null".to_string(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        // 紧凑 JSON 使样本保持短小；下面的截断步骤无论如何都会限制其长度。
        _ => v.to_string(),
    }
}

/// 将 `s` 截断到最多 [`SAMPLE_TRUNCATE_BYTES`] 字节，
/// 尊重 UTF-8 边界（在码点中间截断会产生非法 UTF-8，稍后写入日志时会 panic）。
fn truncate_sample(s: &str) -> String {
    if s.len() <= SAMPLE_TRUNCATE_BYTES {
        return s.to_string();
    }
    // 找到 <= SAMPLE_TRUNCATE_BYTES 的最大字符边界。
    let mut cut = SAMPLE_TRUNCATE_BYTES;
    while cut > 0 && !s.is_char_boundary(cut) {
        cut -= 1;
    }
    let mut out = String::with_capacity(cut + 1);
    out.push_str(&s[..cut]);
    out.push('…');
    out
}

/// 客户端 dateline 归一化（对齐 CLIProxyAPI/sub2api `anthropicfp`）。
///
/// 部分类 CC 客户端检测到非官方 base URL 时，会往
/// `Today's date is YYYY-MM-DD.` 这一句注入隐写信号：4 种撇号码点
/// （ASCII `'`、`’` U+2019、`ʼ` U+02BC、`ʹ` U+02B9）× 2 种分隔符
/// （`-`/`/`）。本函数把命中句整段还原为 ASCII 撇号 + 连字符形式；
/// 其余文本（用户 prose、tool_result、代码块）一字不动。
///
/// 作用域（对齐真实客户端放置位置）：
/// - Anthropic：`system`（字符串或 text 块）全量；`messages[].content` 的
///   text 仅扫 `<system-reminder>` 块内（CC 第 2 轮起 dateline 藏在那里）
/// - OpenAI：`instructions` 全量；`messages[]` / `input[]` 的
///   system/developer 同上 reminder scoping；tools 不扫
///
/// 已规范的句子原样保留（identity）。返回发生改写的文本块个数
/// （单块内命中多句计 1）。
pub fn normalize_client_dateline(body: &mut Value, kind: ApiKind) -> usize {
    match kind {
        ApiKind::Anthropic => normalize_dateline_anthropic(body),
        ApiKind::OpenAi => normalize_dateline_openai(body),
    }
}

fn normalize_dateline_anthropic(body: &mut Value) -> usize {
    let mut count = 0;
    if let Some(system) = body.get_mut("system") {
        match system {
            Value::String(s) => count += normalize_dateline_text(s, false),
            Value::Array(blocks) => {
                for block in blocks.iter_mut() {
                    if let Some(Value::String(text)) = block.get_mut("text") {
                        count += normalize_dateline_text(text, false);
                    }
                }
            }
            _ => {}
        }
    }

    if let Some(Value::Array(messages)) = body.get_mut("messages") {
        for message in messages.iter_mut() {
            if let Some(content) = message.get_mut("content") {
                count += normalize_dateline_message_content(content);
            }
        }
    }
    count
}

fn normalize_dateline_openai(body: &mut Value) -> usize {
    let mut count = 0;

    if let Some(Value::String(instructions)) = body.get_mut("instructions") {
        count += normalize_dateline_text(instructions, false);
    }

    for key in ["messages", "input"] {
        if let Some(Value::Array(items)) = body.get_mut(key) {
            for item in items.iter_mut() {
                // 角色不过滤，与 sub2api 一致：改写只发生在
                // <system-reminder> 块内，由 scoping 守门
                if let Some(content) = item.get_mut("content") {
                    count += normalize_dateline_message_content(content);
                }
            }
        }
    }
    count
}

/// 单条消息 content：字符串或块数组。text 块按 reminder scoping 处理。
fn normalize_dateline_message_content(content: &mut Value) -> usize {
    match content {
        Value::String(s) => normalize_dateline_text(s, true),
        Value::Array(blocks) => blocks
            .iter_mut()
            .filter_map(|block| block.get_mut("text"))
            .map(normalize_dateline_message_content)
            .sum(),
        _ => 0,
    }
}

const DATELINE_PREFIX_ASCII: &[u8] = b"Today";
const REMINDER_OPEN: &str = "<system-reminder>";
const REMINDER_CLOSE: &str = "</system-reminder>";

/// 撇号码点：ASCII + 三种 Unicode 变体（隐写信号）。
/// 变体为多字节 UTF-8，长度 1/2/3 字节，按字节序从长到短排列避免前缀吞并。
const DATELINE_APOSTROPHES: &[&str] = &["\u{2019}", "\u{02BC}", "\u{02B9}", "'"];

/// 在 `pos` 处解析 dateline 尾部：撇号 + "s date is YYYY?MM?DD."。
/// 两分隔符必须一致。命中时返回 (句末偏移, 规范形式)。
fn parse_dateline_at(seg: &str, bytes: &[u8], pos: usize) -> Option<(usize, String)> {
    let mut apo_end = pos;
    let mut matched = false;
    for candidate in DATELINE_APOSTROPHES {
        if bytes[pos..].starts_with(candidate.as_bytes()) {
            matched = true;
            apo_end = pos + candidate.len();
            break;
        }
    }
    if !matched {
        return None;
    }
    if !bytes[apo_end..].starts_with(b"s date is ") {
        return None;
    }
    let date_start = apo_end + b"s date is ".len();
    if date_start + 11 > bytes.len() {
        return None;
    }
    // YYYY S MM S DD . （两分隔符一致）
    let w = &bytes[date_start..date_start + 11];
    let digit = |i: usize| w[i].is_ascii_digit();
    if !(digit(0) && digit(1) && digit(2) && digit(3)) {
        return None;
    }
    let sep = w[4];
    if sep != b'-' && sep != b'/' {
        return None;
    }
    if !(digit(5) && digit(6)) || w[7] != sep || !(digit(8) && digit(9)) || w[10] != b'.' {
        return None;
    }
    let year = &seg[date_start..date_start + 4];
    let month = &seg[date_start + 5..date_start + 7];
    let day = &seg[date_start + 8..date_start + 10];
    // 规范形式恒用 ASCII 撇号 + 连字符（对齐 sub2api canonicalize）
    Some((
        date_start + 11,
        format!("Today's date is {year}-{month}-{day}."),
    ))
}

/// 在 `from` 起的字节偏移之后查找子串，返回绝对字节下标。
fn find_sub(hay: &[u8], from: usize, needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || from >= hay.len() {
        return None;
    }
    hay[from..]
        .windows(needle.len())
        .position(|w| w == needle)
        .map(|p| p + from)
}

/// 在字符串内改写全部指纹 dateline 句。
///
/// `reminder_scoped_only` 为 true 时只处理 `<system-reminder>` 块内的子串
/// （messages 内容用），false 时全量（system/instructions 用）。
fn normalize_dateline_text(s: &mut String, reminder_scoped_only: bool) -> usize {
    let hay = s.clone();
    let mut regions: Vec<(usize, usize)> = Vec::new();
    if reminder_scoped_only {
        let bytes = hay.as_bytes();
        let mut cursor = 0usize;
        while let Some(rel) = find_sub(bytes, cursor, REMINDER_OPEN.as_bytes()) {
            let start = rel + REMINDER_OPEN.len();
            let end_rel = find_sub(bytes, start, REMINDER_CLOSE.as_bytes());
            let end = end_rel.unwrap_or(hay.len()).min(hay.len());
            cursor = end.min(hay.len());
            regions.push((start, end));
            if end >= hay.len() {
                break;
            }
        }
    } else {
        regions.push((0, hay.len()));
    }

    let mut changed = false;
    let mut out = String::with_capacity(hay.len());
    let mut last = 0usize;
    for (start, end) in regions {
        out.push_str(&hay[last..start]);
        let (seg, seg_changed) = rewrite_datelines(&hay[start..end]);
        out.push_str(&seg);
        changed |= seg_changed;
        last = end;
    }
    out.push_str(&hay[last..]);
    if !changed {
        return 0;
    }
    *s = out;
    1
}

/// 扫描一段文本，把每个指纹 dateline 句还原为规范形式。
/// 返回 (新文本, 是否有改写)；已规范的句子原样拼回。
fn rewrite_datelines(seg: &str) -> (String, bool) {
    let bytes = seg.as_bytes();
    let mut pieces: Vec<(usize, usize, String)> = Vec::new();
    let mut i = 0usize;
    while i < bytes.len() {
        if !bytes[i..].starts_with(DATELINE_PREFIX_ASCII) {
            i += 1;
            continue;
        }
        i += DATELINE_PREFIX_ASCII.len();
        if let Some((whole_end, canonical)) = parse_dateline_at(seg, bytes, i) {
            let whole_start = i - DATELINE_PREFIX_ASCII.len();
            if seg[whole_start..whole_end] != canonical {
                pieces.push((whole_start, whole_end, canonical));
            }
            i = whole_end;
            continue;
        }
        i += 1;
    }

    if pieces.is_empty() {
        return (seg.to_string(), false);
    }
    let mut out = String::with_capacity(seg.len());
    let mut last = 0usize;
    for (start, end, replacement) in pieces {
        out.push_str(&seg[last..start]);
        out.push_str(&replacement);
        last = end;
    }
    out.push_str(&seg[last..]);
    (out, true)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn detects_iso8601_timestamp_in_system_prompt() {
        let body = json!({
            "system": "Today is 2026-05-04T14:30:00Z. Be concise.",
            "messages": [],
        });
        let findings = detect_volatile_content(&body, ApiKind::Anthropic);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].kind, VolatileKind::Timestamp);
        assert_eq!(findings[0].location, "system");
        assert!(
            findings[0].sample.starts_with("2026-05-04T14:30:00"),
            "sample should be the ISO-8601 substring, got {:?}",
            findings[0].sample
        );
    }

    #[test]
    fn does_not_scan_messages_for_volatile_content() {
        // messages[] are intentionally NOT scanned: strip_volatile_from_prefix
        // cannot touch them, the cache prefix is derived from system +
        // tools only, and scanning messages only produced noise (historical
        // tool_result timestamps / UUIDs). A UUID in a user message must
        // produce no finding.
        let body = json!({
            "messages": [
                {"role": "user", "content": "trace=550e8400-e29b-41d4-a716-446655440000"},
            ],
        });
        let findings = detect_volatile_content(&body, ApiKind::Anthropic);
        assert!(
            findings.is_empty(),
            "messages should not be scanned, got {findings:?}"
        );
    }

    #[test]
    fn detects_request_id_field_in_nested_object() {
        // Tools input_schema with a nested `request_id` field whose
        // value is a non-UUID string. The volatile-substring scan
        // would miss this; the ID-field-name rule catches it.
        let body = json!({
            "tools": [{
                "name": "lookup",
                "description": "Look up a user.",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "user_id": {"type": "string"},
                        "request_id": "req-2026-abc-12345"
                    }
                }
            }],
            "messages": [],
        });
        let findings = detect_volatile_content(&body, ApiKind::Anthropic);
        let id_field = findings
            .iter()
            .find(|f| f.kind == VolatileKind::IdField)
            .expect("expected an IdField finding");
        assert!(
            id_field.location.ends_with(".request_id"),
            "location should end with .request_id, got {:?}",
            id_field.location
        );
        assert!(id_field.sample.contains("req-2026-abc-12345"));
    }

    #[test]
    fn stable_content_yields_zero_findings() {
        // Plain prose with no timestamps, no UUIDs, no ID-named keys.
        let body = json!({
            "system": "You are a helpful assistant. Be concise.",
            "messages": [
                {"role": "user", "content": "Summarize the document below."},
                {"role": "assistant", "content": "Sure — please paste it."},
            ],
            "tools": [{
                "name": "search",
                "description": "Search the corpus.",
                "input_schema": {
                    "type": "object",
                    "properties": {"query": {"type": "string"}}
                }
            }],
        });
        let findings = detect_volatile_content(&body, ApiKind::Anthropic);
        assert!(
            findings.is_empty(),
            "expected zero findings on stable content, got {findings:?}",
        );
    }

    #[test]
    fn caps_findings_at_ten() {
        // Build a body with many UUID-bearing tool descriptions so the
        // detector would otherwise emit > 10 findings. Tools are part of
        // the scanned prefix; messages are not scanned.
        let mut tools = Vec::new();
        for i in 0..30 {
            tools.push(json!({
                "name": format!("tool_{i}"),
                "description": format!("turn {i}: 550e8400-e29b-41d4-a716-446655440000"),
            }));
        }
        let body = json!({"tools": tools});
        let findings = detect_volatile_content(&body, ApiKind::Anthropic);
        assert_eq!(
            findings.len(),
            MAX_FINDINGS_PER_REQUEST,
            "detector must cap findings at {MAX_FINDINGS_PER_REQUEST}",
        );
    }

    #[test]
    fn does_not_mutate_input() {
        let body = json!({
            "system": "Today is 2026-05-04T14:30:00Z.",
            "messages": [{
                "role": "user",
                "content": "trace=550e8400-e29b-41d4-a716-446655440000",
            }],
            "tools": [{
                "name": "lookup",
                "description": "Look up a user.",
                "input_schema": {
                    "type": "object",
                    "properties": {"request_id": "req-abc"}
                }
            }],
        });
        let before = serde_json::to_vec(&body).expect("serialize before");
        let _findings = detect_volatile_content(&body, ApiKind::Anthropic);
        let after = serde_json::to_vec(&body).expect("serialize after");
        assert_eq!(before, after, "detector must NOT mutate input body bytes",);
    }

    #[test]
    fn apikind_anthropic_scans_correct_paths() {
        // Anthropic shape: tools[].description + tools[].input_schema.
        let body = json!({
            "tools": [{
                "name": "lookup",
                "description": "scheduled at 2026-05-04T10:00:00Z",
                "input_schema": {"type": "object"}
            }],
        });
        let anthropic_findings = detect_volatile_content(&body, ApiKind::Anthropic);
        assert_eq!(anthropic_findings.len(), 1);
        assert_eq!(anthropic_findings[0].kind, VolatileKind::Timestamp);
        assert_eq!(
            anthropic_findings[0].location, "tools[0].description",
            "Anthropic shape: tools[].description (NOT tools[].function.description)",
        );

        // OpenAI shape on the same body should find nothing — it
        // expects tools[].function.description, not tools[].description.
        let openai_findings = detect_volatile_content(&body, ApiKind::OpenAi);
        assert!(
            openai_findings.is_empty(),
            "OpenAI walker must not match Anthropic shape, got {openai_findings:?}",
        );

        // OpenAI-shape body matches the OpenAI walker.
        let openai_body = json!({
            "tools": [{
                "type": "function",
                "function": {
                    "name": "lookup",
                    "description": "scheduled at 2026-05-04T10:00:00Z",
                    "parameters": {"type": "object"}
                }
            }],
        });
        let openai_findings = detect_volatile_content(&openai_body, ApiKind::OpenAi);
        assert_eq!(openai_findings.len(), 1);
        assert_eq!(openai_findings[0].kind, VolatileKind::Timestamp);
        assert_eq!(openai_findings[0].location, "tools[0].function.description",);
    }

    #[test]
    fn id_field_with_empty_value_does_not_fire() {
        // request_id present but empty — schemas / clients that
        // declare the field but don't fill it shouldn't trigger.
        let body = json!({
            "tools": [{
                "input_schema": {
                    "properties": {"request_id": ""}
                }
            }],
        });
        let findings = detect_volatile_content(&body, ApiKind::Anthropic);
        assert!(
            findings.iter().all(|f| f.kind != VolatileKind::IdField),
            "empty ID-field values must not trigger; got {findings:?}",
        );
    }

    #[test]
    fn iso8601_with_space_separator_recognized() {
        // RFC 3339 §5.6 allows a space in place of `T`. Ops logs
        // commonly render it that way; we accept it to keep the
        // detector helpful.
        let body = json!({"system": "started at 2026-05-04 14:30:00"});
        let findings = detect_volatile_content(&body, ApiKind::Anthropic);
        assert_eq!(findings.len(), 1);
        assert_eq!(findings[0].kind, VolatileKind::Timestamp);
    }

    #[test]
    fn random_hex_without_v4_nibble_is_not_a_uuid() {
        // 36-char shape with hyphens but version nibble != 4 (here
        // the position-14 char is `0`, e.g. a synthesised legacy
        // identifier). Must NOT be flagged as UUID.
        let body = json!({
            "messages": [{
                "role": "user",
                "content": "id=550e8400-e29b-01d4-a716-446655440000",
            }],
        });
        let findings = detect_volatile_content(&body, ApiKind::Anthropic);
        assert!(
            findings.iter().all(|f| f.kind != VolatileKind::Uuid),
            "non-v4 UUID-shaped string must not match v4 detector; got {findings:?}",
        );
    }

    #[test]
    fn truncate_sample_respects_utf8_boundaries() {
        // 80 bytes of ASCII followed by a multi-byte codepoint at
        // the cut. Must not panic and must not produce invalid UTF-8.
        let mut s = "a".repeat(SAMPLE_TRUNCATE_BYTES);
        s.push('é'); // 2 bytes
        let out = truncate_sample(&s);
        // Round-trip through String (would panic on invalid UTF-8).
        let _ = out.as_bytes();
        assert!(out.ends_with('…'));
    }

    // ─── strip_volatile_from_prefix 测试 ────────────────────────────
    //
    // 策略：system/instructions/developer 消息 AND tools 会被剥离。
    // user/assistant 消息不会被剥离。

    // ─── 内联 ID 测试 ────────────────────────────────────────

    #[test]
    fn detects_inline_id_req_prefix() {
        let body = json!({
            "system": "Request ref req-abc-12345 must be processed.",
            "messages": [],
        });
        let findings = detect_volatile_content(&body, ApiKind::Anthropic);
        let inline = findings.iter().find(|f| f.kind == VolatileKind::InlineId);
        assert!(inline.is_some(), "should detect req- prefix inline ID");
        assert!(inline.unwrap().sample.contains("req-abc-12345"));
    }

    #[test]
    fn detects_inline_id_msg_prefix() {
        let body = json!({
            "system": "Previous message msg_12345abc referenced.",
            "messages": [],
        });
        let findings = detect_volatile_content(&body, ApiKind::Anthropic);
        let inline = findings.iter().find(|f| f.kind == VolatileKind::InlineId);
        assert!(inline.is_some(), "should detect msg_ prefix inline ID");
    }

    #[test]
    fn detects_inline_id_call_prefix() {
        let body = json!({
            "system": "Tool call call_abc123def456 returned.",
            "messages": [],
        });
        let findings = detect_volatile_content(&body, ApiKind::Anthropic);
        let inline = findings.iter().find(|f| f.kind == VolatileKind::InlineId);
        assert!(inline.is_some(), "should detect call_ prefix inline ID");
    }

    #[test]
    fn inline_id_requires_digit_in_suffix() {
        // "req-body" 没有数字——不应被标记
        let body = json!({
            "system": "The req-body field is required.",
            "messages": [],
        });
        let findings = detect_volatile_content(&body, ApiKind::Anthropic);
        assert!(
            findings.iter().all(|f| f.kind != VolatileKind::InlineId),
            "req-body (no digits) must not be flagged as inline ID; got {findings:?}",
        );
    }

    #[test]
    fn inline_id_minimum_suffix_length() {
        // "req-1a" 前缀之后只有 2 个字符——太短
        let body = json!({
            "system": "Short ID req-1a here.",
            "messages": [],
        });
        let findings = detect_volatile_content(&body, ApiKind::Anthropic);
        assert!(
            findings.iter().all(|f| f.kind != VolatileKind::InlineId),
            "req-1a (only 2 suffix chars) must not be flagged; got {findings:?}",
        );
    }

    // ─── 仅日期测试 ────────────────────────────────────────

    #[test]
    fn detects_date_only_dash_format() {
        let body = json!({
            "system": "Today is 2026-06-13. Be concise.",
            "messages": [],
        });
        let findings = detect_volatile_content(&body, ApiKind::Anthropic);
        let date = findings.iter().find(|f| f.kind == VolatileKind::DateOnly);
        assert!(
            date.is_some(),
            "should detect YYYY-MM-DD date; got {:?}",
            findings
        );
        assert_eq!(date.unwrap().sample, "2026-06-13");
    }

    #[test]
    fn detects_date_only_slash_format() {
        let body = json!({
            "system": "Today's date is 2026/06/13.",
            "messages": [],
        });
        let findings = detect_volatile_content(&body, ApiKind::Anthropic);
        let date = findings.iter().find(|f| f.kind == VolatileKind::DateOnly);
        assert!(
            date.is_some(),
            "should detect YYYY/MM/DD date; got {:?}",
            findings
        );
    }

    #[test]
    fn date_only_does_not_match_full_iso8601() {
        let body = json!({
            "system": "Started at 2026-06-13T10:30:00Z.",
            "messages": [],
        });
        let findings = detect_volatile_content(&body, ApiKind::Anthropic);
        assert!(
            findings.iter().all(|f| f.kind != VolatileKind::DateOnly),
            "full ISO-8601 must not be flagged as DateOnly; got {:?}",
            findings,
        );
        assert!(
            findings.iter().any(|f| f.kind == VolatileKind::Timestamp),
            "full ISO-8601 should be detected as Timestamp; got {:?}",
            findings,
        );
    }

    #[test]
    fn date_only_does_not_match_space_separated_datetime() {
        let body = json!({
            "system": "Started at 2026-06-13 10:30:00.",
            "messages": [],
        });
        let findings = detect_volatile_content(&body, ApiKind::Anthropic);
        assert!(
            findings.iter().all(|f| f.kind != VolatileKind::DateOnly),
            "space-separated datetime must not be flagged as DateOnly; got {:?}",
            findings,
        );
    }

    #[test]
    fn date_only_rejects_invalid_month() {
        let body = json!({
            "system": "Invalid 2026-13-01 date.",
            "messages": [],
        });
        let findings = detect_volatile_content(&body, ApiKind::Anthropic);
        assert!(
            findings.iter().all(|f| f.kind != VolatileKind::DateOnly),
            "month 13 must not be flagged as date; got {:?}",
            findings,
        );
    }

    #[test]
    fn date_only_rejects_invalid_day() {
        let body = json!({
            "system": "Invalid 2026-06-32 date.",
            "messages": [],
        });
        let findings = detect_volatile_content(&body, ApiKind::Anthropic);
        assert!(
            findings.iter().all(|f| f.kind != VolatileKind::DateOnly),
            "day 32 must not be flagged as date; got {:?}",
            findings,
        );
    }

    // ─── 客户端 dateline 归一化测试(对齐 sub2api anthropicfp)───

    #[test]
    fn dateline_ascii_hyphen_is_identity() {
        let mut body = json!({
            "system": "Today's date is 2026-07-01.",
            "messages": [],
        });
        let snapshot = body.clone();
        let count = normalize_client_dateline(&mut body, ApiKind::Anthropic);
        assert_eq!(count, 0, "canonical form must be identity");
        assert_eq!(body, snapshot);
    }

    #[test]
    fn dateline_ascii_slash_becomes_hyphen() {
        let mut body = json!({
            "system": "Today's date is 2026/07/01.",
            "messages": [],
        });
        let count = normalize_client_dateline(&mut body, ApiKind::Anthropic);
        assert_eq!(count, 1);
        assert_eq!(
            body.get("system").unwrap().as_str().unwrap(),
            "Today's date is 2026-07-01."
        );
    }

    #[test]
    fn dateline_unicode_apostrophes_normalized() {
        for (name, apo) in [
            ("u2019", '\u{2019}'),
            ("u02bc", '\u{02BC}'),
            ("u02b9", '\u{02B9}'),
        ] {
            let mut body = json!({
                "system": format!("Today{}s date is 2026-07-01.", apo),
                "messages": [],
            });
            let count = normalize_client_dateline(&mut body, ApiKind::Anthropic);
            assert_eq!(count, 1, "{} variant should be rewritten", name);
            assert_eq!(
                body.get("system").unwrap().as_str().unwrap(),
                "Today's date is 2026-07-01.",
                "{} should become ASCII apostrophe",
                name
            );
        }
    }

    #[test]
    fn dateline_mixed_separators_not_matched() {
        // 与 sub2api 一致:两分隔符不一致的句子不匹配,原样保留
        let mut body = json!({
            "system": "Today's date is 2026-07/01.",
            "messages": [],
        });
        let snapshot = body.clone();
        let count = normalize_client_dateline(&mut body, ApiKind::Anthropic);
        assert_eq!(count, 0);
        assert_eq!(body, snapshot);
    }

    #[test]
    fn dateline_prose_and_loose_dates_untouched() {
        // 非指纹句式的日期(用户 prose、代码)不得改写
        let mut body = json!({
            "system": "Today is foo. His date is 2026-06-30. Log at 2026/06/13.",
            "messages": [],
        });
        let snapshot = body.clone();
        let count = normalize_client_dateline(&mut body, ApiKind::Anthropic);
        assert_eq!(count, 0);
        assert_eq!(body, snapshot);
    }

    #[test]
    fn dateline_idempotent() {
        let mut body = json!({
            "system": [{"type": "text", "text": "Today\u{2019}s date is 2026/07/01."}],
            "messages": [],
        });
        let count1 = normalize_client_dateline(&mut body, ApiKind::Anthropic);
        let snapshot = body.clone();
        let count2 = normalize_client_dateline(&mut body, ApiKind::Anthropic);
        assert_eq!(count1, 1);
        assert_eq!(count2, 0);
        assert_eq!(body, snapshot, "must be idempotent");
    }

    #[test]
    fn dateline_messages_only_inside_reminder() {
        // CC 第 2 轮起 dateline 挪进 <system-reminder>;标签外的 prose 不动
        let mut body = json!({
            "messages": [
                {"role": "user", "content": [
                    {"type": "text", "text": "free prose Today\u{2019}s date is 2026/07/01. untouched"},
                    {"type": "text", "text": "<system-reminder>Today\u{2019}s date is 2026/07/01.</system-reminder>"}
                ]}
            ],
        });
        let count = normalize_client_dateline(&mut body, ApiKind::Anthropic);
        assert_eq!(count, 1, "only the reminder-scoped sentence");
        assert!(
            body["messages"][0]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("Today\u{2019}s date is 2026/07/01."),
            "outside reminder must stay untouched"
        );
        assert_eq!(
            body["messages"][0]["content"][1]["text"],
            "<system-reminder>Today's date is 2026-07-01.</system-reminder>"
        );
    }

    #[test]
    fn dateline_tools_not_scanned() {
        // 工具定义不在作用域(sub2api 同)
        let mut body = json!({
            "system": "Today's date is 2026/07/01.",
            "tools": [{
                "name": "t",
                "description": "Today's date is 2026/07/01."
            }],
        });
        let count = normalize_client_dateline(&mut body, ApiKind::Anthropic);
        assert_eq!(count, 1);
        assert!(
            body["tools"][0]["description"]
                .as_str()
                .unwrap()
                .contains("2026/07/01"),
            "tools must not be touched"
        );
    }

    #[test]
    fn dateline_openai_instructions_and_roles() {
        let mut body = json!({
            "instructions": "Today's date is 2026/05/01.",
            "input": [
                {"role": "developer", "content": [{"type": "text", "text": "<system-reminder>Today\u{2019}s date is 2026/04/30.</system-reminder>"}]},
                {"role": "user", "content": "Today\u{2019}s date is 2026/03/04. outside"}
            ],
        });
        let count = normalize_client_dateline(&mut body, ApiKind::OpenAi);
        assert_eq!(count, 2, "instructions + reminder block only");
        assert_eq!(body["instructions"], "Today's date is 2026-05-01.");
        assert_eq!(
            body["input"][0]["content"][0]["text"],
            "<system-reminder>Today's date is 2026-04-30.</system-reminder>"
        );
        assert!(
            body["input"][1]["content"]
                .as_str()
                .unwrap()
                .contains("2026/03/04"),
            "user content outside reminder stays"
        );
    }

    // ─── input[] 中的 system/developer 消息会被剥离 ─────────
}
