mod decode;
mod nested;

pub use decode::decode_agent_server_message;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerMessage {
    TextDelta(String),
    ThinkingDelta(String),
    ThinkingCompleted,
    TokenDelta(i64),
    TurnEnded(TurnUsage),
    Heartbeat,
    Checkpoint(RawCheckpoint),
    KvGet {
        id: u32,
        blob_id: Vec<u8>,
    },
    KvSet {
        id: u32,
        blob_id: Vec<u8>,
        data: Vec<u8>,
    },
    Exec(ExecRequest),
    /// InteractionUpdate field 7:服务端交互查询(web_search/ask_question 等),
    /// 不回复会挂死整流(旧实现移除的导火索),必须回 InteractionResponse
    InteractionQuery(InteractionQuery),
}

/// InteractionQuery 查询种类(对应 oneof variant 字段号)
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct InteractionQuery {
    pub id: u32,
    pub kind: InteractionQueryKind,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InteractionQueryKind {
    WebSearch,
    AskQuestion,
    SwitchMode,
    ExaSearch,
    ExaFetch,
    CreatePlan,
    SetupVm,
}

/// 回合结束信号上的用量。字段号来自 @cursor/sdk 1.0.32 protobuf-es type info:
/// 1 input_tokens|2 output_tokens|3 cache_read_tokens|4 cache_write_tokens|5 reasoning_tokens。
/// 实测 field 1 是本轮完整输入(≈ context 大小);field 3/4 语义三份逆向文档一致
/// (本轮 cache 拆分),是否可信由 CursorSse 自校验(和==input 才采用)。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct TurnUsage {
    pub input_tokens: Option<i64>,
    pub output_tokens: Option<i64>,
    pub cache_read_tokens: Option<i64>,
    pub cache_write_tokens: Option<i64>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawCheckpoint(pub Vec<u8>);

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ExecRequest {
    pub exec_msg_id: u32,
    pub exec_id: String,
    pub kind: ExecKind,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ExecKind {
    RequestContext,
    Mcp {
        name: String,
        tool_call_id: String,
        args: std::collections::BTreeMap<String, Vec<u8>>,
    },
    /// MCP server 状态查询(field 36,上游按 server identifier 探测工具表)
    McpState {
        server_identifiers: Vec<String>,
    },
    /// 子代理调用(field 28,ccextra 不支持)
    Subagent {
        tool_call_id: String,
    },
    Builtin {
        field_number: u64,
    },
}
