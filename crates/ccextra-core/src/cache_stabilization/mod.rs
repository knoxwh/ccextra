//! Phase E 缓存稳定化表面。
//!
//! 将所有的缓存稳定化机制集中到同一个模块下，这样运维人员搜索
//! "tkLite 如何保持 prompt cache 常热" 时，能在一个地方找到答案。
//!
//! 目前已提供：
//!
//! - [`volatile_detector`] — PR-E5：扫描入站请求体，查找会破坏
//!   prompt-cache 命中的模式（ISO 8601 时间戳、UUID v4、
//!   以 ID 命名的字段）。发出 WARN 日志，并可通过
//!   [`strip_volatile_from_prefix`] **剥离** cache prefix（system + tools）
//!   中的易变内容，使 prefix 在多次轮次间保持稳定。
//! - [`drift_detector`] — PR-E6：对每个会话的 cache 热区
//!   （system / tools / early messages）计算结构哈希。首次见到时发出
//!   `cache_drift_first_request`，同一会话的连续请求在任一维度上不一致时
//!   发出 `cache_drift_observed`。
//! - [`tool_def_normalize`] — PR-E1 / PR-E2：按名称对 `tools[]` 进行
//!   字母排序，并递归排序每个 tool 的 `input_schema` / `function.parameters`
//!   内部的 JSON Schema 对象键。
//! - [`anthropic_cache_control`] — PR-E3：当客户未放置任何
//!   `cache_control` 标记时，自动在最后一个 tool 定义上插入一个 ephemeral
//!   标记。**会修改请求字节**。
//! - [`reminder_rstrip`] — PR-E7：折叠 `text` 块（system + messages）中
//!   结尾 `</system-reminder>` 周围的尾部空白，使 CC 的字节级重新序列化
//!   漂移（anthropics/claude-code#48734 form 1）不会使 prompt-cache
//!   prefix 失效。确定性纯函数，幂等。
//! - [`sort_stabilization`] — PR-E8：确定性地对 system prompt 内部的
//!   skills 与 deferred-tools 列表块进行排序，使 CC 的不稳定列表顺序不会
//!   使 cache prefix 漂移。仅作用于 system；
//!   tool 定义的排序由 [`tool_def_normalize`] 处理。

pub mod anthropic_cache_control;
pub mod content_strip;
pub mod drift_detector;
pub mod json_walker;
pub mod reminder_rstrip;
pub mod smoosh_split;
pub mod sort_stabilization;
pub mod tool_def_normalize;
pub mod tool_input_normalize;
pub mod volatile_detector;

pub use content_strip::strip_bookkeeping_content;
pub use drift_detector::{
    compute_structural_hash, derive_session_key, observe_drift, ApiKind as DriftApiKind, DriftState,
};
pub use reminder_rstrip::normalize_reminder_trailing_whitespace;
pub use smoosh_split::split_smooshed_reminders;
pub use sort_stabilization::stabilize_block_sort;
pub use tool_def_normalize::{
    normalize_tool_definitions_openai_chat, normalize_tool_definitions_responses,
};
pub use tool_input_normalize::normalize_tool_use_inputs;
pub use volatile_detector::strip_volatile_from_prefix;
