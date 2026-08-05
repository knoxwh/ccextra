//! PR-E6：缓存破坏漂移检测器。
//!
//! 对已知 LLM 端点上的每个入站请求，对 **cache 热区** 计算一个
//! [`StructuralHash`]：
//!
//! - `system` — 规范化 system prompt 字节的 SHA-256（Anthropic 的
//!   `body.system`；OpenAI Chat 的第一条 `role=system` 消息；
//!   OpenAI Responses 的 `body.instructions` 或第一条 input/messages 的 system/developer 条目）。
//! - `tools` — 稳定 tool 指纹字节的 SHA-256，不含描述。
//! - `early_messages` — 前 8 个消息形态条目（若不足 8 个则为全部）的
//!   规范化字节的 SHA-256，排除 OpenAI 的 system/developer 前缀条目。
//!   跳过预期可能变化且无害的活跃区尾部。窗口大小与 Anthropic
//!   cache_control slot 3 的覆盖范围一致（在典型本地多轮会话中，
//!   即最终 assistant/tool_result 尾部之前的历史）。
//!
//! 在有界 LRU 中按会话跟踪上一次的哈希。当同一会话的后续请求在任一维度上
//! 不一致时，发出一条 `cache_drift_observed` 日志行，列出发生漂移的维度。
//! **绝不修改请求体**——检测器是纯观察者。
//!
//! # 日志级别
//!
//! - **父级 `system`/`tools` 漂移 → WARN。** 跨轮对 system prompt 或
//!   tool 集合的真实变更值得呈现：它可能破坏 cache prefix，且可操作。
//! - **会话级 `early_messages` 漂移 → DEBUG。** early 窗口包含当轮新的
//!   user 消息，它 *预期* 每轮都会变化——那是正常的会话更替，不是缓存破坏。
//!   上游响应中的 cache_read token 数才是判断前缀是否真正破坏的真相来源；
//!   检测器的 early_messages WARN 只是噪音。
//!
//! # 会话身份
//!
//! 身份是两级：
//!
//! - **父级 bucket** — 由请求头派生：`x-tklite-session-key`
//!   （不透明的 CLIProxyAPI sidecar），否则为哈希后的 `x-request-id`，
//!   否则为 `anonymous`。跟踪 system/tools-only 基线，使历史压缩或
//!   新会话无法掩盖真实的 prefix 漂移。
//! - **会话键** — 父级 bucket 加上 `:conv:` 以及
//!   `(model, 规范化首条会话消息)` 的截断 SHA-256。
//!   将共享同一父级 bucket 的并发会话分隔开；规范化形态会屏蔽
//!   `<system-reminder>` 块、丢弃不透明负载之外的 `cache_control` 标记，
//!   并排序对象键，使身份在每请求的更替中存活。
//!
//! # 隐私
//!
//! 原始 API 密钥和授权请求头从不被接受。没有 sidecar bucket 时，
//! `x-request-id` 会作为请求级回退被哈希；两个请求头都没有时，
//! 请求共享 `anonymous` bucket。判别器只以哈希形式存储和记录——
//! 模型名和消息内容永远不会进入日志。
//!
//! # 成本
//!
//! - 对 (system, tools, early_messages) 各做一次 SHA-256 更新。
//!   在 8 KB 的 system prompt 上总计约 200us。
//! - 一次 LRU 查找 + 插入。`lru = "0.12"` 摊还 O(1)。
//! - 一次 `tracing::info!`、`tracing::debug!` 或 `tracing::warn!`。

use std::num::NonZeroUsize;
use std::sync::{Arc, Mutex};

use http::HeaderMap;
use lru::LruCache;
use serde_json::Value;
use sha2::{Digest, Sha256};

use crate::cache_stabilization::tool_def_normalize::sort_schema_keys_recursive;

/// 我们正在哈希的供应商请求体形态。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiKind {
    /// `POST /v1/messages`（Anthropic）。
    Anthropic,
    /// `POST /v1/chat/completions`（OpenAI）。
    OpenAiChat,
    /// `POST /v1/responses`（OpenAI Responses API）。
    OpenAiResponses,
}

impl std::fmt::Display for ApiKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ApiKind::Anthropic => write!(f, "anthropic"),
            ApiKind::OpenAiChat => write!(f, "openai-chat"),
            ApiKind::OpenAiResponses => write!(f, "openai-responses"),
        }
    }
}

/// cache 热区的三轴结构指纹。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StructuralHash {
    pub system: [u8; 32],
    pub tools: [u8; 32],
    pub early_messages: [u8; 32],
    pub early_message_item_hashes: Vec<String>,
}

/// 多少个消息形态条目算作"early"前缀。
const EARLY_MESSAGES_WINDOW: usize = 8;

/// 计算 `kind` 所隐含请求体形态的 [`StructuralHash`]。
///
/// **此函数从不修改输入。**
pub fn compute_structural_hash(body: &Value, kind: ApiKind) -> StructuralHash {
    let system = hash_value(&extract_system(body, kind));
    let tools = stable_tool_fingerprint_hash(&extract_tools(body));
    let early_messages_value = extract_early_messages(body, kind);
    let early_messages = hash_value(&early_messages_value);
    let early_message_item_hashes = early_message_item_hashes(&early_messages_value);
    StructuralHash {
        system,
        tools,
        early_messages,
        early_message_item_hashes,
    }
}

/// 提取 "system" 轴。不存在时返回 `Value::Null`。
fn extract_system(body: &Value, kind: ApiKind) -> Value {
    match kind {
        ApiKind::Anthropic => body.get("system").cloned().unwrap_or(Value::Null),
        ApiKind::OpenAiChat => body
            .get("messages")
            .and_then(|v| v.as_array())
            .and_then(|arr| {
                arr.iter().find(|m| {
                    m.get("role")
                        .and_then(|r| r.as_str())
                        .map(is_openai_system_role)
                        .unwrap_or(false)
                })
            })
            .and_then(|m| m.get("content").cloned())
            .unwrap_or(Value::Null),
        ApiKind::OpenAiResponses => extract_responses_system(body),
    }
}

fn extract_responses_system(body: &Value) -> Value {
    if let Some(instructions) = usable_responses_instructions(body) {
        return instructions;
    }

    first_responses_system_content(body, "input")
        .or_else(|| first_responses_system_content(body, "messages"))
        .unwrap_or(Value::Null)
}

fn usable_responses_instructions(body: &Value) -> Option<Value> {
    let instructions = body.get("instructions")?;
    match instructions {
        Value::Null => None,
        Value::String(s) if s.trim().is_empty() => None,
        _ => Some(instructions.clone()),
    }
}

fn first_responses_system_content(body: &Value, key: &str) -> Option<Value> {
    body.get(key)
        .and_then(Value::as_array)
        .and_then(|arr| {
            arr.iter().find(|item| {
                item.get("role")
                    .and_then(Value::as_str)
                    .map(is_openai_system_role)
                    .unwrap_or(false)
            })
        })
        .and_then(|item| item.get("content").cloned())
}

/// 消息角色是否为 system prompt 角色（system 或 developer）。
/// Chat Completions 与 Responses 的提取都用它，使两种形态在什么算作
/// system 前缀上保持一致。
fn is_openai_system_role(role: &str) -> bool {
    matches!(role, "system" | "developer")
}

fn messages_array(body: &Value, kind: ApiKind) -> Option<&Vec<Value>> {
    match kind {
        ApiKind::Anthropic | ApiKind::OpenAiChat => body.get("messages").and_then(Value::as_array),
        ApiKind::OpenAiResponses => body
            .get("input")
            .and_then(Value::as_array)
            .or_else(|| body.get("messages").and_then(Value::as_array)),
    }
}

/// 请求是否为应从漂移检测中排除的附属后台请求。
///
/// Claude Code 会在与主会话相同的 session key 上发出后台请求——标题生成、
/// haiku 摘要等。这些请求的请求体形态（无 tools、单条消息）不同于主会话
/// （有 tools、多条消息）。若不过滤，漂移检测器会把这两种形态当作连续轮次
/// 比较，并在每个后台请求上发出虚假的 `cache_drift_observed` 警告。
///
/// 附属请求没有值得稳定的 cache prefix（单轮、无可复用前缀），
/// 因此为它们跳过漂移检测不会损失任何东西。
///
/// 启发式：`tools` 缺失/null/为空 且 非 system 消息数为 ≤ 1。消息数排除
/// OpenAI 的 `system`/`developer` 前缀条目，与 [`extract_early_messages`]
/// 保持一致，使附属判断与它所守护的哈希在什么算作"消息"上一致。
pub fn is_ancillary_request(body: &Value, kind: ApiKind) -> bool {
    // 当 tools 缺失、为 null 或为空数组时，它被视为"空"——三种情况都表示
    // "没有可用的 tools"，因此统一处理。
    let tools_empty = body
        .get("tools")
        .map(|t| t.as_array().is_none() || t.as_array().is_some_and(|a| a.is_empty()))
        .unwrap_or(true);
    let msg_count = non_system_message_count(body, kind);
    tools_empty && msg_count <= 1
}

/// 统计消息形态条目数，排除 OpenAI 的 `system`/`developer` 前缀条目，
/// 与 [`extract_early_messages`] 在哈希前应用的过滤一致。Anthropic 的
/// `messages` 内没有 system 角色（其 system prompt 是顶层字段），
/// 因此所有消息都计数。
fn non_system_message_count(body: &Value, kind: ApiKind) -> usize {
    let messages = match messages_array(body, kind) {
        Some(arr) => arr,
        None => return 0,
    };
    match kind {
        ApiKind::Anthropic => messages.len(),
        ApiKind::OpenAiChat | ApiKind::OpenAiResponses => messages
            .iter()
            .filter(|m| {
                m.get("role")
                    .and_then(Value::as_str)
                    .map(|s| !is_openai_system_role(s))
                    .unwrap_or(true)
            })
            .count(),
    }
}

/// 提取 "tools" 轴。
fn extract_tools(body: &Value) -> Value {
    body.get("tools").cloned().unwrap_or(Value::Null)
}

/// 提取前 [`EARLY_MESSAGES_WINDOW`] 个消息形态条目。
fn extract_early_messages(body: &Value, kind: ApiKind) -> Value {
    let messages = match messages_array(body, kind) {
        Some(arr) => arr,
        None => return Value::Null,
    };
    let early: Vec<Value> = match kind {
        ApiKind::OpenAiChat => messages
            .iter()
            .filter(|m| {
                m.get("role")
                    .and_then(Value::as_str)
                    .map(|s| !is_openai_system_role(s))
                    .unwrap_or(true)
            })
            .take(EARLY_MESSAGES_WINDOW)
            .cloned()
            .collect(),
        ApiKind::OpenAiResponses => messages
            .iter()
            .filter(|m| {
                m.get("role")
                    .and_then(Value::as_str)
                    .map(|s| !is_openai_system_role(s))
                    .unwrap_or(true)
            })
            .take(EARLY_MESSAGES_WINDOW)
            .cloned()
            .collect(),
        ApiKind::Anthropic => messages
            .iter()
            .take(EARLY_MESSAGES_WINDOW)
            .cloned()
            .collect(),
    };
    Value::Array(early)
}

/// 属于用户会话的第一条消息形态条目，排除 system/developer 前缀条目。
/// 用作会话隔离的会话判别器。
///
/// - Anthropic：`messages[0]`（`messages` 内无 system 角色）。
/// - OpenAI Chat：第一条 role 不是 system/developer 的 `messages[]` 条目。
/// - OpenAI Responses：第一条携带 role 字段且不是 system/developer 的
///   `input[]`（回退 `messages[]`）条目。无 role 的条目
///   （function_call_output、reasoning）会被跳过——它们只在首次交换后才出现，
///   无法锚定一个会话。
fn first_conversation_message(body: &Value, kind: ApiKind) -> Option<Value> {
    let messages = messages_array(body, kind)?;
    match kind {
        ApiKind::Anthropic => messages.first().cloned(),
        ApiKind::OpenAiChat | ApiKind::OpenAiResponses => messages
            .iter()
            .find(|m| {
                m.get("role")
                    .and_then(Value::as_str)
                    .map(|r| !is_openai_system_role(r))
                    .unwrap_or(false)
            })
            .cloned(),
    }
}

/// 替换判别器输入中每个 `<system-reminder>...</system-reminder>` 跨度的占位符。
/// Claude Code 会把这些块注入第一条 user 消息，其中包含每请求动态内容
/// （CWD、gitStatus、日期）；屏蔽可让会话身份在多次轮次间保持稳定。
const REMINDER_PLACEHOLDER: &str = "<system-reminder>[masked]</system-reminder>";

/// 将 `s` 中的每个 `<system-reminder>...</system-reminder>` 跨度替换为
/// [`REMINDER_PLACEHOLDER`]。顺序跨度扫描器；处理多个块。未闭合的起始标签
/// 保持原样——无论哪种方式都确定。
fn mask_reminders_in_string(s: &str) -> String {
    const OPEN: &str = "<system-reminder";
    const CLOSE: &str = "</system-reminder>";
    let mut out = String::with_capacity(s.len());
    let mut rest = s;
    loop {
        let Some(open_idx) = rest.find(OPEN) else {
            out.push_str(rest);
            break;
        };
        let after_open = &rest[open_idx..];
        // 起始标签可能携带属性；先找到它的 `>`。
        let Some(tag_end) = after_open.find('>') else {
            out.push_str(rest);
            break;
        };
        let body_start = open_idx + tag_end + 1;
        let Some(close_idx) = rest[body_start..].find(CLOSE) else {
            out.push_str(rest);
            break;
        };
        let span_end = body_start + close_idx + CLOSE.len();
        out.push_str(&rest[..open_idx]);
        out.push_str(REMINDER_PLACEHOLDER);
        rest = &rest[span_end..];
    }
    out
}

/// 就地屏蔽消息形态字符串内容（字符串内容和块数组 `text` 字段）中的
/// reminder 块。
fn mask_reminders_in_value(value: &mut Value) {
    match value {
        Value::String(s) => {
            if s.contains("<system-reminder") {
                *s = mask_reminders_in_string(s);
            }
        }
        Value::Array(items) => {
            for item in items.iter_mut() {
                mask_reminders_in_value(item);
            }
        }
        Value::Object(map) => {
            for (_k, v) in map.iter_mut() {
                mask_reminders_in_value(v);
            }
        }
        _ => {}
    }
}

/// 子树为不透明用户负载的键：其中 `cache_control` 是数据而非缓存断点标记，
/// 必须保留。
fn is_opaque_payload_key(key: &str) -> bool {
    matches!(key, "input" | "arguments" | "json")
}

/// 就地移除不透明负载子树之外的 `cache_control` 键。
fn remove_cache_control(value: &mut Value) {
    match value {
        Value::Array(items) => {
            for item in items.iter_mut() {
                remove_cache_control(item);
            }
        }
        Value::Object(map) => {
            map.remove("cache_control");
            for (k, v) in map.iter_mut() {
                if is_opaque_payload_key(k) {
                    continue;
                }
                remove_cache_control(v);
            }
        }
        _ => {}
    }
}

/// 规范化判别器字节：克隆消息、屏蔽 reminders、丢弃不透明负载之外的
/// `cache_control` 标记，然后递归排序所有对象键。数组顺序和标量值保持不变。
fn canonicalize_discriminator(value: &Value) -> Value {
    let mut v = value.clone();
    mask_reminders_in_value(&mut v);
    remove_cache_control(&mut v);
    sort_schema_keys_recursive(&mut v);
    v
}

/// `serde_json::to_vec(value)` 的 SHA-256。
fn hash_value(value: &Value) -> [u8; 32] {
    let bytes = serde_json::to_vec(value).unwrap_or_default();
    let digest = Sha256::digest(&bytes);
    let mut out = [0u8; 32];
    out.copy_from_slice(&digest);
    out
}

/// early-messages 窗口中每条消息的逐条 12 位十六进制 SHA-256 前缀。
///
/// 每个摘要只保留前 12 个十六进制字符（6 字节），以最小化 LRU 内存，
/// 同时仍为位置级漂移识别提供足够的碰撞抵抗力。
fn early_message_item_hashes(messages: &Value) -> Vec<String> {
    messages
        .as_array()
        .map(|items| {
            items
                .iter()
                .map(|v| hex_prefix(&hash_value(v), 6))
                .collect()
        })
        .unwrap_or_default()
}

/// tools 数组的稳定指纹哈希。
///
/// 只包含身份 + 结构字段（tool type、name、input schema），排除易变的
/// `description`——它可能携带每请求动态文本（permissions、CWD、file-state），
/// 会导致结构哈希不必要地漂移。
///
/// 支持三种 tool 形态：
/// - **OpenAI function**：`tools[i].type` + `tools[i].function.{name, parameters}`
/// - **Anthropic**：`tools[i].type` + `tools[i].{name, input_schema}`
/// - **Responses direct**：`tools[i].type` + `tools[i].{name, parameters}`
///
/// 指纹按 (name, type, schema_hash) 排序，因此 tool 顺序变化不会导致哈希漂移。
/// schema 键被递归排序以实现确定性哈希——即使 normalize 阶段尚未运行也自足。
fn stable_tool_fingerprint_hash(tools: &Value) -> [u8; 32] {
    let tools_arr = match tools.as_array() {
        Some(arr) => arr,
        None => return hash_value(&Value::Null),
    };

    let mut fingerprints: Vec<Value> = tools_arr
        .iter()
        .map(|tool| {
            // OpenAI function 形态：tools[i].function.{name, parameters}
            if let Some(func) = tool.get("function") {
                let mut map = serde_json::Map::new();
                map.insert(
                    "type".to_string(),
                    tool.get("type").cloned().unwrap_or(Value::Null),
                );
                map.insert(
                    "name".to_string(),
                    func.get("name").cloned().unwrap_or(Value::Null),
                );
                map.insert(
                    "parameters".to_string(),
                    func.get("parameters").cloned().unwrap_or(Value::Null),
                );
                Value::Object(map)
            } else {
                // 非 function 形态（Anthropic / Responses direct）。
                // 始终包含顶层 type，以避免同名的不同 tool 类型之间发生碰撞。
                let mut map = serde_json::Map::new();
                map.insert(
                    "type".to_string(),
                    tool.get("type").cloned().unwrap_or(Value::Null),
                );
                map.insert(
                    "name".to_string(),
                    tool.get("name").cloned().unwrap_or(Value::Null),
                );
                // 优先用 parameters（Responses direct），回退到 input_schema（Anthropic）。
                let schema = tool
                    .get("parameters")
                    .or_else(|| tool.get("input_schema"))
                    .cloned()
                    .unwrap_or(Value::Null);
                map.insert("schema".to_string(), schema);
                Value::Object(map)
            }
        })
        .collect();

    // 在计算 schema 哈希用于排序之前，先递归排序每个指纹 schema 中的
    // JSON 对象键。覆盖三种 schema 键名："parameters"（OpenAI function）、
    // "schema"（非 function）、以及 "input_schema"（旧 Anthropic）。
    for fp in &mut fingerprints {
        let obj = fp.as_object();
        let has_params = obj.is_some_and(|m| m.contains_key("parameters"));
        let has_schema = obj.is_some_and(|m| m.contains_key("schema"));

        let schema = if has_params {
            fp.get_mut("parameters")
        } else if has_schema {
            fp.get_mut("schema")
        } else {
            fp.get_mut("input_schema")
        };
        if let Some(schema) = schema {
            sort_schema_keys_recursive(schema);
        }
    }

    // 按 (name, type, schema_hash) 排序指纹以获得确定性顺序。当重复的
    // name + type 具有不同 schema 时避免碰撞——tool 顺序变化不再导致哈希漂移。
    fingerprints.sort_by(|a, b| {
        let name_a = a.get("name").and_then(Value::as_str).unwrap_or("");
        let name_b = b.get("name").and_then(Value::as_str).unwrap_or("");
        let type_a = a.get("type").and_then(Value::as_str).unwrap_or("");
        let type_b = b.get("type").and_then(Value::as_str).unwrap_or("");

        let schema_a = a
            .get("parameters")
            .or_else(|| a.get("schema"))
            .or_else(|| a.get("input_schema"));
        let schema_b = b
            .get("parameters")
            .or_else(|| b.get("schema"))
            .or_else(|| b.get("input_schema"));

        name_a
            .cmp(name_b)
            .then_with(|| type_a.cmp(type_b))
            .then_with(|| {
                let hash_a = schema_a.map_or([0u8; 32], hash_value);
                let hash_b = schema_b.map_or([0u8; 32], hash_value);
                hash_a.cmp(&hash_b)
            })
    });

    hash_value(&Value::Array(fingerprints))
}

/// 在父级 bucket 级别跟踪的 system/tools-only 基线。
///
/// 与逐会话的 [`StructuralHash`] 分开维护，使身份轮换（历史压缩重写首条
/// 消息，或真正的新会话）无法悄悄吞掉对 system prompt 或 tool 集合的真实变更。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct ParentBaseline {
    system: [u8; 32],
    tools: [u8; 32],
}

/// 由一个互斥锁守护的两个基线：会话级完整哈希加上父级 system/tools 基线。
type DriftCaches = (LruCache<String, StructuralHash>, LruCache<String, ParentBaseline>);

/// 有界 会话 → 上次所见哈希 状态。
///
/// 两个 map 共享一个互斥锁，使每个请求的两次比较都保持原子性：
///
/// - `cache`：会话身份 → 完整 [`StructuralHash`]。
/// - `parent_cache`：父级 bucket → [`ParentBaseline`]。
#[derive(Clone)]
pub struct DriftState {
    cache: Arc<Mutex<DriftCaches>>,
}

impl DriftState {
    /// 构建一个新的 `DriftState`，每个 map 有界为 `capacity` 个条目。
    ///
    /// # Panics
    ///
    /// 当 `capacity == 0` 时 panic。
    pub fn new(capacity: usize) -> Self {
        let cap = NonZeroUsize::new(capacity).expect("DriftState capacity must be > 0");
        Self {
            cache: Arc::new(Mutex::new((
                LruCache::new(cap),
                LruCache::new(cap),
            ))),
        }
    }
}

impl std::fmt::Debug for DriftState {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let len = self.cache.lock().map(|c| c.0.len()).unwrap_or(0);
        f.debug_struct("DriftState").field("len", &len).finish()
    }
}

/// 将 `current` 与 `identity` 上次看到的哈希比较，并发出结构化 `tracing` 事件。
///
/// 会话级（完整三维，以 `identity.conversation` 为键）：
///
/// - 首次：`cache_drift_first_request`
/// - 哈希相同：无事件
/// - 任一维度不同：`cache_drift_observed`
///
/// 父级（仅 system/tools，以 `identity.parent` 为键）：
///
/// - 不匹配：当会话基线也是新的（压缩 / 新会话）时，发出带
///   `identity_rotated=true` 的 `cache_drift_observed`，
///   使真实的 system/tools 变更无法藏在身份轮换背后。
pub fn observe_drift(state: &DriftState, identity: &SessionIdentity, current: StructuralHash) {
    let session_key = identity.conversation.as_str();
    let session_prefix = session_key_log_prefix(session_key);
    let mut caches = match state.cache.lock() {
        Ok(c) => c,
        Err(poisoned) => {
            tracing::warn!(
                event = "cache_drift_state_mutex_poisoned",
                "drift detector mutex poisoned; recovering"
            );
            poisoned.into_inner()
        }
    };
    let (cache, parent_cache) = &mut *caches;

    let parent_dims = update_parent_baseline(parent_cache, identity, &current);

    let conversation_is_new = !cache.contains(session_key);
    if !parent_dims.is_empty() {
        tracing::warn!(
            event = "cache_drift_observed",
            session_key_hash = %session_prefix,
            drift_dims = %parent_dims.join(","),
            identity_rotated = conversation_is_new,
            current_hash_prefix = %structural_hash_log_prefix(&current),
            "cache_drift detector observed parent-level system/tools change"
        );
    }

    match cache.get(session_key).cloned() {
        None => {
            tracing::info!(
                event = "cache_drift_first_request",
                session_key_hash = %session_prefix,
                current_hash_prefix = %structural_hash_log_prefix(&current),
                "cache_drift detector observed a new session"
            );
            cache.put(session_key.to_string(), current);
        }
        Some(previous) if previous == current => {
            cache.put(session_key.to_string(), current);
        }
        Some(previous) => {
            log_conversation_drift(&session_prefix, &previous, &current);
            cache.put(session_key.to_string(), current);
        }
    }
}

/// 记录会话级漂移（early_messages 变化）。
/// 在可用时发出带详细差异信息的 DEBUG 级日志。
fn log_conversation_drift(
    session_prefix: &str,
    previous: &StructuralHash,
    current: &StructuralHash,
) {
    let dims = drift_dims(previous, current);
    let previous_hash_prefix = structural_hash_log_prefix(previous);
    let current_hash_prefix = structural_hash_log_prefix(current);

    match first_early_message_difference(previous, current) {
        Some(diff) if previous.early_messages != current.early_messages => {
            match (diff.previous_hash, diff.current_hash) {
                (Some(previous_item_hash), Some(current_item_hash)) => tracing::debug!(
                    event = "cache_drift_observed",
                    session_key_hash = %session_prefix,
                    drift_dims = %dims,
                    previous_hash_prefix = %previous_hash_prefix,
                    current_hash_prefix = %current_hash_prefix,
                    early_message_first_diff_index = diff.index,
                    early_message_previous_item_hash = %previous_item_hash,
                    early_message_current_item_hash = %current_item_hash,
                    "cache_drift detector observed structural change between turns"
                ),
                (Some(previous_item_hash), None) => tracing::debug!(
                    event = "cache_drift_observed",
                    session_key_hash = %session_prefix,
                    drift_dims = %dims,
                    previous_hash_prefix = %previous_hash_prefix,
                    current_hash_prefix = %current_hash_prefix,
                    early_message_first_diff_index = diff.index,
                    early_message_previous_item_hash = %previous_item_hash,
                    "cache_drift detector observed structural change between turns"
                ),
                (None, Some(current_item_hash)) => tracing::debug!(
                    event = "cache_drift_observed",
                    session_key_hash = %session_prefix,
                    drift_dims = %dims,
                    previous_hash_prefix = %previous_hash_prefix,
                    current_hash_prefix = %current_hash_prefix,
                    early_message_first_diff_index = diff.index,
                    early_message_current_item_hash = %current_item_hash,
                    "cache_drift detector observed structural change between turns"
                ),
                _ => tracing::debug!(
                    event = "cache_drift_observed",
                    session_key_hash = %session_prefix,
                    drift_dims = %dims,
                    previous_hash_prefix = %previous_hash_prefix,
                    current_hash_prefix = %current_hash_prefix,
                    early_message_first_diff_index = diff.index,
                    "cache_drift detector observed structural change between turns"
                ),
            }
        }
        _ => tracing::debug!(
            event = "cache_drift_observed",
            session_key_hash = %session_prefix,
            drift_dims = %dims,
            previous_hash_prefix = %previous_hash_prefix,
            current_hash_prefix = %current_hash_prefix,
            "cache_drift detector observed structural change between turns"
        ),
    }
}

/// 更新父级基线，若发生变化则返回漂移维度。
/// 返回发生漂移的维度名向量（"system"、"tools"）。
fn update_parent_baseline(
    parent_cache: &mut lru::LruCache<String, ParentBaseline>,
    identity: &SessionIdentity,
    current: &StructuralHash,
) -> Vec<&'static str> {
    let parent_baseline = ParentBaseline {
        system: current.system,
        tools: current.tools,
    };

    let mut parent_dims: Vec<&'static str> = Vec::with_capacity(2);

    if let Some(previous_parent) = parent_cache.get(&identity.parent).copied() {
        if previous_parent.system != parent_baseline.system {
            parent_dims.push("system");
        }
        if previous_parent.tools != parent_baseline.tools {
            parent_dims.push("tools");
        }
    }

    parent_cache.put(identity.parent.clone(), parent_baseline);
    parent_dims
}

/// 两级漂移身份。
///
/// - `parent` — 来自请求头的 bucket（未变的旧逻辑）。
///   跟踪 system/tools-only 基线，使压缩或身份轮换无法悄悄吞掉真实的
///   system/tools 变更。
/// - `conversation` — parent 加上规范化 `(model, first_conversation_message)`
///   的判别器哈希。将共享一个父级 bucket 的并发会话分隔开。
///
/// 两个字符串都是不透明的；绝不直接记录它们。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionIdentity {
    pub parent: String,
    pub conversation: String,
}

/// 从请求头和已稳定化的请求体派生漂移身份。
///
/// 父级 bucket 优先级顺序：
///
/// 1. `x-claude-code-session-id` — Claude Code 原生会话头，整会话稳定。
/// 2. `x-tklite-session-key` — 来自 CLIProxyAPI 的不透明 sidecar bucket。
/// 3. `x-request-id` 请求头（SHA-256 哈希）——每请求回退。
/// 4. 回退 `"anonymous"`。
///
/// 判别器是 `SHA-256(规范化 model + 规范化首条会话消息)`，截断为 16 个
/// 十六进制字符并与父级 bucket 组合。缺失的 model 哈希为 `"nomodel"`，
/// 缺失的消息为 `Value::Null`——身份派生从不失败，也从不修改请求体。
pub fn derive_session_key(headers: &HeaderMap, body: &Value, kind: ApiKind) -> SessionIdentity {
    let parent = derive_parent_bucket(headers);
    let discriminator = conversation_discriminator(body, kind);
    SessionIdentity {
        conversation: format!("{}:conv:{}", parent, discriminator),
        parent,
    }
}

/// 仅依赖请求头的 bucket 派生。
/// 优先级(对齐 CPA 会话身份链):
/// 1. `x-claude-code-session-id` — Claude Code 原生会话头,整会话稳定
/// 2. `x-tklite-session-key` — CLIProxyAPI 不透明 sidecar bucket
/// 3. `x-request-id`(哈希)— 每请求回退
/// 4. `anonymous`
fn derive_parent_bucket(headers: &HeaderMap) -> String {
    if let Some(session_id) = headers
        .get(crate::session::CLAUDE_CODE_SESSION_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        return format!("claude-code:{}", session_id);
    }

    if let Some(session_key) = headers
        .get("x-tklite-session-key")
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        return format!("tklite:{}", session_key);
    }

    if let Some(request_id) = headers.get("x-request-id").and_then(|v| v.to_str().ok()) {
        return format!("request:{}", hash_secret(request_id));
    }

    "anonymous".to_string()
}

/// `(model, first_conversation_message)` 的 16 位十六进制字符判别器。
fn conversation_discriminator(body: &Value, kind: ApiKind) -> String {
    let model = body
        .get("model")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .unwrap_or("nomodel");
    let first_message = first_conversation_message(body, kind).unwrap_or(Value::Null);
    let canonical = canonicalize_discriminator(&first_message);

    let mut hasher = Sha256::new();
    hasher.update(model.as_bytes());
    hasher.update([0u8]); // 分隔符：model 长度无界
    hasher.update(serde_json::to_vec(&canonical).unwrap_or_default());
    let digest = hasher.finalize();
    hex_prefix(&digest, 8)
}

/// SHA-256(session_key) 的 16 字符十六进制前缀。
fn session_key_log_prefix(session_key: &str) -> String {
    let digest = Sha256::digest(session_key.as_bytes());
    hex_prefix(&digest, 16)
}

/// 拼接后的结构哈希的 12 字符十六进制前缀。
fn structural_hash_log_prefix(hash: &StructuralHash) -> String {
    let mut hasher = Sha256::new();
    hasher.update(hash.system);
    hasher.update(hash.tools);
    hasher.update(hash.early_messages);
    let digest = hasher.finalize();
    hex_prefix(&digest, 12)
}

/// 前 `take` 字节的小写十六进制。
fn hex_prefix(bytes: &[u8], take: usize) -> String {
    let take = take.min(bytes.len());
    let mut out = String::with_capacity(take * 2);
    for b in &bytes[..take] {
        const HEX: &[u8; 16] = b"0123456789abcdef";
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0xf) as usize] as char);
    }
    out
}

/// 两个轮次之间 early-message 条目哈希首次不同的位置。
struct EarlyMessageDifference {
    index: usize,
    previous_hash: Option<String>,
    current_hash: Option<String>,
}

/// 找到 `previous` 与 `current` 之间哈希不同的第一条 early-message 条目。
/// 所有条目都匹配时返回 `None`。
fn first_early_message_difference(
    previous: &StructuralHash,
    current: &StructuralHash,
) -> Option<EarlyMessageDifference> {
    let max_len = previous
        .early_message_item_hashes
        .len()
        .max(current.early_message_item_hashes.len());

    for index in 0..max_len {
        let previous_item = previous.early_message_item_hashes.get(index);
        let current_item = current.early_message_item_hashes.get(index);
        if previous_item != current_item {
            return Some(EarlyMessageDifference {
                index,
                previous_hash: previous_item.cloned(),
                current_hash: current_item.cloned(),
            });
        }
    }
    None
}

/// 逗号连接的漂移维度列表。
fn drift_dims(prev: &StructuralHash, curr: &StructuralHash) -> String {
    let mut dims: Vec<&'static str> = Vec::with_capacity(3);
    if prev.system != curr.system {
        dims.push("system");
    }
    if prev.tools != curr.tools {
        dims.push("tools");
    }
    if prev.early_messages != curr.early_messages {
        dims.push("early_messages");
    }
    dims.join(",")
}

/// `secret` 的 SHA-256，截断为 16 个十六进制字符。
fn hash_secret(secret: &str) -> String {
    let digest = Sha256::digest(secret.as_bytes());
    hex_prefix(&digest, 16)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn anthropic_body(system: &str, tools: Value, msgs: Vec<&str>) -> Value {
        let messages: Vec<Value> = msgs
            .into_iter()
            .map(|t| json!({"role": "user", "content": t}))
            .collect();
        json!({
            "model": "claude-3-5-sonnet-20241022",
            "system": system,
            "tools": tools,
            "messages": messages,
        })
    }

    fn make_state() -> DriftState {
        DriftState::new(8)
    }

    /// 使用固定父级 bucket 和给定会话标签的身份。
    /// 测试 `observe_drift` 状态处理的用例使用它，因此不依赖请求头/请求体派生。
    fn identity(conv: &str) -> SessionIdentity {
        SessionIdentity {
            parent: "test-parent".to_string(),
            conversation: format!("test-parent:conv:{}", conv),
        }
    }

    #[test]
    fn first_request_emits_first_request_event() {
        let state = make_state();
        let body = anthropic_body("you are an assistant", json!([]), vec!["hi"]);
        let h = compute_structural_hash(&body, ApiKind::Anthropic);
        let id = identity("session-A");
        assert_eq!(state.cache.lock().unwrap().0.len(), 0);
        observe_drift(&state, &id, h.clone());
        let caches = state.cache.lock().unwrap();
        assert_eq!(caches.0.len(), 1);
        assert_eq!(caches.0.peek(id.conversation.as_str()), Some(&h));
    }

    #[test]
    fn same_hash_emits_no_event() {
        let state = make_state();
        let body = anthropic_body("sys-A", json!([]), vec!["m1"]);
        let h = compute_structural_hash(&body, ApiKind::Anthropic);
        let id = identity("sess");
        observe_drift(&state, &id, h.clone());
        observe_drift(&state, &id, h.clone());
        let caches = state.cache.lock().unwrap();
        assert_eq!(caches.0.len(), 1);
        assert_eq!(caches.0.peek(id.conversation.as_str()), Some(&h));
    }

    #[test]
    fn system_drift_detected_with_correct_dim() {
        let state = make_state();
        let h1 = compute_structural_hash(
            &anthropic_body("sys-A", json!([]), vec!["m1"]),
            ApiKind::Anthropic,
        );
        let h2 = compute_structural_hash(
            &anthropic_body("sys-B", json!([]), vec!["m1"]),
            ApiKind::Anthropic,
        );
        assert_ne!(h1.system, h2.system);
        assert_eq!(h1.tools, h2.tools);
        assert_eq!(h1.early_messages, h2.early_messages);
        assert_eq!(drift_dims(&h1, &h2), "system");
        let id = identity("sess");
        observe_drift(&state, &id, h1);
        observe_drift(&state, &id, h2);
    }

    #[test]
    fn tools_drift_detected_with_correct_dim() {
        let h1 = compute_structural_hash(
            &anthropic_body("sys", json!([{"name": "a"}]), vec!["m1"]),
            ApiKind::Anthropic,
        );
        let h2 = compute_structural_hash(
            &anthropic_body("sys", json!([{"name": "b"}]), vec!["m1"]),
            ApiKind::Anthropic,
        );
        assert_eq!(h1.system, h2.system);
        assert_ne!(h1.tools, h2.tools);
        assert_eq!(h1.early_messages, h2.early_messages);
        assert_eq!(drift_dims(&h1, &h2), "tools");
    }

    #[test]
    fn early_messages_drift_detected_with_correct_dim() {
        let h1 = compute_structural_hash(
            &anthropic_body("sys", json!([]), vec!["m1"]),
            ApiKind::Anthropic,
        );
        let h2 = compute_structural_hash(
            &anthropic_body("sys", json!([]), vec!["DIFFERENT"]),
            ApiKind::Anthropic,
        );
        assert_eq!(h1.system, h2.system);
        assert_eq!(h1.tools, h2.tools);
        assert_ne!(h1.early_messages, h2.early_messages);
        assert_eq!(drift_dims(&h1, &h2), "early_messages");
    }

    #[test]
    fn multi_dim_drift_lists_all_changed_dims() {
        let h1 = compute_structural_hash(
            &anthropic_body("sys-A", json!([{"name": "a"}]), vec!["m1"]),
            ApiKind::Anthropic,
        );
        let h2 = compute_structural_hash(
            &anthropic_body("sys-B", json!([{"name": "b"}]), vec!["X"]),
            ApiKind::Anthropic,
        );
        assert_eq!(drift_dims(&h1, &h2), "system,tools,early_messages");
    }

    #[test]
    fn lru_evicts_at_capacity() {
        let state = DriftState::new(2);
        let h = compute_structural_hash(
            &anthropic_body("s", json!([]), vec!["m"]),
            ApiKind::Anthropic,
        );
        observe_drift(&state, &identity("s1"), h.clone());
        observe_drift(&state, &identity("s2"), h.clone());
        observe_drift(&state, &identity("s3"), h);
        let caches = state.cache.lock().unwrap();
        assert_eq!(caches.0.len(), 2);
        assert!(!caches.0.contains(identity("s1").conversation.as_str()));
        assert!(caches.0.contains(identity("s2").conversation.as_str()));
        assert!(caches.0.contains(identity("s3").conversation.as_str()));
    }

    #[test]
    fn does_not_mutate_input() {
        let body = anthropic_body(
            "sys",
            json!([{"name": "t1", "input_schema": {"type": "object"}}]),
            vec!["m1", "m2", "m3", "m4"],
        );
        let original_bytes = serde_json::to_vec(&body).expect("serialize");
        let _ = compute_structural_hash(&body, ApiKind::Anthropic);
        let _ = compute_structural_hash(&body, ApiKind::OpenAiChat);
        let _ = compute_structural_hash(&body, ApiKind::OpenAiResponses);
        let after_bytes = serde_json::to_vec(&body).expect("re-serialize");
        assert_eq!(original_bytes, after_bytes);
    }

    #[test]
    fn session_key_uses_tklite_session_key() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-tklite-session-key",
            "codex:opaque-session-key".parse().unwrap(),
        );
        headers.insert("x-request-id", "req-abc123".parse().unwrap());
        let body = anthropic_body("sys", json!([]), vec!["hi"]);
        let id = derive_session_key(&headers, &body, ApiKind::Anthropic);
        assert_eq!(id.parent, "tklite:codex:opaque-session-key");
        assert!(id
            .conversation
            .starts_with("tklite:codex:opaque-session-key:conv:"));
    }

    #[test]
    fn session_key_prefers_claude_code_session_id() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-claude-code-session-id",
            "cc-sess-0123".parse().unwrap(),
        );
        headers.insert("x-tklite-session-key", "codex:opaque".parse().unwrap());
        headers.insert("x-request-id", "req-abc123".parse().unwrap());
        let body = anthropic_body("sys", json!([]), vec!["hi"]);
        let id = derive_session_key(&headers, &body, ApiKind::Anthropic);
        assert_eq!(id.parent, "claude-code:cc-sess-0123");
        assert!(id.conversation.starts_with("claude-code:cc-sess-0123:conv:"));
    }

    #[test]
    fn session_key_tklite_session_takes_priority_over_request_id() {
        let mut headers = HeaderMap::new();
        headers.insert("x-tklite-session-key", "codex:stable".parse().unwrap());
        headers.insert("x-request-id", "req-unique".parse().unwrap());
        let body = anthropic_body("sys", json!([]), vec!["hi"]);
        let id = derive_session_key(&headers, &body, ApiKind::Anthropic);
        assert_eq!(id.parent, "tklite:codex:stable");
    }

    #[test]
    fn session_key_ignores_empty_tklite_session_key() {
        let mut headers = HeaderMap::new();
        headers.insert("x-tklite-session-key", "   ".parse().unwrap());
        headers.insert("x-request-id", "req-abc123".parse().unwrap());
        let body = anthropic_body("sys", json!([]), vec!["hi"]);
        let id = derive_session_key(&headers, &body, ApiKind::Anthropic);
        assert!(id.parent.starts_with("request:"));
    }

    #[test]
    fn session_key_hashes_request_id() {
        let mut headers = HeaderMap::new();
        headers.insert("x-request-id", "req-abc123".parse().unwrap());
        let body = anthropic_body("sys", json!([]), vec!["hi"]);
        let id = derive_session_key(&headers, &body, ApiKind::Anthropic);
        assert!(!id.parent.contains("req-abc123"));
        assert!(id.parent.starts_with("request:"));
    }

    #[test]
    fn session_key_ignores_api_key_and_headroom_session_id() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-api-key",
            "sk-very-private-api-key-12345".parse().unwrap(),
        );
        headers.insert(
            "x-headroom-session-id",
            "user-session-abc123".parse().unwrap(),
        );
        let body = anthropic_body("sys", json!([]), vec!["hi"]);
        let id = derive_session_key(&headers, &body, ApiKind::Anthropic);
        assert_eq!(id.parent, "anonymous");
    }

    #[test]
    fn session_key_falls_back_to_anonymous() {
        let body = anthropic_body("sys", json!([]), vec!["hi"]);
        let id = derive_session_key(&HeaderMap::new(), &body, ApiKind::Anthropic);
        assert_eq!(id.parent, "anonymous");
    }

    // --- 会话隔离：判别器 + 两级 observe ---

    fn anthropic_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert("x-tklite-session-key", "parent-bucket".parse().unwrap());
        headers
    }

    #[test]
    fn same_parent_different_first_messages_produce_different_identities() {
        let headers = anthropic_headers();
        let a = anthropic_body("sys", json!([]), vec!["hello A"]);
        let b = anthropic_body("sys", json!([]), vec!["hello B"]);
        let ida = derive_session_key(&headers, &a, ApiKind::Anthropic);
        let idb = derive_session_key(&headers, &b, ApiKind::Anthropic);
        assert_eq!(ida.parent, idb.parent);
        assert_ne!(ida.conversation, idb.conversation);
    }

    #[test]
    fn same_parent_different_models_produce_different_identities() {
        let headers = anthropic_headers();
        let mut a = anthropic_body("sys", json!([]), vec!["hi"]);
        let mut b = a.clone();
        a["model"] = json!("model-1");
        b["model"] = json!("model-2");
        let ida = derive_session_key(&headers, &a, ApiKind::Anthropic);
        let idb = derive_session_key(&headers, &b, ApiKind::Anthropic);
        assert_ne!(ida.conversation, idb.conversation);
    }

    #[test]
    fn appending_messages_preserves_identity() {
        let headers = anthropic_headers();
        let short = anthropic_body("sys", json!([]), vec!["hi"]);
        let long = anthropic_body("sys", json!([]), vec!["hi", "more", "even more"]);
        assert_eq!(
            derive_session_key(&headers, &short, ApiKind::Anthropic).conversation,
            derive_session_key(&headers, &long, ApiKind::Anthropic).conversation
        );
    }

    #[test]
    fn changing_system_or_tools_preserves_identity() {
        let headers = anthropic_headers();
        let a = anthropic_body("sys-A", json!([{"name": "t1"}]), vec!["hi"]);
        let b = anthropic_body("sys-B", json!([{"name": "t2"}]), vec!["hi"]);
        assert_eq!(
            derive_session_key(&headers, &a, ApiKind::Anthropic).conversation,
            derive_session_key(&headers, &b, ApiKind::Anthropic).conversation
        );
    }

    #[test]
    fn rewriting_first_message_produces_new_identity() {
        let headers = anthropic_headers();
        let a = anthropic_body("sys", json!([]), vec!["original question", "answer"]);
        let b = anthropic_body("sys", json!([]), vec!["[compacted summary]", "answer"]);
        assert_ne!(
            derive_session_key(&headers, &a, ApiKind::Anthropic).conversation,
            derive_session_key(&headers, &b, ApiKind::Anthropic).conversation
        );
    }

    #[test]
    fn relocating_cache_control_preserves_identity() {
        let headers = anthropic_headers();
        let a = json!({
            "model": "m",
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "hi", "cache_control": {"type": "ephemeral"}}
            ]}],
        });
        let b = json!({
            "model": "m",
            "messages": [{"role": "user", "content": [
                {"type": "text", "text": "hi"}
            ], "cache_control": {"type": "ephemeral"}}],
        });
        assert_eq!(
            derive_session_key(&headers, &a, ApiKind::Anthropic).conversation,
            derive_session_key(&headers, &b, ApiKind::Anthropic).conversation
        );
    }

    #[test]
    fn cache_control_inside_opaque_payload_changes_identity() {
        // `arguments` 是不透明用户负载：其中的 cache_control 是数据而非标记，
        // 必须参与身份计算。
        let headers = anthropic_headers();
        let a = json!({
            "model": "m",
            "messages": [{"role": "user", "content": [
                {"type": "tool_result", "arguments": {"x": 1}}
            ]}],
        });
        let b = json!({
            "model": "m",
            "messages": [{"role": "user", "content": [
                {"type": "tool_result", "arguments": {"x": 1, "cache_control": {}}}
            ]}],
        });
        assert_ne!(
            derive_session_key(&headers, &a, ApiKind::Anthropic).conversation,
            derive_session_key(&headers, &b, ApiKind::Anthropic).conversation
        );
    }

    #[test]
    fn reordering_json_keys_preserves_identity() {
        let headers = anthropic_headers();
        let a = json!({
            "model": "m",
            "messages": [{"role": "user", "content": "hi", "extra": {"a": 1, "b": 2}}],
        });
        let b = json!({
            "model": "m",
            "messages": [{"extra": {"b": 2, "a": 1}, "content": "hi", "role": "user"}],
        });
        assert_eq!(
            derive_session_key(&headers, &a, ApiKind::Anthropic).conversation,
            derive_session_key(&headers, &b, ApiKind::Anthropic).conversation
        );
    }

    #[test]
    fn reminder_content_churn_preserves_identity() {
        let headers = anthropic_headers();
        let a = json!({
            "model": "m",
            "messages": [{"role": "user", "content": "<system-reminder>\nCWD: /tmp/a\nDate: 2026-07-22\n</system-reminder>\nreal question"}],
        });
        let b = json!({
            "model": "m",
            "messages": [{"role": "user", "content": "<system-reminder>\nCWD: /other\nDate: 2026-07-23\n</system-reminder>\nreal question"}],
        });
        assert_eq!(
            derive_session_key(&headers, &a, ApiKind::Anthropic).conversation,
            derive_session_key(&headers, &b, ApiKind::Anthropic).conversation
        );
    }

    #[test]
    fn mask_reminders_handles_multiple_blocks_and_attributes() {
        let masked = mask_reminders_in_string(
            "a<system-reminder>x</system-reminder>b<system-reminder foo=\"bar\">y</system-reminder>c",
        );
        assert_eq!(
            masked,
            format!("a{}b{}c", REMINDER_PLACEHOLDER, REMINDER_PLACEHOLDER)
        );
    }

    #[test]
    fn mask_reminders_leaves_unterminated_block_as_is() {
        let s = "hello <system-reminder> never closed";
        assert_eq!(mask_reminders_in_string(s), s);
    }

    #[test]
    fn openai_chat_discriminator_skips_system_prefix() {
        let headers = anthropic_headers();
        let body = json!({
            "model": "gpt-x",
            "messages": [
                {"role": "system", "content": "sys"},
                {"role": "developer", "content": "dev"},
                {"role": "user", "content": "first real"},
            ],
        });
        let msg = first_conversation_message(&body, ApiKind::OpenAiChat).unwrap();
        assert_eq!(msg["content"], json!("first real"));
        let id = derive_session_key(&headers, &body, ApiKind::OpenAiChat);
        assert!(id.conversation.contains(":conv:"));
    }

    #[test]
    fn openai_responses_discriminator_skips_roleless_items() {
        let body = json!({
            "model": "gpt-x",
            "input": [
                {"type": "function_call_output", "call_id": "c1", "output": "x"},
                {"role": "system", "content": "sys"},
                {"role": "user", "content": "first real"},
            ],
        });
        let msg = first_conversation_message(&body, ApiKind::OpenAiResponses).unwrap();
        assert_eq!(msg["content"], json!("first real"));
    }

    #[test]
    fn openai_responses_discriminator_falls_back_to_messages() {
        let body = json!({
            "model": "gpt-x",
            "messages": [
                {"role": "developer", "content": "dev"},
                {"role": "user", "content": "first real"},
            ],
        });
        let msg = first_conversation_message(&body, ApiKind::OpenAiResponses).unwrap();
        assert_eq!(msg["content"], json!("first real"));
    }

    #[test]
    fn missing_model_and_message_use_sentinels_without_panicking() {
        let headers = anthropic_headers();
        let body = json!({"system": "only system"});
        let id = derive_session_key(&headers, &body, ApiKind::Anthropic);
        assert!(id.conversation.starts_with("tklite:parent-bucket:conv:"));
    }

    #[test]
    fn alternating_conversations_do_not_cross_compare() {
        // 一个父级 bucket 下的两个会话，交替轮次使用不同的 system prompt。
        // 各自维护自己的基线，因此会话级缓存从不将 A 与 B 比较。
        // 父级缓存报告真正的 system 更替——这是该设计保留可见的剩余信号。
        let state = make_state();
        let ida = identity("A");
        let idb = identity("B");
        let ha1 = compute_structural_hash(
            &anthropic_body("sys-A", json!([]), vec!["a1"]),
            ApiKind::Anthropic,
        );
        let hb1 = compute_structural_hash(
            &anthropic_body("sys-B", json!([]), vec!["b1"]),
            ApiKind::Anthropic,
        );
        let ha2 = compute_structural_hash(
            &anthropic_body("sys-A", json!([]), vec!["a1", "a2"]),
            ApiKind::Anthropic,
        );
        observe_drift(&state, &ida, ha1);
        observe_drift(&state, &idb, hb1);
        observe_drift(&state, &ida, ha2.clone());
        let caches = state.cache.lock().unwrap();
        assert_eq!(caches.0.len(), 2);
        assert_eq!(caches.0.peek(ida.conversation.as_str()), Some(&ha2));
    }

    #[test]
    fn identity_rotation_with_same_system_tools_keeps_parent_baseline() {
        // 压缩重写首条消息：会话键轮换，但 system/tools 不变，
        // 因此父级基线不得报告漂移，且必须跟随轮换后的身份。
        let state = make_state();
        let h1 = compute_structural_hash(
            &anthropic_body("sys", json!([]), vec!["original"]),
            ApiKind::Anthropic,
        );
        let h2 = compute_structural_hash(
            &anthropic_body("sys", json!([]), vec!["[summary]", "turn-2"]),
            ApiKind::Anthropic,
        );
        let id1 = identity("conv-1");
        let id2 = SessionIdentity {
            parent: id1.parent.clone(),
            conversation: "test-parent:conv:rotated".to_string(),
        };
        observe_drift(&state, &id1, h1);
        observe_drift(&state, &id2, h2);
        let caches = state.cache.lock().unwrap();
        let parent = caches.1.peek(&id1.parent).unwrap();
        assert_eq!(
            parent.system,
            compute_structural_hash(
                &anthropic_body("sys", json!([]), vec!["[summary]", "turn-2"]),
                ApiKind::Anthropic,
            )
            .system
        );
    }

    #[test]
    fn identity_rotation_with_system_change_updates_parent_baseline() {
        let state = make_state();
        let h1 = compute_structural_hash(
            &anthropic_body("sys-old", json!([]), vec!["original"]),
            ApiKind::Anthropic,
        );
        let h2 = compute_structural_hash(
            &anthropic_body("sys-new", json!([]), vec!["[summary]"]),
            ApiKind::Anthropic,
        );
        let id1 = identity("conv-1");
        let id2 = SessionIdentity {
            parent: id1.parent.clone(),
            conversation: "test-parent:conv:rotated".to_string(),
        };
        observe_drift(&state, &id1, h1);
        observe_drift(&state, &id2, h2.clone());
        let caches = state.cache.lock().unwrap();
        let parent = caches.1.peek(&id1.parent).unwrap();
        assert_eq!(parent.system, h2.system);
    }

    #[test]
    fn openai_chat_extracts_first_system_message() {
        let body = json!({
            "model": "gpt-4",
            "messages": [
                {"role": "system", "content": "you are a helpful assistant"},
                {"role": "user", "content": "hi"},
            ],
            "tools": [],
        });
        let h1 = compute_structural_hash(&body, ApiKind::OpenAiChat);
        let body2 = json!({
            "model": "gpt-4",
            "messages": [
                {"role": "system", "content": "you are a different assistant"},
                {"role": "user", "content": "hi"},
            ],
            "tools": [],
        });
        let h2 = compute_structural_hash(&body2, ApiKind::OpenAiChat);
        assert_ne!(h1.system, h2.system);
        assert_eq!(h1.early_messages, h2.early_messages);
    }

    #[test]
    fn openai_responses_uses_instructions_and_input() {
        let body = json!({
            "model": "gpt-4",
            "instructions": "be brief",
            "tools": [],
            "input": [
                {"type": "message", "role": "user", "content": "hello"},
            ],
        });
        let h1 = compute_structural_hash(&body, ApiKind::OpenAiResponses);
        let body2 = json!({
            "model": "gpt-4",
            "instructions": "be verbose",
            "tools": [],
            "input": [
                {"type": "message", "role": "user", "content": "hello"},
            ],
        });
        let h2 = compute_structural_hash(&body2, ApiKind::OpenAiResponses);
        assert_ne!(h1.system, h2.system);
        assert_eq!(h1.early_messages, h2.early_messages);
    }

    #[test]
    fn tools_description_change_does_not_drift() {
        let body = json!({
            "model": "gpt-5.4",
            "instructions": "be brief",
            "tools": [{
                "type": "function",
                "function": {
                    "name": "search",
                    "description": "cwd=/tmp/a permissions=read",
                    "parameters": {"type": "object", "properties": {"q": {"type": "string"}}}
                }
            }],
            "input": [{"role": "user", "content": "hello"}]
        });
        let body2 = json!({
            "model": "gpt-5.4",
            "instructions": "be brief",
            "tools": [{
                "type": "function",
                "function": {
                    "name": "search",
                    "description": "cwd=/different permissions=read,write",
                    "parameters": {"type": "object", "properties": {"q": {"type": "string"}}}
                }
            }],
            "input": [{"role": "user", "content": "hello"}]
        });

        let h1 = compute_structural_hash(&body, ApiKind::OpenAiResponses);
        let h2 = compute_structural_hash(&body2, ApiKind::OpenAiResponses);

        assert_eq!(h1.tools, h2.tools);
        assert_eq!(h1.system, h2.system);
        assert_eq!(h1.early_messages, h2.early_messages);
    }

    #[test]
    fn openai_responses_developer_input_counts_as_system_when_no_instructions() {
        let body = json!({
            "model": "gpt-5.4",
            "tools": [],
            "input": [
                {"role": "developer", "content": "system A"},
                {"role": "user", "content": "hello"}
            ]
        });
        let body2 = json!({
            "model": "gpt-5.4",
            "tools": [],
            "input": [
                {"role": "developer", "content": "system B"},
                {"role": "user", "content": "hello"}
            ]
        });

        let h1 = compute_structural_hash(&body, ApiKind::OpenAiResponses);
        let h2 = compute_structural_hash(&body2, ApiKind::OpenAiResponses);

        assert_ne!(h1.system, h2.system);
    }

    #[test]
    fn openai_responses_instructions_take_precedence_over_developer_input_for_drift() {
        let body = json!({
            "model": "gpt-5.4",
            "instructions": "canonical system",
            "tools": [],
            "input": [
                {"role": "developer", "content": "system A"},
                {"role": "user", "content": "hello"}
            ]
        });
        let body2 = json!({
            "model": "gpt-5.4",
            "instructions": "canonical system",
            "tools": [],
            "input": [
                {"role": "developer", "content": "system B"},
                {"role": "user", "content": "hello"}
            ]
        });

        let h1 = compute_structural_hash(&body, ApiKind::OpenAiResponses);
        let h2 = compute_structural_hash(&body2, ApiKind::OpenAiResponses);

        assert_eq!(h1.system, h2.system);
    }

    #[test]
    fn openai_responses_empty_instructions_fall_back_to_developer_for_drift() {
        let body = json!({
            "instructions": "",
            "input": [{"role": "developer", "content": [{"type": "input_text", "text": "system A"}]}],
            "tools": []
        });
        let body2 = json!({
            "instructions": "",
            "input": [{"role": "developer", "content": [{"type": "input_text", "text": "system B"}]}],
            "tools": []
        });

        let h1 = compute_structural_hash(&body, ApiKind::OpenAiResponses);
        let h2 = compute_structural_hash(&body2, ApiKind::OpenAiResponses);

        assert_ne!(h1.system, h2.system);
    }

    #[test]
    fn openai_responses_whitespace_instructions_fall_back_to_developer_for_drift() {
        let body = json!({
            "instructions": " \n",
            "input": [{"role": "developer", "content": [{"type": "input_text", "text": "system A"}]}],
            "tools": []
        });
        let body2 = json!({
            "instructions": " \n",
            "input": [{"role": "developer", "content": [{"type": "input_text", "text": "system B"}]}],
            "tools": []
        });

        let h1 = compute_structural_hash(&body, ApiKind::OpenAiResponses);
        let h2 = compute_structural_hash(&body2, ApiKind::OpenAiResponses);

        assert_ne!(h1.system, h2.system);
    }

    #[test]
    fn openai_responses_null_instructions_fall_back_to_developer_input_for_drift() {
        let body = json!({
            "model": "gpt-5.4",
            "instructions": null,
            "tools": [],
            "input": [
                {"role": "developer", "content": "system A"},
                {"role": "user", "content": "hello"}
            ]
        });
        let body2 = json!({
            "model": "gpt-5.4",
            "instructions": null,
            "tools": [],
            "input": [
                {"role": "developer", "content": "system B"},
                {"role": "user", "content": "hello"}
            ]
        });

        let h1 = compute_structural_hash(&body, ApiKind::OpenAiResponses);
        let h2 = compute_structural_hash(&body2, ApiKind::OpenAiResponses);

        assert_ne!(h1.system, h2.system);
    }

    #[test]
    fn openai_responses_early_messages_ignore_system_and_developer_items() {
        let body = json!({
            "model": "gpt-5.4",
            "tools": [],
            "input": [
                {"role": "developer", "content": "system A"},
                {"role": "system", "content": "legacy system A"},
                {"role": "user", "content": "hello"}
            ]
        });
        let body2 = json!({
            "model": "gpt-5.4",
            "tools": [],
            "input": [
                {"role": "developer", "content": "system B"},
                {"role": "system", "content": "legacy system B"},
                {"role": "user", "content": "hello"}
            ]
        });

        let h1 = compute_structural_hash(&body, ApiKind::OpenAiResponses);
        let h2 = compute_structural_hash(&body2, ApiKind::OpenAiResponses);

        assert_ne!(h1.system, h2.system);
        assert_eq!(h1.early_messages, h2.early_messages);
    }

    #[test]
    fn openai_responses_messages_fallback_matches_cache_key_axes() {
        let body = json!({
            "model": "gpt-5.4",
            "tools": [],
            "messages": [
                {"role": "developer", "content": "system A"},
                {"role": "user", "content": "hello"}
            ]
        });
        let body2 = json!({
            "model": "gpt-5.4",
            "tools": [],
            "messages": [
                {"role": "developer", "content": "system B"},
                {"role": "user", "content": "hello"}
            ]
        });

        let h1 = compute_structural_hash(&body, ApiKind::OpenAiResponses);
        let h2 = compute_structural_hash(&body2, ApiKind::OpenAiResponses);

        assert_ne!(h1.system, h2.system);
        assert_eq!(h1.early_messages, h2.early_messages);
    }

    #[test]
    fn early_message_item_hashes_identify_changed_position() {
        let before = extract_early_messages(
            &anthropic_body("sys", json!([]), vec!["first", "second", "third"]),
            ApiKind::Anthropic,
        );
        let after = extract_early_messages(
            &anthropic_body("sys", json!([]), vec!["first", "changed", "third"]),
            ApiKind::Anthropic,
        );

        let previous = early_message_item_hashes(&before);
        let current = early_message_item_hashes(&after);

        assert_eq!(previous.len(), 3);
        assert_eq!(current.len(), 3);
        assert_eq!(previous[0], current[0]);
        assert_ne!(previous[1], current[1]);
        assert_eq!(previous[2], current[2]);
    }

    #[test]
    fn early_message_item_hashes_are_12_hex_prefixes() {
        let messages = extract_early_messages(
            &anthropic_body("sys", json!([]), vec!["hello", "world"]),
            ApiKind::Anthropic,
        );
        let hashes = early_message_item_hashes(&messages);
        assert_eq!(hashes.len(), 2);
        for prefix in &hashes {
            assert_eq!(prefix.len(), 12, "each prefix must be exactly 12 hex chars");
            assert!(
                prefix
                    .chars()
                    .all(|c| c.is_ascii_digit() || matches!(c, 'a'..='f')),
                "prefix must be lowercase hex: {}",
                prefix
            );
        }
    }

    #[test]
    fn structural_hash_early_message_item_hashes_are_12_hex_prefixes() {
        let h = compute_structural_hash(
            &anthropic_body("sys", json!([]), vec!["a", "b", "c"]),
            ApiKind::Anthropic,
        );
        assert_eq!(h.early_message_item_hashes.len(), 3);
        for prefix in &h.early_message_item_hashes {
            assert_eq!(prefix.len(), 12, "each prefix must be exactly 12 hex chars");
            assert!(
                prefix
                    .chars()
                    .all(|c| c.is_ascii_digit() || matches!(c, 'a'..='f')),
                "prefix must be lowercase hex: {}",
                prefix
            );
        }
    }

    #[test]
    fn early_messages_window_caps_at_eight() {
        let h1 = compute_structural_hash(
            &anthropic_body(
                "s",
                json!([]),
                vec!["a", "b", "c", "d", "e", "f", "g", "h", "i"],
            ),
            ApiKind::Anthropic,
        );
        // 只修改第 9 条消息不能使 early_messages 漂移。
        let h2 = compute_structural_hash(
            &anthropic_body(
                "s",
                json!([]),
                vec!["a", "b", "c", "d", "e", "f", "g", "h", "DIFFERENT"],
            ),
            ApiKind::Anthropic,
        );
        assert_eq!(h1.early_messages, h2.early_messages);
        // 但修改第 1 条消息必须使其漂移。
        let h3 = compute_structural_hash(
            &anthropic_body(
                "s",
                json!([]),
                vec!["DIFFERENT", "b", "c", "d", "e", "f", "g", "h", "i"],
            ),
            ApiKind::Anthropic,
        );
        assert_ne!(h1.early_messages, h3.early_messages);
    }

    #[test]
    fn first_early_message_difference_reports_changed_item() {
        let previous = compute_structural_hash(
            &anthropic_body("sys", json!([]), vec!["one", "two", "three"]),
            ApiKind::Anthropic,
        );
        let current = compute_structural_hash(
            &anthropic_body("sys", json!([]), vec!["one", "changed", "three"]),
            ApiKind::Anthropic,
        );

        let difference = first_early_message_difference(&previous, &current).unwrap();
        assert_eq!(difference.index, 1);
        assert_eq!(
            difference.previous_hash,
            Some(previous.early_message_item_hashes[1].clone())
        );
        assert_eq!(
            difference.current_hash,
            Some(current.early_message_item_hashes[1].clone())
        );
    }

    #[test]
    fn first_early_message_difference_reports_missing_previous_item() {
        let previous = compute_structural_hash(
            &anthropic_body("sys", json!([]), vec!["one"]),
            ApiKind::Anthropic,
        );
        let current = compute_structural_hash(
            &anthropic_body("sys", json!([]), vec!["one", "two"]),
            ApiKind::Anthropic,
        );

        let difference = first_early_message_difference(&previous, &current).unwrap();
        assert_eq!(difference.index, 1);
        assert_eq!(difference.previous_hash, None);
        assert_eq!(
            difference.current_hash,
            Some(current.early_message_item_hashes[1].clone())
        );
    }

    #[test]
    fn ancillary_request_detected_when_tools_empty_and_single_message() {
        // 标题生成形态：空 tools、单条 user 消息。
        let body = json!({
            "model": "claude-3-5-sonnet-20241022",
            "system": "you are an assistant",
            "tools": [],
            "messages": [{"role": "user", "content": "Write the title"}],
        });
        assert!(is_ancillary_request(&body, ApiKind::Anthropic));
    }

    #[test]
    fn main_conversation_not_ancillary_when_tools_present() {
        // 主会话：有 tools、多条消息。
        let body = json!({
            "model": "claude-3-5-sonnet-20241022",
            "system": "you are an assistant",
            "tools": [{"name": "bash"}],
            "messages": [
                {"role": "user", "content": "hi"},
                {"role": "assistant", "content": "hello"}
            ],
        });
        assert!(!is_ancillary_request(&body, ApiKind::Anthropic));
    }

    #[test]
    fn ancillary_request_when_tools_empty_and_no_messages() {
        // 边界情况：空 tools、零条消息。
        let body = json!({
            "model": "claude-3-5-sonnet-20241022",
            "system": "you are an assistant",
            "tools": [],
            "messages": [],
        });
        assert!(is_ancillary_request(&body, ApiKind::Anthropic));
    }

    #[test]
    fn ancillary_request_responses_uses_input_fallback() {
        // OpenAI Responses 形态：空 tools、`input`（而非 `messages`）中单个条目。
        // 验证 messages_array 的 input→messages 回退。
        let body = json!({
            "model": "gpt-5.4",
            "instructions": "be brief",
            "tools": [],
            "input": [{"role": "user", "content": "Write the title"}],
        });
        assert!(is_ancillary_request(&body, ApiKind::OpenAiResponses));
    }

    #[test]
    fn ancillary_request_when_tools_field_absent() {
        // 完全没有 `tools` 字段 + 单条消息 → 视为附属
        // （tools 缺失通过 unwrap_or(true) 映射为空）。
        let body = json!({
            "model": "claude-3-5-sonnet-20241022",
            "system": "you are an assistant",
            "messages": [{"role": "user", "content": "hi"}],
        });
        assert!(is_ancillary_request(&body, ApiKind::Anthropic));
    }

    #[test]
    fn ancillary_request_openai_chat_with_system_prefix() {
        // 带 system 前缀 + 单条 user 消息的 OpenAI Chat 标题请求。
        // system 条目不得计入消息总数（与 extract_early_messages 一致），
        // 否则 msg_count=2，守卫将无法跳过它。
        let body = json!({
            "model": "gpt-4",
            "messages": [
                {"role": "system", "content": "be brief"},
                {"role": "user", "content": "Write the title"}
            ],
            "tools": [],
        });
        assert!(is_ancillary_request(&body, ApiKind::OpenAiChat));
    }

    #[test]
    fn ancillary_request_responses_with_developer_prefix() {
        // 带 developer 前缀 + `input` 中单个 user 条目的 OpenAI Responses
        // 标题请求。developer 条目不得计入消息总数。
        let body = json!({
            "model": "gpt-5.4",
            "tools": [],
            "input": [
                {"role": "developer", "content": "system"},
                {"role": "user", "content": "Write the title"}
            ],
        });
        assert!(is_ancillary_request(&body, ApiKind::OpenAiResponses));
    }

    #[test]
    fn ancillary_request_when_tools_null() {
        // tools:null 必须与缺失/空 tools 同等对待——"没有可用的 tools"——
        // 因此单消息请求是附属请求。
        let body = json!({
            "model": "claude-3-5-sonnet-20241022",
            "system": "you are an assistant",
            "tools": null,
            "messages": [{"role": "user", "content": "hi"}],
        });
        assert!(is_ancillary_request(&body, ApiKind::Anthropic));
    }
}
