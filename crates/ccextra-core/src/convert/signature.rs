// 签名提供方识别与兼容性判定(对齐 CPA internal/signature)
//
// 覆盖 claude(E/R 双层 + CAIS)、gemini(protobuf field2 信封 + bypass 哨兵)、
// gpt(Fernet)、kimi(固定长度)、grok(无信封高熵)五族。
// 只做传输形状校验,不证明可解密性。

use base64::engine::general_purpose::{STANDARD, STANDARD_NO_PAD, URL_SAFE, URL_SAFE_NO_PAD};
use base64::Engine;

/// Claude/Gemini/GPT/Kimi 签名长度上限(对齐 MaxClaudeThinkingSignatureLen 等)
const MAX_SIGNATURE_LEN: usize = 32 * 1024 * 1024;
/// Grok encrypted_content 长度上限(对齐 MaxGrokEncryptedContentLen)
const MAX_GROK_LEN: usize = 8 * 1024 * 1024;
/// Grok/Kimi 解码后最小长度与熵比下限
const MIN_GROK_DECODED_LEN: usize = 32;
const MIN_ENTROPY_RATIO: f64 = 0.85;

/// Gemini 合成历史 bypass 哨兵(对齐 GeminiSkipThoughtSignatureValidator)
pub const GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR: &str = "skip_thought_signature_validator";
/// Gemini 第二个 bypass 哨兵(对齐 GeminiContextEngineeringBypass)
pub const GEMINI_CONTEXT_ENGINEERING_BYPASS: &str = "context_engineering_is_the_way_to_go";

/// Kimi 两个观测固定原始长度(对齐 kimiThinkingSignatureLens)
const KIMI_NON_STREAMING_LEN: usize = 12946;
const KIMI_STREAMING_LEN: usize = 4340;

/// 签名提供方族(对齐 SignatureProvider)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureProvider {
    Unknown,
    Claude,
    Gemini,
    GeminiBypass,
    Gpt,
    Kimi,
    /// 仅目标族:xAI 无信封无版本字节无固定长度,检测永不返回 grok
    Grok,
}

/// 签名所在块类型(对齐 SignatureBlockKind)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureBlockKind {
    Unknown,
    ClaudeThinking,
    GeminiModelPart,
    GeminiFunctionCall,
    GptReasoning,
}

/// 兼容性处置动作(对齐 SignatureCompatibilityAction)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SignatureAction {
    Preserve,
    DropBlock,
    DropSignature,
    ReplaceWithGeminiBypass,
    NoCompatibleReplacement,
}

/// 兼容性判定结果(对齐 SignatureCompatibilityDecision)
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignatureDecision {
    pub target: SignatureProvider,
    pub detected: SignatureProvider,
    pub compatible: bool,
    pub action: SignatureAction,
    pub replacement: String,
    pub normalized: String,
}

/// 模型名 → 可安全回放该模型历史的提供方族(对齐 SignatureProviderFromModelName)
pub fn signature_provider_from_model_name(model_name: &str) -> SignatureProvider {
    let lower = model_name.trim().to_ascii_lowercase();
    if lower.contains("claude") {
        return SignatureProvider::Claude;
    }
    if lower.contains("gemini") {
        return SignatureProvider::Gemini;
    }
    if lower.contains("gpt")
        || lower.contains("openai")
        || lower.contains("codex")
        || lower.starts_with("o1")
        || lower.starts_with("o3")
        || lower.starts_with("o4")
    {
        return SignatureProvider::Gpt;
    }
    if lower.contains("kimi")
        || lower.contains("moonshot")
        || lower.starts_with("k2")
        || lower.starts_with("k3")
    {
        return SignatureProvider::Kimi;
    }
    if lower.contains("grok") {
        return SignatureProvider::Grok;
    }
    SignatureProvider::Unknown
}

/// 自描述信封可能产生的 base64 首字符(对齐 selfDescribingSignatureFirstChars)。
/// base64 首字符即首字节右移两位,一次比较即可排除全部已知信封:
/// 'C' ← 0x08..0x0b Claude CAIS(0x08);'E' ← 0x10..0x13 Claude 单层/Gemini field2(0x12);
/// 'R' ← 0x44..0x47 Claude 双层(0x45,内层 'E');'g' ← 0x80..0x83 GPT Fernet(0x80)。
/// Gemini ascii_uuid 信封故意不列:首字节是 UUID 首个十六进制字符,散落在
/// 'M'/'N'/'O'/'Y'/'Z',且永不 replay-safe。
const SELF_DESCRIBING_FIRST_CHARS: &[u8] = b"CERg";

/// 结构预过滤:假结果确定,真结果只收窄候选集(对齐 maybeSelfDescribingSignatureEnvelope)
fn maybe_self_describing_envelope(raw: &str) -> bool {
    match raw.as_bytes().first() {
        Some(&b) => SELF_DESCRIBING_FIRST_CHARS.contains(&b),
        None => false,
    }
}

/// 拆分本仓库显式 provider 前缀信封(对齐 SplitSignatureProviderPrefix)。
/// 返回 (前缀族, 去前缀载荷, 是否命中)。前缀族未知则不拆。
pub fn split_signature_provider_prefix(raw: &str) -> (SignatureProvider, String, bool) {
    let trimmed = raw.trim();
    let Some(idx) = trimmed.find('#') else {
        return (SignatureProvider::Unknown, raw.to_string(), false);
    };
    let provider = provider_from_cache_prefix(&trimmed[..idx]);
    if provider == SignatureProvider::Unknown {
        return (SignatureProvider::Unknown, raw.to_string(), false);
    }
    (provider, trimmed[idx + 1..].trim().to_string(), true)
}

/// 前缀 → 提供方族(对齐 SignatureProviderFromCachePrefix)。
/// 故意比模型名映射严格,防 "claude-cache#..." 之类被当作可信来源。
fn provider_from_cache_prefix(prefix: &str) -> SignatureProvider {
    match prefix.trim().to_ascii_lowercase().as_str() {
        "claude" | "anthropic" | "cais" | "claude-cais" | "claude_cais" | "ccmax"
        | "claude-code-max" | "claude_code_max" => SignatureProvider::Claude,
        "gemini" | "google" => SignatureProvider::Gemini,
        "openai" | "gpt" | "codex" => SignatureProvider::Gpt,
        _ => SignatureProvider::Unknown,
    }
}

/// 剥掉 provider 缓存前缀后的载荷,即应回放给上游的值
/// (对齐 SignaturePayloadWithoutProviderPrefix)
pub fn signature_payload_without_prefix(raw: &str) -> String {
    let (_, unprefixed, ok) = split_signature_provider_prefix(raw);
    if ok {
        unprefixed
    } else {
        raw.trim().to_string()
    }
}

// ---- protobuf wire 原语(对齐 google.golang.org/protobuf/encoding/protowire)----

/// wire type 编号
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum WireType {
    Varint = 0,
    Fixed64 = 1,
    Bytes = 2,
    Fixed32 = 5,
}

fn wire_type_from(n: u64) -> Option<WireType> {
    match n {
        0 => Some(WireType::Varint),
        1 => Some(WireType::Fixed64),
        2 => Some(WireType::Bytes),
        5 => Some(WireType::Fixed32),
        // group(3/4)是已废弃特性,签名载荷均为扁平结构;
        // 出现即按畸形处理,不猜测 ConsumeGroup 语义
        _ => None,
    }
}

/// 消费 varint,返回 (值, 消费字节数)。超过 10 字节视为溢出。
fn consume_varint(b: &[u8]) -> Option<(u64, usize)> {
    let mut v: u64 = 0;
    for i in 0..10 {
        let &byte = b.get(i)?;
        if i == 9 && byte > 1 {
            return None;
        }
        v |= u64::from(byte & 0x7f) << (7 * i);
        if byte & 0x80 == 0 {
            return Some((v, i + 1));
        }
    }
    None
}

/// 消费 tag,返回 (字段号, wire type, 消费字节数)
fn consume_tag(b: &[u8]) -> Option<(u64, WireType, usize)> {
    let (v, n) = consume_varint(b)?;
    let typ = wire_type_from(v & 7)?;
    let num = v >> 3;
    if num == 0 {
        return None;
    }
    Some((num, typ, n))
}

/// 消费 length-delimited 字段,返回 (内容切片, 消费字节数含长度前缀)
fn consume_bytes(b: &[u8]) -> Option<(&[u8], usize)> {
    let (len, n) = consume_varint(b)?;
    let len = usize::try_from(len).ok()?;
    let end = n.checked_add(len)?;
    if end > b.len() {
        return None;
    }
    Some((&b[n..end], end))
}

/// 消费一个字段值(不含 tag),返回消费字节数
fn consume_field_value(typ: WireType, b: &[u8]) -> Option<usize> {
    match typ {
        WireType::Varint => consume_varint(b).map(|(_, n)| n),
        WireType::Fixed64 => (b.len() >= 8).then_some(8),
        WireType::Fixed32 => (b.len() >= 4).then_some(4),
        WireType::Bytes => consume_bytes(b).map(|(_, n)| n),
    }
}

/// 遍历消息全部字段,对每个字段调用 visit(字段号, 类型, 原始值字节)。
/// 任一字段畸形或 visit 返回 Err 即整体失败(对齐 walkClaudeProtobufFields)。
fn walk_protobuf_fields(
    msg: &[u8],
    mut visit: impl FnMut(u64, WireType, &[u8]) -> Result<(), String>,
) -> Result<(), String> {
    let mut offset = 0;
    while offset < msg.len() {
        let (num, typ, n) = consume_tag(&msg[offset..]).ok_or_else(malformed_tag)?;
        offset += n;
        let value_len = consume_field_value(typ, &msg[offset..]).ok_or_else(malformed_tag)?;
        visit(num, typ, &msg[offset..offset + value_len])?;
        offset += value_len;
    }
    Ok(())
}

fn malformed_tag() -> String {
    "malformed protobuf field".to_string()
}

// ---- Claude E/R 双层签名(对齐 claude_validation.go)----

/// 剥可选缓存前缀(对齐 stripClaudeSignaturePrefix:取首个 '#' 之后)
fn strip_claude_signature_prefix(raw: &str) -> String {
    let sig = raw.trim();
    match sig.find('#') {
        Some(idx) => sig[idx + 1..].trim().to_string(),
        None => sig.to_string(),
    }
}

/// 归一化为 Antigravity 回放所需的双层 R 形(对齐 NormalizeClaudeThinkingSignature)。
/// strict 为真时额外校验 protobuf 树。
pub fn normalize_claude_thinking_signature(raw: &str, strict: bool) -> Result<String, String> {
    let sig = strip_claude_signature_prefix(raw);
    if sig.is_empty() {
        return Err("empty signature".to_string());
    }
    if sig.len() > MAX_SIGNATURE_LEN {
        return Err("signature exceeds maximum length".to_string());
    }
    match sig.as_bytes()[0] {
        b'R' => {
            validate_claude_double_layer(&sig, strict)?;
            Ok(sig)
        }
        b'E' => {
            validate_claude_single_layer_content(&sig, strict)?;
            Ok(STANDARD.encode(sig.as_bytes()))
        }
        other => Err(format!(
            "invalid signature: expected 'E' or 'R' prefix, got {:?}",
            other as char
        )),
    }
}

/// 归一化为 Claude 原生提供方所需的单层 E 形
/// (对齐 NormalizeClaudeProviderNativeThinkingSignature)
pub fn normalize_claude_provider_native(raw: &str, strict: bool) -> Result<String, String> {
    let sig = strip_claude_signature_prefix(raw);
    if sig.is_empty() {
        return Err("empty signature".to_string());
    }
    if sig.len() > MAX_SIGNATURE_LEN {
        return Err("signature exceeds maximum length".to_string());
    }
    match sig.as_bytes()[0] {
        b'E' => {
            validate_claude_single_layer_content(&sig, strict)?;
            Ok(sig)
        }
        b'R' => {
            validate_claude_double_layer(&sig, strict)?;
            let decoded = STANDARD
                .decode(sig.as_bytes())
                .map_err(|_| "invalid double-layer signature: base64 decode failed".to_string())?;
            String::from_utf8(decoded)
                .map_err(|_| "invalid double-layer signature: not utf8".to_string())
        }
        other => Err(format!(
            "invalid signature: expected 'E' or 'R' prefix, got {:?}",
            other as char
        )),
    }
}

/// Claude 签名有效性(对齐 IsValidClaudeThinkingSignature,strict 走完整校验)
pub fn is_valid_claude_thinking_signature(raw: &str, strict: bool) -> bool {
    normalize_claude_thinking_signature(raw, strict).is_ok()
}

fn validate_claude_double_layer(sig: &str, strict: bool) -> Result<(), String> {
    let decoded = STANDARD
        .decode(sig.as_bytes())
        .map_err(|_| "invalid double-layer signature: base64 decode failed".to_string())?;
    if decoded.is_empty() {
        return Err("invalid double-layer signature: empty after decode".to_string());
    }
    if decoded[0] != b'E' {
        return Err("invalid double-layer signature: inner does not start with 'E'".to_string());
    }
    let inner = String::from_utf8(decoded)
        .map_err(|_| "invalid double-layer signature: not utf8".to_string())?;
    validate_claude_single_layer_content(&inner, strict)
}

fn validate_claude_single_layer_content(sig: &str, strict: bool) -> Result<(), String> {
    let decoded = STANDARD
        .decode(sig.as_bytes())
        .map_err(|_| "invalid single-layer signature: base64 decode failed".to_string())?;
    if decoded.is_empty() {
        return Err("invalid single-layer signature: empty after decode".to_string());
    }
    if decoded[0] != 0x12 {
        return Err(format!(
            "invalid Claude signature: expected first byte 0x12, got 0x{:02x}",
            decoded[0]
        ));
    }
    if !strict {
        return Ok(());
    }
    inspect_claude_signature_payload(&decoded)
}

/// 校验解码后的 Claude thinking protobuf 载荷(对齐 InspectClaudeSignaturePayload)。
/// CPA 的 ClaudeSignatureTree 元数据(routing/infra/schema 分类)只喂日志 reason,
/// ccextra 无该日志,故只保留校验语义不保留分类字段。
fn inspect_claude_signature_payload(payload: &[u8]) -> Result<(), String> {
    if payload.is_empty() {
        return Err("invalid Claude signature: empty payload".to_string());
    }
    if payload[0] != 0x12 {
        return Err(format!(
            "invalid Claude signature: expected first byte 0x12, got 0x{:02x}",
            payload[0]
        ));
    }
    let container = extract_bytes_field(payload, 2, "top-level protobuf")?;
    let channel_block = extract_bytes_field(&container, 1, "Claude Field 2 container")?;
    inspect_claude_channel_block(&channel_block)
}

fn inspect_claude_channel_block(channel_block: &[u8]) -> Result<(), String> {
    let mut have_channel_id = false;
    walk_protobuf_fields(channel_block, |num, typ, raw| {
        match num {
            1 => {
                if typ != WireType::Varint {
                    return Err(
                        "invalid Claude signature: Field 2.1.1 channel_id must be varint"
                            .to_string(),
                    );
                }
                consume_varint(raw).ok_or_else(|| {
                    "invalid Claude signature: failed to decode Field 2.1.1 channel_id".to_string()
                })?;
                have_channel_id = true;
            }
            2 => {
                if typ != WireType::Varint {
                    return Err(
                        "invalid Claude signature: Field 2.1.2 field2 must be varint".to_string(),
                    );
                }
                consume_varint(raw).ok_or_else(|| {
                    "invalid Claude signature: failed to decode Field 2.1.2 field2".to_string()
                })?;
            }
            6 => {
                if typ != WireType::Bytes {
                    return Err(
                        "invalid Claude signature: Field 2.1.6 model_text must be bytes"
                            .to_string(),
                    );
                }
                let (value, _) = consume_bytes(raw).ok_or_else(|| {
                    "invalid Claude signature: failed to decode Field 2.1.6 model_text".to_string()
                })?;
                if std::str::from_utf8(value).is_err() {
                    return Err(
                        "invalid Claude signature: Field 2.1.6 model_text is not valid UTF-8"
                            .to_string(),
                    );
                }
            }
            7 => {
                if typ != WireType::Varint {
                    return Err("invalid Claude signature: Field 2.1.7 must be varint".to_string());
                }
                consume_varint(raw).ok_or_else(|| {
                    "invalid Claude signature: failed to decode Field 2.1.7".to_string()
                })?;
            }
            _ => {}
        }
        Ok(())
    })?;
    if !have_channel_id {
        return Err("invalid Claude signature: missing Field 2.1.1 channel_id".to_string());
    }
    Ok(())
}

/// 取指定字段的 bytes 值:类型不符或字段缺失均报错(对齐 extractClaudeBytesField)
fn extract_bytes_field(msg: &[u8], field_num: u64, scope: &str) -> Result<Vec<u8>, String> {
    let mut value: Option<Vec<u8>> = None;
    walk_protobuf_fields(msg, |num, typ, raw| {
        if num != field_num {
            return Ok(());
        }
        if typ != WireType::Bytes {
            return Err(format!(
                "invalid Claude signature: {scope} field {field_num} must be bytes"
            ));
        }
        let (bytes, _) = consume_bytes(raw).ok_or_else(|| {
            format!("invalid Claude signature: failed to decode {scope} field {field_num}")
        })?;
        value = Some(bytes.to_vec());
        Ok(())
    })?;
    value.ok_or_else(|| format!("invalid Claude signature: missing {scope} field {field_num}"))
}

// ---- Claude CAIS 信封(对齐 claude_validation.go CAIS 段)----

/// CAIS 解码后首字节:顶层 field 1 varint 的 protobuf tag
const CLAUDE_CAIS_MARKER: u8 = 0x08;
/// 区分 CAIS channel block 与任意 protobuf 载荷的 model_text 前缀
const CLAUDE_CAIS_MODEL_TEXT_PREFIX: &str = "claude-";

/// CAIS 签名有效性(对齐 IsValidClaudeCAISSignature)
pub fn is_valid_claude_cais_signature(raw: &str) -> bool {
    inspect_claude_cais_signature(raw).is_ok()
}

/// 解码并结构校验 CAIS 签名。校验是结构性的而非精确匹配:
/// 只要求必需字段存在且类型正确、model_text 带 claude- 前缀、context id 是规范 UUID。
fn inspect_claude_cais_signature(raw: &str) -> Result<(), String> {
    let sig = strip_claude_signature_prefix(raw);
    if sig.is_empty() {
        return Err("empty signature".to_string());
    }
    if sig.len() > MAX_SIGNATURE_LEN {
        return Err("signature exceeds maximum length".to_string());
    }
    // 首字节 0x08 的载荷 base64 后必以 'C' 开头(0x08>>2 == 2),
    // 先查这个可让 E/R 与 Gemini 信封免解码即被拒
    if sig.as_bytes()[0] != b'C' {
        return Err("invalid Claude CAIS signature: expected 'C' prefix".to_string());
    }
    let decoded = STANDARD
        .decode(sig.as_bytes())
        .map_err(|_| "invalid Claude CAIS signature: base64 decode failed".to_string())?;
    if decoded.is_empty() {
        return Err("invalid Claude CAIS signature: empty after decode".to_string());
    }
    if decoded[0] != CLAUDE_CAIS_MARKER {
        return Err(format!(
            "invalid Claude CAIS signature: expected first byte 0x{CLAUDE_CAIS_MARKER:02x}, got 0x{:02x}",
            decoded[0]
        ));
    }

    let mut container: Option<Vec<u8>> = None;
    walk_protobuf_fields(&decoded, |num, typ, raw_field| {
        match num {
            1 => {
                cais_varint(typ, raw_field, "CAIS top-level field 1 envelope version")?;
            }
            2 => {
                container = Some(cais_bytes(
                    typ,
                    raw_field,
                    "CAIS top-level field 2 container",
                )?);
            }
            3 => {
                cais_varint(typ, raw_field, "CAIS top-level field 3 trailer")?;
            }
            _ => {}
        }
        Ok(())
    })?;
    let container = container.ok_or_else(|| {
        "invalid Claude CAIS signature: missing top-level field 2 container".to_string()
    })?;

    let mut channel_block: Option<Vec<u8>> = None;
    walk_protobuf_fields(&container, |num, typ, raw_field| {
        if num == 1 {
            channel_block = Some(cais_bytes(
                typ,
                raw_field,
                "CAIS container field 1 channel block",
            )?);
        }
        Ok(())
    })?;
    let channel_block = channel_block.ok_or_else(|| {
        "invalid Claude CAIS signature: missing container field 1 channel block".to_string()
    })?;

    inspect_cais_channel_block(&channel_block)
}

fn inspect_cais_channel_block(channel_block: &[u8]) -> Result<(), String> {
    let mut have_channel_id = false;
    let mut have_signature_bytes = false;
    let mut have_model_text = false;
    walk_protobuf_fields(channel_block, |num, typ, raw| {
        match num {
            1 => {
                cais_varint(typ, raw, "CAIS channel field 1 channel_id")?;
                have_channel_id = true;
            }
            3 => {
                cais_varint(typ, raw, "CAIS channel field 3 version")?;
            }
            5 => {
                let value = cais_bytes(typ, raw, "CAIS channel field 5 signature bytes")?;
                if value.is_empty() {
                    return Err("invalid Claude CAIS signature: channel field 5 signature bytes must not be empty".to_string());
                }
                have_signature_bytes = true;
            }
            6 => {
                let value = cais_utf8(typ, raw, "CAIS channel field 6 model_text")?;
                if !value.starts_with(CLAUDE_CAIS_MODEL_TEXT_PREFIX) {
                    return Err(format!(
                        "invalid Claude CAIS signature: channel field 6 model_text must start with {CLAUDE_CAIS_MODEL_TEXT_PREFIX:?}, got {value:?}"
                    ));
                }
                have_model_text = true;
            }
            7 => {
                cais_varint(typ, raw, "CAIS channel field 7")?;
            }
            8 => {
                cais_utf8(typ, raw, "CAIS channel field 8 block kind")?;
            }
            11 => {
                let value = cais_utf8(typ, raw, "CAIS channel field 11 context id")?;
                if !is_canonical_uuid(&value) {
                    return Err(format!(
                        "invalid Claude CAIS signature: channel field 11 context id must be a canonical UUID, got {value:?}"
                    ));
                }
            }
            _ => {}
        }
        Ok(())
    })?;
    if !have_channel_id {
        return Err(
            "invalid Claude CAIS signature: missing channel field 1 channel_id".to_string(),
        );
    }
    if !have_signature_bytes {
        return Err(
            "invalid Claude CAIS signature: missing channel field 5 signature bytes".to_string(),
        );
    }
    if !have_model_text {
        return Err(
            "invalid Claude CAIS signature: missing channel field 6 model_text".to_string(),
        );
    }
    Ok(())
}

fn cais_varint(typ: WireType, raw: &[u8], label: &str) -> Result<u64, String> {
    if typ != WireType::Varint {
        return Err(format!(
            "invalid Claude CAIS signature: {label} must be varint"
        ));
    }
    consume_varint(raw)
        .map(|(v, _)| v)
        .ok_or_else(|| format!("invalid Claude CAIS signature: failed to decode {label}"))
}

fn cais_bytes(typ: WireType, raw: &[u8], label: &str) -> Result<Vec<u8>, String> {
    if typ != WireType::Bytes {
        return Err(format!(
            "invalid Claude CAIS signature: {label} must be bytes"
        ));
    }
    consume_bytes(raw)
        .map(|(v, _)| v.to_vec())
        .ok_or_else(|| format!("invalid Claude CAIS signature: failed to decode {label}"))
}

fn cais_utf8(typ: WireType, raw: &[u8], label: &str) -> Result<String, String> {
    let value = cais_bytes(typ, raw, label)?;
    String::from_utf8(value)
        .map_err(|_| format!("invalid Claude CAIS signature: {label} must be valid UTF-8"))
}

/// 规范 UUID 形状:36 字符,8/13/18/23 位为 '-',其余十六进制(对齐 isCanonicalUUID)
fn is_canonical_uuid(s: &str) -> bool {
    let b = s.as_bytes();
    if b.len() != 36 {
        return false;
    }
    b.iter().enumerate().all(|(i, &c)| match i {
        8 | 13 | 18 | 23 => c == b'-',
        _ => c.is_ascii_digit() || (b'a'..=b'f').contains(&c) || (b'A'..=b'F').contains(&c),
    })
}

// ---- Gemini thoughtSignature(对齐 gemini_validation.go)----

/// 是否为 Gemini 文档化的合成历史 bypass 哨兵(对齐 IsGeminiThoughtSignatureBypass)
pub fn is_gemini_thought_signature_bypass(raw: &str) -> bool {
    matches!(
        raw.trim(),
        GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR | GEMINI_CONTEXT_ENGINEERING_BYPASS
    )
}

/// Gemini 签名解码:先带 padding 标准 base64,再裸标准 base64
/// (对齐 decodeGeminiThoughtSignature:StdEncoding 后 RawStdEncoding)
fn decode_gemini_thought_signature(sig: &str) -> Result<Vec<u8>, String> {
    if sig.len() > MAX_SIGNATURE_LEN {
        return Err("Gemini thought signature exceeds maximum length".to_string());
    }
    STANDARD
        .decode(sig.as_bytes())
        .or_else(|_| STANDARD_NO_PAD.decode(sig.as_bytes()))
        .map_err(|_| "invalid Gemini thought signature: base64 decode failed".to_string())
}

/// Gemini 签名形状校验(对齐 InspectGeminiThoughtSignature)。
/// 只校验不透明传输信封,不证明签名来自 Gemini 或可被 Gemini 解密。
/// `require_known_envelope` 为真时要求解码载荷命中已观测 protobuf 信封,
/// 从而拒掉 base64 UUID 之类不透明值。
fn inspect_gemini_thought_signature(
    raw: &str,
    allow_bypass_sentinel: bool,
    require_known_envelope: bool,
) -> Result<(), String> {
    let sig = raw.trim();
    if sig.is_empty() {
        return Err("empty Gemini thought signature".to_string());
    }
    if is_valid_claude_cais_signature(sig) {
        return Err("invalid Gemini thought signature: detected Claude CAIS signature".to_string());
    }
    if is_gemini_thought_signature_bypass(sig) {
        return if allow_bypass_sentinel {
            Ok(())
        } else {
            Err("Gemini thought signature bypass sentinel is not allowed".to_string())
        };
    }
    let decoded = decode_gemini_thought_signature(sig)?;
    if decoded.is_empty() {
        return Err("invalid Gemini thought signature: empty decoded payload".to_string());
    }
    let known_envelope = is_gemini_field2_envelope(&decoded);
    if require_known_envelope && !known_envelope {
        return Err("invalid Gemini thought signature: unknown envelope".to_string());
    }
    Ok(())
}

/// Gemini 签名有效性(对齐 IsValidGeminiThoughtSignature)
fn is_valid_gemini_thought_signature(raw: &str, require_known_envelope: bool) -> bool {
    inspect_gemini_thought_signature(raw, false, require_known_envelope).is_ok()
}

/// 唯一 replay-safe 的 Gemini 信封:protobuf field2 单记录形
/// (对齐 classifyGeminiThoughtSignatureEnvelope + isGeminiField2Envelope:
/// ascii_uuid 先判且永不 known,再判 field2 要求 RecordCount == 1 且 opaque 载荷非空)
fn is_gemini_field2_envelope(decoded: &[u8]) -> bool {
    if decoded.is_empty() || is_ascii_uuid_bytes(decoded) {
        return false;
    }
    match consume_gemini_field2_field1_value(decoded) {
        Some(value) => is_likely_gemini_opaque_payload(&value) || is_ascii_uuid_bytes(&value),
        None => false,
    }
}

/// 解 field2{ field1{ value } } 双层单记录信封,要求每层都恰好消费完
/// (对齐 consumeGeminiField2Field1Value)
fn consume_gemini_field2_field1_value(decoded: &[u8]) -> Option<Vec<u8>> {
    let (num, typ, n) = consume_tag(decoded)?;
    if num != 2 || typ != WireType::Bytes {
        return None;
    }
    let (container, consumed) = consume_bytes(&decoded[n..])?;
    if n + consumed != decoded.len() {
        return None;
    }
    let (inner_num, inner_typ, inner_n) = consume_tag(container)?;
    if inner_num != 1 || inner_typ != WireType::Bytes {
        return None;
    }
    let (value, inner_consumed) = consume_bytes(&container[inner_n..])?;
    if inner_n + inner_consumed != container.len() {
        return None;
    }
    Some(value.to_vec())
}

/// 信封体是 Google Tink 原语输出:一个前缀类型字节(0x01 选 TINK 前缀)+
/// 四字节大端 key id + 密文。只校验前缀类型字节,因为它是格式常量而 key id
/// 是 Google 会轮换的密钥材料(对齐 isLikelyGeminiOpaquePayload 注释)。
fn is_likely_gemini_opaque_payload(value: &[u8]) -> bool {
    !value.is_empty() && value[0] == 0x01
}

/// 36 字节 ASCII UUID(对齐 isASCIIUUIDBytes)
fn is_ascii_uuid_bytes(decoded: &[u8]) -> bool {
    decoded.len() == 36 && is_canonical_uuid(&String::from_utf8_lossy(decoded))
}

/// 已识别的 Gemini 提供方签名(对齐 isRecognizedGeminiProviderSignature):
/// CAIS 优先排除,再要求已知信封
fn is_recognized_gemini_provider_signature(raw: &str) -> bool {
    if is_valid_claude_cais_signature(raw) {
        return false;
    }
    is_valid_gemini_thought_signature(raw, true)
}

// ---- GPT Fernet / Kimi 固定长度 / Grok 无信封(对齐 gpt_/kimi_/grok_validation.go)----

/// 仅校验 GPT Fernet 信封外层形状,不验证可解密性(对齐 InspectGPTReasoningSignature)。
/// Fernet token 结构:version(1) + timestamp(8) + IV(16) + ciphertext(>=16,16 字节对齐) + HMAC(32)
pub fn is_valid_gpt_reasoning_signature(signature: &str) -> bool {
    // 最小长度 = 1 + 8 + 16 + 16 + 32 = 73 字节(含最短 ciphertext)
    const MIN_DECODED_LEN: usize = 1 + 8 + 16 + 16 + 32;
    let sig = signature.trim();
    if sig.is_empty()
        || sig.len() > MAX_SIGNATURE_LEN
        || !sig.starts_with("gAAAA")
        || !sig
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'='))
    {
        return false;
    }
    let Ok(decoded) = URL_SAFE_NO_PAD
        .decode(sig)
        .or_else(|_| URL_SAFE.decode(sig))
    else {
        return false;
    };
    if decoded.len() < MIN_DECODED_LEN || decoded[0] != 0x80 {
        return false;
    }
    let ciphertext_len = decoded.len() - 1 - 8 - 16 - 32;
    ciphertext_len > 0 && ciphertext_len % 16 == 0
}

/// Kimi thinking 签名传输形状(对齐 InspectKimiThinkingSignature)。
/// 与 Claude/Gemini/GPT 不同,此校验对载荷本身一无所证:只报告长度与字符类命中
/// Kimi 产出。长度是唯一信号,故必须在全部自描述信封探测都拒绝后才跑,
/// 防长度巧合捕获他族签名。
pub fn is_valid_kimi_thinking_signature(raw: &str) -> bool {
    let sig = raw.trim();
    if sig.is_empty() || sig != raw {
        return false;
    }
    if sig.len() != KIMI_NON_STREAMING_LEN && sig.len() != KIMI_STREAMING_LEN {
        return false;
    }
    if sig.contains('=') {
        return false;
    }
    if !sig
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/')
    {
        return false;
    }
    if split_signature_provider_prefix(sig).2 {
        return false;
    }
    if rejects_foreign_envelope(sig) {
        return false;
    }
    let Ok(decoded) = STANDARD_NO_PAD.decode(sig) else {
        return false;
    };
    byte_entropy_ratio(&decoded) >= MIN_ENTROPY_RATIO
}

/// Grok encrypted_content 传输形状(对齐 InspectGrokEncryptedContent)。
/// 这不是提供方分类器,不得当分类器用:xAI 无自描述信封,观测载荷与均匀随机字节
/// 不可区分。调用方必须先由模型/路由确认来源,再把结果当回放安全检查。
pub fn is_valid_grok_encrypted_content(raw: &str) -> bool {
    let sig = raw.trim();
    if sig.is_empty() || sig.len() > MAX_GROK_LEN || sig != raw {
        return false;
    }
    // unpadded standard base64
    if sig.contains('=') {
        return false;
    }
    if !sig
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'+' || b == b'/')
    {
        return false;
    }
    if split_signature_provider_prefix(sig).2 {
        return false;
    }
    if rejects_foreign_envelope(sig) {
        return false;
    }
    // Kimi 同样无信封,预过滤收窄不了,必须无条件跑
    if is_valid_kimi_thinking_signature(sig) {
        return false;
    }
    let Ok(decoded) = STANDARD_NO_PAD.decode(sig) else {
        return false;
    };
    if decoded.len() < MIN_GROK_DECODED_LEN {
        return false;
    }
    byte_entropy_ratio(&decoded) >= MIN_ENTROPY_RATIO
}

/// 外来信封拒绝(对齐 Grok/Kimi 校验里的 maybeSelfDescribingSignatureEnvelope 分支)。
/// 只对自描述信封可能产生的 base64 首字符跑,原生密文均匀分布,
/// 约 92% 真实流量一次比较即跳过整条链。
fn rejects_foreign_envelope(sig: &str) -> bool {
    if !maybe_self_describing_envelope(sig) {
        return false;
    }
    sig.starts_with("gAAAA")
        || is_valid_claude_thinking_signature(sig, true)
        || is_valid_claude_cais_signature(sig)
        || is_valid_gemini_thought_signature(sig, true)
}

// ---- 检测与兼容性决策(对齐 provider_compatibility.go)----

/// 分类能回放 raw 的提供方族(对齐 DetectSignatureProvider)。
/// Claude strict 校验必须跑在 Gemini 之前:Gemini 3 签名同样解自 E 前缀 base64,
/// 浅层前缀检查下会看着像 Claude。
///
/// CPA 的 DetectSignatureProviderForBlock 把 block_kind 透给
/// isRecognizedGeminiProviderSignature,而后者并不读它,故此处不带该参数。
pub fn detect_signature_provider(raw: &str) -> SignatureProvider {
    let sig = raw.trim();
    if sig.is_empty() {
        return SignatureProvider::Unknown;
    }
    let (prefixed, unprefixed, has_prefix) = split_signature_provider_prefix(sig);
    if has_prefix {
        match prefixed {
            SignatureProvider::Gemini => {
                if is_gemini_thought_signature_bypass(&unprefixed) {
                    return SignatureProvider::GeminiBypass;
                }
                if is_recognized_gemini_provider_signature(&unprefixed) {
                    return SignatureProvider::Gemini;
                }
            }
            SignatureProvider::Claude => {
                if is_valid_claude_thinking_signature(&unprefixed, true)
                    || is_valid_claude_cais_signature(&unprefixed)
                {
                    return SignatureProvider::Claude;
                }
            }
            SignatureProvider::Gpt if is_valid_gpt_reasoning_signature(&unprefixed) => {
                return SignatureProvider::Gpt;
            }
            _ => {}
        }
        return SignatureProvider::Unknown;
    }
    if sig.contains('#') {
        return SignatureProvider::Unknown;
    }
    // bypass 哨兵是字面量而非信封,必须在结构预过滤拒掉它之前匹配
    if is_gemini_thought_signature_bypass(sig) {
        return SignatureProvider::GeminiBypass;
    }
    // 探测从最强标记到最弱:GPT 有字面 "gAAAA";CAIS 有 0x08 + 字面 "claude-";
    // Claude 单/双层有 0x12 + 同一字面;Gemini 只验线格式无字面可锚,最弱故最后。
    // 预过滤只门控信封探测:不成信封的 blob 直接落到下面的长度探测,
    // 因为 Kimi 均匀分布的 base64 约 6% 概率以 "CERg" 开头。
    if maybe_self_describing_envelope(sig) {
        if is_valid_gpt_reasoning_signature(sig) {
            return SignatureProvider::Gpt;
        }
        if is_valid_claude_cais_signature(sig) {
            return SignatureProvider::Claude;
        }
        if is_valid_claude_thinking_signature(sig, true) {
            return SignatureProvider::Claude;
        }
        if is_recognized_gemini_provider_signature(sig) {
            return SignatureProvider::Gemini;
        }
    }
    // Kimi 无信封,只能在上面全部自描述探测都拒绝后才认领。排最后意味着
    // 长度巧合永远捕获不了别族签名。
    if is_valid_kimi_thinking_signature(sig) {
        return SignatureProvider::Kimi;
    }
    SignatureProvider::Unknown
}

/// 把签名回放进 target 的安全处置策略(对齐 DecideSignatureCompatibilityForModel)。
/// reason 字段只喂 CPA 日志,不移植。
pub fn decide_signature_compatibility(
    target: SignatureProvider,
    raw: &str,
    block_kind: SignatureBlockKind,
) -> SignatureDecision {
    let target = normalize_signature_target_provider(target);
    let detected = detect_signature_provider(raw);
    if signature_provider_matches_target(target, detected) {
        return SignatureDecision {
            target,
            detected,
            compatible: true,
            action: SignatureAction::Preserve,
            replacement: String::new(),
            normalized: normalize_compatible_signature_for_provider(target, raw),
        };
    }
    let (action, replacement) = match target {
        // Gemini 可用文档化哨兵绕过合成/不兼容的 model-part 签名
        SignatureProvider::Gemini => match block_kind {
            SignatureBlockKind::GeminiFunctionCall
            | SignatureBlockKind::GeminiModelPart
            | SignatureBlockKind::Unknown => (
                SignatureAction::ReplaceWithGeminiBypass,
                GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR.to_string(),
            ),
            _ => (SignatureAction::DropBlock, String::new()),
        },
        // Claude 对 thinking 块无跨提供方 bypass 哨兵
        SignatureProvider::Claude => (SignatureAction::DropBlock, String::new()),
        // GPT reasoning encrypted_content 无法由他族签名合成
        SignatureProvider::Gpt => (SignatureAction::DropBlock, String::new()),
        // Kimi 是唯一能在签名不匹配时保住 reasoning 文本的目标:其 Messages 端点
        // 从不回读该字段,思维连续性走 OpenAI 风格 reasoning_content
        SignatureProvider::Kimi => (SignatureAction::DropSignature, String::new()),
        // xAI 回放时解密 encrypted_content,外来或被改动的 blob 直接 400
        SignatureProvider::Grok => (SignatureAction::DropBlock, String::new()),
        SignatureProvider::Unknown | SignatureProvider::GeminiBypass => {
            (SignatureAction::NoCompatibleReplacement, String::new())
        }
    };
    SignatureDecision {
        target,
        detected,
        compatible: false,
        action,
        replacement,
        normalized: String::new(),
    }
}

/// 字节熵比(对齐 byteEntropyRatio)
fn byte_entropy_ratio(buf: &[u8]) -> f64 {
    if buf.is_empty() {
        return 0.0;
    }
    let mut counts = [0usize; 256];
    for &b in buf {
        counts[b as usize] += 1;
    }
    let n = buf.len() as f64;
    let mut entropy = 0.0;
    for &count in &counts {
        if count == 0 {
            continue;
        }
        let p = count as f64 / n;
        entropy -= p * p.log2();
    }
    let max_symbols = buf.len().min(256);
    if max_symbols <= 1 {
        return 0.0;
    }
    entropy / (max_symbols as f64).log2()
}

/// GeminiBypass 目标归一为 Gemini(对齐 normalizeSignatureTargetProvider)
fn normalize_signature_target_provider(provider: SignatureProvider) -> SignatureProvider {
    match provider {
        SignatureProvider::GeminiBypass => SignatureProvider::Gemini,
        other => other,
    }
}

/// 对齐 signatureProviderMatchesTarget。Grok 故意缺席:检测永不产出它,
/// Grok 目标必须由来源(模型/路由)+ is_valid_grok_encrypted_content 判定回放安全。
fn signature_provider_matches_target(
    target: SignatureProvider,
    detected: SignatureProvider,
) -> bool {
    match target {
        SignatureProvider::Gemini => {
            detected == SignatureProvider::Gemini || detected == SignatureProvider::GeminiBypass
        }
        SignatureProvider::Claude => detected == SignatureProvider::Claude,
        SignatureProvider::Gpt => detected == SignatureProvider::Gpt,
        SignatureProvider::Kimi => detected == SignatureProvider::Kimi,
        _ => false,
    }
}

/// 对齐 normalizeCompatibleSignatureForProvider。block_kind 在 CPA 里只透给
/// isRecognizedGeminiProviderSignature(后者不读),故此处不带。
fn normalize_compatible_signature_for_provider(target: SignatureProvider, raw: &str) -> String {
    let payload = signature_payload_without_prefix(raw);
    match normalize_signature_target_provider(target) {
        SignatureProvider::Claude => {
            if is_valid_claude_cais_signature(&payload) {
                return payload;
            }
            normalize_claude_provider_native(&payload, false).unwrap_or_default()
        }
        SignatureProvider::Gemini => {
            if is_gemini_thought_signature_bypass(&payload) {
                return payload;
            }
            if is_recognized_gemini_provider_signature(&payload) {
                return payload;
            }
            String::new()
        }
        SignatureProvider::Gpt => {
            if is_valid_gpt_reasoning_signature(&payload) {
                payload
            } else {
                String::new()
            }
        }
        SignatureProvider::Kimi => {
            if is_valid_kimi_thinking_signature(&payload) {
                payload
            } else {
                String::new()
            }
        }
        _ => String::new(),
    }
}

/// 可回放的提供方原生签名(对齐 CompatibleSignatureForProviderBlock)
pub fn compatible_signature_for_provider_block(
    target: SignatureProvider,
    raw: &str,
    block_kind: SignatureBlockKind,
) -> Option<String> {
    let decision = decide_signature_compatibility(target, raw, block_kind);
    if !decision.compatible || decision.normalized.is_empty() {
        return None;
    }
    Some(decision.normalized)
}

/// Antigravity Claude 回放要求的双层 R 形(对齐 CompatibleAntigravityClaudeThinkingSignature)。
/// 只接受严格可识别为 Claude 的签名,Gemini E 前缀信封无法从更松的旁路混进来。
pub fn compatible_antigravity_claude_thinking_signature(raw: &str) -> Option<String> {
    if detect_signature_provider(raw) != SignatureProvider::Claude {
        return None;
    }
    normalize_claude_thinking_signature(&signature_payload_without_prefix(raw), true).ok()
}

// ---- antigravity 请求侧解析(对齐 antigravity_claude_request.go)----
//
// CPA 的 carrier 编解码与签名恢复缓存均未移植,故此处只保留客户端自带签名那条路:
// CPA 默认 cache 模式,raw 非空时只走 resolveProviderCompatibleSignature,
// 失败即返回空,既不回退 bypass 归一化也不查恢复缓存。

/// 对齐 resolveProviderCompatibleSignature
pub fn resolve_provider_compatible_signature(
    target: SignatureProvider,
    raw: &str,
    block_kind: SignatureBlockKind,
) -> String {
    if raw.is_empty() {
        return String::new();
    }
    if target == SignatureProvider::Claude {
        return compatible_antigravity_claude_thinking_signature(raw).unwrap_or_default();
    }
    compatible_signature_for_provider_block(target, raw, block_kind).unwrap_or_default()
}

/// thinking 块签名解析(对齐 resolveThinkingSignatureRequired 的无缓存/无 carrier 子集)。
/// 返回空串表示该块无可用签名,调用方按目标族决定丢块还是保文本。
pub fn resolve_thinking_signature(model_name: &str, raw: &str) -> String {
    let target = signature_provider_from_model_name(model_name);
    if target == SignatureProvider::Gemini {
        // 非 carrier 输入在 CPA 里解出 inner=raw、marked=false,blockKind 固定 ModelPart
        return resolve_provider_compatible_signature(
            target,
            raw,
            SignatureBlockKind::GeminiModelPart,
        );
    }
    resolve_provider_compatible_signature(target, raw, SignatureBlockKind::Unknown)
}

/// tool_use 块签名解析(对齐 resolveToolUseThoughtSignature,allowSyntheticFallback=true)。
/// Anthropic tool_use 块不带签名字段,故 raw 通常为 None:
/// claude 目标返回空(不发 thoughtSignature),其余目标回落 Gemini bypass 哨兵。
pub fn resolve_tool_use_thought_signature(model_name: &str, raw: Option<&str>) -> String {
    let target = signature_provider_from_model_name(model_name);
    if let Some(value) = raw {
        let block_kind = if target == SignatureProvider::Gemini {
            SignatureBlockKind::GeminiFunctionCall
        } else {
            SignatureBlockKind::Unknown
        };
        let signature = resolve_provider_compatible_signature(target, value, block_kind);
        if !signature.is_empty() {
            return signature;
        }
    }
    if target == SignatureProvider::Claude {
        return String::new();
    }
    GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR.to_string()
}

// ---- antigravity 响应侧(对齐 antigravity_claude_response.go)----

/// 模型组(对齐 cache.GetModelGroup)。注意判定顺序 gpt → claude → gemini,
/// 与 signature_provider_from_model_name(claude 优先)不同,别互换。
pub fn model_group(model_name: &str) -> String {
    if model_name.contains("gpt") {
        return "gpt".to_string();
    }
    if model_name.contains("claude") {
        return "claude".to_string();
    }
    if model_name.contains("gemini") {
        return "gemini".to_string();
    }
    model_name.to_string()
}

/// R 形(双层 base64)解回 E 形(单层,Anthropic 格式)(对齐 decodeSignature)。
/// 非 R 前缀原样返回;R 前缀但解码失败返回 None(CPA 返回空串跳过该签名)。
pub fn decode_signature(signature: &str) -> Option<String> {
    if signature.is_empty() {
        return Some(String::new());
    }
    if !signature.starts_with('R') {
        return Some(signature.to_string());
    }
    let decoded = STANDARD.decode(signature).ok()?;
    String::from_utf8(decoded).ok()
}

/// 出站 signature 字段值(对齐 formatClaudeSignatureValue)。
/// claude 组解回提供方原生 E 形,解码失败得空串(CPA 照发空值,不省字段);
/// 其余组原样透传(Gemini 签名是原生回放状态)。
pub fn format_claude_signature_value(model_name: &str, signature: &str) -> String {
    if model_group(model_name) == "claude" {
        return decode_signature(signature).unwrap_or_default();
    }
    signature.to_string()
}

/// 测试夹具:构造真实形状的 Claude 签名(对齐 CPA buildClaudeSignaturePayload)。
/// 各转换层测试共用,避免手写假签名被严格校验拒掉。
#[cfg(test)]
pub(crate) mod fixtures {
    use base64::engine::general_purpose::STANDARD;
    use base64::Engine;

    fn append_varint(buf: &mut Vec<u8>, mut value: u64) {
        while value >= 0x80 {
            buf.push((value as u8) | 0x80);
            value >>= 7;
        }
        buf.push(value as u8);
    }

    fn append_tag(buf: &mut Vec<u8>, field: u64, wire: u64) {
        append_varint(buf, (field << 3) | wire);
    }

    fn append_bytes(buf: &mut Vec<u8>, field: u64, value: &[u8]) {
        append_tag(buf, field, 2);
        append_varint(buf, value.len() as u64);
        buf.extend_from_slice(value);
    }

    /// Claude 提供方原生签名(E 形,单层 base64)
    pub(crate) fn claude_native_signature(
        channel_id: u64,
        field2: Option<u64>,
        model_text: &str,
        include_field7: bool,
    ) -> String {
        let mut channel_block = Vec::new();
        append_tag(&mut channel_block, 1, 0);
        append_varint(&mut channel_block, channel_id);
        if let Some(value) = field2 {
            append_tag(&mut channel_block, 2, 0);
            append_varint(&mut channel_block, value);
        }
        if !model_text.is_empty() {
            append_bytes(&mut channel_block, 6, model_text.as_bytes());
        }
        if include_field7 {
            append_tag(&mut channel_block, 7, 0);
            append_varint(&mut channel_block, 0);
        }

        let mut container = Vec::new();
        append_bytes(&mut container, 1, &channel_block);
        append_bytes(&mut container, 2, &[0x11; 12]);
        append_bytes(&mut container, 3, &[0x22; 12]);
        append_bytes(&mut container, 4, &[0x33; 48]);

        let mut payload = Vec::new();
        append_bytes(&mut payload, 2, &container);
        append_tag(&mut payload, 3, 0);
        append_varint(&mut payload, 1);

        STANDARD.encode(payload)
    }

    /// 上游(Antigravity)形态签名(R 形,双层 base64)
    pub(crate) fn claude_upstream_signature(native: &str) -> String {
        STANDARD.encode(native.as_bytes())
    }

    /// CPA testAnthropicNativeSignature 等价夹具
    pub(crate) fn claude_native_default() -> String {
        claude_native_signature(12, Some(2), "claude-sonnet-4-6", true)
    }

    /// 真实 Fernet 形状 GPT 签名
    #[allow(dead_code)]
    pub(crate) fn valid_gpt_reasoning_signature() -> String {
        let mut raw = vec![0x80u8];
        raw.extend_from_slice(&[0u8; 8]);
        raw.extend_from_slice(&[1u8; 16]);
        raw.extend_from_slice(&[2u8; 16]);
        raw.extend_from_slice(&[3u8; 32]);
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(raw)
    }

    /// 非 Anthropic 原始签名(48 字节 0x34,无信封)
    #[allow(dead_code)]
    pub(crate) fn non_claude_raw_signature() -> String {
        STANDARD.encode([0x34u8; 48])
    }
}

#[cfg(test)]
mod tests {
    use super::fixtures::{
        claude_native_default, claude_native_signature, claude_upstream_signature,
        non_claude_raw_signature,
    };
    use super::*;

    #[test]
    fn test_claude_native_signature_reencoded_for_upstream() {
        // 对齐 CPA CacheModeAcceptsNativeSignature:E 形入站 → R 形出站
        let native = claude_native_default();
        let upstream = claude_upstream_signature(&native);
        assert_eq!(
            detect_signature_provider(&native),
            SignatureProvider::Claude
        );
        assert_eq!(
            resolve_thinking_signature("claude-sonnet-4-6", &native),
            upstream
        );
    }

    #[test]
    fn test_claude_double_layer_signature_passthrough() {
        // R 形已是上游形态,原样回放
        let native = claude_native_default();
        let upstream = claude_upstream_signature(&native);
        assert_eq!(
            resolve_thinking_signature("claude-sonnet-4-6", &upstream),
            upstream
        );
    }

    #[test]
    fn test_invalid_signature_yields_empty() {
        // 对齐 CPA CacheModeDropsInvalidSignature:非 Claude 信封一律空串(丢块)
        let foreign = non_claude_raw_signature();
        assert_eq!(
            resolve_thinking_signature("claude-sonnet-4-6", &foreign),
            ""
        );
        assert_eq!(resolve_thinking_signature("claude-sonnet-4-6", ""), "");
        // 极简载荷(只有 channel_id)仍严格有效,对齐 CPA testMinimalAnthropicSignature
        let minimal = claude_native_signature(12, None, "", false);
        assert_eq!(
            compatible_antigravity_claude_thinking_signature(&minimal),
            Some(claude_upstream_signature(&minimal))
        );
        // E 形但 container 缺 channel block:严格校验拒掉
        let broken = STANDARD.encode([0x12u8, 0x00]);
        assert_eq!(
            compatible_antigravity_claude_thinking_signature(&broken),
            None
        );
    }

    #[test]
    fn test_format_claude_signature_value() {
        let native = claude_native_default();
        let upstream = claude_upstream_signature(&native);
        // claude 组:R 形解回原生 E 形,无前缀
        assert_eq!(
            format_claude_signature_value("claude-sonnet-4-6", &upstream),
            native
        );
        // 解码失败得空串(CPA 照发空值)
        assert_eq!(
            format_claude_signature_value("claude-sonnet-4-6", "R!!!"),
            ""
        );
        // 其余组原样透传(Gemini 签名是原生回放状态)
        assert_eq!(
            format_claude_signature_value("gemini-2.5-pro", &upstream),
            upstream
        );
    }

    #[test]
    fn test_model_group_order() {
        // 对齐 GetModelGroup:gpt → claude → gemini → 模型名本身
        assert_eq!(model_group("gpt-5.6-terra"), "gpt");
        assert_eq!(model_group("claude-sonnet-4-6"), "claude");
        assert_eq!(model_group("gemini-2.5-pro"), "gemini");
        assert_eq!(model_group("grok-4.6"), "grok-4.6");
    }

    /// xorshift 填充的高熵字节,标准无填充 base64
    fn high_entropy_base64(byte_len: usize) -> String {
        let mut buf = vec![0u8; byte_len];
        let mut state = 0x2545_F491_4F6C_DD1Du64;
        for byte in buf.iter_mut() {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            *byte = (state & 0xff) as u8;
        }
        STANDARD_NO_PAD.encode(&buf)
    }

    #[test]
    fn test_kimi_fixed_raw_lengths() {
        // 对齐 kimiThinkingSignatureLens:只认原始长度 12946 / 4340
        // 12946 = 4*3236+2 → 9709 字节;4340 = 4*1085 → 3255 字节
        let non_streaming = high_entropy_base64(9709);
        let streaming = high_entropy_base64(3255);
        assert_eq!(non_streaming.len(), KIMI_NON_STREAMING_LEN);
        assert_eq!(streaming.len(), KIMI_STREAMING_LEN);
        assert!(is_valid_kimi_thinking_signature(&non_streaming));
        assert!(is_valid_kimi_thinking_signature(&streaming));
        // 长度不符 / 带填充 / 低熵一律拒
        assert!(!is_valid_kimi_thinking_signature(&high_entropy_base64(64)));
        assert!(!is_valid_kimi_thinking_signature(&format!(
            "{non_streaming}="
        )));
        assert!(!is_valid_kimi_thinking_signature(
            &STANDARD_NO_PAD.encode(vec![7u8; 9709])
        ));
    }

    #[test]
    fn test_grok_encrypted_content() {
        // 无信封高熵 blob 通过;kimi 固定长度、外来信封、低熵拒掉
        let opaque = high_entropy_base64(64);
        assert!(is_valid_grok_encrypted_content(&opaque));
        assert!(!is_valid_grok_encrypted_content(&high_entropy_base64(9709)));
        assert!(!is_valid_grok_encrypted_content(
            &claude_upstream_signature(&claude_native_default())
        ));
        assert!(!is_valid_grok_encrypted_content(
            &STANDARD_NO_PAD.encode(vec![7u8; 64])
        ));
        assert!(!is_valid_grok_encrypted_content(""));
    }

    #[test]
    fn test_tool_use_thought_signature_fallback() {
        // 对齐 resolveToolUseThoughtSignature(allowSyntheticFallback=true):
        // claude 目标空串(不发 thoughtSignature),其余目标回落 bypass 哨兵
        assert_eq!(
            resolve_tool_use_thought_signature("claude-opus-5", None),
            ""
        );
        assert_eq!(
            resolve_tool_use_thought_signature("gemini-2.5-pro", None),
            GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR
        );
        assert_eq!(
            resolve_tool_use_thought_signature("gpt-5.6", None),
            GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR
        );
        // gemini 目标:bypass 哨兵原样回放,无法识别的 blob 回落哨兵
        assert_eq!(
            resolve_tool_use_thought_signature(
                "gemini-2.5-pro",
                Some(GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR)
            ),
            GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR
        );
        assert_eq!(
            resolve_tool_use_thought_signature("gemini-2.5-pro", Some(&high_entropy_base64(48))),
            GEMINI_SKIP_THOUGHT_SIGNATURE_VALIDATOR
        );
    }
}
