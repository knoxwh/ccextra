mod decode;
mod nested;

pub use decode::decode_agent_server_message;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerMessage {
    TextDelta(String),
    ThinkingDelta(String),
    ThinkingCompleted,
    TokenDelta(i64),
    TurnEnded,
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
