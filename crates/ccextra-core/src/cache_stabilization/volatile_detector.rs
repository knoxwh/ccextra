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
//! **剥离**函数（`strip_volatile_from_prefix`）通过将易变子串替换为
//! 确定性占位符来修改 cache prefix（system + tools）。user/assistant
//! 消息从不被触碰。
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

use super::json_walker::ToolWalker;
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
    // messages[] 故意不扫描，因为 (a) strip_volatile_from_prefix 无法触碰它们，
    // 那里的警告无从处理；(b) cache prefix 仅由 system + tools 派生，
    // 因此 messages 内的易变内容不会破坏 prefix。扫描它们只会产生噪音
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
    // + tools。messages[] / input[] 中的条目故意不扫描——
    // strip_volatile_from_prefix 无法触碰它们，cache prefix 仅由
    // instructions + tools 派生，扫描它们只会产生噪音
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

// ─── 易变内容消除 ────────────────────────────────────────────────────
//
// 将 **cache prefix**（system + tools）中的易变子串替换为确定性占位符，
// 使前缀在多次轮次之间保持字节一致，尽管存在每请求的 ID/时间戳。
//
// user/assistant 消息故意不触碰——它们位于活跃区（live zone），
// 那里的每轮变化是预期且无害的。

/// ISO-8601 时间戳的占位符。每次替换长度相同，
/// 使之后所有内容的字节偏移保持稳定。
const TIMESTAMP_PLACEHOLDER: &str = "___TIMESTAMP___";

/// 仅日期值（`YYYY-MM-DD` / `YYYY/MM/DD`）的占位符。
const DATE_PLACEHOLDER: &str = "___DATE___";

/// UUID v4 的占位符。每次替换长度相同。
const UUID_PLACEHOLDER: &str = "___UUID___";

/// ID 字段值的占位符。
const ID_PLACEHOLDER: &str = "___ID___";

/// 替换 cache prefix（system + tools）中的易变内容。
///
/// **只修改 prefix**——system prompt 和 tool 定义。
/// user/assistant 消息保持不动，因为它们每轮变化，且不属于缓存前缀。
///
/// 返回所做的替换次数（用于可观测性）。
pub fn strip_volatile_from_prefix(body: &mut Value, kind: ApiKind) -> usize {
    match kind {
        ApiKind::Anthropic => strip_anthropic_volatile(body),
        ApiKind::OpenAi => strip_openai_volatile(body),
    }
}

/// 从 Anthropic 请求体中剥离易变内容。
/// 覆盖：system prompt + tools[].description + tools[].input_schema。
fn strip_anthropic_volatile(body: &mut Value) -> usize {
    let mut count = 0;

    // 从 system prompt（字符串或块数组）中剥离易变内容。
    count += strip_anthropic_system(body);

    // tools[].description + tools[].input_schema
    if let Some(mut walker) = ToolWalker::new(body) {
        count += walker.for_each(|tool| {
            let mut tool_count = 0;
            if let Some(Value::String(desc)) = tool.get_mut("description") {
                tool_count += strip_string(desc);
            }
            if let Some(schema) = tool.get_mut("input_schema") {
                tool_count += strip_value_recursive(schema);
            }
            tool_count
        });
    }

    count
}

/// 从 OpenAI 请求体中剥离易变内容。
/// 覆盖：instructions/system messages + tools[].function.* + 直接 tool 形态。
fn strip_openai_volatile(body: &mut Value) -> usize {
    let mut count = 0;

    count += strip_openai_prefix_text(body);

    if let Some(mut walker) = ToolWalker::new(body) {
        count += walker.for_each(strip_openai_tool);
    }

    count
}

/// 从单个 OpenAI tool 定义中剥离易变内容。
/// 同时处理 Chat/Completions 形态（function 包装）和 Responses 直接形态。
fn strip_openai_tool(tool: &mut Value) -> usize {
    let mut count = 0;

    if let Some(function) = tool.get_mut("function") {
        // Chat/Completions 形态：tools[].function.{description, parameters}
        if let Some(Value::String(desc)) = function.get_mut("description") {
            count += strip_string(desc);
        }
        if let Some(params) = function.get_mut("parameters") {
            count += strip_value_recursive(params);
        }
    } else if is_responses_direct_tool(tool) {
        // Responses 直接形态：tools[].{description, parameters}
        if let Some(Value::String(desc)) = tool.get_mut("description") {
            count += strip_string(desc);
        }
        if let Some(params) = tool.get_mut("parameters") {
            count += strip_value_recursive(params);
        }
    }

    count
}

/// 从 Anthropic `system` 字段（字符串或块数组）中剥离易变内容。
fn strip_anthropic_system(body: &mut Value) -> usize {
    let mut count = 0;
    let Some(system) = body.get_mut("system") else {
        return 0;
    };
    match system {
        Value::String(s) => {
            count += strip_string(s);
        }
        Value::Array(blocks) => {
            for block in blocks.iter_mut() {
                if let Some(Value::String(text)) = block.get_mut("text") {
                    count += strip_string(text);
                }
            }
        }
        _ => {}
    }
    count
}

/// 从 OpenAI 前缀中剥离易变内容：`instructions`、
/// `messages[].role==system` 以及 `input[].role in (system, developer)`。
fn strip_openai_prefix_text(body: &mut Value) -> usize {
    let mut count = 0;
    if let Some(Value::String(instructions)) = body.get_mut("instructions") {
        count += strip_string(instructions);
    }
    if let Some(Value::Array(messages)) = body.get_mut("messages") {
        for message in messages {
            match message.get("role").and_then(Value::as_str) {
                Some("system") | Some("developer") => {
                    count += strip_openai_content(message.get_mut("content"));
                }
                _ => {}
            }
        }
    }
    if let Some(Value::Array(input)) = body.get_mut("input") {
        for item in input {
            match item.get("role").and_then(Value::as_str) {
                Some("system") | Some("developer") => {
                    count += strip_openai_content(item.get_mut("content"));
                }
                _ => {}
            }
        }
    }
    count
}

/// 从 OpenAI content 字段（字符串或 parts 数组）中剥离易变内容。
fn strip_openai_content(content: Option<&mut Value>) -> usize {
    let mut count = 0;
    match content {
        Some(Value::String(s)) => {
            count += strip_string(s);
        }
        Some(Value::Array(parts)) => {
            for part in parts.iter_mut() {
                if let Some(Value::String(text)) = part.get_mut("text") {
                    count += strip_string(text);
                }
            }
        }
        _ => {}
    }
    count
}

/// 替换字符串值中的易变子串。
fn strip_string(s: &mut String) -> usize {
    let mut count = 0;
    let bytes = s.as_bytes();
    let len = bytes.len();
    let mut replacements: Vec<(usize, usize, &str)> = Vec::new();

    let mut i = 0usize;
    while i < len {
        if i + 19 <= len && looks_like_iso8601(&bytes[i..i + 19]) {
            // 找到时间戳的完整范围（时区后缀等）
            let end = find_timestamp_end(&bytes[i..], 19);
            replacements.push((i, i + end, TIMESTAMP_PLACEHOLDER));
            i += end;
            count += 1;
            continue;
        }
        // 仅日期：YYYY-MM-DD 或 YYYY/MM/DD（10 字节）。
        if looks_like_date_only(s, i) {
            replacements.push((i, i + 10, DATE_PLACEHOLDER));
            i += 10;
            count += 1;
            continue;
        }
        if i + 36 <= len && looks_like_uuid_v4(&bytes[i..i + 36]) {
            replacements.push((i, i + 36, UUID_PLACEHOLDER));
            i += 36;
            count += 1;
            continue;
        }
        // 检查内联 ID 前缀
        if let Some(end) = find_inline_id_at(s, i) {
            replacements.push((i, end, ID_PLACEHOLDER));
            i = end;
            count += 1;
            continue;
        }
        i += 1;
    }

    if replacements.is_empty() {
        return count;
    }

    // 构建替换后的字符串。无法进行原地子串替换，因为占位符与原串长度不同，
    // 因此重建字符串。
    let mut result = String::with_capacity(s.len());
    let mut last_end = 0;
    for (start, end, placeholder) in replacements {
        result.push_str(&s[last_end..start]);
        result.push_str(placeholder);
        last_end = end;
    }
    result.push_str(&s[last_end..]);
    *s = result;
    count
}

/// 将时间戳窗口扩展到超过 19 字节最小值，以包含时区后缀
/// （`Z`、`+HH:MM`、`+HHMM`、`.sss`）。
fn find_timestamp_end(rest: &[u8], min_len: usize) -> usize {
    let mut end = min_len;
    let len = rest.len();

    // 小数秒：`.sss`（最多 9 位数字）
    if end < len && rest[end] == b'.' {
        end += 1;
        while end < len && rest[end].is_ascii_digit() && end - min_len < 10 {
            end += 1;
        }
    }
    // `Z` 后缀
    if end < len && (rest[end] == b'Z' || rest[end] == b'z') {
        end += 1;
    }
    // `+HH:MM` 或 `+HHMM` 或 `-HH:MM` 或 `-HHMM`
    else if end < len && (rest[end] == b'+' || rest[end] == b'-') {
        end += 1;
        // 2 位数字小时
        if end + 2 <= len && rest[end..end + 2].iter().all(u8::is_ascii_digit) {
            end += 2;
            // 可选的冒号 + 分钟
            if end < len && rest[end] == b':' {
                end += 1;
            }
            if end + 2 <= len && rest[end..end + 2].iter().all(u8::is_ascii_digit) {
                end += 2;
            }
        }
    }
    end
}

/// 递归遍历 Value，替换字符串中的易变子串和 ID 字段值。
fn strip_value_recursive(v: &mut Value) -> usize {
    let mut count = 0;
    match v {
        Value::String(s) => count += strip_string(s),
        Value::Array(items) => {
            for item in items.iter_mut() {
                count += strip_value_recursive(item);
            }
        }
        Value::Object(map) => {
            for (k, sub) in map.iter_mut() {
                if is_id_named_key(k) && !is_value_empty(sub) {
                    // 将整个值替换为占位符
                    // ——ID 字段值每请求唯一，无论其格式如何都会破坏前缀。
                    *sub = Value::String(ID_PLACEHOLDER.to_string());
                    count += 1;
                    continue; // 不要递归进入占位符
                }
                count += strip_value_recursive(sub);
            }
        }
        _ => {}
    }
    count
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cache_stabilization::drift_detector::{
        compute_structural_hash, ApiKind as DriftApiKind,
    };
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

    #[test]
    fn strips_anthropic_system_string() {
        let mut body = json!({
            "system": "Today is 2026-06-14. Request req-1234567890abcdef.",
            "tools": []
        });
        let count = strip_volatile_from_prefix(&mut body, ApiKind::Anthropic);
        assert!(count >= 2);
        assert_eq!(body["system"], "Today is ___DATE___. Request ___ID___.");
    }

    #[test]
    fn strips_anthropic_system_blocks() {
        let mut body = json!({
            "system": [{"type":"text","text":"Session 550e8400-e29b-41d4-a716-446655440000"}],
            "tools": []
        });
        let count = strip_volatile_from_prefix(&mut body, ApiKind::Anthropic);
        assert!(count >= 1);
        assert_eq!(body["system"][0]["text"], "Session ___UUID___");
    }

    #[test]
    fn strip_id_field_from_tool_schema() {
        let mut body = json!({
            "system": "You are helpful.",
            "messages": [],
            "tools": [{
                "name": "lookup",
                "input_schema": {
                    "type": "object",
                    "properties": {
                        "request_id": "req-abc-12345"
                    }
                }
            }],
        });
        let count = strip_volatile_from_prefix(&mut body, ApiKind::Anthropic);
        assert!(count >= 1);
        let value = body["tools"][0]["input_schema"]["properties"]["request_id"]
            .as_str()
            .unwrap();
        assert_eq!(value, ID_PLACEHOLDER);
    }

    #[test]
    fn strip_does_not_touch_user_messages() {
        let mut body = json!({
            "system": "Be concise.",
            "messages": [{
                "role": "user",
                "content": "trace=550e8400-e29b-41d4-a716-446655440000"
            }],
        });
        let count = strip_volatile_from_prefix(&mut body, ApiKind::Anthropic);
        // 只有 system 在前缀中——messages 不会被剥离
        assert_eq!(count, 0);
        // user 消息仍保留 UUID
        let content = body["messages"][0]["content"].as_str().unwrap();
        assert!(content.contains("550e8400-e29b-41d4-a716-446655440000"));
    }

    #[test]
    fn strip_idempotent_on_stable_content() {
        let mut body = json!({
            "system": "You are a helpful assistant. Be concise.",
            "messages": [],
            "tools": [{
                "name": "search",
                "description": "Search the corpus.",
                "input_schema": {"type": "object"}
            }],
        });
        let count1 = strip_volatile_from_prefix(&mut body, ApiKind::Anthropic);
        assert_eq!(count1, 0);
        let before = serde_json::to_vec(&body).unwrap();
        let count2 = strip_volatile_from_prefix(&mut body, ApiKind::Anthropic);
        assert_eq!(count2, 0);
        let after = serde_json::to_vec(&body).unwrap();
        assert_eq!(
            before, after,
            "double-strip on stable content must be byte-equal"
        );
    }

    #[test]
    fn strip_system_timestamp_with_timezone() {
        let mut body = json!({
            "system": "Started at 2026-05-04T14:30:00.123Z. End.",
            "messages": [],
        });
        let count = strip_volatile_from_prefix(&mut body, ApiKind::Anthropic);
        assert!(count >= 1);
        let system = body.get("system").unwrap().as_str().unwrap();
        assert!(system.contains(TIMESTAMP_PLACEHOLDER));
    }

    #[test]
    fn strip_multiple_volatile_in_system() {
        let mut body = json!({
            "system": "ts=2026-05-04T14:30:00Z uuid=550e8400-e29b-41d4-a716-446655440000",
            "messages": [],
        });
        let count = strip_volatile_from_prefix(&mut body, ApiKind::Anthropic);
        assert!(count >= 2);
    }

    #[test]
    fn strips_openai_chat_system_message() {
        let mut body = json!({
            "messages": [
                {"role":"system", "content":"Today is 2026/06/14"},
                {"role":"user", "content":"keep 2026/06/14 untouched"}
            ],
            "tools": []
        });
        let count = strip_volatile_from_prefix(&mut body, ApiKind::OpenAi);
        assert_eq!(body["messages"][0]["content"], "Today is ___DATE___");
        assert_eq!(body["messages"][1]["content"], "keep 2026/06/14 untouched");
        assert!(count >= 1);
    }

    #[test]
    fn strip_anthropic_system_block_array_timestamp() {
        let mut body = json!({
            "system": [
                {"type": "text", "text": "Started at 2026-05-04T14:30:00Z."},
                {"type": "text", "text": "Be concise."},
            ],
            "messages": [],
        });
        let count = strip_volatile_from_prefix(&mut body, ApiKind::Anthropic);
        assert!(count >= 1);
        let text = body["system"][0]["text"].as_str().unwrap();
        assert!(text.contains(TIMESTAMP_PLACEHOLDER));
    }

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

    #[test]
    fn strip_inline_id_in_system() {
        let mut body = json!({
            "system": "Processing req-abc-12345 now.",
            "messages": [],
        });
        let count = strip_volatile_from_prefix(&mut body, ApiKind::Anthropic);
        assert!(count >= 1);
        let system = body.get("system").unwrap().as_str().unwrap();
        assert!(system.contains(ID_PLACEHOLDER));
    }

    #[test]
    fn strip_inline_id_msg_in_system() {
        let mut body = json!({
            "system": "Ref msg_98765xyz here.",
            "messages": [],
        });
        let count = strip_volatile_from_prefix(&mut body, ApiKind::Anthropic);
        assert!(count >= 1);
    }

    #[test]
    fn strip_inline_id_does_not_replace_stable_words() {
        // "req-body" 没有数字——不应被剥离
        let mut body = json!({
            "system": "The req-body format is standard.",
            "messages": [],
        });
        let count = strip_volatile_from_prefix(&mut body, ApiKind::Anthropic);
        assert_eq!(count, 0);
        let system = body.get("system").unwrap().as_str().unwrap();
        assert!(system.contains("req-body"));
    }

    #[test]
    fn strip_inline_id_in_tool_description() {
        let mut body = json!({
            "system": "Be helpful.",
            "messages": [],
            "tools": [{
                "name": "lookup",
                "description": "Look up request req-abc-12345.",
                "input_schema": {"type": "object"}
            }],
        });
        let count = strip_volatile_from_prefix(&mut body, ApiKind::Anthropic);
        assert!(count >= 1);
        let desc = body["tools"][0]["description"].as_str().unwrap();
        assert!(desc.contains(ID_PLACEHOLDER));
        assert!(!desc.contains("req-abc-12345"));
    }

    #[test]
    fn strip_openai_system_inline_id() {
        let mut body = json!({
            "messages": [
                {"role": "system", "content": "Reference msg_12345abc here."},
                {"role": "user", "content": "hello"},
            ],
        });
        let count = strip_volatile_from_prefix(&mut body, ApiKind::OpenAi);
        assert!(count >= 1);
        let system_content = body["messages"][0]["content"].as_str().unwrap();
        assert!(system_content.contains(ID_PLACEHOLDER));
    }

    #[test]
    fn strip_openai_responses_direct_tool_parameters_and_description() {
        let mut body = json!({
            "instructions": "Be helpful.",
            "input": [],
            "tools": [{
                "type": "computer",
                "name": "click",
                "description": "Use at 2026-06-13T10:30:00Z",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "trace": {"type": "string", "description": "trace msg_abc123"},
                        "when": {"type": "string", "example": "2026/06/13"}
                    }
                }
            }]
        });
        let count = strip_volatile_from_prefix(&mut body, ApiKind::OpenAi);
        assert!(count >= 1);
        let tool = &body["tools"][0];
        assert!(tool["description"]
            .as_str()
            .unwrap()
            .contains(TIMESTAMP_PLACEHOLDER));
        assert!(tool["parameters"]["properties"]["trace"]["description"]
            .as_str()
            .unwrap()
            .contains(ID_PLACEHOLDER));
        assert!(tool["parameters"]["properties"]["when"]["example"]
            .as_str()
            .unwrap()
            .contains(DATE_PLACEHOLDER));
    }

    #[test]
    fn strip_openai_responses_direct_tool_findings_are_reported() {
        let body = json!({
            "input": [],
            "tools": [{
                "type": "computer",
                "name": "click",
                "description": "Use at 2026-06-13T10:30:00Z",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "trace": {"type": "string", "description": "trace msg_abc123"},
                        "when": {"type": "string", "example": "2026/06/13"}
                    }
                }
            }]
        });
        let findings = detect_volatile_content(&body, ApiKind::OpenAi);
        assert!(findings
            .iter()
            .any(|f| f.location == "tools[0].description"));
        assert!(findings
            .iter()
            .any(|f| f.location == "tools[0].parameters.properties.trace.description"));
        assert!(findings
            .iter()
            .any(|f| f.location == "tools[0].parameters.properties.when.example"));
    }

    #[test]
    fn stripped_direct_tool_volatiles_keep_responses_drift_stable() {
        let mut body = json!({
            "model": "gpt-5.4",
            "instructions": "Be helpful.",
            "input": [{"role": "user", "content": "hello"}],
            "tools": [{
                "type": "computer",
                "name": "click",
                "description": "Use at 2026-06-13T10:30:00Z",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "trace": {"type": "string", "description": "trace msg_abc123"},
                        "when": {"type": "string", "example": "2026/06/13"}
                    }
                }
            }]
        });
        let mut body2 = json!({
            "model": "gpt-5.4",
            "instructions": "Be helpful.",
            "input": [{"role": "user", "content": "hello"}],
            "tools": [{
                "type": "computer",
                "name": "click",
                "description": "Use at 2026-06-14T11:31:00Z",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "trace": {"type": "string", "description": "trace msg_def456"},
                        "when": {"type": "string", "example": "2026/06/14"}
                    }
                }
            }]
        });

        strip_volatile_from_prefix(&mut body, ApiKind::OpenAi);
        strip_volatile_from_prefix(&mut body2, ApiKind::OpenAi);

        let h1 = compute_structural_hash(&body, DriftApiKind::OpenAiResponses);
        let h2 = compute_structural_hash(&body2, DriftApiKind::OpenAiResponses);
        assert_eq!(h1.tools, h2.tools);
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

    #[test]
    fn strip_date_in_system() {
        let mut body = json!({
            "system": "Today is 2026-06-13. Be concise.",
            "messages": [],
        });
        let count = strip_volatile_from_prefix(&mut body, ApiKind::Anthropic);
        assert!(count >= 1);
        let system = body.get("system").unwrap().as_str().unwrap();
        assert!(system.contains(DATE_PLACEHOLDER));
    }

    #[test]
    fn strip_date_slash_in_system() {
        let mut body = json!({
            "system": "Today's date is 2026/06/13.",
            "messages": [],
        });
        let count = strip_volatile_from_prefix(&mut body, ApiKind::Anthropic);
        assert!(count >= 1);
        let system = body.get("system").unwrap().as_str().unwrap();
        assert!(system.contains(DATE_PLACEHOLDER));
    }

    #[test]
    fn strip_timestamp_in_system() {
        let mut body = json!({
            "system": "At 2026-06-13T10:30:00Z do something.",
            "messages": [],
        });
        let count = strip_volatile_from_prefix(&mut body, ApiKind::Anthropic);
        assert!(count >= 1);
        let system = body.get("system").unwrap().as_str().unwrap();
        assert!(system.contains(TIMESTAMP_PLACEHOLDER));
    }

    // ─── input[] 中的 system/developer 消息会被剥离 ─────────

    #[test]
    fn strips_responses_input_system_message() {
        let mut body = json!({
            "model": "gpt-5.4",
            "instructions": "",
            "input": [
                {"role": "system", "content": "Today's date is 2026/06/13. Be concise."},
                {"role": "user", "content": "Hello"}
            ],
            "tools": [],
        });
        let count = strip_volatile_from_prefix(&mut body, ApiKind::OpenAi);
        assert!(count >= 1);
        let system_content = body["input"][0]["content"].as_str().unwrap();
        assert!(system_content.contains(DATE_PLACEHOLDER));
    }

    #[test]
    fn strips_responses_input_timestamp() {
        let mut body = json!({
            "model": "gpt-5.4",
            "instructions": "",
            "input": [
                {"role": "system", "content": "Session started 2026-06-13T10:30:00Z."},
                {"role": "user", "content": "Hello"}
            ],
            "tools": [],
        });
        let count = strip_volatile_from_prefix(&mut body, ApiKind::OpenAi);
        assert!(count >= 1);
        let system_content = body["input"][0]["content"].as_str().unwrap();
        assert!(system_content.contains(TIMESTAMP_PLACEHOLDER));
    }

    #[test]
    fn strip_does_not_touch_input_user_messages() {
        let mut body = json!({
            "model": "gpt-5.4",
            "instructions": "",
            "input": [
                {"role": "system", "content": "You are helpful."},
                {"role": "user", "content": "trace=550e8400-e29b-41d4-a716-446655440000"}
            ],
            "tools": [],
        });
        let count = strip_volatile_from_prefix(&mut body, ApiKind::OpenAi);
        assert_eq!(
            count, 0,
            "stable system + user message should have nothing to strip"
        );
        // user 消息 UUID 应保持不动
        let user_content = body["input"][1]["content"].as_str().unwrap();
        assert!(user_content.contains("550e8400-e29b-41d4-a716-446655440000"));
    }

    #[test]
    fn strips_responses_input_developer_message() {
        // cliproxyapi 将 Claude system 转换为 OpenAI 的 "developer" 角色
        // developer 消息会被剥离（前缀易变策略）。
        let mut body = json!({
            "model": "gpt-5.4",
            "instructions": "",
            "input": [
                {"role": "developer", "content": "Today's date is 2026/06/13. Be concise."},
                {"role": "user", "content": "Hello"}
            ],
            "tools": [],
        });
        let count = strip_volatile_from_prefix(&mut body, ApiKind::OpenAi);
        assert!(count >= 1);
        let dev_content = body["input"][0]["content"].as_str().unwrap();
        assert!(dev_content.contains(DATE_PLACEHOLDER));
    }

    #[test]
    fn strips_openai_responses_instructions_and_developer_input() {
        let mut body = json!({
            "instructions": "Generated at 2026-06-14T12:34:56Z",
            "input": [
                {"role":"developer", "content":"Call msg_abc123"},
                {"role":"user", "content":"keep msg_abc123 untouched"}
            ],
            "tools": []
        });
        let count = strip_volatile_from_prefix(&mut body, ApiKind::OpenAi);
        assert_eq!(body["instructions"], "Generated at ___TIMESTAMP___");
        assert_eq!(body["input"][0]["content"], "Call ___ID___");
        assert_eq!(body["input"][1]["content"], "keep msg_abc123 untouched");
        assert!(count >= 2);
    }
}
