use super::{encode_bytes, encode_varint, generated};
use prost::Message;

pub const BUILTIN_REJECTION: &str =
    "Tool not available in this environment. Use the MCP tools provided instead.";

pub const INTERACTION_REJECTION: &str =
    "Interaction is not available in this environment.";

pub fn encode_heartbeat() -> Vec<u8> {
    generated::AgentClientMessage {
        message: Some(generated::agent_client_message::Message::ClientHeartbeat(
            generated::ClientHeartbeat {},
        )),
    }
    .encode_to_vec()
}

/// 回复 InteractionQuery(AgentClientMessage field 6):ccextra 不提供
/// web_search/ask_question 等交互能力,按查询种类回对应 reject/error,
/// 避免服务端等回复挂死整流(90s idle 超时杀流)
pub fn encode_interaction_response(query: super::raw_wire::InteractionQuery) -> Vec<u8> {
    use generated::interaction_response::Result;
    let result = match query.kind {
        super::raw_wire::InteractionQueryKind::WebSearch => {
            Result::WebSearchRequestResponse(generated::WebSearchRequestResponse {
                result: Some(generated::web_search_request_response::Result::Rejected(
                    generated::WebSearchRequestResponseRejected {
                        reason: INTERACTION_REJECTION.into(),
                    },
                )),
            })
        }
        super::raw_wire::InteractionQueryKind::AskQuestion => {
            Result::AskQuestionInteractionResponse(generated::AskQuestionInteractionResponse {
                result: Some(generated::AskQuestionResult {
                    result: Some(generated::ask_question_result::Result::Rejected(
                        generated::AskQuestionRejected {
                            reason: INTERACTION_REJECTION.into(),
                        },
                    )),
                }),
            })
        }
        super::raw_wire::InteractionQueryKind::SwitchMode => {
            Result::SwitchModeRequestResponse(generated::SwitchModeRequestResponse {
                result: Some(generated::switch_mode_request_response::Result::Rejected(
                    generated::SwitchModeRequestResponseRejected {
                        reason: INTERACTION_REJECTION.into(),
                    },
                )),
            })
        }
        super::raw_wire::InteractionQueryKind::ExaSearch => {
            Result::ExaSearchRequestResponse(generated::ExaSearchRequestResponse {
                result: Some(generated::exa_search_request_response::Result::Rejected(
                    generated::ExaSearchRequestResponseRejected {
                        reason: INTERACTION_REJECTION.into(),
                    },
                )),
            })
        }
        super::raw_wire::InteractionQueryKind::ExaFetch => {
            Result::ExaFetchRequestResponse(generated::ExaFetchRequestResponse {
                result: Some(generated::exa_fetch_request_response::Result::Rejected(
                    generated::ExaFetchRequestResponseRejected {
                        reason: INTERACTION_REJECTION.into(),
                    },
                )),
            })
        }
        super::raw_wire::InteractionQueryKind::CreatePlan => {
            Result::CreatePlanRequestResponse(generated::CreatePlanRequestResponse {
                result: Some(generated::CreatePlanResult {
                    result: Some(generated::create_plan_result::Result::Error(
                        generated::CreatePlanError {
                            error: INTERACTION_REJECTION.into(),
                        },
                    )),
                    ..Default::default()
                }),
            })
        }
        super::raw_wire::InteractionQueryKind::SetupVm => {
            // SetupVmEnvironmentResult 只有 success variant:回空 success 解锁
            Result::SetupVmEnvironmentResult(generated::SetupVmEnvironmentResult {
                result: Some(generated::setup_vm_environment_result::Result::Success(
                    generated::SetupVmEnvironmentSuccess {},
                )),
            })
        }
    };
    generated::AgentClientMessage {
        message: Some(generated::agent_client_message::Message::InteractionResponse(
            generated::InteractionResponse {
                id: query.id,
                result: Some(result),
            },
        )),
    }
    .encode_to_vec()
}

pub fn encode_kv_get(id: u32, blob: Option<&[u8]>) -> Vec<u8> {
    let mut result = Vec::new();
    if let Some(blob) = blob {
        encode_bytes(1, blob, &mut result);
    }
    encode_kv(id, 2, &result)
}

pub fn encode_kv_set(id: u32) -> Vec<u8> {
    encode_kv(id, 3, &[])
}

fn encode_kv(id: u32, field: u64, result: &[u8]) -> Vec<u8> {
    let mut kv = Vec::new();
    encode_varint(8, &mut kv);
    encode_varint(u64::from(id), &mut kv);
    encode_bytes(field, result, &mut kv);
    let mut message = Vec::new();
    encode_bytes(3, &kv, &mut message);
    message
}

fn encode_exec(id: u32, exec_id: &str, field: u64, result: &[u8]) -> Vec<u8> {
    let mut exec = Vec::new();
    encode_varint(8, &mut exec);
    encode_varint(u64::from(id), &mut exec);
    encode_bytes(field, result, &mut exec);
    encode_bytes(15, exec_id.as_bytes(), &mut exec);
    let mut message = Vec::new();
    encode_bytes(2, &exec, &mut message);
    message
}

pub fn encode_request_context_result(
    id: u32,
    exec_id: &str,
    tools: Vec<generated::McpToolDefinition>,
) -> Vec<u8> {
    let context = generated::RequestContext {
        tools,
        ..Default::default()
    }
    .encode_to_vec();
    let mut success = Vec::new();
    encode_bytes(1, &context, &mut success);
    let mut result = Vec::new();
    encode_bytes(1, &success, &mut result);
    encode_exec(id, exec_id, 10, &result)
}

/// 回复 McpStateExecArgs(field 36):发布 proxy server 的工具表,
/// 对齐 CLI mcpState handler 的 McpStateSuccess 形状
pub fn encode_mcp_state_result(
    id: u32,
    exec_id: &str,
    tools: Vec<generated::McpToolDefinition>,
) -> Vec<u8> {
    let success = generated::McpStateSuccess {
        servers: vec![generated::McpStateServer {
            server_name: "proxy".into(),
            server_identifier: "proxy".into(),
            tools,
            ..Default::default()
        }],
    }
    .encode_to_vec();
    let mut result = Vec::new();
    encode_bytes(1, &success, &mut result);
    encode_exec(id, exec_id, 36, &result)
}

/// 回复 SubagentArgs(field 28):ccextra 不支持子代理,回 error
pub fn encode_subagent_error(id: u32, exec_id: &str) -> Vec<u8> {
    let error = generated::SubagentError {
        error: "Subagents are not supported in this environment.".into(),
        ..Default::default()
    }
    .encode_to_vec();
    let mut result = Vec::new();
    encode_bytes(2, &error, &mut result);
    encode_exec(id, exec_id, 28, &result)
}

pub fn encode_mcp_result(id: u32, exec_id: &str, content: &str, is_error: bool) -> Vec<u8> {
    let mut text = Vec::new();
    encode_bytes(1, content.as_bytes(), &mut text);
    let mut item = Vec::new();
    encode_bytes(1, &text, &mut item);
    let mut success = Vec::new();
    encode_bytes(1, &item, &mut success);
    if is_error {
        success.extend_from_slice(&[0x10, 0x01]);
    }
    let mut result = Vec::new();
    encode_bytes(1, &success, &mut result);
    encode_exec(id, exec_id, 11, &result)
}

pub fn encode_builtin_rejection(id: u32, exec_id: &str, field: u64) -> Option<Vec<u8>> {
    let (result_field, rejection_field, reason_field) = match field {
        2 | 14 => (field, if field == 14 { 5 } else { 4 }, 3),
        3 | 4 => (field, 6, 2),
        5 => (field, 2, 1),
        7..=9 => (field, 3, 2),
        16 => (field, 3, 3),
        17 => (field, 3, 1),
        18 => (field, 3, 2),
        20 => (field, 2, 2),
        21 => (field, 4, 1),
        22 | 23 => (field, 2, 1),
        _ => return None,
    };
    let mut rejected = Vec::new();
    encode_bytes(reason_field, BUILTIN_REJECTION.as_bytes(), &mut rejected);
    let mut result = Vec::new();
    encode_bytes(rejection_field, &rejected, &mut result);
    Some(encode_exec(id, exec_id, result_field, &result))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kv_and_exec_responses_keep_ids_and_result_kinds() {
        let get =
            generated::AgentClientMessage::decode(encode_kv_get(42, Some(b"blob")).as_slice())
                .unwrap();
        let generated::agent_client_message::Message::KvClientMessage(kv) = get.message.unwrap()
        else {
            panic!()
        };
        assert_eq!(kv.id, 42);
        assert!(matches!(
            kv.message,
            Some(generated::kv_client_message::Message::GetBlobResult(_))
        ));

        let exec = generated::AgentClientMessage::decode(
            encode_mcp_result(7, "exec-9", "ok", true).as_slice(),
        )
        .unwrap();
        let generated::agent_client_message::Message::ExecClientMessage(exec) =
            exec.message.unwrap()
        else {
            panic!()
        };
        assert_eq!(exec.id, 7);
        assert_eq!(exec.exec_id, "exec-9");
        let Some(generated::exec_client_message::Message::McpResult(result)) = exec.message else {
            panic!()
        };
        let Some(generated::mcp_result::Result::Success(result)) = result.result else {
            panic!()
        };
        assert!(result.is_error);
    }

    #[test]
    fn builtin_rejection_uses_matching_result_field() {
        for field in [2, 3, 4, 5, 7, 8, 9, 14, 16, 17, 18, 20, 21, 22, 23] {
            let data = encode_builtin_rejection(10, "exec", field).unwrap();
            let message = generated::AgentClientMessage::decode(data.as_slice()).unwrap();
            let generated::agent_client_message::Message::ExecClientMessage(exec) =
                message.message.unwrap()
            else {
                panic!()
            };
            assert_eq!(exec.id, 10);
            assert_eq!(exec.exec_id, "exec");
            assert!(exec.message.is_some());
            assert!(data
                .windows(BUILTIN_REJECTION.len())
                .any(|w| w == BUILTIN_REJECTION.as_bytes()));
        }
    }
}
