use super::*;
use crate::http::{
    AppState, ConfigSnapshot, LoggingConfig, NormalizeConfig, PayloadRule, ProviderRefreshConfig,
    RuntimeConfig, UserAgentSet, app, auth::auth_cache, claude_relay::CLAUDE_ACCEPT_ENCODING,
    claude_relay::is_claude_passthrough_header_excluded,
};
use crate::upstream::UpstreamClient;
use axum::{
    Router,
    body::Body,
    http::{HeaderMap, HeaderValue, Request, StatusCode},
    response::Response,
    routing::post,
};
use ccextra_core::cache_stabilization::drift_detector::DriftState;
use ccextra_core::route::{ModelConfig, Protocol, ProviderConfig};
use flate2::{Compression, write::GzEncoder};
use futures::StreamExt;
use std::io::Write;
use std::sync::{
    Arc, Mutex,
    atomic::{AtomicBool, AtomicUsize, Ordering},
};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::RwLock;
use tower::ServiceExt;

#[derive(Debug, Clone)]
struct Captured {
    path_and_query: String,
    headers: HeaderMap,
    body: Bytes,
}

async fn spawn_mock(router: Router) -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        let _ = axum::serve(listener, router).await;
    });
    format!("http://{addr}")
}

fn model_exact(name: &str) -> ModelConfig {
    ModelConfig {
        name: name.into(),
        alias: name.into(),
        ..Default::default()
    }
}

fn official_provider(base_url: String, models: Vec<ModelConfig>) -> ProviderConfig {
    ProviderConfig::new(
        "anthropic-official".into(),
        Protocol::AnthropicOfficial,
        vec![base_url],
        String::new(),
        Some("direct".into()),
        false,
        models,
    )
}

fn build_state(
    providers: Vec<ProviderConfig>,
    secret: Option<String>,
    payload_rules: Vec<PayloadRule>,
) -> AppState {
    let runtime = RuntimeConfig {
        normalize: NormalizeConfig {
            enabled: true,
            drift_detector: true,
            drift_tools_description: false,
        },
        logging: LoggingConfig {
            level: "info".into(),
            request_body: true,
            log_dir: None,
        },
        secret,
        upstream: UpstreamClient::new(Some("direct".into())),
        user_agents: UserAgentSet {
            claude_cli: Arc::new("claude-cli/2.1.319".into()),
            codex_tui: Arc::new("codex_cli_rs/0.153.3".into()),
            grok_version: Arc::new("1.0.46".into()),
            antigravity: Arc::new("antigravity/hub/2.10.0".into()),
        },
        thinking_registry: Arc::new(vec![]),
    };
    let snapshot = Arc::new(ConfigSnapshot {
        version: 1,
        providers,
        payload_rules,
        runtime,
        refresh: ProviderRefreshConfig::default(),
    });
    AppState {
        config: Arc::new(RwLock::new(snapshot)),
        reload_lock: Arc::new(tokio::sync::Mutex::new(())),
        reload: Arc::new(|| Box::pin(async { Err(anyhow::anyhow!("unused")) })),
        drift: DriftState::new(64),
        replay_cache: crate::sse::replay_cache::ReplayCache::new(Duration::from_secs(60), 64),
        ag_replay_cache: crate::sse::antigravity_replay_cache::AgReplayCache::new(
            Duration::from_secs(60),
            64,
        ),
        kimi_thinking_replay_cache:
            crate::sse::kimi_thinking_replay_cache::KimiThinkingReplayCache::new(
                Duration::from_secs(60),
                64,
            ),
        last_input_tokens: Arc::new(Mutex::new(
            crate::http::session_tokens::SessionTokenCache::new(),
        )),
    }
}

async fn spawn_capture_mock(
    status: StatusCode,
    resp_headers: Vec<(&'static str, &'static str)>,
    resp_body: &'static [u8],
) -> (String, Arc<Mutex<Vec<Captured>>>) {
    let captured = Arc::new(Mutex::new(Vec::new()));
    let cap_m = Arc::clone(&captured);
    let cap_c = Arc::clone(&captured);
    let make_handler = move |cap: Arc<Mutex<Vec<Captured>>>| {
        let rh = resp_headers.clone();
        move |uri: axum::http::Uri, headers: HeaderMap, body: Bytes| {
            let cap = Arc::clone(&cap);
            let rh = rh.clone();
            async move {
                cap.lock().unwrap().push(Captured {
                    path_and_query: uri
                        .path_and_query()
                        .map(|p| p.as_str().to_string())
                        .unwrap_or_default(),
                    headers,
                    body,
                });
                let mut builder = Response::builder().status(status);
                for (k, v) in rh {
                    builder = builder.header(k, v);
                }
                builder.body(Body::from(resp_body)).unwrap()
            }
        }
    };
    let router = Router::new()
        .route("/v1/messages", post(make_handler(cap_m)))
        .route("/v1/messages/count_tokens", post(make_handler(cap_c)));
    (spawn_mock(router).await, captured)
}

fn default_official_state(base_url: String) -> AppState {
    build_state(
        vec![official_provider(
            base_url,
            vec![model_exact("claude-opus-5-5"), model_exact("claude-*")],
        )],
        None,
        vec![],
    )
}

/// 测试 1、2、3、4、5:头透传、不补 beta、原始 body 逐字节、UA 缺失不发/存在原样、剔除 key 头、AE 原样转发/缺省由 wreq 自报
#[tokio::test]
async fn test_01_to_05_headers_raw_body_ua_and_ae() {
    let (base, cap) = spawn_capture_mock(StatusCode::OK, vec![], br#"{"ok":true}"#).await;
    let state = default_official_state(base);

    // 1 + 3 + 4(无 UA) + 5:原始字节含空白、非字典序、Unicode 与大数
    let raw_body = concat!(
        "{\n",
        "  \"z_big\": 9007199254740993123456,\n",
        "  \"model\":   \"claude-opus-5-5\",\n",
        "  \"note\": \"中文🚀\",\n",
        "  \"messages\": [ { \"role\": \"user\", \"content\": \"hi\" } ]\n",
        "}"
    )
    .as_bytes();
    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("authorization", "Bearer test-oat")
        .header("anthropic-beta", "a,b,oauth-2025-04-20")
        .header("anthropic-version", "2023-06-01")
        .header("x-api-key", "should-strip-1")
        .header("x-ccextra-key", "should-strip-2")
        .header("accept-encoding", CLAUDE_ACCEPT_ENCODING)
        .body(Body::from(Bytes::from_static(raw_body)))
        .unwrap();
    let resp = app(state.clone()).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // 2 + 4(有 UA):beta 不含 oauth 不追加;UA 原样透传
    let req2 = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("authorization", "Bearer test-oat-2")
        .header("anthropic-beta", "prompt-caching-2024-07-31")
        .header("anthropic-version", "2023-06-01")
        .header("user-agent", "my-custom-cc/9.9.9")
        .body(Body::from(r#"{"model":"claude-opus-5-5","messages":[]}"#))
        .unwrap();
    let resp2 = app(state).oneshot(req2).await.unwrap();
    assert_eq!(resp2.status(), StatusCode::OK);

    let items = cap.lock().unwrap();
    assert_eq!(items.len(), 2);
    // 断言 1
    assert_eq!(
        items[0].headers.get("authorization").unwrap(),
        "Bearer test-oat"
    );
    assert_eq!(
        items[0].headers.get("anthropic-beta").unwrap(),
        "a,b,oauth-2025-04-20"
    );
    assert_eq!(
        items[0].headers.get("anthropic-version").unwrap(),
        "2023-06-01"
    );
    // 断言 3
    assert_eq!(items[0].body.as_ref(), raw_body);
    // 断言 4(无 UA 不发;有 UA 原样)
    assert!(items[0].headers.get("user-agent").is_none());
    assert_eq!(
        items[1].headers.get("user-agent").unwrap(),
        "my-custom-cc/9.9.9"
    );
    // 断言 5
    assert!(items[0].headers.get("x-api-key").is_none());
    assert!(items[0].headers.get("x-ccextra-key").is_none());
    // 入站 AE(CC 金样)原样转发
    assert_eq!(
        items[0].headers.get("accept-encoding").unwrap(),
        CLAUDE_ACCEPT_ENCODING
    );
    // 入站缺 AE:ccextra 不补,由 wreq(tower-http 解压层)自报缺省值
    assert_eq!(
        items[1].headers.get("accept-encoding").unwrap(),
        "zstd,gzip,deflate,br"
    );
    // 断言 2
    assert_eq!(
        items[1].headers.get("anthropic-beta").unwrap(),
        "prompt-caching-2024-07-31"
    );
}

/// 测试 6:缺 Authorization(缺失、空值、全空白)本地 401,mock 未收到请求
#[tokio::test]
async fn test_06_missing_or_blank_authorization_returns_local_401() {
    let (base, cap) = spawn_capture_mock(StatusCode::OK, vec![], b"{}").await;
    let state = default_official_state(base);

    for auth in [None, Some(""), Some("   \t  ")] {
        let mut builder = Request::builder().method("POST").uri("/v1/messages");
        if let Some(v) = auth {
            builder = builder.header("authorization", v);
        }
        let req = builder
            .body(Body::from(r#"{"model":"claude-opus-5-5","messages":[]}"#))
            .unwrap();
        let resp = app(state.clone()).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
        let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
        assert_eq!(json["error"]["type"], "authentication_error");
    }
    // Connection: authorization 试图经 hop-by-hop 剥掉 Authorization 时同样本地 401
    let req_conn = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("connection", "authorization")
        .header("authorization", "Bearer test-oat")
        .body(Body::from(r#"{"model":"claude-opus-5-5","messages":[]}"#))
        .unwrap();
    assert_eq!(
        app(state).oneshot(req_conn).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    assert!(cap.lock().unwrap().is_empty());
}

/// 测试 7:200/429/400 响应头(含限额头、request-id、x-should-retry、retry-after)与错误体逐字节原样透传
#[tokio::test]
async fn test_07_response_headers_and_error_body_byte_identical() {
    let raw_err: &'static [u8] =
        br#"{"type":"error","error":{"type":"invalid_request_error","message":"beta rejected","extra_keep":{"k":[1,2]}},"request_id":"req_raw_123"}"#;
    for status in [
        StatusCode::OK,
        StatusCode::TOO_MANY_REQUESTS,
        StatusCode::BAD_REQUEST,
    ] {
        let (base, _) = spawn_capture_mock(
            status,
            vec![
                ("content-type", "application/json; charset=utf-8"),
                ("anthropic-ratelimit-unified-5h-utilization", "0.42"),
                ("request-id", "req_raw_123"),
                ("x-should-retry", "false"),
                ("retry-after", "17"),
            ],
            raw_err,
        )
        .await;
        let state = default_official_state(base);
        let req = Request::builder()
            .method("POST")
            .uri("/v1/messages")
            .header("authorization", "Bearer test-oat")
            .body(Body::from(r#"{"model":"claude-opus-5-5","messages":[]}"#))
            .unwrap();
        let resp = app(state).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), status);
        assert_eq!(
            resp.headers()
                .get("anthropic-ratelimit-unified-5h-utilization")
                .unwrap(),
            "0.42"
        );
        assert_eq!(resp.headers().get("request-id").unwrap(), "req_raw_123");
        assert_eq!(resp.headers().get("x-should-retry").unwrap(), "false");
        assert_eq!(resp.headers().get("retry-after").unwrap(), "17");
        let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(body.as_ref(), raw_err);
    }
}

/// 测试 8:含 : ping 且缺 message_stop 结束的 SSE 逐字节透传,无 keepalive 也无 timeout_error
#[tokio::test]
async fn test_08_sse_ping_and_eof_without_message_stop_byte_identical() {
    let raw_sse: &'static [u8] =
        b"event: message_start\ndata: {\"type\":\"message_start\"}\n\n: ping\n\nevent: ping\ndata: {\"type\":\"ping\"}\n\n";
    let (base, _) = spawn_capture_mock(
        StatusCode::OK,
        vec![("content-type", "text/event-stream")],
        raw_sse,
    )
    .await;
    let state = default_official_state(base);
    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("authorization", "Bearer test-oat")
        .body(Body::from(
            r#"{"model":"claude-opus-5-5","stream":true,"messages":[]}"#,
        ))
        .unwrap();
    let resp = app(state).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(body.as_ref(), raw_sse);
    let text = std::str::from_utf8(&body).unwrap();
    assert!(!text.contains("keepalive"));
    assert!(!text.contains("timeout_error"));
}

/// 测试 9:上游 302 不跟随,原样回传给客户端
#[tokio::test]
async fn test_09_redirect_302_not_followed() {
    let (base, cap) = spawn_capture_mock(
        StatusCode::FOUND,
        vec![("location", "https://evil.example/steal")],
        b"redirecting",
    )
    .await;
    let state = default_official_state(base);
    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("authorization", "Bearer test-oat")
        .body(Body::from(r#"{"model":"claude-opus-5-5","messages":[]}"#))
        .unwrap();
    let resp = app(state).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::FOUND);
    assert_eq!(
        resp.headers().get("location").unwrap(),
        "https://evil.example/steal"
    );
    assert_eq!(cap.lock().unwrap().len(), 1);
}

/// 测试 10:count_tokens 满足 1-9(透传、不补 beta、原始体、无 UA、剔 key 与 AE 原样、本地 401、响应头/错误体、302 不跟)且不触碰 fp 池
#[tokio::test]
async fn test_10_count_tokens_passthrough_and_zero_fp_clients() {
    let raw_resp: &'static [u8] = br#"{"input_tokens":123,"extra":"keep"}"#;
    let (base, cap) = spawn_capture_mock(
        StatusCode::OK,
        vec![
            ("content-type", "application/json"),
            ("request-id", "req_ct_1"),
            ("anthropic-ratelimit-unified-5h-utilization", "0.11"),
        ],
        raw_resp,
    )
    .await;
    let state = default_official_state(base);

    // 缺 Authorization 本地 401
    let req_unauth = Request::builder()
        .method("POST")
        .uri("/v1/messages/count_tokens")
        .body(Body::from(r#"{"model":"claude-opus-5-5","messages":[]}"#))
        .unwrap();
    assert_eq!(
        app(state.clone())
            .oneshot(req_unauth)
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert!(cap.lock().unwrap().is_empty());

    // 正常透传
    let raw_body = b"{\n  \"z\": 1,\n  \"model\": \"claude-opus-5-5\",\n  \"messages\": []\n}";
    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages/count_tokens?beta=true")
        .header("authorization", "Bearer test-ct-oat")
        .header("anthropic-beta", "token-counting-2024-11-01")
        .header("anthropic-version", "2023-06-01")
        .header("x-api-key", "strip-me")
        .header("x-ccextra-key", "strip-me-2")
        .header("accept-encoding", CLAUDE_ACCEPT_ENCODING)
        .body(Body::from(Bytes::from_static(raw_body)))
        .unwrap();
    let resp = app(state.clone()).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers().get("request-id").unwrap(), "req_ct_1");
    assert_eq!(
        resp.headers()
            .get("anthropic-ratelimit-unified-5h-utilization")
            .unwrap(),
        "0.11"
    );
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(body.as_ref(), raw_resp);

    {
        let items = cap.lock().unwrap();
        assert_eq!(items.len(), 1);
        assert_eq!(
            items[0].path_and_query,
            "/v1/messages/count_tokens?beta=true"
        );
        assert_eq!(items[0].body.as_ref(), raw_body);
        assert_eq!(
            items[0].headers.get("authorization").unwrap(),
            "Bearer test-ct-oat"
        );
        assert_eq!(
            items[0].headers.get("anthropic-beta").unwrap(),
            "token-counting-2024-11-01"
        );
        assert!(items[0].headers.get("user-agent").is_none());
        assert!(items[0].headers.get("x-api-key").is_none());
        assert!(items[0].headers.get("x-ccextra-key").is_none());
        assert_eq!(
            items[0].headers.get("accept-encoding").unwrap(),
            CLAUDE_ACCEPT_ENCODING
        );
    }
    let upstream = &state.config.read().await.runtime.upstream;
    assert_eq!(upstream.fp_client_count(), 0);
    assert_eq!(upstream.official_client_count(), 1);

    // 302 不跟随
    let (base302, cap302) = spawn_capture_mock(
        StatusCode::FOUND,
        vec![("location", "https://evil.example/ct")],
        b"302",
    )
    .await;
    let state302 = default_official_state(base302);
    let req302 = Request::builder()
        .method("POST")
        .uri("/v1/messages/count_tokens")
        .header("authorization", "Bearer test-ct-oat")
        .body(Body::from(r#"{"model":"claude-opus-5-5","messages":[]}"#))
        .unwrap();
    assert_eq!(
        app(state302).oneshot(req302).await.unwrap().status(),
        StatusCode::FOUND
    );
    assert_eq!(cap302.lock().unwrap().len(), 1);
}

/// 测试 11:官方请求不触发 apply_payload_overrides(含显式 protocol: claude 与无协议通配规则)
#[tokio::test]
async fn test_11_payload_overrides_never_modify_official_body() {
    let (base, cap) = spawn_capture_mock(StatusCode::OK, vec![], b"{}").await;
    let rules = vec![
        PayloadRule {
            models: vec!["*".into()],
            protocol: Some(Protocol::Claude),
            params: [("temperature".into(), serde_json::json!(0.99))]
                .into_iter()
                .collect(),
            matchers: vec![],
        }
        .compiled(),
        PayloadRule {
            models: vec!["*".into()],
            protocol: None,
            params: [("max_tokens".into(), serde_json::json!(123))]
                .into_iter()
                .collect(),
            matchers: vec![],
        }
        .compiled(),
    ];
    let state = build_state(
        vec![official_provider(
            base,
            vec![model_exact("claude-opus-5-5")],
        )],
        None,
        rules,
    );
    let raw = br#"{ "model": "claude-opus-5-5", "messages": [] }"#;
    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("authorization", "Bearer test-oat")
        .body(Body::from(Bytes::from_static(raw)))
        .unwrap();
    let resp = app(state).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(cap.lock().unwrap()[0].body.as_ref(), raw);
}

/// 测试 12 & 25:设 secret_key 时 sk-ant- 直接 401 且不进 AUTH_CACHE;不设 secret_key 时正常转发
#[tokio::test]
async fn test_12_and_25_sk_ant_secret_guard_and_no_secret_passthrough() {
    let (base, cap) = spawn_capture_mock(StatusCode::OK, vec![], br#"{"ok":1}"#).await;

    // 25: 不设 secret_key 的官方实例收到 sk-ant-oat-test → 正常转发
    let no_secret_state = default_official_state(base.clone());
    let req_ok = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("authorization", "Bearer sk-ant-oat-test")
        .body(Body::from(r#"{"model":"claude-opus-5-5","messages":[]}"#))
        .unwrap();
    assert_eq!(
        app(no_secret_state).oneshot(req_ok).await.unwrap().status(),
        StatusCode::OK
    );
    assert_eq!(cap.lock().unwrap().len(), 1);

    // 12: 设 secret_key(bcrypt) 时 sk-ant-oat... → 401 且不写 AUTH_CACHE
    let hash = bcrypt::hash("sk-ccextra-real-secret", 4).unwrap();
    let with_secret_state = build_state(
        vec![official_provider(
            base,
            vec![model_exact("claude-opus-5-5")],
        )],
        Some(hash.clone()),
        vec![],
    );
    let oat = "sk-ant-oat01-never-cache-me";
    let req_deny = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("authorization", format!("Bearer {oat}"))
        .body(Body::from(r#"{"model":"claude-opus-5-5","messages":[]}"#))
        .unwrap();
    assert_eq!(
        app(with_secret_state)
            .oneshot(req_deny)
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    let forbidden_cache_key = format!("{hash}\u{0}{oat}");
    assert!(
        !auth_cache().lock().unwrap().contains(&forbidden_cache_key),
        "sk-ant- 订阅 token 不得写入 AUTH_CACHE"
    );
    assert_eq!(cap.lock().unwrap().len(), 1);
}

/// 测试 14:探针 JSON 转义、header_wait 映射与 AccessFields::for_official
#[tokio::test]
async fn test_14_probe_json_escape_header_wait_and_access_fields() {
    let (base, cap) = spawn_capture_mock(StatusCode::OK, vec![], br#"{"ok":true}"#).await;
    let state = default_official_state(base);

    // \u0063 = 'c'
    let escaped = br#"{"model":"\u0063laude-opus-5-5","stream":true,"messages":[]}"#;
    let probe = probe_inbound_body(escaped).unwrap();
    assert_eq!(probe.model, "claude-opus-5-5");
    assert!(probe.stream);

    // false / 缺省 / null 均判非流
    assert!(
        !probe_inbound_body(br#"{"model":"m","stream":false}"#)
            .unwrap()
            .stream
    );
    assert!(!probe_inbound_body(br#"{"model":"m"}"#).unwrap().stream);
    assert!(
        !probe_inbound_body(br#"{"model":"m","stream":null}"#)
            .unwrap()
            .stream
    );

    // header_wait 映射
    assert_eq!(messages_header_wait(true), crate::upstream::SEND_TIMEOUT);
    assert_eq!(
        messages_header_wait(false),
        crate::upstream::NON_STREAM_SEND_TIMEOUT
    );
    assert_eq!(
        crate::upstream::NON_STREAM_SEND_TIMEOUT,
        Duration::from_secs(600)
    );

    // access log 行格式包含 stream=true 与 model/provider
    let fields = crate::http::access_log::AccessFields::for_official(
        "claude-opus-5-5",
        "anthropic-official",
        "claude-opus-5-5",
        true,
    );
    let line = crate::http::access_log::access_line(Some(&fields), 200, 12, 34);
    assert!(line.contains("model=claude-opus-5-5"), "{line}");
    assert!(line.contains("provider=anthropic-official"), "{line}");
    assert!(line.contains("protocol=AnthropicOfficial"), "{line}");
    assert!(line.contains("stream=true"), "{line}");

    // 端到端路由成功且原样转发转义字节
    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("authorization", "Bearer test-oat")
        .body(Body::from(Bytes::from_static(escaped)))
        .unwrap();
    assert_eq!(
        app(state).oneshot(req).await.unwrap().status(),
        StatusCode::OK
    );
    assert_eq!(cap.lock().unwrap()[0].body.as_ref(), escaped);
}

/// 测试 15:通配命中官方透传且 GET /v1/models 不含 claude-*
#[tokio::test]
async fn test_15_wildcard_snapshot_passthrough_and_models_list_skips_wildcard() {
    let (base, cap) = spawn_capture_mock(StatusCode::OK, vec![], br#"{"ok":true}"#).await;
    let state = default_official_state(base);

    let raw = br#"{  "model": "claude-haiku-4-5-20251001", "messages": [] }"#;
    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("authorization", "Bearer test-oat")
        .body(Body::from(Bytes::from_static(raw)))
        .unwrap();
    assert_eq!(
        app(state.clone()).oneshot(req).await.unwrap().status(),
        StatusCode::OK
    );
    assert_eq!(cap.lock().unwrap()[0].body.as_ref(), raw);

    let models_req = Request::builder()
        .method("GET")
        .uri("/v1/models")
        .body(Body::empty())
        .unwrap();
    let models_resp = app(state).oneshot(models_req).await.unwrap();
    assert_eq!(models_resp.status(), StatusCode::OK);
    let bytes = axum::body::to_bytes(models_resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    let ids: Vec<&str> = json["data"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["id"].as_str().unwrap())
        .collect();
    assert_eq!(ids, vec!["claude-opus-5-5"]);
    assert!(!ids.contains(&"claude-*"));
}

/// 测试 16:入站非 identity 或非 UTF-8 Content-Encoding 在读 body 之前返回 415(invalid_request_error);identity 与缺失放行
#[tokio::test]
async fn test_16_inbound_content_encoding_415_before_body_read() {
    let (base, cap) = spawn_capture_mock(StatusCode::OK, vec![], b"{}").await;
    let state = default_official_state(base);

    let bad_values = [
        HeaderValue::from_static("gzip"),
        HeaderValue::from_static("GZIP"),
        HeaderValue::from_static("identity, gzip"),
        HeaderValue::from_bytes(&[0xff, 0xfe]).unwrap(),
    ];
    for val in bad_values {
        for uri in ["/v1/messages", "/v1/messages/count_tokens"] {
            let body_polled = Arc::new(AtomicBool::new(false));
            let polled_clone = Arc::clone(&body_polled);
            let stream = futures::stream::poll_fn(move |_cx| {
                polled_clone.store(true, Ordering::SeqCst);
                std::task::Poll::Ready(None::<Result<Bytes, std::io::Error>>)
            });
            let req = Request::builder()
                .method("POST")
                .uri(uri)
                .header("authorization", "Bearer test-oat")
                .header("content-encoding", val.clone())
                .body(Body::from_stream(stream))
                .unwrap();
            let resp = app(state.clone()).oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::UNSUPPORTED_MEDIA_TYPE);
            assert!(
                !body_polled.load(Ordering::SeqCst),
                "415 必须在 read_inbound_body 之前触发"
            );
            let bytes = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            let json: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
            assert_eq!(json["error"]["type"], "invalid_request_error");
        }
    }
    assert!(cap.lock().unwrap().is_empty());

    // identity / IDENTITY / 无该头放行
    for ok_ce in [None, Some("identity"), Some("IDENTITY")] {
        let mut builder = Request::builder()
            .method("POST")
            .uri("/v1/messages")
            .header("authorization", "Bearer test-oat");
        if let Some(ce) = ok_ce {
            builder = builder.header("content-encoding", ce);
        }
        let req = builder
            .body(Body::from(r#"{"model":"claude-opus-5-5","messages":[]}"#))
            .unwrap();
        assert_eq!(
            app(state.clone()).oneshot(req).await.unwrap().status(),
            StatusCode::OK
        );
    }
    assert_eq!(cap.lock().unwrap().len(), 3);
}

/// 测试 17:请求/响应双向剔除静动态 hop-by-hop 头(含 proxy-connection/http2-settings/Connection 动态头)与响应 set-cookie,且静态表为排除集子集
#[tokio::test]
async fn test_17_hop_by_hop_both_directions_and_subset_invariant() {
    let static_hops = [
        "connection",
        "keep-alive",
        "proxy-connection",
        "proxy-authenticate",
        "proxy-authorization",
        "te",
        "trailer",
        "transfer-encoding",
        "upgrade",
        "http2-settings",
    ];
    for h in static_hops {
        assert!(is_static_hop_by_hop(h), "{h} 应在静态 hop-by-hop 表内");
        assert!(
            is_claude_passthrough_header_excluded(h, &[]),
            "{h} 应同时在 claude_relay 排除集内,防止漂移"
        );
    }

    // 请求方向单元测试(含全部 10 项静态 hop-by-hop + Connection 动态头)
    let mut inbound = HeaderMap::new();
    inbound.insert("authorization", HeaderValue::from_static("Bearer oat"));
    inbound.insert(
        "connection",
        HeaderValue::from_static("x-req-hop, keep-alive"),
    );
    inbound.insert("x-req-hop", HeaderValue::from_static("drop-me"));
    inbound.insert("x-keep", HeaderValue::from_static("stay"));
    for h in static_hops {
        if h != "connection" {
            inbound.insert(
                axum::http::HeaderName::from_bytes(h.as_bytes()).unwrap(),
                HeaderValue::from_static("v"),
            );
        }
    }
    let out_req = official_request_headers(&inbound);
    for h in static_hops {
        assert!(out_req.get(h).is_none(), "请求方向应剔除 {h}");
    }
    assert!(out_req.get("x-req-hop").is_none());
    assert_eq!(out_req.get("x-keep").unwrap(), "stay");

    // 响应方向端到端 + 单元(Connection: x-foo + set-cookie + 全部静态 hop-by-hop)
    let (base, _) = spawn_capture_mock(
        StatusCode::OK,
        vec![
            ("connection", "x-foo"),
            ("x-foo", "1"),
            ("set-cookie", "sid=secret"),
            ("x-keep-resp", "ok"),
        ],
        b"{}",
    )
    .await;
    let state = default_official_state(base);
    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("authorization", "Bearer test-oat")
        .body(Body::from(r#"{"model":"claude-opus-5-5","messages":[]}"#))
        .unwrap();
    let resp = app(state).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert!(resp.headers().get("x-foo").is_none());
    assert!(resp.headers().get("set-cookie").is_none());
    assert_eq!(resp.headers().get("x-keep-resp").unwrap(), "ok");

    let mut up = HeaderMap::new();
    for h in static_hops {
        up.insert(
            axum::http::HeaderName::from_bytes(h.as_bytes()).unwrap(),
            HeaderValue::from_static("v"),
        );
    }
    let mut dst = HeaderMap::new();
    forward_response_headers(&up, &mut dst);
    for h in static_hops {
        assert!(dst.get(h).is_none(), "响应方向应剔除 {h}");
    }
}

/// 测试 18:上游 gzip 响应透明解压并移除 content-encoding(入站带 AE 与缺 AE 均如此);未知编码 foo 原样保留字节与头
#[tokio::test]
async fn test_18_response_gzip_decompressed_and_unknown_encoding_preserved() {
    let plain = b"event: message_start\ndata: {\"ok\":true}\n\n";
    let mut enc = GzEncoder::new(Vec::new(), Compression::default());
    enc.write_all(plain).unwrap();
    let gz_bytes = Bytes::from(enc.finish().unwrap());
    let gz_for_route = gz_bytes.clone();

    let unknown_bytes = Bytes::from_static(b"opaque-foo-compressed-bytes");
    let unk_for_route = unknown_bytes.clone();

    let router = Router::new().route(
        "/v1/messages",
        post(move |headers: HeaderMap| {
            let gz = gz_for_route.clone();
            let unk = unk_for_route.clone();
            async move {
                if headers.get("x-case").and_then(|v| v.to_str().ok()) == Some("unknown") {
                    Response::builder()
                        .status(StatusCode::OK)
                        .header("content-encoding", "foo")
                        .header("content-type", "application/json")
                        .body(Body::from(unk))
                        .unwrap()
                } else {
                    Response::builder()
                        .status(StatusCode::OK)
                        .header("content-encoding", "gzip")
                        .header("content-type", "text/event-stream")
                        .body(Body::from(gz))
                        .unwrap()
                }
            }
        }),
    );
    let base = spawn_mock(router).await;
    let state = default_official_state(base);

    // gzip 响应:解压且无 content-encoding;入站 AE 由客户端带入(原样转发)时 wreq 仍解压
    for inbound_ae in [None, Some(CLAUDE_ACCEPT_ENCODING)] {
        let mut req_gz = Request::builder()
            .method("POST")
            .uri("/v1/messages")
            .header("authorization", "Bearer test-oat");
        if let Some(ae) = inbound_ae {
            req_gz = req_gz.header("accept-encoding", ae);
        }
        let req_gz = req_gz
            .body(Body::from(r#"{"model":"claude-opus-5-5","stream":true}"#))
            .unwrap();
        let resp_gz = app(state.clone()).oneshot(req_gz).await.unwrap();
        assert_eq!(resp_gz.status(), StatusCode::OK);
        assert!(
            resp_gz.headers().get("content-encoding").is_none(),
            "inbound AE {inbound_ae:?}"
        );
        assert!(resp_gz.headers().get("content-length").is_none());
        let out_gz = axum::body::to_bytes(resp_gz.into_body(), usize::MAX)
            .await
            .unwrap();
        assert_eq!(out_gz.as_ref(), plain, "inbound AE {inbound_ae:?}");
    }

    // 未知编码 foo:保留头与原始字节
    let req_unk = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("authorization", "Bearer test-oat")
        .header("x-case", "unknown")
        .body(Body::from(r#"{"model":"claude-opus-5-5"}"#))
        .unwrap();
    let resp_unk = app(state).oneshot(req_unk).await.unwrap();
    assert_eq!(resp_unk.status(), StatusCode::OK);
    assert_eq!(resp_unk.headers().get("content-encoding").unwrap(), "foo");
    let out_unk = axum::body::to_bytes(resp_unk.into_body(), usize::MAX)
        .await
        .unwrap();
    assert_eq!(out_unk, unknown_bytes);
}

/// 测试 21:探针回落与重复键(null/非布尔 stream、重复 model/stream 在 messages 与 count_tokens 均正常转发;非法 JSON/缺 model 回落 4xx)
#[tokio::test]
async fn test_21_probe_fallback_non_bool_stream_and_duplicate_keys() {
    let (base, cap) = spawn_capture_mock(StatusCode::OK, vec![], br#"{"input_tokens":77}"#).await;
    let state = default_official_state(base);

    let cases: [&[u8]; 3] = [
        br#"{"model":"claude-opus-5-5","stream":null,"messages":[]}"#,
        br#"{"model":"claude-opus-5-5","stream":"yes","messages":[]}"#,
        br#"{"model":"bad","model":"claude-opus-5-5","stream":true,"stream":false,"messages":[]}"#,
    ];
    for body in cases {
        let p = probe_inbound_body(body).unwrap();
        assert_eq!(p.model, "claude-opus-5-5");
        assert!(!p.stream);

        for uri in ["/v1/messages", "/v1/messages/count_tokens"] {
            let req = Request::builder()
                .method("POST")
                .uri(uri)
                .header("authorization", "Bearer test-oat")
                .body(Body::from(Bytes::from_static(body)))
                .unwrap();
            let resp = app(state.clone()).oneshot(req).await.unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            let out = axum::body::to_bytes(resp.into_body(), usize::MAX)
                .await
                .unwrap();
            assert_eq!(out.as_ref(), br#"{"input_tokens":77}"#);
        }
    }
    assert_eq!(cap.lock().unwrap().len(), 6);

    // body 不是 JSON 或缺 model:回落旧路径返回 4xx(非 500)
    for bad in [b"not-json".as_slice(), br#"{"messages":[]}"#.as_slice()] {
        let req = Request::builder()
            .method("POST")
            .uri("/v1/messages")
            .header("authorization", "Bearer test-oat")
            .body(Body::from(Bytes::from_static(bad)))
            .unwrap();
        let resp = app(state.clone()).oneshot(req).await.unwrap();
        assert_eq!(resp.status(), StatusCode::BAD_REQUEST);
    }
    let req_ct_missing = Request::builder()
        .method("POST")
        .uri("/v1/messages/count_tokens")
        .header("authorization", "Bearer test-oat")
        .body(Body::from(r#"{"messages":[]}"#))
        .unwrap();
    assert_eq!(
        app(state).oneshot(req_ct_missing).await.unwrap().status(),
        StatusCode::BAD_REQUEST
    );
}

/// 测试 22 & 23:无官方 provider 时不影响 8222(不 415);通配边界(claude-*/claude-/claude-** 404,未来快照命中)
#[tokio::test]
async fn test_22_and_23_no_official_non_regression_and_wildcard_http_boundaries() {
    // 22: 无官方 provider 的实例收到 Content-Encoding: gzip 仍走原有逻辑(400 解析失败,非 415)
    let relay_only = build_state(
        vec![ProviderConfig::new(
            "relay".into(),
            Protocol::Claude,
            vec!["https://relay.invalid".into()],
            "sk-relay".into(),
            Some("direct".into()),
            false,
            vec![model_exact("claude-opus-5-5")],
        )],
        None,
        vec![],
    );
    let req_gz = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("content-encoding", "gzip")
        .body(Body::from(Bytes::from_static(b"\x1f\x8b\x08\x00invalid")))
        .unwrap();
    let resp_gz = app(relay_only).oneshot(req_gz).await.unwrap();
    assert_eq!(resp_gz.status(), StatusCode::BAD_REQUEST);

    // 23: 通配边界
    let (base, cap) = spawn_capture_mock(StatusCode::OK, vec![], br#"{"ok":true}"#).await;
    let state = default_official_state(base);
    for bad_model in ["claude-*", "claude-", "claude-**"] {
        let body = format!(r#"{{"model":"{bad_model}","messages":[]}}"#);
        let req = Request::builder()
            .method("POST")
            .uri("/v1/messages")
            .header("authorization", "Bearer test-oat")
            .body(Body::from(body))
            .unwrap();
        assert_eq!(
            app(state.clone()).oneshot(req).await.unwrap().status(),
            StatusCode::NOT_FOUND
        );
    }
    assert!(cap.lock().unwrap().is_empty());

    let raw_future = br#"{ "model": "claude-sonnet-9-9-20300101", "messages": [] }"#;
    let req_ok = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("authorization", "Bearer test-oat")
        .body(Body::from(Bytes::from_static(raw_future)))
        .unwrap();
    assert_eq!(
        app(state).oneshot(req_ok).await.unwrap().status(),
        StatusCode::OK
    );
    assert_eq!(cap.lock().unwrap()[0].body.as_ref(), raw_future);
}

/// 测试 24:本地错误映射(端口关闭 → 502 api_error;响应头超时注入短 wait → 408 timeout_error;生产等待上限核对)
#[tokio::test]
async fn test_24_local_error_mapping_502_408_and_wait_helpers() {
    assert_eq!(messages_header_wait(true), crate::upstream::SEND_TIMEOUT);
    assert_eq!(
        messages_header_wait(false),
        crate::upstream::NON_STREAM_SEND_TIMEOUT
    );
    assert_eq!(count_tokens_header_wait(), crate::upstream::SEND_TIMEOUT);

    // 端口关闭 → 502 api_error
    let unused = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let closed_addr = unused.local_addr().unwrap();
    drop(unused);
    let closed_state = default_official_state(format!("http://{closed_addr}"));
    let req_502 = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("authorization", "Bearer test-oat")
        .body(Body::from(r#"{"model":"claude-opus-5-5","messages":[]}"#))
        .unwrap();
    let resp_502 = app(closed_state).oneshot(req_502).await.unwrap();
    assert_eq!(resp_502.status(), StatusCode::BAD_GATEWAY);
    let j502: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(resp_502.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(j502["error"]["type"], "api_error");

    // 挂起不回响应头 + 注入 100ms wait → 408 timeout_error
    let hang_router = Router::new().route(
        "/v1/messages",
        post(|| async {
            tokio::time::sleep(Duration::from_secs(5)).await;
            StatusCode::OK
        }),
    );
    let hang_base = spawn_mock(hang_router).await;
    let hang_state = default_official_state(hang_base);
    let snapshot = hang_state.config.read().await.clone();
    let route = ccextra_core::route::resolve_route("claude-opus-5-5", &snapshot.providers).unwrap();
    let mut headers = HeaderMap::new();
    headers.insert("authorization", HeaderValue::from_static("Bearer test-oat"));
    let err = relay_official_with_timeout(
        &snapshot,
        &route,
        OfficialRelayParams {
            path: "/v1/messages",
            inbound_headers: &headers,
            query: None,
            body_bytes: Bytes::from_static(br#"{"model":"claude-opus-5-5","messages":[]}"#),
            wait: Duration::from_millis(100),
            log_timing: false,
        },
    )
    .await
    .unwrap_err();
    assert_eq!(err.status, StatusCode::REQUEST_TIMEOUT);
    let resp_408 = axum::response::IntoResponse::into_response(err);
    let j408: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(resp_408.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(j408["error"]["type"], "timeout_error");
}

/// 测试 26:base_url 带或不带末尾 / 拼接 path 均为 /v1/messages(无 //),且保留 query
#[tokio::test]
async fn test_26_base_url_trailing_slash_join_no_double_slash() {
    let (base, cap) = spawn_capture_mock(StatusCode::OK, vec![], b"{}").await;
    for url in [format!("{base}/"), base] {
        let state = default_official_state(url);
        let req = Request::builder()
            .method("POST")
            .uri("/v1/messages?beta=true")
            .header("authorization", "Bearer test-oat")
            .body(Body::from(r#"{"model":"claude-opus-5-5","messages":[]}"#))
            .unwrap();
        assert_eq!(
            app(state).oneshot(req).await.unwrap().status(),
            StatusCode::OK
        );
    }
    let items = cap.lock().unwrap();
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].path_and_query, "/v1/messages?beta=true");
    assert_eq!(items[1].path_and_query, "/v1/messages?beta=true");
}

/// 测试 27:Accept 与 Content-Type 原样透传;缺 Accept 原样不发(不补 */*,也不强写 application/json)
#[tokio::test]
async fn test_27_accept_and_content_type_verbatim_without_scrub() {
    let (base, cap) = spawn_capture_mock(StatusCode::OK, vec![], b"{}").await;
    let state = default_official_state(base);

    let req1 = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("authorization", "Bearer test-oat")
        .header("accept", "text/event-stream")
        .header("content-type", "application/json; charset=utf-8")
        .body(Body::from(r#"{"model":"claude-opus-5-5","messages":[]}"#))
        .unwrap();
    assert_eq!(
        app(state.clone()).oneshot(req1).await.unwrap().status(),
        StatusCode::OK
    );

    // 无 Accept、无 Content-Type
    let req2 = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("authorization", "Bearer test-oat")
        .body(Body::from(r#"{"model":"claude-opus-5-5","messages":[]}"#))
        .unwrap();
    assert_eq!(
        app(state).oneshot(req2).await.unwrap().status(),
        StatusCode::OK
    );

    let items = cap.lock().unwrap();
    assert_eq!(items[0].headers.get("accept").unwrap(), "text/event-stream");
    assert_eq!(
        items[0].headers.get("content-type").unwrap(),
        "application/json; charset=utf-8"
    );
    let accept2 = items[1].headers.get("accept").and_then(|v| v.to_str().ok());
    // wreq 无默认头:缺 Accept 时原样不发(不补 */*,也不强写 application/json)
    assert_eq!(accept2, None, "缺 Accept 时不得补写,实际: {accept2:?}");
    assert_eq!(
        items[1].headers.get("content-type").unwrap(),
        "application/json"
    );
}

/// 测试 28:不做 stale 连接重试,不调 drop_pooled_client(读请求后断连 → 502 api_error,mock 仅收到 1 次,池计数不变)
#[tokio::test]
async fn test_28_no_stale_retry_and_no_pool_eviction() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let fail_hits = Arc::new(AtomicUsize::new(0));
    let fail_hits_srv = Arc::clone(&fail_hits);
    tokio::spawn(async move {
        let mut total = 0usize;
        while let Ok((mut sock, _)) = listener.accept().await {
            total += 1;
            let mut buf = [0u8; 4096];
            let mut read = 0usize;
            while read < buf.len() {
                match sock.read(&mut buf[read..]).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        read += n;
                        if buf[..read].windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                }
            }
            if total == 1 {
                // 预热第 1 次,使 official_clients 缓存建立
                let _ = sock
                    .write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\n{}",
                    )
                    .await;
                let _ = sock.shutdown().await;
            } else {
                // 收到请求后发响应头前直接断开
                fail_hits_srv.fetch_add(1, Ordering::SeqCst);
                drop(sock);
            }
        }
    });

    let state = default_official_state(format!("http://{addr}"));
    let make_req = || {
        Request::builder()
            .method("POST")
            .uri("/v1/messages")
            .header("authorization", "Bearer test-oat")
            .body(Body::from(r#"{"model":"claude-opus-5-5","messages":[]}"#))
            .unwrap()
    };
    assert_eq!(
        app(state.clone())
            .oneshot(make_req())
            .await
            .unwrap()
            .status(),
        StatusCode::OK
    );
    let upstream = state.config.read().await.runtime.upstream.clone();
    assert_eq!(upstream.official_client_count(), 1);

    let resp = app(state).oneshot(make_req()).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    let body: serde_json::Value = serde_json::from_slice(
        &axum::body::to_bytes(resp.into_body(), usize::MAX)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(body["error"]["type"], "api_error");
    assert_eq!(fail_hits.load(Ordering::SeqCst), 1, "不得触发 stale 重试");
    assert_eq!(upstream.official_client_count(), 1, "不得丢池重建");
}

/// 测试 29:流式停顿超过 SEND_TIMEOUT(3s)不被掐;上游在 message_stop 前直接断流时结束连接且不补任何事件
#[tokio::test]
async fn test_29_streaming_pause_over_send_timeout_and_abrupt_drop_without_synthetic_events() {
    // 29a: 流首帧后停顿 3.2s(> #[cfg(test)] SEND_TIMEOUT=3s),再发 ping 与 message_stop
    let part1: &'static [u8] = b"event: message_start\ndata: {\"type\":\"message_start\"}\n\n";
    let part2: &'static [u8] =
        b": ping\n\nevent: message_stop\ndata: {\"type\":\"message_stop\"}\n\n";
    let router = Router::new().route(
        "/v1/messages",
        post(move || async move {
            let stream = async_stream::stream! {
                yield Ok::<Bytes, std::io::Error>(Bytes::from_static(part1));
                tokio::time::sleep(Duration::from_millis(3200)).await;
                yield Ok::<Bytes, std::io::Error>(Bytes::from_static(part2));
            };
            Response::builder()
                .status(StatusCode::OK)
                .header("content-type", "text/event-stream")
                .body(Body::from_stream(stream))
                .unwrap()
        }),
    );
    let base = spawn_mock(router).await;
    let state = default_official_state(base);
    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("authorization", "Bearer test-oat")
        .body(Body::from(
            r#"{"model":"claude-opus-5-5","stream":true,"messages":[]}"#,
        ))
        .unwrap();
    let resp = app(state).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = axum::body::to_bytes(resp.into_body(), usize::MAX)
        .await
        .unwrap();
    let mut expected = Vec::new();
    expected.extend_from_slice(part1);
    expected.extend_from_slice(part2);
    assert_eq!(body.as_ref(), expected.as_slice());
    assert!(
        !std::str::from_utf8(&body)
            .unwrap()
            .contains("timeout_error")
    );

    // 29b: 上游在 message_stop 前直接断开 chunked TCP 连接 → 只收到已发字节,不补任何事件
    let abrupt_part: &'static [u8] =
        b"event: content_block_start\ndata: {\"type\":\"content_block_start\"}\n\n";
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        if let Ok((mut sock, _)) = listener.accept().await {
            let mut buf = [0u8; 4096];
            let mut read = 0usize;
            while read < buf.len() {
                match sock.read(&mut buf[read..]).await {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        read += n;
                        if buf[..read].windows(4).any(|w| w == b"\r\n\r\n") {
                            break;
                        }
                    }
                }
            }
            let head = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n{:x}\r\n",
                abrupt_part.len()
            );
            let _ = sock.write_all(head.as_bytes()).await;
            let _ = sock.write_all(abrupt_part).await;
            let _ = sock.write_all(b"\r\n").await;
            let _ = sock.flush().await;
            // 不发 0\r\n\r\n,直接断开连接
            drop(sock);
        }
    });

    let state_drop = default_official_state(format!("http://{addr}"));
    let req_drop = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("authorization", "Bearer test-oat")
        .body(Body::from(
            r#"{"model":"claude-opus-5-5","stream":true,"messages":[]}"#,
        ))
        .unwrap();
    let resp_drop = app(state_drop).oneshot(req_drop).await.unwrap();
    assert_eq!(resp_drop.status(), StatusCode::OK);
    let mut collected = Vec::new();
    let mut stream = resp_drop.into_body().into_data_stream();
    while let Some(item) = stream.next().await {
        match item {
            Ok(chunk) => collected.extend_from_slice(&chunk),
            Err(_) => break,
        }
    }
    assert_eq!(collected.as_slice(), abrupt_part);
    let text = std::str::from_utf8(&collected).unwrap();
    assert!(!text.contains("timeout_error"));
    assert!(!text.contains("message_stop"));
    assert!(!text.contains("keepalive"));
}

/// 测试 30:非 claude 精确条目(name: foo, alias: foo)不调用 sanitize_passthrough_prompt,body 逐字节不变
#[tokio::test]
async fn test_30_non_claude_exact_official_model_not_sanitized() {
    let (base, cap) = spawn_capture_mock(StatusCode::OK, vec![], br#"{"ok":true}"#).await;
    let state = build_state(
        vec![official_provider(base, vec![model_exact("foo")])],
        None,
        vec![],
    );
    let raw_body = br#"{
  "model": "foo",
  "system": [
    {"type": "text", "text": "x-anthropic-billing-header: fp=abc123"},
    {"type": "text", "text": "<identity>You are Claude</identity>\n<rules>keep verbatim</rules>"}
  ],
  "messages": [
    {"role": "system", "content": "x-anthropic-billing-header: fp=xyz\n<identity>Claude</identity>"},
    {"role": "user", "content": "hello"}
  ]
}"#;
    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("authorization", "Bearer test-oat")
        .body(Body::from(Bytes::from_static(raw_body)))
        .unwrap();
    let resp = app(state).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(cap.lock().unwrap()[0].body.as_ref(), raw_body);
}

/// 读第一个 TLS 记录里的 ClientHello,抽出 (cipher suites, 扩展类型序列, ALPN 列表原始字节)。
/// 指纹车道关闭了 GREASE 与扩展乱序,同一构建两次连接这三项应逐字节相同。
async fn capture_client_hello_shape(ln: TcpListener) -> (Vec<u8>, Vec<u16>, Vec<u8>) {
    let (mut sock, _) = ln.accept().await.unwrap();
    let mut raw = Vec::new();
    let mut buf = [0u8; 16384];
    loop {
        let n = sock.read(&mut buf).await.unwrap();
        assert!(n > 0, "peer closed before full ClientHello record");
        raw.extend_from_slice(&buf[..n]);
        if raw.len() >= 5 && raw.len() >= 5 + u16::from_be_bytes([raw[3], raw[4]]) as usize {
            break;
        }
    }
    assert_eq!(raw[0], 0x16, "not a TLS handshake record");
    let b = &raw[5 + 4..]; // 记录头 + 握手头
    let u16_at = |i: usize| u16::from_be_bytes([b[i], b[i + 1]]) as usize;
    let mut q = 34;
    q += 1 + b[q] as usize; // session id
    let n = u16_at(q);
    let ciphers = b[q + 2..q + 2 + n].to_vec();
    q += 2 + n;
    q += 1 + b[q] as usize; // compression
    let end = q + 2 + u16_at(q);
    q += 2;
    let (mut exts, mut alpn) = (Vec::new(), Vec::new());
    while q + 4 <= end {
        let t = u16_at(q) as u16;
        let l = u16_at(q + 2);
        if t == 16 {
            alpn = b[q + 4..q + 4 + l].to_vec();
        }
        exts.push(t);
        q += 4 + l;
    }
    (ciphers, exts, alpn)
}

/// 测试 31:官方透传出站 ClientHello 与 Claude 指纹车道构建一致(Bun 套件/扩展顺序,ALPN 仅 http/1.1),
/// 且只进 official_clients、不碰 fp_clients
#[tokio::test]
async fn test_31_official_uses_claude_fingerprint_client_hello() {
    let ln = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = ln.local_addr().unwrap().port();
    let cap = tokio::spawn(capture_client_hello_shape(ln));
    // 用主机名带 SNI;握手被 mock 读完 Hello 后断开,官方路径本地映射 502
    let state = default_official_state(format!("https://localhost:{port}"));
    let req = Request::builder()
        .method("POST")
        .uri("/v1/messages")
        .header("authorization", "Bearer test-oat")
        .body(Body::from(r#"{"model":"claude-opus-5-5","messages":[]}"#))
        .unwrap();
    let resp = app(state.clone()).oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_GATEWAY);
    let official = tokio::time::timeout(Duration::from_secs(10), cap)
        .await
        .expect("no ClientHello from official path")
        .unwrap();

    let ln = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let port = ln.local_addr().unwrap().port();
    let cap = tokio::spawn(capture_client_hello_shape(ln));
    let fp = crate::claude_tls_fp::build_fingerprint_client("direct", false).unwrap();
    let _ = tokio::time::timeout(
        Duration::from_secs(10),
        fp.get(format!("https://localhost:{port}/")).send(),
    )
    .await;
    let lane = tokio::time::timeout(Duration::from_secs(10), cap)
        .await
        .expect("no ClientHello from fingerprint lane")
        .unwrap();

    assert_eq!(
        official, lane,
        "官方透传 ClientHello 应与 Claude 指纹车道一致"
    );
    assert_eq!(official.2, b"\x00\x09\x08http/1.1", "ALPN 只报 http/1.1");
    let upstream = state.config.read().await.runtime.upstream.clone();
    assert_eq!(upstream.official_client_count(), 1);
    assert_eq!(upstream.fp_client_count(), 0);
}
