use super::{credential::CursorCredential, provider, store};
use crate::http::{
    self, AppState, ConfigSnapshot, LoggingConfig, NormalizeConfig, ProviderRefreshConfig,
    ReloadFn, RuntimeConfig, UserAgentSet,
};
use crate::upstream::UpstreamClient;
use axum::body::{to_bytes, Body};
use axum::http::{Request, Response, StatusCode};
use bytes::Bytes;
use ccextra_core::convert::cursor::proto::{
    decode_fields, encode_bytes, encode_varint, reply, ConnectFrame, ConnectFrameDecoder, Field,
    DEFAULT_MAX_FRAME_SIZE,
};
use ccextra_core::route::{ModelConfig, Protocol, ProviderConfig};
use serde_json::{json, Value};
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::sync::RwLock;
use tower::util::ServiceExt;

fn fixture(url: String) -> (AppState, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let credential = CursorCredential {
        access_token: "test-access".into(),
        refresh_token: "test-refresh".into(),
        sub: "test-account".into(),
        expires_at: Some(i64::MAX),
    };
    store::save(dir.path(), &credential).unwrap();
    let metadata = [
        (
            "auth_dir".to_string(),
            dir.path().to_string_lossy().to_string(),
        ),
        (
            "credential_id".to_string(),
            provider::credential_fingerprint(&credential),
        ),
        ("client_version".to_string(), "cli-test".into()),
    ]
    .into();
    let provider = ProviderConfig::new(
        "cursor".into(),
        Protocol::Cursor,
        vec![url],
        credential.access_token,
        Some("direct".into()),
        false,
        vec![ModelConfig {
            name: "composer-2".into(),
            alias: "cursor-test".into(),
            ..Default::default()
        }],
    )
    .with_metadata(metadata);
    let agents = UserAgentSet {
        claude_cli: Arc::new("test".into()),
        codex_tui: Arc::new("test".into()),
        grok_version: Arc::new("test".into()),
        antigravity: Arc::new("test".into()),
    };
    let reload: ReloadFn = Arc::new(|| Box::pin(async { Err(anyhow::anyhow!("unused")) }));
    let state = AppState {
        config: Arc::new(RwLock::new(Arc::new(ConfigSnapshot {
            version: 1,
            providers: vec![provider],
            payload_rules: vec![],
            runtime: RuntimeConfig {
                normalize: NormalizeConfig {
                    enabled: false,
                    drift_detector: false,
                },
                logging: LoggingConfig {
                    level: "info".into(),
                    request_body: false,
                },
                secret: None,
                upstream: UpstreamClient::new(None),
                user_agents: agents,
                thinking_registry: Arc::new(vec![]),
            },
            refresh: ProviderRefreshConfig::default(),
        }))),
        reload_lock: Arc::new(tokio::sync::Mutex::new(())),
        reload,
        drift: ccextra_core::cache_stabilization::drift_detector::DriftState::new(100),
        replay_cache: crate::sse::replay_cache::ReplayCache::new(Duration::from_secs(60), 100),
        cursor_sessions: super::session::CursorSessions::default(),
        last_input_tokens: Arc::new(Mutex::new(http::session_tokens::SessionTokenCache::new())),
    };
    (state, dir)
}

fn field(number: u64, data: &[u8]) -> Vec<u8> {
    let mut message = Vec::new();
    encode_bytes(number, data, &mut message);
    message
}

fn exec_call(id: u32, exec_id: &str, call_id: &str) -> Vec<u8> {
    let mut mcp = field(1, b"lookup");
    mcp.extend(field(3, call_id.as_bytes()));
    let mut exec = Vec::new();
    ccextra_core::convert::cursor::proto::encode_tag(1, 0, &mut exec);
    encode_varint(u64::from(id), &mut exec);
    exec.extend(field(15, exec_id.as_bytes()));
    exec.extend(field(11, &mcp));
    field(2, &exec)
}

fn kv_request(id: u32, kind: u64, blob: Option<&[u8]>) -> Vec<u8> {
    let mut data = field(1, b"blob-1");
    if let Some(blob) = blob {
        data.extend(field(2, blob));
    }
    let mut kv = Vec::new();
    ccextra_core::convert::cursor::proto::encode_tag(1, 0, &mut kv);
    encode_varint(u64::from(id), &mut kv);
    kv.extend(field(kind, &data));
    field(4, &kv)
}

fn context_request(id: u32, exec_id: &str) -> Vec<u8> {
    let mut exec = Vec::new();
    ccextra_core::convert::cursor::proto::encode_tag(1, 0, &mut exec);
    encode_varint(u64::from(id), &mut exec);
    exec.extend(field(15, exec_id.as_bytes()));
    exec.extend(field(10, &[]));
    field(2, &exec)
}

async fn tool_upstream() -> (
    String,
    tokio::task::JoinHandle<()>,
    tokio::sync::oneshot::Sender<()>,
    tokio::sync::oneshot::Receiver<()>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let (parked_tx, parked_rx) = tokio::sync::oneshot::channel();
    let (replied_tx, replied_rx) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let (socket, _) = listener.accept().await.unwrap();
        let mut connection = h2::server::handshake(socket).await.unwrap();
        let (request, mut response) = connection.accept().await.unwrap().unwrap();
        let mut body = request.into_body();
        let driver = tokio::spawn(async move {
            while let Some(request) = connection.accept().await {
                request.unwrap();
            }
        });
        let _initial = body.data().await.unwrap().unwrap();
        assert!(!body.is_end_stream());
        let mut sender = response.send_response(Response::new(()), false).unwrap();
        let mut tool_calls = exec_call(7, "exec-a", "call-a");
        tool_calls.extend(exec_call(9, "exec-b", "call-b"));
        sender
            .send_data(
                Bytes::from(ConnectFrame::encode(&tool_calls, 0).unwrap()),
                false,
            )
            .unwrap();
        parked_rx.await.unwrap();
        let mut controls = kv_request(42, 3, Some(b"stored"));
        controls.extend(kv_request(43, 2, None));
        controls.extend(context_request(44, "exec-context"));
        controls.extend(field(3, CHECKPOINT));
        sender
            .send_data(
                Bytes::from(ConnectFrame::encode(&controls, 0).unwrap()),
                false,
            )
            .unwrap();
        let mut decoder = ConnectFrameDecoder::new(DEFAULT_MAX_FRAME_SIZE);
        let mut frames = std::collections::VecDeque::new();
        let expected = [
            reply::encode_kv_set(42),
            reply::encode_kv_get(43, Some(b"stored")),
        ];
        for item in expected {
            while frames.is_empty() {
                let chunk = body.data().await.unwrap().unwrap();
                frames.extend(decoder.push(&chunk).unwrap());
            }
            assert_eq!(frames.pop_front().unwrap().payload, item);
            assert!(!body.is_end_stream());
        }
        while frames.is_empty() {
            let chunk = body.data().await.unwrap().unwrap();
            frames.extend(decoder.push(&chunk).unwrap());
        }
        let frame = frames.pop_front().unwrap();
        let outer = decode_fields(&frame.payload).unwrap();
        let [Field::Bytes {
            number: 2,
            value: exec,
        }] = outer.as_slice()
        else {
            panic!("expected exec client message");
        };
        let fields = decode_fields(exec).unwrap();
        assert!(fields.iter().any(|field| matches!(
            field,
            Field::Varint {
                number: 1,
                value: 44
            }
        )));
        assert!(fields.iter().any(|field| matches!(
            field,
            Field::Bytes {
                number: 15,
                value: b"exec-context"
            }
        )));
        assert!(fields
            .iter()
            .any(|field| matches!(field, Field::Bytes { number: 10, .. })));
        replied_tx.send(()).unwrap();
        let expected = [
            reply::encode_mcp_result(7, "exec-a", "A", false),
            reply::encode_mcp_result(9, "exec-b", "B", true),
        ];
        for item in expected {
            while frames.is_empty() {
                let chunk = body.data().await.unwrap().unwrap();
                frames.extend(decoder.push(&chunk).unwrap());
            }
            assert_eq!(frames.pop_front().unwrap().payload, item);
            assert!(!body.is_end_stream());
        }
        sender.send_data(text_frame("tools done"), false).unwrap();
        sender.send_data(end_frame(), true).unwrap();
        let _ = tokio::time::timeout(Duration::from_secs(1), driver).await;
    });
    (url, task, parked_tx, replied_rx)
}

fn interaction(kind: u64, value: &[u8]) -> Vec<u8> {
    field(1, &field(kind, value))
}

fn text_frame(text: &str) -> Bytes {
    let message = interaction(1, &field(1, text.as_bytes()));
    Bytes::from(ConnectFrame::encode(&message, 0).unwrap())
}

fn end_frame() -> Bytes {
    Bytes::from(ConnectFrame::encode(b"{}", 2).unwrap())
}

const CHECKPOINT: &[u8] = b"\x0a\x01x\x98\x06\x01";

fn assert_checkpoint_request(initial: &[u8]) {
    use ccextra_core::convert::cursor::proto::{decode_fields, Field};
    let mut frames = ConnectFrameDecoder::new(DEFAULT_MAX_FRAME_SIZE);
    let frames = frames.push(initial).unwrap();
    assert_eq!(frames.len(), 1);
    let payload = &frames[0].payload;
    let run = decode_fields(payload)
        .unwrap()
        .into_iter()
        .find_map(|field| match field {
            Field::Bytes { number: 1, value } => Some(value),
            _ => None,
        })
        .unwrap();
    let state = decode_fields(run)
        .unwrap()
        .into_iter()
        .find_map(|field| match field {
            Field::Bytes { number: 1, value } => Some(value),
            _ => None,
        })
        .unwrap();
    assert_eq!(state, CHECKPOINT);
}

#[derive(Clone, Copy)]
enum Scenario {
    Success,
    Eof,
    Connect429,
    Http503,
    Checkpoint,
    ExpectCheckpoint,
}

async fn mock_upstream(
    scenarios: Vec<Scenario>,
) -> (
    String,
    tokio::task::JoinHandle<()>,
    Arc<std::sync::atomic::AtomicUsize>,
) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}", listener.local_addr().unwrap());
    let count = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let calls = count.clone();
    let task = tokio::spawn(async move {
        for scenario in scenarios {
            let (socket, _) = listener.accept().await.unwrap();
            let mut connection = h2::server::handshake(socket).await.unwrap();
            let (request, mut response) = connection.accept().await.unwrap().unwrap();
            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            assert_eq!(request.uri().path(), "/agent.v1.AgentService/Run");
            assert_eq!(request.headers()["authorization"], "Bearer test-access");
            let mut body = request.into_body();
            let driver = tokio::spawn(async move {
                while let Some(request) = connection.accept().await {
                    request.unwrap();
                }
            });
            let initial = body.data().await.unwrap().unwrap();
            assert_eq!(initial[0], 0);
            assert!(!body.is_end_stream());
            if matches!(scenario, Scenario::ExpectCheckpoint) {
                assert_checkpoint_request(&initial);
            }
            let status = if matches!(scenario, Scenario::Http503) {
                StatusCode::SERVICE_UNAVAILABLE
            } else {
                StatusCode::OK
            };
            let mut sender = response
                .send_response(Response::builder().status(status).body(()).unwrap(), false)
                .unwrap();
            match scenario {
                Scenario::Success | Scenario::ExpectCheckpoint => {
                    sender.send_data(text_frame("pong"), false).unwrap();
                    sender.send_data(end_frame(), true).unwrap();
                }
                Scenario::Checkpoint => {
                    sender.send_data(Bytes::from(ConnectFrame::encode(&field(3, CHECKPOINT), 0).unwrap()), false).unwrap();
                    sender.send_data(text_frame("pong"), false).unwrap();
                    sender.send_data(end_frame(), true).unwrap();
                }
                Scenario::Eof => sender.send_data(text_frame("partial"), true).unwrap(),
                Scenario::Connect429 => sender.send_data(Bytes::from(ConnectFrame::encode(
                    br#"{"error":{"code":"resource_exhausted","message":"quota exceeded"}}"#, 2,
                ).unwrap()), true).unwrap(),
                Scenario::Http503 => sender.send_data(Bytes::new(), true).unwrap(),
            }
            drop(sender);
            let _ = tokio::time::timeout(Duration::from_secs(1), driver).await;
            drop(body);
        }
    });
    (url, task, count)
}

fn request(body: Value) -> Request<Body> {
    Request::builder()
        .uri("/v1/messages")
        .method("POST")
        .header("content-type", "application/json")
        .header("x-claude-code-session-id", "cursor-session-1")
        .body(Body::from(body.to_string()))
        .unwrap()
}

fn prompt(stream: bool) -> Value {
    json!({ "model": "cursor-test", "max_tokens": 128, "stream": stream,
        "messages": [{ "role": "user", "content": "ping" }] })
}

async fn call(state: AppState, body: Value) -> (StatusCode, String) {
    let response = tokio::time::timeout(
        Duration::from_secs(5),
        http::app(state).oneshot(request(body)),
    )
    .await
    .unwrap()
    .unwrap();
    let status = response.status();
    let bytes = tokio::time::timeout(
        Duration::from_secs(5),
        to_bytes(response.into_body(), 1024 * 1024),
    )
    .await
    .unwrap()
    .unwrap();
    (status, String::from_utf8(bytes.to_vec()).unwrap())
}

#[tokio::test]
async fn text_roundtrip_json_and_sse_end_at_connect_trailer() {
    let (url, server, calls) = mock_upstream(vec![Scenario::Success, Scenario::Success]).await;
    let (state, _dir) = fixture(url);
    let (status, body) = call(state.clone(), prompt(false)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let data: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(data["model"], "cursor-test");
    assert_eq!(data["content"][0]["text"], "pong");
    assert_eq!(data["stop_reason"], "end_turn");
    let (status, stream) = call(state, prompt(true)).await;
    assert_eq!(status, StatusCode::OK, "{stream}");
    assert!(stream.contains("event: message_start"), "{stream}");
    assert!(stream.contains("event: message_stop"), "{stream}");
    assert!(stream.contains("pong"), "{stream}");
    assert!(!stream.contains("event: error"), "{stream}");
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
}

#[tokio::test]
async fn partial_output_then_eof_is_stream_error_not_success() {
    let (url, server, _) = mock_upstream(vec![Scenario::Eof]).await;
    let (state, _dir) = fixture(url);
    let (status, stream) = call(state, prompt(true)).await;
    assert_eq!(status, StatusCode::OK, "{stream}");
    assert!(stream.contains("partial"), "{stream}");
    assert!(stream.contains("event: error"), "{stream}");
    assert!(!stream.contains("event: message_stop"), "{stream}");
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn parked_stream_handles_controls_before_multiple_tool_results_resume() {
    let (url, server, parked, replied) = tool_upstream().await;
    let (state, dir) = fixture(url);
    let mut first = prompt(false);
    first["tools"] = json!([{
        "name": "lookup", "description": "Look up data",
        "input_schema": { "type": "object", "properties": {} },
    }]);
    let (status, body) = call(state.clone(), first.clone()).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let data: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(data["stop_reason"], "tool_use");
    assert_eq!(data["content"].as_array().unwrap().len(), 2);
    assert_eq!(data["content"][0]["id"], "call-a");
    assert_eq!(data["content"][1]["id"], "call-b");
    parked.send(()).unwrap();
    tokio::time::timeout(Duration::from_secs(5), replied)
        .await
        .unwrap()
        .unwrap();
    let identity = provider::credential_fingerprint(&store::load(dir.path()).unwrap());
    let conversation =
        ccextra_core::convert::cursor::conversation_id(&identity, "cursor-session-1");
    assert_eq!(
        state
            .cursor_sessions
            .checkpoint(&conversation, &identity)
            .unwrap()
            .0,
        CHECKPOINT,
    );

    first["messages"] = json!([
        { "role": "user", "content": "ping" },
        { "role": "assistant", "content": data["content"] },
        { "role": "user", "content": [
            { "type": "tool_result", "tool_use_id": "call-b", "content": "B", "is_error": true },
            { "type": "tool_result", "tool_use_id": "call-a", "content": "A" },
        ] },
    ]);
    let (status, body) = call(state, first).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let data: Value = serde_json::from_str(&body).unwrap();
    assert_eq!(data["stop_reason"], "end_turn");
    assert_eq!(data["content"][0]["text"], "tools done");
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn reuses_raw_checkpoint_only_with_stable_session() {
    let (url, server, calls) =
        mock_upstream(vec![Scenario::Checkpoint, Scenario::ExpectCheckpoint]).await;
    let (state, _dir) = fixture(url);
    let (status, body) = call(state.clone(), prompt(false)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    let mut next = prompt(false);
    next["messages"] = json!([
        { "role": "user", "content": "ping" },
        { "role": "assistant", "content": "pong" },
        { "role": "user", "content": "again" },
    ]);
    let (status, body) = call(state, next).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);
}

#[tokio::test]
async fn retries_401_once_with_rotated_disk_token_and_publishes_it() {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let (state, dir) = fixture(format!("http://{}", listener.local_addr().unwrap()));
    let auth_dir = dir.path().to_path_buf();
    let server = tokio::spawn(async move {
        for attempt in 0..2 {
            let (socket, _) = listener.accept().await.unwrap();
            let mut connection = h2::server::handshake(socket).await.unwrap();
            let (request, mut response) = connection.accept().await.unwrap().unwrap();
            assert_eq!(
                request.headers()["authorization"],
                if attempt == 0 {
                    "Bearer test-access"
                } else {
                    "Bearer test-access-next"
                }
            );
            let mut body = request.into_body();
            let driver = tokio::spawn(async move {
                while let Some(next) = connection.accept().await {
                    next.unwrap();
                }
            });
            body.data().await.unwrap().unwrap();
            let status = if attempt == 0 {
                StatusCode::UNAUTHORIZED
            } else {
                StatusCode::OK
            };
            let mut sender = response
                .send_response(Response::builder().status(status).body(()).unwrap(), false)
                .unwrap();
            if attempt == 0 {
                let mut credential = store::load(&auth_dir).unwrap();
                credential.access_token = "test-access-next".into();
                store::save(&auth_dir, &credential).unwrap();
                sender.send_data(Bytes::new(), true).unwrap();
            } else {
                sender.send_data(text_frame("fresh"), false).unwrap();
                sender.send_data(end_frame(), true).unwrap();
            }
            drop(sender);
            let _ = tokio::time::timeout(Duration::from_secs(1), driver).await;
        }
    });
    let (status, body) = call(state.clone(), prompt(false)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap()["content"][0]["text"],
        "fresh"
    );
    assert_eq!(
        state.config.read().await.providers[0].key,
        "test-access-next"
    );
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn retries_503_before_output_but_not_connect_429() {
    let (url, server, calls) = mock_upstream(vec![Scenario::Http503, Scenario::Success]).await;
    let (state, _dir) = fixture(url);
    let (status, body) = call(state, prompt(false)).await;
    assert_eq!(status, StatusCode::OK, "{body}");
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap()["content"][0]["text"],
        "pong"
    );
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 2);

    let (url, server, calls) = mock_upstream(vec![Scenario::Connect429]).await;
    let (state, _dir) = fixture(url);
    let (status, body) = call(state, prompt(true)).await;
    assert_eq!(status, StatusCode::TOO_MANY_REQUESTS, "{body}");
    assert_eq!(
        serde_json::from_str::<Value>(&body).unwrap()["error"]["type"],
        "api_error"
    );
    tokio::time::timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
}
