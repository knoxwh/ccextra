use ccextra_core::convert::cursor::proto::{
    decode_agent_server_message, decode_fields, decode_get_usable_models_response, encode_bytes,
    encode_get_usable_models_request, encode_tag, encode_varint, generated, parse_connect_end_stream,
    ConnectError, ConnectFrame, ConnectFrameDecoder, ConnectFrameError, ExecKind, Field,
    ServerMessage, CONNECT_COMPRESSION_FLAG, CONNECT_END_STREAM_FLAG,
};
use ccextra_core::convert::cursor::{build_run_request, conversation_id};
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
        })
    );

    assert_eq!(
        parse_connect_end_stream(br#"{"error":null}"#).unwrap(),
        None
    );
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
    assert_eq!(text.matches("Be brief").count(), 1);
    assert!(text.contains("ASSISTANT_TOOL_CALL"));
    assert!(text.contains("TOOL_RESULT"));
    assert!(text.contains("OUTPUT CONSTRAINTS"));
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
    assert!(text.contains("Be brief"));
    assert!(text.contains("Find status"));
    assert!(!text.contains("Continue from the conversation above"));
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
