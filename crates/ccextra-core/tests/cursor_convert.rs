use ccextra_core::convert::cursor::proto::{
    decode_agent_server_message, decode_fields, decode_get_usable_models_response, encode_bytes,
    encode_get_usable_models_request, encode_tag, encode_varint, generated,
    parse_connect_end_stream, reply, ConnectError, ConnectFrame, ConnectFrameDecoder,
    ConnectFrameError, ExecKind, Field, InteractionQuery, InteractionQueryKind, ServerMessage,
    WireError, CONNECT_COMPRESSION_FLAG, CONNECT_END_STREAM_FLAG,
};
use ccextra_core::convert::cursor::{build_run_request, conversation_id, CursorRunRequest};
use flate2::{write::GzEncoder, Compression};
use prost::Message;
use serde_json::json;
use std::io::Write;

fn run(payload: &[u8]) -> generated::AgentRunRequest {
    let client = generated::AgentClientMessage::decode(payload).unwrap();
    match client.message.unwrap() {
        generated::agent_client_message::Message::RunRequest(request) => request,
        _ => panic!("expected run_request"),
    }
}

/// 解析 root blob 的 system content(对齐 Plus:真 system 进 KV blob)
fn blob_system(request: &CursorRunRequest) -> String {
    assert_eq!(request.blob_store.len(), 1);
    let bytes = request.blob_store.values().next().unwrap();
    let value: serde_json::Value = serde_json::from_slice(bytes).unwrap();
    assert_eq!(value["role"], "system");
    value["content"].as_str().unwrap().to_string()
}

fn message(number: u64, value: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    encode_bytes(number, value, &mut out);
    out
}

// ---------------------------------------------------------------------------
// 1. Connect 帧与 Trailer 解码
// ---------------------------------------------------------------------------

#[test]
fn connect_decoder_handles_split_compressed_and_end_stream_frames() {
    let mut decoder = ConnectFrameDecoder::new(1024);
    let frame = ConnectFrame::encode(b"hello", 0).unwrap();
    assert!(decoder.push(&frame[..3]).unwrap().is_empty());
    let frames = decoder.push(&frame[3..]).unwrap();
    assert_eq!(frames[0].payload, b"hello");

    let mut encoder = GzEncoder::new(Vec::new(), Compression::default());
    encoder.write_all(b"compressed").unwrap();
    let compressed = encoder.finish().unwrap();
    let frame = ConnectFrame::encode(&compressed, CONNECT_COMPRESSION_FLAG).unwrap();
    let decoded = decoder.push(&frame).unwrap().pop().unwrap();
    assert_eq!(decoded.decoded_payload(1024).unwrap(), b"compressed");

    let trailer = ConnectFrame::encode(br#"{"error":null}"#, CONNECT_END_STREAM_FLAG).unwrap();
    let end = decoder.push(&trailer).unwrap().pop().unwrap();
    assert_eq!(end.flags, CONNECT_END_STREAM_FLAG);
}

#[test]
fn connect_decoder_rejects_oversized_frame() {
    let frame = ConnectFrame::encode(b"1234", 0).unwrap();
    let err = ConnectFrameDecoder::new(3).push(&frame).unwrap_err();
    assert_eq!(err, ConnectFrameError::FrameTooLarge);
}

#[test]
fn end_stream_trailer_preserves_remote_error() {
    let result = parse_connect_end_stream(
        br#"{"error":{"code":"permission_denied","message":"no access"}}"#,
    )
    .unwrap();
    assert_eq!(
        result,
        Some(ConnectError {
            code: "permission_denied".into(),
            message: "no access".into(),
            details: Vec::new(),
        })
    );

    assert_eq!(
        parse_connect_end_stream(br#"{"error":null}"#).unwrap(),
        None
    );
}

#[test]
fn end_stream_trailer_extracts_readable_details() {
    // details value 是 base64 的序列化 protobuf;字符串字段按可打印段提取
    // (fixture: field 2 len 11 "hello world")
    let result = parse_connect_end_stream(
        br#"{"error":{"code":"not_found","message":"Error","details":[{"value":"EgtoZWxsbyB3b3JsZA=="}]}}"#,
    )
    .unwrap();
    let error = result.unwrap();
    assert_eq!(error.details, vec!["hello world".to_string()]);
}

// ---------------------------------------------------------------------------
// 2. Unary 模型目录
// ---------------------------------------------------------------------------

#[test]
fn usable_models_request_is_unframed_unary_protobuf() {
    assert!(encode_get_usable_models_request(&[]).is_empty());
    let ids = vec!["custom-model".to_string()];
    let request = generated::GetUsableModelsRequest::decode(
        encode_get_usable_models_request(&ids).as_slice(),
    )
    .unwrap();
    assert_eq!(request.custom_model_ids, ids);
}

#[test]
fn usable_models_response_accepts_raw_and_connect_framed_body() {
    let response = generated::GetUsableModelsResponse {
        models: vec![generated::ModelDetails {
            model_id: "gpt-5".into(),
            ..Default::default()
        }],
    };
    let payload = response.encode_to_vec();
    let raw = decode_get_usable_models_response(&payload).unwrap();
    assert_eq!(raw.models[0].model_id, "gpt-5");

    let mut framed = ConnectFrame::encode(&payload, 0).unwrap();
    framed.extend(ConnectFrame::encode(br#"{"error":null}"#, CONNECT_END_STREAM_FLAG).unwrap());
    let decoded = decode_get_usable_models_response(&framed).unwrap();
    assert_eq!(decoded.models[0].model_id, "gpt-5");
}

#[test]
fn usable_models_response_surfaces_connect_trailer_error() {
    let frame = ConnectFrame::encode(
        br#"{"error":{"code":"unavailable","message":"retry"}}"#,
        CONNECT_END_STREAM_FLAG,
    )
    .unwrap();
    let err = decode_get_usable_models_response(&frame).unwrap_err();
    assert!(err.to_string().contains("unavailable"));
}

// ---------------------------------------------------------------------------
// 3. Raw-Wire 混合帧、非规范字段序与溢出防护
// ---------------------------------------------------------------------------

#[test]
fn mixed_server_fields_keep_exec_and_trailing_interaction() {
    let interaction = message(1, &message(1, &message(1, b"hello")));
    let mut entry = Vec::new();
    encode_bytes(1, b"query", &mut entry);
    encode_bytes(2, br#"{"type":"string"}"#, &mut entry);

    let mut mcp = Vec::new();
    encode_bytes(1, b"search", &mut mcp);
    encode_bytes(2, &entry, &mut mcp);
    encode_bytes(3, b"call-1", &mut mcp);

    let mut exec = Vec::new();
    encode_tag(1, 0, &mut exec);
    encode_varint(7, &mut exec);
    encode_bytes(15, b"exec-1", &mut exec);
    encode_bytes(11, &mcp, &mut exec);

    let checkpoint = vec![0x0a, 0x01, b'A', 0x98, 0x06, 0x01];
    let mut data = message(2, &exec);
    data.extend_from_slice(&interaction);
    data.extend_from_slice(&message(3, &checkpoint));

    let messages = decode_agent_server_message(&data).unwrap();
    assert_eq!(messages.len(), 3);
    assert_eq!(
        messages[0],
        ServerMessage::Exec(ccextra_core::convert::cursor::proto::ExecRequest {
            exec_msg_id: 7,
            exec_id: "exec-1".into(),
            kind: ExecKind::Mcp {
                name: "search".into(),
                tool_call_id: "call-1".into(),
                args: [("query".into(), br#"{"type":"string"}"#.to_vec())].into(),
            },
        },)
    );
    assert_eq!(messages[1], ServerMessage::TextDelta("hello".into()));
    assert_eq!(
        messages[2],
        ServerMessage::Checkpoint(ccextra_core::convert::cursor::proto::RawCheckpoint(
            checkpoint
        ),)
    );
}

#[test]
fn kv_decoder_keeps_id_when_field_order_is_not_canonical() {
    let mut get_args = Vec::new();
    encode_bytes(1, b"blob-1", &mut get_args);
    let mut kv = message(2, &get_args);
    encode_tag(1, 0, &mut kv);
    encode_varint(42, &mut kv);

    let messages = decode_agent_server_message(&message(4, &kv)).unwrap();
    assert_eq!(
        messages,
        vec![ServerMessage::KvGet {
            id: 42,
            blob_id: b"blob-1".to_vec(),
        }]
    );
}

#[test]
fn mcp_state_and_subagent_exec_decode_and_reply() {
    // field 36 = McpStateExecArgs{server_identifiers},field 28 = SubagentArgs{tool_call_id}
    let mut mcp_state_args = Vec::new();
    encode_bytes(1, b"proxy", &mut mcp_state_args);
    let mut exec = Vec::new();
    encode_tag(1, 0, &mut exec);
    encode_varint(7, &mut exec);
    encode_bytes(15, b"exec-1", &mut exec);
    encode_bytes(36, &mcp_state_args, &mut exec);
    let messages = decode_agent_server_message(&message(2, &exec)).unwrap();
    assert_eq!(
        messages,
        vec![ServerMessage::Exec(
            ccextra_core::convert::cursor::proto::ExecRequest {
                exec_msg_id: 7,
                exec_id: "exec-1".into(),
                kind: ExecKind::McpState {
                    server_identifiers: vec!["proxy".into()],
                },
            },
        )]
    );

    let mut subagent_args = Vec::new();
    encode_bytes(1, b"call-1", &mut subagent_args);
    let mut exec = Vec::new();
    encode_tag(1, 0, &mut exec);
    encode_varint(8, &mut exec);
    encode_bytes(15, b"exec-2", &mut exec);
    encode_bytes(28, &subagent_args, &mut exec);
    let messages = decode_agent_server_message(&message(2, &exec)).unwrap();
    assert_eq!(
        messages,
        vec![ServerMessage::Exec(
            ccextra_core::convert::cursor::proto::ExecRequest {
                exec_msg_id: 8,
                exec_id: "exec-2".into(),
                kind: ExecKind::Subagent {
                    tool_call_id: "call-1".into(),
                },
            },
        )]
    );

    // 回复编码:McpStateExecResult(field 36)与 SubagentResult(field 28)
    let tools = vec![generated::McpToolDefinition {
        name: "read_file".into(),
        provider_identifier: "proxy".into(),
        tool_name: "read_file".into(),
        ..Default::default()
    }];
    let payload =
        ccextra_core::convert::cursor::proto::reply::encode_mcp_state_result(7, "exec-1", tools);
    let client = generated::AgentClientMessage::decode(payload.as_slice()).unwrap();
    let generated::agent_client_message::Message::ExecClientMessage(exec) = client.message.unwrap()
    else {
        panic!("expected exec reply");
    };
    assert_eq!(exec.id, 7);
    assert_eq!(exec.exec_id, "exec-1");
    let generated::exec_client_message::Message::McpStateExecResult(result) = exec.message.unwrap()
    else {
        panic!("expected mcp_state_exec_result");
    };
    let generated::mcp_state_exec_result::Result::Success(success) = result.result.unwrap() else {
        panic!("expected success");
    };
    assert_eq!(success.servers.len(), 1);
    assert_eq!(success.servers[0].server_identifier, "proxy");
    assert_eq!(success.servers[0].tools.len(), 1);

    let payload = ccextra_core::convert::cursor::proto::reply::encode_subagent_error(8, "exec-2");
    let client = generated::AgentClientMessage::decode(payload.as_slice()).unwrap();
    let generated::agent_client_message::Message::ExecClientMessage(exec) = client.message.unwrap()
    else {
        panic!("expected exec reply");
    };
    assert_eq!(exec.id, 8);
    let generated::exec_client_message::Message::SubagentResult(result) = exec.message.unwrap()
    else {
        panic!("expected subagent_result");
    };
    assert!(matches!(
        result.result.unwrap(),
        generated::subagent_result::Result::Error(_)
    ));
}

#[test]
fn turn_ended_carries_usage_fields() {
    // TurnEndedUpdate:1 input|2 output|3 cache_read|4 cache_write|5 reasoning(全 optional varint)
    let mut turn_ended = Vec::new();
    encode_tag(1, 0, &mut turn_ended);
    encode_varint(1000, &mut turn_ended);
    encode_tag(2, 0, &mut turn_ended);
    encode_varint(200, &mut turn_ended);
    encode_tag(3, 0, &mut turn_ended);
    encode_varint(600, &mut turn_ended);
    encode_tag(4, 0, &mut turn_ended);
    encode_varint(50, &mut turn_ended);
    let messages = decode_agent_server_message(&message(1, &message(14, &turn_ended))).unwrap();
    assert_eq!(
        messages,
        vec![ServerMessage::TurnEnded(
            ccextra_core::convert::cursor::proto::TurnUsage {
                input_tokens: Some(1000),
                output_tokens: Some(200),
                cache_read_tokens: Some(600),
                cache_write_tokens: Some(50),
                reasoning_tokens: None,
            },
        )]
    );

    // 空 payload:全 None,不误报用量
    let messages = decode_agent_server_message(&message(1, &message(14, &[]))).unwrap();
    assert_eq!(messages, vec![ServerMessage::TurnEnded(Default::default())]);
}

#[test]
fn malformed_nested_length_is_rejected() {
    let err = decode_agent_server_message(&[0x0a, 0x05, 0x0a, 0x04, b'x']).unwrap_err();
    assert!(matches!(
        err,
        ccextra_core::convert::cursor::proto::WireError::LengthOverflow
    ));
}

// ---------------------------------------------------------------------------
// 4. 请求转换、冷启动/首轮与 Checkpoint 注入
// ---------------------------------------------------------------------------

#[test]
fn cold_request_flattens_history_and_defines_mcp_schema() {
    let body = json!({
        "system": [{"type":"text", "text":"Be brief"}],
        "messages": [
            {"role":"user", "content":"Find status"},
            {"role":"assistant", "content":[{"type":"tool_use", "id":"call-1", "name":"lookup", "input":{"id":7}}]},
            {"role":"user", "content":[{"type":"tool_result", "tool_use_id":"call-1", "content":"ok"}, {"type":"text", "text":"Continue"}]}
        ],
        "tools": [{"name":"lookup", "description":"Find status", "input_schema":{"type":"object", "properties":{"id":{"type":"integer"}}}}],
        "max_tokens": 256
    });
    let request =
        build_run_request(&body, "composer-2-medium", "conversation", "msg-1", None).unwrap();
    let run = run(&request.payload);
    let action = match run.action.unwrap().action.unwrap() {
        generated::conversation_action::Action::UserMessageAction(action) => action,
        _ => panic!("expected user message"),
    };
    let text = action.user_message.unwrap().text;
    // 对齐 Plus:system 进 root blob,不进 UserText
    assert!(!text.contains("Be brief"));
    assert!(text.contains("ASSISTANT_TOOL_CALL"));
    assert!(text.contains("TOOL_RESULT"));
    assert!(text.contains("OUTPUT CONSTRAINTS"));
    assert_eq!(blob_system(&request), "Be brief");
    assert_eq!(run.model_details.unwrap().model_id, "composer-2-medium");
    let tools = run.mcp_tools.unwrap().mcp_tools;
    assert_eq!(tools[0].provider_identifier, "proxy");
    let schema = prost_types::Value::decode(tools[0].input_schema.as_slice()).unwrap();
    assert!(matches!(
        schema.kind,
        Some(prost_types::value::Kind::StructValue(_))
    ));
    assert_eq!(request.blob_store.len(), 1);
    assert_eq!(
        run.conversation_state
            .unwrap()
            .root_prompt_messages_json
            .len(),
        1
    );
}

#[test]
fn first_turn_with_system_has_no_continuation_tail() {
    let body = json!({
        "system": "Be brief. TOOL_RESULT: is a label in examples.",
        "messages": [{"role":"user", "content":"Find status about TOOL_RESULT:"}]
    });
    let request = build_run_request(&body, "composer-2", "conv", "msg-1", None).unwrap();
    let run = run(&request.payload);
    let action = match run.action.unwrap().action.unwrap() {
        generated::conversation_action::Action::UserMessageAction(action) => action,
        _ => panic!("expected user message"),
    };
    let text = action.user_message.unwrap().text;
    // 对齐 Plus:system 进 root blob;单轮 UserText 为原文,无 USER: 前缀
    assert!(!text.contains("Be brief"));
    assert_eq!(text, "Find status about TOOL_RESULT:");
    assert_eq!(
        blob_system(&request),
        "Be brief. TOOL_RESULT: is a label in examples."
    );
    assert!(!text.contains("Continue from the conversation above"));
}

#[test]
fn empty_system_falls_back_to_default_prompt() {
    // 对齐 Plus:无 system 时 root blob 兜底默认提示词
    let body = json!({"messages": [{"role":"user", "content":"hi"}]});
    let request = build_run_request(&body, "composer-2", "conv", "msg-1", None).unwrap();
    assert_eq!(blob_system(&request), "You are a helpful assistant.");
}

#[test]
fn image_blocks_become_selected_images() {
    // 对齐 Plus:image 块进 SelectedContext.selected_images,不进 UserText
    let body = json!({
        "messages": [{"role":"user", "content":[
            {"type":"text", "text":"describe"},
            {"type":"image", "source":{"type":"base64", "media_type":"image/png", "data":"aGk="}}
        ]}]
    });
    let request = build_run_request(&body, "composer-2", "conv", "msg-1", None).unwrap();
    let run = run(&request.payload);
    let action = match run.action.unwrap().action.unwrap() {
        generated::conversation_action::Action::UserMessageAction(action) => action,
        _ => panic!("expected user message"),
    };
    let user_message = action.user_message.unwrap();
    assert_eq!(user_message.text, "describe");
    let images = user_message.selected_context.unwrap().selected_images;
    assert_eq!(images.len(), 1);
    assert_eq!(images[0].mime_type, "image/png");
    assert_eq!(
        images[0].data_or_blob_id,
        Some(generated::selected_image::DataOrBlobId::Data(
            b"hi".to_vec()
        ))
    );
    assert_eq!(images[0].uuid.len(), 32);
}

#[test]
fn messages_system_role_merges_into_system_prompt() {
    // 对齐 Plus:messages 内 system 消息并入 prompt 文本,不报未知角色、不算对话轮
    let body = json!({
        "messages": [
            {"role":"system", "content":"You are terse"},
            {"role":"user", "content":"hi"}
        ]
    });
    let request = build_run_request(&body, "composer-2", "conv", "msg-1", None).unwrap();
    let run = run(&request.payload);
    let action = match run.action.unwrap().action.unwrap() {
        generated::conversation_action::Action::UserMessageAction(action) => action,
        _ => panic!("expected user message"),
    };
    let text = action.user_message.unwrap().text;
    // 对齐 Plus:messages 内 system 并入 root blob,不进 UserText;单轮无前缀无尾巴
    assert!(!text.contains("SYSTEM:"));
    assert_eq!(text, "hi");
    assert_eq!(blob_system(&request), "You are terse");
    assert!(!text.contains("Continue from the conversation above"));
}

#[test]
fn messages_system_role_with_history_keeps_continuation_tail() {
    let body = json!({
        "messages": [
            {"role":"system", "content":"You are terse"},
            {"role":"user", "content":"one"},
            {"role":"assistant", "content":"two"},
            {"role":"user", "content":"three"}
        ]
    });
    let request = build_run_request(&body, "composer-2", "conv", "msg-1", None).unwrap();
    let run = run(&request.payload);
    let action = match run.action.unwrap().action.unwrap() {
        generated::conversation_action::Action::UserMessageAction(action) => action,
        _ => panic!("expected user message"),
    };
    let text = action.user_message.unwrap().text;
    assert!(!text.contains("SYSTEM:"));
    assert!(text.contains("ASSISTANT: two"));
    assert!(text.contains("Continue from the conversation above"));
    assert_eq!(blob_system(&request), "You are terse");
}

#[test]
fn single_tool_result_without_checkpoint_uses_continuation() {
    let body = json!({
        "messages": [{"role":"user", "content":[
            {"type":"tool_result", "tool_use_id":"call-1", "content":"ok"},
            {"type":"text", "text":"Next"}
        ]}]
    });
    let request = build_run_request(&body, "composer-2", "conv", "msg", None).unwrap();
    let action = run(&request.payload).action.unwrap().action.unwrap();
    let generated::conversation_action::Action::UserMessageAction(action) = action else {
        panic!("expected user message");
    };
    let text = action.user_message.unwrap().text;
    assert!(text.contains("TOOL_RESULT:"));
    assert!(text.contains("Continue from the conversation above"));
}

#[test]
fn checkpoint_is_embedded_without_reencoding_unknown_fields() {
    let checkpoint = [0xf2, 0x07, 0x04, 1, 2, 3, 4];
    let body = json!({"system":"Do not repeat", "messages":[{"role":"user", "content":"Next"}]});
    let request = build_run_request(&body, "composer-2", "conv", "msg", Some(&checkpoint)).unwrap();
    let Field::Bytes {
        value: run_bytes, ..
    } = decode_fields(&request.payload).unwrap()[0]
    else {
        panic!()
    };
    let Field::Bytes { value: state, .. } = decode_fields(run_bytes).unwrap()[0] else {
        panic!()
    };
    assert_eq!(state, checkpoint);
    assert!(request.blob_store.is_empty());
    let parsed = run(&request.payload);
    assert!(parsed.action.unwrap().action.is_some());
    assert_eq!(
        conversation_id("account", "session"),
        conversation_id("account", "session")
    );
    assert_ne!(
        conversation_id("account", "session"),
        conversation_id("other", "session")
    );
}

#[test]
fn checkpoint_with_trailing_system_keeps_last_user_message() {
    // Claude Code 会在 user 后追加 system reminder,末条常是 system;
    // checkpoint 续接必须取最后一条非 system 消息,不能让尾部 reminder 挤掉用户输入
    let checkpoint = [0x0a, 0x01, b'A', 0x98, 0x06, 0x01];
    let body = json!({
        "messages": [
            {"role":"user", "content":"我叫小明"},
            {"role":"assistant", "content":"你好"},
            {"role":"user", "content":"我叫什么名字?"},
            {"role":"system", "content":"<total_tokens>15000</total_tokens>"}
        ]
    });
    let request = build_run_request(&body, "composer-2", "conv", "msg", Some(&checkpoint)).unwrap();
    let action = run(&request.payload).action.unwrap().action.unwrap();
    let generated::conversation_action::Action::UserMessageAction(action) = action else {
        panic!("expected user message");
    };
    let text = action.user_message.unwrap().text;
    assert!(text.contains("我叫什么名字?"));
    assert!(!text.contains("我叫小明"));
    assert!(!text.contains("total_tokens"));
}

#[test]
fn body_effort_maps_to_requested_model_parameters() {
    let body = json!({
        "messages": [{"role":"user", "content":"hi"}],
        "output_config": {"effort": "high"}
    });
    let request = build_run_request(&body, "composer-2", "conv", "msg-1", None).unwrap();
    let run = run(&request.payload);
    // 非家族模型:ModelDetails 用裸 id,effort 走 RequestedModel.parameters
    assert_eq!(run.model_details.unwrap().model_id, "composer-2");
    let requested = run.requested_model.unwrap();
    assert_eq!(requested.model_id, "composer-2");
    assert_eq!(requested.parameters.len(), 1);
    assert_eq!(requested.parameters[0].id, "reasoning_effort");
    assert_eq!(requested.parameters[0].value, "high");
}

#[test]
fn pinned_model_params_win_over_body_effort() {
    let body = json!({
        "messages": [{"role":"user", "content":"hi"}],
        "reasoning_effort": "high"
    });
    let request = build_run_request(&body, "auto:reasoning_effort=low", "conv", "msg-1", None).unwrap();
    let run = run(&request.payload);
    assert_eq!(run.model_details.unwrap().model_id, "auto");
    let requested = run.requested_model.unwrap();
    // 白名单钉住的参数优先,body effort 不覆盖
    assert_eq!(requested.parameters.len(), 1);
    assert_eq!(requested.parameters[0].value, "low");
}

#[test]
fn no_effort_leaves_requested_model_absent() {
    let body = json!({"messages": [{"role":"user", "content":"hi"}]});
    let request = build_run_request(&body, "composer-2", "conv", "msg-1", None).unwrap();
    let run = run(&request.payload);
    assert!(run.requested_model.is_none());
}

#[test]
fn disabled_thinking_sends_none_effort() {
    let body = json!({
        "messages": [{"role":"user", "content":"hi"}],
        "thinking": {"type":"disabled"}
    });
    let request = build_run_request(&body, "composer-2", "conv", "msg-1", None).unwrap();
    let run = run(&request.payload);
    let requested = run.requested_model.unwrap();
    assert_eq!(requested.parameters[0].id, "reasoning_effort");
    assert_eq!(requested.parameters[0].value, "none");
}

#[test]
fn family_model_appends_clamped_effort_to_model_id() {
    // Claude Code thinking.budget_tokens → level → 钳制到目录等级 → 拼接
    let body = json!({
        "messages": [{"role":"user", "content":"hi"}],
        "thinking": {"type":"enabled", "budget_tokens": 8192}
    });
    let model = "grok-4.7:effort_levels=low+medium+high+xhigh";
    let request = build_run_request(&body, model, "conv", "msg-1", None).unwrap();
    let run = run(&request.payload);
    // budget 8192 → medium;等级已消费进 model id,不再走 RequestedModel
    assert_eq!(run.model_details.unwrap().model_id, "grok-4.7-medium");
    assert!(run.requested_model.is_none());
}

#[test]
fn family_model_clamps_effort_to_available_levels() {
    // budget 100000 → xhigh,目录无 xhigh 时钳到 high
    let body = json!({
        "messages": [{"role":"user", "content":"hi"}],
        "thinking": {"type":"enabled", "budget_tokens": 100000}
    });
    let model = "grok-4.7:effort_levels=low+medium+high";
    let request = build_run_request(&body, model, "conv", "msg-1", None).unwrap();
    let run = run(&request.payload);
    assert_eq!(run.model_details.unwrap().model_id, "grok-4.7-high");
}

#[test]
fn family_model_pinned_effort_wins_over_body() {
    let body = json!({
        "messages": [{"role":"user", "content":"hi"}],
        "thinking": {"type":"enabled", "budget_tokens": 8192}
    });
    let model = "grok-4.7:reasoning_effort=high,effort_levels=low+medium+high+xhigh";
    let request = build_run_request(&body, model, "conv", "msg-1", None).unwrap();
    let run = run(&request.payload);
    // 白名单钉住 high,body budget 映射 medium 不覆盖
    assert_eq!(run.model_details.unwrap().model_id, "grok-4.7-high");
    assert!(run.requested_model.is_none());
}

#[test]
fn family_model_without_effort_keeps_bare_base() {
    let body = json!({"messages": [{"role":"user", "content":"hi"}]});
    let model = "grok-4.7:effort_levels=low+medium+high+xhigh";
    let request = build_run_request(&body, model, "conv", "msg-1", None).unwrap();
    let run = run(&request.payload);
    assert_eq!(run.model_details.unwrap().model_id, "grok-4.7");
}

#[test]
fn family_model_without_effort_uses_default_level() {
    // 目录无裸 base 条目时携带 effort_default:无 effort 请求也拼缺省等级,
    // 否则上游 not_found(实测 muse-spark-1.3 裸 id 被拒)
    let body = json!({"messages": [{"role":"user", "content":"hi"}]});
    let model = "muse-spark-1.3:effort_levels=minimal+low+medium+high+max,effort_default=medium";
    let request = build_run_request(&body, model, "conv", "msg-1", None).unwrap();
    let run = run(&request.payload);
    assert_eq!(run.model_details.unwrap().model_id, "muse-spark-1.3-medium");
    // effort_levels/effort_default 是内部标记:家族模式下 parameters 恒空,
    // RequestedModel(field 9)整体不发
    assert!(run.requested_model.is_none());
}

#[test]
fn family_model_disabled_thinking_clamps_to_lowest_level() {
    // 显式禁用(none)钳到目录最低等级:家族无 none 变体
    let body = json!({
        "messages": [{"role":"user", "content":"hi"}],
        "thinking": {"type":"disabled"}
    });
    let model = "grok-4.7:effort_levels=low+medium+high+xhigh";
    let request = build_run_request(&body, model, "conv", "msg-1", None).unwrap();
    let run = run(&request.payload);
    assert_eq!(run.model_details.unwrap().model_id, "grok-4.7-low");
}

#[test]
fn top_level_reasoning_effort_falls_back_for_non_family() {
    // OpenAI 风格入站顶层 reasoning_effort:非家族模型走 parameters 回退
    let body = json!({
        "messages": [{"role":"user", "content":"hi"}],
        "reasoning_effort": "high"
    });
    let request = build_run_request(&body, "composer-2", "conv", "msg-1", None).unwrap();
    let run = run(&request.payload);
    let requested = run.requested_model.unwrap();
    assert_eq!(requested.parameters[0].id, "reasoning_effort");
    assert_eq!(requested.parameters[0].value, "high");
}

#[test]
fn family_model_pinned_auto_effort_falls_back_to_body() {
    // 钉 auto 等于不钉:回退 body budget 映射,不钳到最低档
    let body = json!({
        "messages": [{"role":"user", "content":"hi"}],
        "thinking": {"type":"enabled", "budget_tokens": 8192}
    });
    let model = "grok-4.7:reasoning_effort=auto,effort_levels=low+medium+high+xhigh";
    let request = build_run_request(&body, model, "conv", "msg-1", None).unwrap();
    let run = run(&request.payload);
    assert_eq!(run.model_details.unwrap().model_id, "grok-4.7-medium");
    // 钉参 auto 不重复下发
    assert!(run.requested_model.is_none());
}

#[test]
fn interaction_query_decodes_from_asm_field_7_and_partial_tool_call_is_ignored() {
    // ASM field 7:InteractionQuery{id=5, web_search(field 2)}
    let mut query = Vec::new();
    query.extend_from_slice(&[0x08, 0x05]);
    encode_bytes(2, b"", &mut query);
    // IU field 7 是 partial_tool_call(工具参数流式增量),不是查询
    let partial = message(1, &message(7, b"args"));
    let mut data = partial.clone();
    data.extend_from_slice(&message(7, &query));
    let messages = decode_agent_server_message(&data).unwrap();
    assert_eq!(
        messages,
        vec![ServerMessage::InteractionQuery(
            InteractionQuery {
                id: 5,
                kind: InteractionQueryKind::WebSearch,
            }
        )]
    );
    // 纯 partial_tool_call 帧不产生任何消息,也不误报查询
    assert!(decode_agent_server_message(&partial).unwrap().is_empty());
}

#[test]
fn interaction_response_roundtrips_query_id_and_kind() {
    let query = InteractionQuery {
        id: 9,
        kind: InteractionQueryKind::AskQuestion,
    };
    let data = reply::encode_interaction_response(query);
    let message = generated::AgentClientMessage::decode(data.as_slice()).unwrap();
    let generated::agent_client_message::Message::InteractionResponse(response) =
        message.message.unwrap()
    else {
        panic!("expected InteractionResponse");
    };
    assert_eq!(response.id, 9);
    assert!(matches!(
        response.result,
        Some(generated::interaction_response::Result::AskQuestionInteractionResponse(_))
    ));
}

#[test]
fn server_control_abort_fails_decode_immediately() {
    // ASM field 5:ExecServerControlMessage(服务端主动中止)
    let data = message(5, &message(1, b""));
    let err = decode_agent_server_message(&data).unwrap_err();
    assert!(matches!(
        err,
        WireError::ServerAbort
    ));
}

#[test]
fn conversation_state_double_writes_turns_and_turns_old() {
    let body = json!({
        "messages": [
            {"role":"user", "content":"first"},
            {"role":"assistant", "content":"answer one"},
            {"role":"user", "content":"second"}
        ]
    });
    let request = build_run_request(&body, "composer-2", "conv", "msg-1", None).unwrap();
    let run = run(&request.payload);
    let state = run.conversation_state.unwrap();
    // turns(field 8) 与 turns_old(field 2) 双写,内容一致
    assert_eq!(state.turns.len(), 2);
    assert_eq!(state.turns_old.len(), 2);
    for (turn, old) in state.turns.iter().zip(state.turns_old.iter()) {
        assert_eq!(turn, old);
        let parsed = generated::ConversationTurnStructure::decode(turn.as_slice()).unwrap();
        let generated::conversation_turn_structure::Turn::AgentConversationTurn(agent) =
            parsed.turn.unwrap()
        else {
            panic!("expected agent turn");
        };
        let user = generated::UserMessage::decode(agent.user_message.as_slice()).unwrap();
        assert!(!user.text.is_empty());
    }
    // 第一回合含 assistant 步骤,第二回合只有 user 文本
    let first = generated::ConversationTurnStructure::decode(state.turns[0].as_slice()).unwrap();
    let generated::conversation_turn_structure::Turn::AgentConversationTurn(agent) =
        first.turn.unwrap()
    else {
        panic!()
    };
    assert_eq!(agent.steps.len(), 1);
}
