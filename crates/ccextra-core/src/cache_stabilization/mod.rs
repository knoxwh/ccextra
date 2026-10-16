//! Phase E 缓存稳定化表面。
//!
//! 将所有的缓存稳定化机制集中到同一个模块下，这样运维人员搜索
//! "缓存稳定化" 时，能在一个地方找到答案。
//!
//! 目前已提供：
//!
//! - [`volatile_detector`] — PR-E5：扫描入站请求体，查找会破坏
//!   prompt-cache 命中的模式（ISO 8601 时间戳、UUID v4、
//!   以 ID 命名的字段）。仅发出 WARN 日志（只读观测）。
//!   [`normalize_client_dateline`] 把 "Today's date is …" 指纹句还原为
//!   ASCII 撇号 + 连字符日期（对齐 sub2api anthropicfp，抹除
//!   4 种撇号 × 2 种分隔符的 3 bit 隐写信号）。原占位符剥离路径已
//!   下线并移除（替换改变上游可见语义）。
//! - [`drift_detector`] — PR-E6：对每个会话的 cache 热区
//!   （system / tools / early messages）计算结构哈希。首次见到时发出
//!   DEBUG `cache_drift_first_request`；父级 `system`/`tools` 不一致时
//!   WARN `cache_drift_observed`（`scope=parent`）；会话级不一致时
//!   DEBUG `cache_drift_observed`（`scope=conversation`）。建议与
//!   `normalize.enabled` 同开。不稳定父级（request-id/anonymous）跳过基线。
//! - [`tool_def_normalize`] — PR-E1 / PR-E2：按名称对 `tools[]` 进行
//!   字母排序，并递归排序每个 tool 的 `input_schema` / `function.parameters`
//!   内部的 JSON Schema 对象键。
//! - [`reminder_rstrip`] — PR-E7：折叠 `text` 块（system + messages）中
//!   结尾 `</system-reminder>` 周围的尾部空白，使 CC 的字节级重新序列化
//!   漂移（anthropics/claude-code#48734 form 1）不会使 prompt-cache
//!   prefix 失效。确定性纯函数，幂等。
//! - [`sort_stabilization`] — PR-E8：确定性地对 system prompt 内部的
//!   skills 与 deferred-tools 列表块进行排序，使 CC 的不稳定列表顺序不会
//!   使 cache prefix 漂移。仅作用于 system；
//!   tool 定义的排序由 [`tool_def_normalize`] 处理。
//! - [`smoosh_split`] — PR-E9：从 `tool_result.content` 中剥离开被揉合的
//!   `<system-reminder>` 块，拆成独立 text 块（幂等、无损；仅 Anthropic）。
//! - [`tool_input_normalize`] — PR-E11：将 `tool_use.input` 键顺序归一化
//!   以匹配工具 schema（schema 键在前，其余字典序；无损、幂等；仅 Anthropic）。

pub mod drift_detector;
pub mod json_walker;
pub mod reminder_rstrip;
pub mod smoosh_split;
pub mod sort_stabilization;
pub mod tool_def_normalize;
pub mod tool_input_normalize;
pub mod volatile_detector;

pub use drift_detector::{
    compute_structural_hash, derive_session_key, is_stable_parent_bucket,
    maybe_observe_request_drift, maybe_observe_request_drift_with, observe_cache_usage,
    observe_drift, ApiKind as DriftApiKind, DriftObserveOptions, DriftState,
};
pub use reminder_rstrip::normalize_reminder_trailing_whitespace;
pub use smoosh_split::split_smooshed_reminders;
pub use sort_stabilization::stabilize_block_sort;
pub use tool_def_normalize::{
    normalize_tool_definitions_openai_chat, normalize_tool_definitions_responses,
};
pub use tool_input_normalize::normalize_tool_use_inputs;
pub use volatile_detector::normalize_client_dateline;
