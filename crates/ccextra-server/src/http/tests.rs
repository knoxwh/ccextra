use super::*;
use crate::http::handlers::messages::*;
use crate::http::handlers::models::*;
use crate::upstream::UpstreamClient;
use axum::body::{to_bytes, Body};
use axum::http::{header, HeaderMap, Request, StatusCode};
use ccextra_core::route::{Protocol, ProviderConfig};
use serde_json::{json, Value};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex as StdMutex};
use tokio::sync::RwLock;
use tower::util::ServiceExt;

mod snapshot;

// 测试用默认 User-Agent 值
const TEST_CLAUDE_CLI: &str = "claude-cli/2.1.258";
const TEST_CODEX_TUI: &str = "codex_cli_rs/0.153.3 (Mac OS 26.6.2; arm64)";
const TEST_GROK_VERSION: &str = "1.0.46";
const TEST_ANTIGRAVITY: &str = "antigravity/hub/2.10.0 darwin/arm64";

fn test_user_agents() -> UserAgentSet {
    UserAgentSet {
        claude_cli: Arc::new(TEST_CLAUDE_CLI.to_string()),
        codex_tui: Arc::new(TEST_CODEX_TUI.to_string()),
        grok_version: Arc::new(TEST_GROK_VERSION.to_string()),
        antigravity: Arc::new(TEST_ANTIGRAVITY.to_string()),
    }
}

#[test]
fn upstream_log_stem_includes_request_sequence() {
    let first = upstream_log_stem("sessabcd", 123, 7, "openairesponses");
    let second = upstream_log_stem("sessabcd", 123, 8, "openairesponses");
    assert_ne!(first, second);
    assert_eq!(first, "sessabcd_123_7.openairesponses");
}

#[test]
fn inbound_headers_json_redacts_secrets_keeps_rest() {
    let mut headers = HeaderMap::new();
    headers.insert("x-api-key", "sk-live-secret".parse().unwrap());
    headers.insert("authorization", "Bearer abc".parse().unwrap());
    headers.insert("user-agent", "claude-cli/2.1.250".parse().unwrap());
    headers.insert("x-claude-code-session-id", "sess-1".parse().unwrap());
    headers.insert("anthropic-beta", "oauth-2025-04-20".parse().unwrap());
    let dumped = inbound_headers_json(&headers);
    assert_eq!(dumped["x-api-key"], "[redacted]");
    assert_eq!(dumped["authorization"], "[redacted]");
    assert_eq!(dumped["user-agent"], "claude-cli/2.1.250");
    assert_eq!(dumped["x-claude-code-session-id"], "sess-1");
    assert_eq!(dumped["anthropic-beta"], "oauth-2025-04-20");
}

#[test]
fn inbound_headers_json_keeps_duplicate_names() {
    let mut headers = HeaderMap::new();
    headers.append("x-extra", "one".parse().unwrap());
    headers.append("x-extra", "two".parse().unwrap());
    let dumped = inbound_headers_json(&headers);
    assert_eq!(dumped["x-extra"], json!(["one", "two"]));
}

fn headers_with(pairs: &[(&str, &str)]) -> HeaderMap {
    let mut h = HeaderMap::new();
    for (k, v) in pairs {
        h.insert(
            k.parse::<axum::http::header::HeaderName>().unwrap(),
            v.parse().unwrap(),
        );
    }
    h
}

fn relay_has_header_value(headers: &HeaderMap, name: &str, expected: &str) -> bool {
    headers
        .get_all(name)
        .iter()
        .any(|value| value.to_str().ok() == Some(expected))
}

#[test]
fn test_should_inject_prompt_cache_key_matrix() {
    // chat+grok 假
    assert!(!should_inject_prompt_cache_key(
        true,
        Protocol::OpenAiChat,
        "grok-4.6"
    ));
    assert!(!should_inject_prompt_cache_key(
        true,
        Protocol::OpenAiChat,
        "Grok-4.6"
    ));
    // responses+grok 真
    assert!(should_inject_prompt_cache_key(
        true,
        Protocol::OpenAiResponses,
        "grok-4.6"
    ));
    // chat+非 grok 真
    assert!(should_inject_prompt_cache_key(
        true,
        Protocol::OpenAiChat,
        "gpt-4"
    ));
    // 开关关一律假
    assert!(!should_inject_prompt_cache_key(
        false,
        Protocol::OpenAiChat,
        "gpt-4"
    ));
    assert!(!should_inject_prompt_cache_key(
        false,
        Protocol::OpenAiResponses,
        "grok-4.6"
    ));
    // 非 openai 假
    assert!(!should_inject_prompt_cache_key(
        true,
        Protocol::Claude,
        "grok-4.6"
    ));
}

#[test]
fn test_openai_outbound_model_invalid_payload_falls_back_in_body() {
    for protocol in [Protocol::OpenAiChat, Protocol::OpenAiResponses] {
        for invalid in [
            Value::String(String::new()),
            Value::String("  ".to_string()),
            Value::Null,
            Value::Bool(true),
        ] {
            let mut body = json!({"model": invalid});
            assert_eq!(
                resolve_outbound_model(&mut body, "grok-4.6", protocol),
                "grok-4.6"
            );
            assert_eq!(body["model"], "grok-4.6");
        }
    }
}

#[test]
fn test_non_openai_invalid_payload_model_keeps_body() {
    for protocol in [Protocol::Claude, Protocol::Gemini, Protocol::Antigravity] {
        let mut body = json!({"model": null});
        assert_eq!(
            resolve_outbound_model(&mut body, "claude-opus-5", protocol),
            "claude-opus-5"
        );
        assert!(body["model"].is_null());
    }
}

#[test]
fn test_outbound_model_keeps_nonempty_payload_override() {
    let mut body = json!({"model": "grok-4.6"});
    assert_eq!(
        resolve_outbound_model(&mut body, "gpt-4", Protocol::OpenAiChat),
        "grok-4.6"
    );
    assert_eq!(body["model"], "grok-4.6");
}

#[test]
fn test_claude_relay_header_filtering() {
    let headers = headers_with(&[
        ("x-custom-header", "custom-value"),
        ("anthropic-beta", "beta-a,beta-a"),
        ("x-api-key", "inbound-key"),
        ("authorization", "Bearer inbound"),
        ("host", "inbound.example"),
        ("content-length", "99"),
        ("connection", "keep-alive, x-remove-me"),
        ("transfer-encoding", "chunked"),
        ("user-agent", "claude-code/inbound"),
        ("x-remove-me", "remove-me"),
    ]);
    let out = claude_relay_headers(&headers);
    assert!(relay_has_header_value(
        &out,
        "x-custom-header",
        "custom-value"
    ));
    assert!(relay_has_header_value(
        &out,
        "anthropic-beta",
        "beta-a,beta-a"
    ));
    for excluded in [
        "x-api-key",
        "authorization",
        "host",
        "content-length",
        "connection",
        "transfer-encoding",
        "user-agent",
        "x-remove-me",
    ] {
        assert!(!out.contains_key(excluded), "{excluded}");
    }
}

#[test]
fn test_claude_relay_beta_matrix() {
    // 矩阵覆盖:重合值保留、普通值保留、缺失不补
    let cases: [(&[&str], Option<&str>); 3] = [
        (
            &["custom-beta,custom-beta"],
            Some("custom-beta,custom-beta"),
        ),
        (&["caller-beta"], Some("caller-beta")),
        (&[], None),
    ];
    for (input, expected) in cases {
        let mut map = HeaderMap::new();
        for val in input {
            map.append(
                axum::http::header::HeaderName::from_static("anthropic-beta"),
                axum::http::HeaderValue::from_str(val).unwrap(),
            );
        }
        let out = claude_relay_headers(&map);
        match expected {
            Some(exp) => assert!(
                relay_has_header_value(&out, "anthropic-beta", exp),
                "input={input:?}"
            ),
            None => assert!(!out.contains_key("anthropic-beta"), "input={input:?}"),
        }
    }
}

#[test]
fn test_claude_relay_identity_headers_passthrough_only() {
    let headers = headers_with(&[
        ("anthropic-version", "2023-06-01"),
        ("x-app", "cli"),
        ("x-stainless-os", "macOS"),
    ]);
    let out = claude_relay_headers(&headers);
    assert!(relay_has_header_value(
        &out,
        "anthropic-version",
        "2023-06-01"
    ));
    assert!(relay_has_header_value(&out, "x-app", "cli"));
    assert!(relay_has_header_value(&out, "x-stainless-os", "macOS"));
    assert!(!out.contains_key("x-stainless-arch"));

    let out2 = claude_relay_headers(&HeaderMap::new());
    assert!(out2.is_empty());
}

fn mock_state() -> AppState {
    let providers_yaml = r#"
- name: test-claude
  protocol: claude
  base_url: "https://mock.example.com"
  key: sk-test
  models:
    - name: claude-opus-5
      alias: test-opus
- name: test-openai
  protocol: openai_chat
  base_url: "https://mock-openai.example.com"
  key: sk-openai
  proxy_url: "direct"
  models:
    - name: gpt-4
      alias: test-gpt
"#;
    let providers: Vec<ProviderConfig> = serde_yaml::from_str(providers_yaml).unwrap();
    let runtime = RuntimeConfig {
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
        user_agents: test_user_agents(),
        thinking_registry: Arc::new(vec![]),
    };
    let config_snapshot = Arc::new(ConfigSnapshot {
        version: 1,
        providers,
        payload_rules: vec![],
        runtime,
        refresh: ProviderRefreshConfig::default(),
    });
    AppState {
        config: Arc::new(RwLock::new(config_snapshot)),
        reload_lock: Arc::new(tokio::sync::Mutex::new(())),
        reload: reload_returning_secret(None),
        drift: DriftState::new(1000),
        replay_cache: crate::sse::replay_cache::ReplayCache::new(
            std::time::Duration::from_secs(3600),
            1024,
        ),
        last_input_tokens: Arc::new(std::sync::Mutex::new(
            crate::http::session_tokens::SessionTokenCache::new(),
        )),
        cursor: Arc::new(std::sync::RwLock::new(None)),
    }
}

/// 首次上游 200 空流尚未向客户端输出时，应重试一次并转发第二次结果。
#[tokio::test]
async fn test_openai_responses_retries_empty_stream_before_output() {
    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let handler_attempts = Arc::clone(&attempts);
    let upstream = Router::new().route(
            "/responses",
            post(move || {
                let attempts = Arc::clone(&handler_attempts);
                async move {
                    let body = if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                        String::new()
                    } else {
                        concat!(
                            "event: response.created\n",
                            "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_retry\",\"model\":\"gpt-5\"}}\n\n",
                            "event: response.output_text.delta\n",
                            "data: {\"type\":\"response.output_text.delta\",\"delta\":\"retry ok\"}\n\n",
                            "event: response.completed\n",
                            "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_retry\",\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n"
                        )
                        .to_string()
                    };
                    ([(header::CONTENT_TYPE, "text/event-stream")], body)
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, upstream).await.unwrap();
    });

    let state = mock_state();
    let provider_yaml = format!(
        r#"
name: test-responses
protocol: openai_responses
base_url: "http://{}"
key: sk-test
proxy_url: "direct"
models:
  - name: gpt-5
    alias: test-responses
"#,
        upstream_addr
    );
    let provider: ProviderConfig = serde_yaml::from_str(&provider_yaml).unwrap();
    Arc::make_mut(&mut *state.config.write().await)
        .providers
        .push(provider);
    let app = app(state);
    let request = Request::builder()
        .uri("/v1/messages")
        .method("POST")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&json!({
                "model": "test-responses",
                "max_tokens": 64,
                "stream": true,
                "messages": [{"role": "user", "content": "hi"}]
            }))
            .unwrap(),
        ))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let response_body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    server.abort();

    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert!(String::from_utf8_lossy(&response_body).contains("retry ok"));
}

/// 首帧 error:内部重试一次后仍是 error → 返回 502 anthropic error,
/// 不提交 200 SSE(客户端 Claude Code 收到干净 HTTP 状态才能整回合重试)。
#[tokio::test]
async fn test_openai_responses_first_frame_error_returns_502() {
    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let handler_attempts = Arc::clone(&attempts);
    let upstream = Router::new().route(
        "/responses",
        post(move || {
            let attempts = Arc::clone(&handler_attempts);
            async move {
                attempts.fetch_add(1, Ordering::SeqCst);
                (
                    [(header::CONTENT_TYPE, "text/event-stream")],
                    "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"api_error\",\"message\":\"upstream boom\"}}\n\n",
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, upstream).await.unwrap();
    });

    let state = mock_state();
    let provider_yaml = format!(
        r#"
name: test-first-frame-error
protocol: openai_responses
base_url: "http://{}"
key: sk-test
proxy_url: "direct"
models:
  - name: gpt-5
    alias: test-first-frame-error
"#,
        upstream_addr
    );
    let provider: ProviderConfig = serde_yaml::from_str(&provider_yaml).unwrap();
    Arc::make_mut(&mut *state.config.write().await)
        .providers
        .push(provider);
    let app = app(state);
    let request = Request::builder()
        .uri("/v1/messages")
        .method("POST")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&json!({
                "model": "test-first-frame-error",
                "max_tokens": 64,
                "stream": true,
                "messages": [{"role": "user", "content": "hi"}]
            }))
            .unwrap(),
        ))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let response_body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    server.abort();

    // 内部重试一次,共 2 次上游命中
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert_eq!(status, StatusCode::BAD_GATEWAY);
    let v: Value = serde_json::from_slice(&response_body).unwrap();
    assert_eq!(v["type"], "error");
    assert!(v["error"]["message"]
        .as_str()
        .unwrap()
        .contains("upstream boom"));
}

/// 多 URL 回退后的首帧重试必须打当前生效的 base_url,不倒退回已失败的 [0]。
#[tokio::test]
async fn test_first_frame_retry_uses_active_base_url() {
    let bad_hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let good_hits = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let bad_handler = Arc::clone(&bad_hits);
    let good_handler = Arc::clone(&good_hits);
    // bad:恒 503,触发同轮轮转到 good
    let bad = Router::new().route(
        "/responses",
        post(move || {
            let hits = Arc::clone(&bad_handler);
            async move {
                hits.fetch_add(1, Ordering::SeqCst);
                (
                    axum::http::StatusCode::SERVICE_UNAVAILABLE,
                    [(header::CONTENT_TYPE, "application/json")],
                    r#"{"error":{"message":"bad down"}}"#,
                )
            }
        }),
    );
    // good:首次命中返回首帧 error,第二次(send_retry)返回正常流
    let good = Router::new().route(
        "/responses",
        post(move || {
            let hits = Arc::clone(&good_handler);
            async move {
                let body = if hits.fetch_add(1, Ordering::SeqCst) == 0 {
                    "event: error\ndata: {\"type\":\"error\",\"error\":{\"type\":\"api_error\",\"message\":\"first frame boom\"}}\n\n".to_string()
                } else {
                    concat!(
                        "event: response.created\n",
                        "data: {\"type\":\"response.created\",\"response\":{\"id\":\"resp_active\",\"model\":\"gpt-5\"}}\n\n",
                        "event: response.output_text.delta\n",
                        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"active ok\"}\n\n",
                        "event: response.completed\n",
                        "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_active\",\"output\":[],\"usage\":{\"input_tokens\":1,\"output_tokens\":1}}}\n\n"
                    )
                    .to_string()
                };
                ([(header::CONTENT_TYPE, "text/event-stream")], body)
            }
        }),
    );
    let bad_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let bad_addr = bad_listener.local_addr().unwrap();
    let bad_server = tokio::spawn(async move { axum::serve(bad_listener, bad).await.unwrap() });
    let good_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let good_addr = good_listener.local_addr().unwrap();
    let good_server = tokio::spawn(async move { axum::serve(good_listener, good).await.unwrap() });

    let state = mock_state();
    let provider: ProviderConfig = serde_yaml::from_str(&format!(
        "name: test-active-url\nprotocol: openai_responses\nbase_url: [http://{}, http://{}]\nkey: sk-test\nproxy_url: direct\nmodels:\n  - name: gpt-5\n    alias: test-active-url\n",
        bad_addr, good_addr
    ))
    .unwrap();
    Arc::make_mut(&mut *state.config.write().await)
        .providers
        .push(provider);
    let app = app(state);
    let request = Request::builder()
        .uri("/v1/messages")
        .method("POST")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&json!({
                "model": "test-active-url",
                "max_tokens": 64,
                "stream": true,
                "messages": [{"role": "user", "content": "hi"}]
            }))
            .unwrap(),
        ))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let response_body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    bad_server.abort();
    good_server.abort();

    assert_eq!(status, StatusCode::OK);
    assert!(String::from_utf8_lossy(&response_body).contains("active ok"));
    // bad 只在轮转时命中一次,首帧重试不得倒退打 bad
    assert_eq!(bad_hits.load(Ordering::SeqCst), 1);
    // good:轮转轮一次 + 首帧重试一次
    assert_eq!(good_hits.load(Ordering::SeqCst), 2);
}

/// Claude 直通:越档 effort 在发上游前被钳制;`*claude*` 模型原样直通。
#[tokio::test]
async fn test_claude_passthrough_clamps_effort_before_upstream() {
    let captured = Arc::new(std::sync::Mutex::new(Value::Null));
    let handler_captured = Arc::clone(&captured);
    let upstream = Router::new().route(
        "/v1/messages",
        post(move |body: axum::body::Bytes| {
            let captured = Arc::clone(&handler_captured);
            async move {
                *captured.lock().unwrap() = serde_json::from_slice(&body).unwrap_or(json!({}));
                (
                    [(header::CONTENT_TYPE, "application/json")],
                    json!({
                        "id": "msg_1",
                        "type": "message",
                        "role": "assistant",
                        "model": "glm-5.3",
                        "content": [{"type": "text", "text": "ok"}],
                        "stop_reason": "end_turn",
                        "usage": {"input_tokens": 1, "output_tokens": 1}
                    })
                    .to_string(),
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, upstream).await.unwrap();
    });

    let state = mock_state();
    let providers_yaml = format!(
        r#"
- name: test-bailian
  protocol: claude
  base_url: "http://{}"
  key: sk-test
  proxy_url: "direct"
  models:
    - name: glm-5.3
      alias: test-glm
    - name: claude-opus-5
      alias: test-claude-local
"#,
        upstream_addr
    );
    let providers: Vec<ProviderConfig> = serde_yaml::from_str(&providers_yaml).unwrap();
    Arc::make_mut(&mut *state.config.write().await)
        .providers
        .extend(providers);
    Arc::make_mut(&mut *state.config.write().await)
        .runtime
        .thinking_registry = Arc::new(vec![ccextra_core::thinking::ModelCapability {
        id: "glm-5.3".into(),
        reasoning_levels: vec!["low".into(), "high".into(), "max".into()],
        force_effort: None,
    }]);
    let app = app(state);

    // 非 Claude 模型:medium 越档,钳到 low(与 low/high 等距,tie 取低)
    let request = Request::builder()
        .uri("/v1/messages")
        .method("POST")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&json!({
                "model": "test-glm",
                "max_tokens": 64,
                "output_config": {"effort": "medium"},
                "system": [
                    {"type": "text", "text": "x-anthropic-billing-header: fp=abc"},
                    {"type": "text", "text": "# Memory\nPath: /tmp"}
                ],
                "messages": [{"role": "user", "content": "hi"}]
            }))
            .unwrap(),
        ))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let sent = captured.lock().unwrap().clone();
    assert_eq!(sent["model"], "glm-5.3");
    assert_eq!(sent["output_config"]["effort"], "low");
    assert_eq!(sent["messages"][0]["content"], "hi");
    // 计费指纹块被剥离,白名单段保留
    let sys = sent["system"].as_array().unwrap();
    assert_eq!(sys.len(), 1);
    assert!(sys[0]["text"].as_str().unwrap().contains("# Memory"));

    // Claude 模型:glob `*claude*` 跳过,越档 effort 与 system 原样直通
    let claude_identity = "You are Claude Code, Anthropic's official CLI for Claude.";
    let request = Request::builder()
        .uri("/v1/messages")
        .method("POST")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&json!({
                "model": "test-claude-local",
                "max_tokens": 64,
                "output_config": {"effort": "medium"},
                "system": claude_identity,
                "messages": [{"role": "user", "content": "hi"}]
            }))
            .unwrap(),
        ))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let sent = captured.lock().unwrap().clone();
    assert_eq!(sent["model"], "claude-opus-5");
    assert_eq!(sent["output_config"]["effort"], "medium");
    assert_eq!(sent["system"], claude_identity);

    server.abort();
}

/// GPT Responses 在请求前剥离格式无效的 encrypted_content，避免触发 400 重试。
#[tokio::test]
async fn test_openai_responses_sanitizes_invalid_encrypted_content_before_request() {
    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let handler_attempts = Arc::clone(&attempts);
    let saw_encrypted = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let saw_trimmed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let handler_encrypted = Arc::clone(&saw_encrypted);
    let handler_trimmed = Arc::clone(&saw_trimmed);
    let upstream = Router::new().route(
        "/responses",
        post(move |body: axum::body::Bytes| {
            let attempts = Arc::clone(&handler_attempts);
            let saw_encrypted = Arc::clone(&handler_encrypted);
            let saw_trimmed = Arc::clone(&handler_trimmed);
            async move {
                attempts.fetch_add(1, Ordering::SeqCst);
                let parsed: Value = serde_json::from_slice(&body).unwrap_or(json!({}));
                let has_enc = parsed
                    .get("input")
                    .and_then(|v| v.as_array())
                    .map(|items| {
                        items.iter().any(|item| {
                            item.get("type").and_then(|t| t.as_str()) == Some("reasoning")
                                && item.get("encrypted_content").is_some()
                        })
                    })
                    .unwrap_or(false);
                if has_enc {
                    saw_encrypted.store(true, Ordering::SeqCst);
                    (
                        StatusCode::BAD_REQUEST,
                        [(header::CONTENT_TYPE, "application/json")],
                        json!({
                            "error": {
                                "type": "invalid_request_error",
                                "code": "invalid_encrypted_content",
                                "message": "invalid_encrypted_content"
                            }
                        })
                        .to_string(),
                    )
                } else {
                    saw_trimmed.store(true, Ordering::SeqCst);
                    (
                        StatusCode::OK,
                        [(header::CONTENT_TYPE, "application/json")],
                        json!({
                            "type": "response.completed",
                            "response": {
                                "id": "resp_trim",
                                "model": "gpt-5",
                                "output": [{
                                    "type": "message",
                                    "content": [{"type": "output_text", "text": "trimmed ok"}]
                                }],
                                "usage": {"input_tokens": 1, "output_tokens": 1}
                            }
                        })
                        .to_string(),
                    )
                }
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, upstream).await.unwrap();
    });

    let state = mock_state();
    let provider_yaml = format!(
        r#"
name: test-responses-trim
protocol: openai_responses
base_url: "http://{}"
key: sk-test
proxy_url: "direct"
models:
  - name: gpt-5
    alias: test-responses-trim
"#,
        upstream_addr
    );
    let provider: ProviderConfig = serde_yaml::from_str(&provider_yaml).unwrap();
    Arc::make_mut(&mut *state.config.write().await)
        .providers
        .push(provider);
    let app = app(state);
    let request = Request::builder()
        .uri("/v1/messages")
        .method("POST")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&json!({
                "model": "test-responses-trim",
                "max_tokens": 64,
                "stream": false,
                "messages": [
                    {"role": "assistant", "content": [
                        {"type": "thinking", "thinking": "t", "signature": "gAAAA-replay"}
                    ]},
                    {"role": "user", "content": "hi"}
                ]
            }))
            .unwrap(),
        ))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let response_body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    server.abort();

    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert!(
        !saw_encrypted.load(Ordering::SeqCst),
        "首次请求不应带 encrypted_content"
    );
    assert!(
        saw_trimmed.load(Ordering::SeqCst),
        "请求应在发送前剥离无效 encrypted_content"
    );
    assert_eq!(status, StatusCode::OK);
    assert!(String::from_utf8_lossy(&response_body).contains("trimmed ok"));
}

#[tokio::test]
async fn test_openai_responses_retries_valid_encrypted_content_after_upstream_rejection() {
    const VALID: &str = "gAAAAAAAAAAAAQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyAhIiMkJSYnKCkqKywtLi8wMTIzNDU2Nzg5Ojs8PT4_QA";
    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let saw_encrypted = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let saw_trimmed = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let handler_attempts = Arc::clone(&attempts);
    let handler_encrypted = Arc::clone(&saw_encrypted);
    let handler_trimmed = Arc::clone(&saw_trimmed);
    let upstream = Router::new().route(
            "/responses",
            post(move |body: axum::body::Bytes| {
                let attempts = Arc::clone(&handler_attempts);
                let saw_encrypted = Arc::clone(&handler_encrypted);
                let saw_trimmed = Arc::clone(&handler_trimmed);
                async move {
                    let parsed: Value = serde_json::from_slice(&body).unwrap();
                    let has_encrypted = parsed["input"].as_array().unwrap().iter().any(|item| {
                        item["type"] == "reasoning" && item.get("encrypted_content").is_some()
                    });
                    if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                        saw_encrypted.store(has_encrypted, Ordering::SeqCst);
                        (
                            StatusCode::BAD_REQUEST,
                            [(header::CONTENT_TYPE, "application/json")],
                            r#"{"error":{"code":"invalid_encrypted_content"}}"#,
                        )
                    } else {
                        saw_trimmed.store(!has_encrypted, Ordering::SeqCst);
                        (
                            StatusCode::OK,
                            [(header::CONTENT_TYPE, "application/json")],
                            r#"{"type":"response.completed","response":{"id":"resp_retry","model":"gpt-5","output":[],"usage":{"input_tokens":1,"output_tokens":1}}}"#,
                        )
                    }
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });

    let state = mock_state();
    let provider: ProviderConfig = serde_yaml::from_str(&format!(
            "name: test-responses-retry\nprotocol: openai_responses\nbase_url: http://{}\nkey: sk-test\nproxy_url: direct\nmodels:\n  - name: gpt-5\n    alias: test-responses-retry\n",
            upstream_addr
        ))
        .unwrap();
    Arc::make_mut(&mut *state.config.write().await)
        .providers
        .push(provider);
    let app = app(state);
    let request = Request::builder()
            .uri("/v1/messages")
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::to_vec(&json!({
                    "model": "test-responses-retry", "max_tokens": 64, "stream": false,
                    "messages": [
                        {"role": "assistant", "content": [{"type": "thinking", "thinking": "t", "signature": VALID}]},
                        {"role": "user", "content": "hi"}
                    ]
                }))
                .unwrap(),
            ))
            .unwrap();
    let response = app.oneshot(request).await.unwrap();
    server.abort();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
    assert!(saw_encrypted.load(Ordering::SeqCst));
    assert!(saw_trimmed.load(Ordering::SeqCst));
}

#[tokio::test]
async fn test_openai_responses_retries_on_422_invalid_encrypted_content() {
    const VALID: &str = "gAAAAAAAAAAAAQIDBAUGBwgJCgsMDQ4PEBESExQVFhcYGRobHB0eHyAhIiMkJSYnKCkqKywtLi8wMTIzNDU2Nzg5Ojs8PT4_QA";
    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let handler_attempts = Arc::clone(&attempts);
    let upstream = Router::new().route(
            "/responses",
            post(move || {
                let attempts = Arc::clone(&handler_attempts);
                async move {
                    if attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                        (
                            StatusCode::UNPROCESSABLE_ENTITY,
                            [(header::CONTENT_TYPE, "application/json")],
                            r#"{"error":{"code":"invalid_encrypted_content","message":"could not decrypt"}}"#,
                        )
                    } else {
                        (
                            StatusCode::OK,
                            [(header::CONTENT_TYPE, "application/json")],
                            r#"{"type":"response.completed","response":{"id":"resp_422_ok","model":"gpt-5","output":[],"usage":{"input_tokens":1,"output_tokens":1}}}"#,
                        )
                    }
                }
            }),
        );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });

    let state = mock_state();
    let provider: ProviderConfig = serde_yaml::from_str(&format!(
            "name: test-422-retry\nprotocol: openai_responses\nbase_url: http://{}\nkey: sk-test\nproxy_url: direct\nmodels:\n  - name: gpt-5\n    alias: test-422-retry\n",
            upstream_addr
        ))
        .unwrap();
    Arc::make_mut(&mut *state.config.write().await)
        .providers
        .push(provider);
    let app = app(state);
    let request = Request::builder()
            .uri("/v1/messages")
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(
                serde_json::to_vec(&json!({
                    "model": "test-422-retry", "max_tokens": 64, "stream": false,
                    "messages": [
                        {"role": "assistant", "content": [{"type": "thinking", "thinking": "t", "signature": VALID}]},
                        {"role": "user", "content": "hi"}
                    ]
                }))
                .unwrap(),
            ))
            .unwrap();
    let response = app.oneshot(request).await.unwrap();
    server.abort();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(attempts.load(Ordering::SeqCst), 2);
}

#[tokio::test]
async fn test_gpt_responses_strips_execute_only_payload_fields() {
    let captured: Arc<StdMutex<Option<Value>>> = Arc::new(StdMutex::new(None));
    let handler_captured = Arc::clone(&captured);
    let upstream = Router::new().route(
        "/responses",
        post(move |body: axum::body::Bytes| {
            let captured = Arc::clone(&handler_captured);
            async move {
                *captured.lock().unwrap() = Some(serde_json::from_slice(&body).unwrap());
                (
                    [(header::CONTENT_TYPE, "application/json")],
                    json!({
                        "type": "response.completed",
                        "response": {
                            "id": "resp_final",
                            "model": "gpt-5",
                            "output": [],
                            "usage": {"input_tokens": 1, "output_tokens": 1}
                        }
                    })
                    .to_string(),
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        axum::serve(listener, upstream).await.unwrap();
    });

    let state = mock_state();
    let provider: ProviderConfig = serde_yaml::from_str(&format!(
        r#"
name: test-responses-final
protocol: openai_responses
base_url: "http://{}"
key: sk-test
proxy_url: "direct"
models:
  - name: gpt-5
    alias: test-responses-final
"#,
        upstream_addr
    ))
    .unwrap();
    Arc::make_mut(&mut *state.config.write().await)
        .providers
        .push(provider);
    Arc::make_mut(&mut *state.config.write().await)
        .payload_rules
        .push(PayloadRule {
            models: vec!["test-responses-final".into()],
            protocol: Some(Protocol::OpenAiResponses),
            params: json!({
                "previous_response_id": "resp_previous",
                "generate": true,
                "safety_identifier": "safe-id",
                "stream_options": {"include_usage": true},
                "prompt_cache_retention": "24h",
                "temperature": 0.1
            })
            .as_object()
            .unwrap()
            .clone(),
        });
    let app = app(state);
    let request = Request::builder()
        .uri("/v1/messages")
        .method("POST")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&json!({
                "model": "test-responses-final",
                "max_tokens": 64,
                "messages": [{"role": "user", "content": "hi"}]
            }))
            .unwrap(),
        ))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    server.abort();

    assert_eq!(response.status(), StatusCode::OK);
    let body = captured.lock().unwrap().clone().unwrap();
    for key in [
        "previous_response_id",
        "generate",
        "safety_identifier",
        "stream_options",
        "prompt_cache_retention",
    ] {
        assert!(body.get(key).is_none(), "{key} 不应发往 GPT Responses");
    }
    assert_eq!(body["temperature"], 0.1);
}

#[tokio::test]
async fn test_health_check() {
    let app = app(mock_state());
    let req = Request::builder()
        .uri("/health")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), 1024).await.unwrap();
    assert_eq!(body, "ok");
}

#[tokio::test]
async fn test_missing_model_field() {
    let app = app(mock_state());
    let body_json = json!({"messages": [{"role": "user", "content": "hi"}]});
    let req = Request::builder()
        .uri("/v1/messages")
        .method("POST")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body_json).unwrap()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let body = to_bytes(resp.into_body(), 1024).await.unwrap();
    let body_json: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body_json["type"], "error");
    assert_eq!(body_json["error"]["type"], "invalid_request_error");
    assert!(body_json["error"]["message"]
        .as_str()
        .unwrap()
        .contains("缺少 model 字段"));
}

#[tokio::test]
async fn test_invalid_json() {
    let app = app(mock_state());
    let req = Request::builder()
        .uri("/v1/messages")
        .method("POST")
        .header("content-type", "application/json")
        .body(Body::from("{invalid json"))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let body = to_bytes(resp.into_body(), 1024).await.unwrap();
    let body_json: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body_json["type"], "error");
    assert_eq!(body_json["error"]["type"], "invalid_request_error");
    assert!(body_json["error"]["message"]
        .as_str()
        .unwrap()
        .contains("JSON 解析失败"));
}

#[tokio::test]
async fn test_model_not_found() {
    let app = app(mock_state());
    let body_json = json!({
        "model": "unknown-model",
        "messages": [{"role": "user", "content": "hi"}]
    });
    let req = Request::builder()
        .uri("/v1/messages")
        .method("POST")
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(&body_json).unwrap()))
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    let body = to_bytes(resp.into_body(), 1024).await.unwrap();
    let body_json: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body_json["type"], "error");
    assert_eq!(body_json["error"]["type"], "not_found_error");
    assert!(body_json["error"]["message"]
        .as_str()
        .unwrap()
        .contains("未找到"));
}

#[test]
fn test_payload_override_wildcard() {
    let mut body = json!({"model": "glm-5.1", "max_tokens": 1024});
    let rules = vec![PayloadRule {
        models: vec!["*glm*".into()],
        protocol: Some(Protocol::OpenAiChat),
        params: {
            let mut map = serde_json::Map::new();
            map.insert("max_tokens".into(), json!(32000));
            map.insert("temperature".into(), json!(0.1));
            map
        },
    }];
    apply_payload_overrides(&mut body, "glm-5.1", Protocol::OpenAiChat, &rules);
    assert_eq!(body["max_tokens"], 32000);
    assert_eq!(body["temperature"], 0.1);
}

#[test]
fn test_payload_override_protocol_gate() {
    // 规则限定 openai_chat,claude 直通不应注入
    let mut body = json!({"model": "glm-5.1", "max_tokens": 1024});
    let rules = vec![PayloadRule {
        models: vec!["*glm*".into()],
        protocol: Some(Protocol::OpenAiChat),
        params: {
            let mut map = serde_json::Map::new();
            map.insert("tool_stream".into(), json!(true));
            map
        },
    }];
    apply_payload_overrides(&mut body, "glm-5.1", Protocol::Claude, &rules);
    assert!(body.get("tool_stream").is_none());
    assert_eq!(body["max_tokens"], 1024);
}

#[test]
fn test_payload_override_claude_requires_explicit_protocol() {
    // 无 protocol 字段的规则作用于 claude 直通:默认不注入
    let mut body = json!({"model": "evol-opus-5", "max_tokens": 1024});
    let rules = vec![PayloadRule {
        models: vec!["*evol*".into()],
        protocol: None,
        params: {
            let mut map = serde_json::Map::new();
            map.insert("max_tokens".into(), json!(32000));
            map
        },
    }];
    apply_payload_overrides(&mut body, "evol-opus-5", Protocol::Claude, &rules);
    assert_eq!(
        body["max_tokens"], 1024,
        "claude 直通无 protocol 规则不注入"
    );

    // 显式声明 protocol: claude → 注入
    let rules2 = vec![PayloadRule {
        models: vec!["*evol*".into()],
        protocol: Some(Protocol::Claude),
        params: {
            let mut map = serde_json::Map::new();
            map.insert("max_tokens".into(), json!(32000));
            map
        },
    }];
    apply_payload_overrides(&mut body, "evol-opus-5", Protocol::Claude, &rules2);
    assert_eq!(body["max_tokens"], 32000, "显式 claude 协议应注入");
}

#[test]
fn test_payload_override_no_match() {
    let mut body = json!({"model": "claude-opus", "max_tokens": 1024});
    let rules = vec![PayloadRule {
        models: vec!["*glm*".into()],
        protocol: None,
        params: {
            let mut map = serde_json::Map::new();
            map.insert("max_tokens".into(), json!(32000));
            map
        },
    }];
    apply_payload_overrides(&mut body, "claude-opus", Protocol::OpenAiChat, &rules);
    assert_eq!(body["max_tokens"], 1024);
}

#[test]
fn test_payload_override_star_matches_all() {
    let mut body = json!({"model": "any-model"});
    let rules = vec![PayloadRule {
        models: vec!["*".into()],
        protocol: None,
        params: {
            let mut map = serde_json::Map::new();
            map.insert("temperature".into(), json!(0.5));
            map
        },
    }];
    apply_payload_overrides(&mut body, "any-model", Protocol::OpenAiChat, &rules);
    assert_eq!(body["temperature"], 0.5);
}

#[tokio::test]
async fn test_models_endpoint() {
    let app = app(mock_state());
    let req = Request::builder()
        .uri("/v1/models")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let body = to_bytes(resp.into_body(), 1024).await.unwrap();
    let json: Value = serde_json::from_slice(&body).unwrap();
    let data = json["data"].as_array().unwrap();
    assert_eq!(data.len(), 2);
    assert_eq!(data[0]["id"], "test-opus");
    assert_eq!(data[0]["object"], "model");
    assert_eq!(data[0]["owned_by"], "test-claude");
    assert_eq!(data[0]["display_name"], "test-opus");
    assert_eq!(data[1]["id"], "test-gpt");
}

#[test]
fn test_build_models_list() {
    let providers_yaml = r#"
- name: p1
  protocol: claude
  base_url: "https://x.com"
  key: k
  models:
    - name: upstream-a
      alias: alias-a
- name: p2
  protocol: openai_chat
  base_url: "https://y.com"
  key: k
  models:
    - name: upstream-b
      alias: alias-b
    - name: upstream-c
      alias: alias-c
"#;
    let providers: Vec<ProviderConfig> = serde_yaml::from_str(providers_yaml).unwrap();
    let json = build_models_list(&providers);
    let data = json["data"].as_array().unwrap();
    assert_eq!(data.len(), 3);
    assert_eq!(data[0]["id"], "alias-a");
    assert_eq!(data[1]["id"], "alias-b");
    assert_eq!(data[2]["id"], "alias-c");
    assert_eq!(data[2]["owned_by"], "p2");
}

#[test]
fn test_build_models_list_custom_context() {
    let providers_yaml = r#"
- name: p1
  protocol: claude
  base_url: "https://x.com"
  key: k
  models:
    - name: upstream-a
      alias: alias-a
      max_input_tokens: 128000
      max_tokens: 32000
"#;
    let providers: Vec<ProviderConfig> = serde_yaml::from_str(providers_yaml).unwrap();
    let json = build_models_list(&providers);
    let data = json["data"].as_array().unwrap();
    assert_eq!(data[0]["max_input_tokens"], 128000);
    assert_eq!(data[0]["max_tokens"], 32000);
}

#[test]
fn test_build_models_list_default_context() {
    let providers_yaml = r#"
- name: p1
  protocol: claude
  base_url: "https://x.com"
  key: k
  models:
    - name: upstream-a
      alias: alias-a
"#;
    let providers: Vec<ProviderConfig> = serde_yaml::from_str(providers_yaml).unwrap();
    let json = build_models_list(&providers);
    let data = json["data"].as_array().unwrap();
    assert_eq!(data[0]["max_input_tokens"], 200000);
    assert_eq!(data[0]["max_tokens"], 64000);
}

#[test]
fn test_secret_validation() {
    // 测试 secret 验证逻辑
    struct Case {
        name: &'static str,
        headers: HeaderMap,
        secret: Option<String>,
        expect_ok: bool,
    }

    let mut h_ok = HeaderMap::new();
    h_ok.insert("x-api-key", "s3cret".parse().unwrap());

    let mut h_wrong = HeaderMap::new();
    h_wrong.insert("x-api-key", "wrong".parse().unwrap());

    let mut bearer_ok = HeaderMap::new();
    bearer_ok.insert("authorization", "Bearer s3cret".parse().unwrap());

    let mut bearer_wrong = HeaderMap::new();
    bearer_wrong.insert("authorization", "Bearer wrong".parse().unwrap());

    let cases = vec![
        Case {
            name: "ok",
            headers: h_ok,
            secret: Some("s3cret".into()),
            expect_ok: true,
        },
        Case {
            name: "missing",
            headers: HeaderMap::new(),
            secret: Some("s3cret".into()),
            expect_ok: false,
        },
        Case {
            name: "wrong",
            headers: h_wrong,
            secret: Some("s3cret".into()),
            expect_ok: false,
        },
        Case {
            name: "bearer ok",
            headers: bearer_ok,
            secret: Some("s3cret".into()),
            expect_ok: true,
        },
        Case {
            name: "bearer wrong",
            headers: bearer_wrong,
            secret: Some("s3cret".into()),
            expect_ok: false,
        },
        Case {
            name: "disabled",
            headers: HeaderMap::new(),
            secret: None,
            expect_ok: true,
        },
    ];

    for case in cases {
        let result = check_secret(&case.headers, &case.secret);
        assert_eq!(
            result.is_ok(),
            case.expect_ok,
            "Failed at case: {}",
            case.name
        );
    }
}

#[tokio::test]
async fn test_models_requires_secret() {
    let state = mock_state();
    Arc::make_mut(&mut *state.config.write().await)
        .runtime
        .secret = Some("s3cret".into());
    let app = app(state);
    // 无 key → 401
    let req = Request::builder()
        .uri("/v1/models")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::UNAUTHORIZED);
    // 带 key → OK
    let req = Request::builder()
        .uri("/v1/models")
        .header("x-api-key", "s3cret")
        .body(Body::empty())
        .unwrap();
    let resp = app.oneshot(req).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
}

/// 构造带指定 secret 的 reload 闭包(providers 保持空,validate 必过)
fn reload_returning_secret(secret: Option<String>) -> ReloadFn {
    Arc::new(move || {
        let secret = secret.clone();
        Box::pin(async move {
            Ok(ReloadData {
                refresh: ProviderRefreshConfig::default(),
                providers: vec![],
                payload_rules: vec![],
                normalize: NormalizeConfig {
                    enabled: false,
                    drift_detector: false,
                },
                logging: LoggingConfig {
                    level: "info".into(),
                    request_body: false,
                },
                secret,
                proxy_url: None,
                antigravity: None,
                user_agents: test_user_agents(),
                thinking_registry: Arc::new(vec![]),
            })
        })
    })
}

/// /reload 真正把新 secret 装进 RuntimeConfig:重载前无 key 放行,
/// 重载后同样请求应 401。删掉 handle_reload 里的 runtime 写入则失败。
#[tokio::test]
async fn test_reload_applies_new_secret() {
    let mut state = mock_state();
    assert!(state.config.read().await.runtime.secret.is_none());
    state.reload = reload_returning_secret(Some("sk-after-reload".into()));
    let app = app(state);

    let models_req = || {
        Request::builder()
            .uri("/v1/models")
            .body(Body::empty())
            .unwrap()
    };
    let before = app.clone().oneshot(models_req()).await.unwrap();
    assert_eq!(before.status(), StatusCode::OK, "重载前无 secret 应放行");

    let reload = Request::builder()
        .uri("/reload")
        .method("POST")
        .body(Body::empty())
        .unwrap();
    let resp = app.clone().oneshot(reload).await.unwrap();
    assert_eq!(resp.status(), StatusCode::OK, "/reload 应成功");

    let after = app.clone().oneshot(models_req()).await.unwrap();
    assert_eq!(
        after.status(),
        StatusCode::UNAUTHORIZED,
        "重载后新 secret 应生效"
    );
    let ok = Request::builder()
        .uri("/v1/models")
        .header("x-api-key", "sk-after-reload")
        .body(Body::empty())
        .unwrap();
    assert_eq!(app.oneshot(ok).await.unwrap().status(), StatusCode::OK);
}

/// /reload 替换 normalize 与全局代理:新 UpstreamClient 带上新 proxy_url。
#[tokio::test]
async fn test_reload_applies_normalize_and_proxy() {
    let mut state = mock_state();
    state.reload = Arc::new(|| {
        Box::pin(async {
            Ok(ReloadData {
                refresh: ProviderRefreshConfig::default(),
                providers: vec![],
                payload_rules: vec![],
                normalize: NormalizeConfig {
                    enabled: true,
                    drift_detector: true,
                },
                logging: LoggingConfig {
                    level: "debug".into(),
                    request_body: true,
                },
                secret: None,
                proxy_url: Some("socks5://127.0.0.1:1080".into()),
                antigravity: None,
                user_agents: test_user_agents(),
                thinking_registry: Arc::new(vec![]),
            })
        })
    });
    let config = state.config.clone();
    let req = Request::builder()
        .uri("/reload")
        .method("POST")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app(state).oneshot(req).await.unwrap().status(),
        StatusCode::OK
    );

    let snapshot = config.read().await;
    let rt = &snapshot.runtime;
    assert!(rt.normalize.enabled, "normalize 应随 /reload 更新");
    assert!(rt.normalize.drift_detector);
    assert!(rt.logging.request_body, "logging 应随 /reload 更新");
    assert_eq!(
        rt.upstream.resolve_proxy_for_test(None),
        "socks5://127.0.0.1:1080",
        "全局代理应随 /reload 生效"
    );
}

/// 上游等待释放信号时 reload 必须完成,不能依赖固定延迟制造并发窗口。
#[tokio::test]
async fn test_reload_completes_while_request_inflight() {
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    // 两个信号分别确认请求到达和允许上游返回;失败退出时自动取消任务。
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let (req_tx, req_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let mut tasks = tokio::task::JoinSet::new();
    tasks.spawn(async move {
        use tokio::io::AsyncReadExt;
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 1024];
        assert!(sock.read(&mut buf).await.unwrap() > 0);
        req_tx.send(()).unwrap();
        release_rx.await.expect("reload 完成前不得释放上游");
        sock.write_all(
            b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 2\r\n\r\n{}",
        )
        .await
        .unwrap();
    });

    let mut state = mock_state();
    // 让 test-claude 指向慢 mock
    Arc::make_mut(&mut *state.config.write().await).providers[0]
        .set_base_url_for_test(format!("http://{addr}"));

    state.reload = reload_returning_secret(None);

    let app = app(state);

    // 发起进行中请求,不 await 完成
    let inflight_app = app.clone();
    tasks.spawn(async move {
        let req = Request::builder()
            .uri("/v1/messages")
            .method("POST")
            .header("content-type", "application/json")
            .body(Body::from(
                json!({
                    "model": "test-opus",
                    "stream": false,
                    "messages": [{"role": "user", "content": "hi"}]
                })
                .to_string(),
            ))
            .unwrap();
        let response = inflight_app.oneshot(req).await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    });

    // 收到请求后再发 reload;上游仍在等待释放信号。
    tokio::time::timeout(std::time::Duration::from_secs(2), req_rx)
        .await
        .expect("等待上游收到请求超时")
        .expect("channel 异常");

    // /reload 应在 500ms 内完成,不被上游请求的锁阻塞
    let reload_req = Request::builder()
        .uri("/reload")
        .method("POST")
        .body(Body::empty())
        .unwrap();
    let result = tokio::time::timeout(
        std::time::Duration::from_millis(500),
        app.oneshot(reload_req),
    )
    .await;
    let response = result
        .expect("/reload 超时:handle_messages 在上游请求期间仍持有配置读锁")
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    release_tx.send(()).unwrap();
    tokio::time::timeout(std::time::Duration::from_secs(2), async {
        while let Some(result) = tasks.join_next().await {
            result.unwrap();
        }
    })
    .await
    .expect("释放上游后请求必须完成");
}

#[tokio::test]
async fn test_config_snapshot_is_the_only_request_configuration() {
    let state = mock_state();
    {
        let mut config = state.config.write().await;
        let snapshot = Arc::make_mut(&mut config);
        snapshot.providers.clear();
        snapshot.runtime.secret = Some("snapshot-key".into());
    }
    let response = app(state)
        .oneshot(
            Request::builder()
                .uri("/v1/models")
                .header("x-api-key", "snapshot-key")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    let body: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(body["data"], json!([]), "删除的 provider 不得从旧镜像复活");
}

/// 实际 reload 完成后，旧后台结果不得恢复已删除的 provider。
#[tokio::test]
async fn test_config_snapshot_version_guards_against_stale_injection() {
    let state = mock_state();
    let old = Arc::clone(&*state.config.read().await);
    let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let config = state.config.clone();
    let mut tasks = tokio::task::JoinSet::new();
    tasks.spawn(async move {
        ready_tx.send(()).unwrap();
        release_rx.await.unwrap();
        publish_refreshed_providers(&config, old.version, old.providers.clone())
            .await
            .unwrap()
    });
    tokio::time::timeout(std::time::Duration::from_secs(2), ready_rx)
        .await
        .unwrap()
        .unwrap();
    handle_reload(axum::extract::State(state.clone()))
        .await
        .unwrap();
    release_tx.send(()).unwrap();
    assert!(
        !tokio::time::timeout(std::time::Duration::from_secs(2), tasks.join_next())
            .await
            .unwrap()
            .unwrap()
            .unwrap()
    );
    let current = state.config.read().await;
    assert_eq!(current.version, 2);
    assert!(current.providers.is_empty());
}

// ── to_anthropic_error 上游错误透传 ─────────────────────────────────

#[test]
fn upstream_error_openai_standard_shape() {
    // OpenAI 标准 {"error":{type,message}},透传 type 并映射
    let body = br#"{"error":{"type":"rate_limit_error","message":"You are sending requests too quickly"}}"#;
    let out: Value = serde_json::from_slice(&to_anthropic_error(body)).unwrap();
    assert_eq!(out["error"]["type"], "rate_limit_error");
    assert_eq!(
        out["error"]["message"],
        "You are sending requests too quickly"
    );
}

#[test]
fn upstream_error_bailian_nested_message() {
    // 阿里云百炼:code/message 平铺,messages 是嵌套 JSON 字符串
    let body = br#"{"code":"Throttling.RateQuota","message":"{\"error\":{\"message\":\"The engine is currently overloaded, please try again later\",\"type\":\"EngineOverloadedError\",\"param\":null,\"code\":\"EngineOverloadedError\"}}"}"#;
    let out: Value = serde_json::from_slice(&to_anthropic_error(body)).unwrap();
    // EngineOverloadedError 含 overload → overloaded_error
    assert_eq!(out["error"]["type"], "overloaded_error");
    assert_eq!(
        out["error"]["message"],
        "The engine is currently overloaded, please try again later"
    );
}

#[test]
fn upstream_error_bailian_plain_code_message() {
    // code 含 quota → rate_limit_error;message 为普通字符串直接透传
    let body = br#"{"code":"Throttling.RateQuota","message":"limit exceeded"}"#;
    let out: Value = serde_json::from_slice(&to_anthropic_error(body)).unwrap();
    assert_eq!(out["error"]["type"], "rate_limit_error");
    assert_eq!(out["error"]["message"], "limit exceeded");
}

#[test]
fn upstream_error_bailian_invalid_auth_code() {
    // code 含 auth → authentication_error
    let body = br#"{"code":"InvalidApiKey","message":"invalid api key"}"#;
    let out: Value = serde_json::from_slice(&to_anthropic_error(body)).unwrap();
    assert_eq!(out["error"]["type"], "authentication_error");
    assert_eq!(out["error"]["message"], "invalid api key");
}

#[test]
fn upstream_error_unparseable_falls_back_to_raw() {
    // 非 JSON body:兜底 "upstream error"
    let out: Value =
        serde_json::from_slice(&to_anthropic_error(b"<html>502 bad gateway</html>")).unwrap();
    assert_eq!(out["error"]["type"], "api_error");
    assert_eq!(out["error"]["message"], "<html>502 bad gateway</html>");
}

use crate::test_support::spawn_captured_server;

#[tokio::test]
async fn test_claude_relay_forwards_headers_and_overrides_auth() {
    let (server, captured) = spawn_captured_server("/v1/messages", StatusCode::OK, "{}").await;

    let state = mock_state();
    Arc::make_mut(&mut *state.config.write().await).providers[0]
        .set_base_url_for_test(server.url.clone());
    let app = app(state);
    let request = Request::builder()
        .uri("/v1/messages")
        .method("POST")
        .header("content-type", "application/json")
        .header("x-custom-header", "custom-value")
        .header("anthropic-beta", "beta-a")
        .header("anthropic-beta", "beta-b")
        .header("x-api-key", "inbound-key")
        .header("authorization", "Bearer inbound")
        .header("user-agent", "claude-code/inbound")
        .header("connection", "x-remove-me")
        .header("x-remove-me", "remove-me")
        .body(Body::from(
            json!({
                "model": "test-opus",
                "max_tokens": 64,
                "stream": false,
                "messages": [{"role": "user", "content": "hi"}]
            })
            .to_string(),
        ))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        captured.header("authorization").as_deref(),
        Some("Bearer sk-test")
    );
    assert_eq!(
        captured.header("x-custom-header").as_deref(),
        Some("custom-value")
    );
    assert_eq!(
        captured.header_values("anthropic-beta"),
        vec!["beta-a", "beta-b"]
    );
    assert_eq!(
        captured.header_values("user-agent"),
        vec!["claude-code/inbound"]
    );
    assert!(!captured.has_header("x-api-key"));
    assert!(!captured.has_header("x-remove-me"));
    assert_eq!(captured.body()["model"], "claude-opus-5");
}

#[tokio::test]
async fn test_count_tokens_relay_uses_fallback_user_agent() {
    let (server, captured) = spawn_captured_server(
        "/v1/messages/count_tokens",
        StatusCode::OK,
        r#"{"input_tokens":123}"#,
    )
    .await;

    let state = mock_state();
    Arc::make_mut(&mut *state.config.write().await).providers[0]
        .set_base_url_for_test(server.url.clone());
    let app = app(state);
    let request = Request::builder()
        .uri("/v1/messages/count_tokens")
        .method("POST")
        .header("content-type", "application/json")
        .header("x-custom-header", "custom-value")
        .header("anthropic-beta", "beta-a")
        .header("x-api-key", "inbound-key")
        .body(Body::from(
            json!({"model": "test-opus", "messages": []}).to_string(),
        ))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 1024).await.unwrap();

    assert_eq!(status, StatusCode::OK);
    assert_eq!(body, r#"{"input_tokens":123}"#);
    assert_eq!(
        captured.header("authorization").as_deref(),
        Some("Bearer sk-test")
    );
    assert_eq!(
        captured.header("x-custom-header").as_deref(),
        Some("custom-value")
    );
    assert_eq!(captured.header("anthropic-beta").as_deref(), Some("beta-a"));
    assert_eq!(captured.header_values("user-agent"), vec![TEST_CLAUDE_CLI]);
    assert!(!captured.has_header("x-api-key"));
}

// ── B1:非流 body 有界读取接线 ───────────────────────────────────────

#[tokio::test]
async fn non_stream_body_limits_and_passthrough() {
    use crate::{
        limits::SUCCESS_BODY_LIMIT,
        test_support::{padded_json, TestServer},
    };
    for path in ["/v1/messages", "/v1/messages/count_tokens"] {
        for (status, len) in [
            (StatusCode::OK, 1024 * 1024),
            (StatusCode::OK, SUCCESS_BODY_LIMIT + 1),
            (StatusCode::UNAUTHORIZED, 300 * 1024),
            (StatusCode::TOO_MANY_REQUESTS, 300 * 1024),
            (StatusCode::INTERNAL_SERVER_ERROR, 300 * 1024),
        ] {
            // 截断前缀是合法 JSON,不能采用其中的结构化错误字段。
            let json = if status.is_success() {
                if path.ends_with("count_tokens") {
                    r#"{"input_tokens":42}"#
                } else {
                    r#"{"id":"msg_1","type":"message","role":"assistant","content":[{"type":"text","text":"hello"}]}"#
                }
            } else {
                r#"{"error":{"type":"rate_limit_error","message":"quota exceeded"}}"#
            };
            let payload = padded_json(json, len);
            let server = TestServer::reply(status, payload.clone()).await;
            let state = mock_state();
            Arc::make_mut(&mut *state.config.write().await).providers[0]
                .set_base_url_for_test(server.url.clone());
            let response = post_test_request(state, path, "test-opus").await;
            let expected = if len > SUCCESS_BODY_LIMIT {
                StatusCode::BAD_GATEWAY
            } else {
                status
            };
            assert_eq!(response.status(), expected, "{path} {status}");
            let bytes = to_bytes(response.into_body(), SUCCESS_BODY_LIMIT)
                .await
                .unwrap();
            if expected.is_success() {
                assert_eq!(bytes, payload, "直通须逐字节一致");
            } else {
                let error: Value = serde_json::from_slice(&bytes).unwrap();
                assert_eq!(
                    error["error"]["type"],
                    if path.ends_with("count_tokens") && status == StatusCode::UNAUTHORIZED {
                        "authentication_error"
                    } else {
                        "api_error"
                    }
                );
                if !status.is_success() {
                    assert!(error["error"]["message"]
                        .as_str()
                        .unwrap()
                        .contains("已截断"));
                }
            }
        }
    }
}

/// count_tokens 上游 429 响应 body 传输中断:保留 429,不升级为 500
#[tokio::test]
async fn test_count_tokens_error_body_transport_failure_preserves_429() {
    // 裸 TCP:回 429 头 + 声明 100000 字节但只发部分,随后断开
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let (mut sock, _) = listener.accept().await.unwrap();
        let mut buf = [0u8; 4096];
        let mut seen = 0;
        while seen < 4096 {
            let n = sock.read(&mut buf).await.unwrap_or(0);
            if n == 0 {
                break;
            }
            seen += n;
            if buf[..n].windows(4).any(|w| w == b"\r\n\r\n") {
                break;
            }
        }
        let head = "HTTP/1.1 429 Too Many Requests\r\ncontent-type: application/json\r\ncontent-length: 100000\r\n\r\n";
        sock.write_all(head.as_bytes()).await.unwrap();
        sock.write_all(&[b'e'; 64]).await.unwrap();
        sock.shutdown().await.unwrap();
    });

    let state = mock_state();
    Arc::make_mut(&mut *state.config.write().await).providers[0]
        .set_base_url_for_test(format!("http://{upstream_addr}"));
    let app = app(state);
    let request = Request::builder()
        .uri("/v1/messages/count_tokens")
        .method("POST")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({"model": "test-opus", "messages": []}).to_string(),
        ))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();
    let body = to_bytes(response.into_body(), 1024).await.unwrap();
    server.abort();

    assert_eq!(
        status,
        StatusCode::TOO_MANY_REQUESTS,
        "传输失败不得把 429 改成 500"
    );
    let v: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["error"]["type"], "api_error");
    assert!(
        v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("读取上游错误响应失败"),
        "消息应说明读取失败: {}",
        v["error"]["message"]
    );
}

#[tokio::test]
async fn test_chat_grok_headers_and_skips_prompt_cache_key() {
    let (server, captured) = spawn_captured_server(
        "/chat/completions",
        StatusCode::OK,
        json!({
            "id": "chatcmpl_grok",
            "model": "grok-4.6",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "ok"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        })
        .to_string(),
    )
    .await;

    let state = mock_state();
    let provider_yaml = format!(
        r#"
name: test-grok-chat
protocol: openai_chat
base_url: "{}"
key: sk-test
proxy_url: "direct"
prompt_cache_key: true
models:
  - name: grok-4.6
    alias: test-grok-chat
"#,
        server.url
    );
    let provider: ProviderConfig = serde_yaml::from_str(&provider_yaml).unwrap();
    Arc::make_mut(&mut *state.config.write().await)
        .providers
        .push(provider);
    let app = app(state);
    let request = Request::builder()
        .uri("/v1/messages")
        .method("POST")
        .header("content-type", "application/json")
        .header("x-claude-code-session-id", "sess-abc-123")
        .body(Body::from(
            serde_json::to_vec(&json!({
                "model": "test-grok-chat",
                "max_tokens": 64,
                "stream": false,
                "messages": [{"role": "user", "content": "hi"}]
            }))
            .unwrap(),
        ))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();

    assert_eq!(status, StatusCode::OK);
    let ua = captured.header("user-agent").unwrap_or_default();
    assert!(
        ua.starts_with("grok-pager/1.0.46 grok-shell/1.0.46 ("),
        "UA 应为交互式 Grok CLI,实际 {ua}"
    );
    assert_eq!(
        captured.header("x-xai-token-auth").as_deref(),
        Some("xai-grok-cli")
    );
    assert_eq!(
        captured.header("x-grok-client-version").as_deref(),
        Some("1.0.46")
    );
    assert_eq!(
        captured.header("x-grok-client-identifier").as_deref(),
        Some("grok-pager")
    );
    assert_eq!(
        captured.header("x-grok-client-mode").as_deref(),
        Some("interactive")
    );
    assert!(!captured.has_header("x-authenticateresponse"));
    assert_eq!(
        captured.header("x-grok-model-override").as_deref(),
        Some("grok-4.6")
    );
    assert_eq!(
        captured.header("x-grok-conv-id").as_deref(),
        Some("sess-abc-123")
    );
    assert!(!captured.has_header("x-grok-doom-loop-check"));
    assert!(!captured.has_header("x-grok-req-id"));
    assert!(!captured.has_header("x-grok-session-id"));
    assert!(!captured.has_header("x-grok-agent-id"));
    assert!(!captured.has_header("x-grok-turn-idx"));
    assert!(!captured.has_header("session-id"));
    assert!(!captured.has_header("thread-id"));
    assert!(
        captured.body().get("prompt_cache_key").is_none(),
        "chat+grok 开关开也不注入"
    );
}

#[tokio::test]
async fn test_responses_grok_still_injects_prompt_cache_key() {
    let (server, captured) = spawn_captured_server(
        "/responses",
        StatusCode::OK,
        json!({
            "type": "response.completed",
            "response": {
                "id": "resp_grok",
                "model": "grok-4.6",
                "output": [{
                    "type": "message",
                    "content": [{"type": "output_text", "text": "ok"}]
                }],
                "usage": {"input_tokens": 1, "output_tokens": 1}
            }
        })
        .to_string(),
    )
    .await;

    let state = mock_state();
    let provider_yaml = format!(
        r#"
name: test-grok-responses
protocol: openai_responses
base_url: "{}"
key: sk-test
proxy_url: "direct"
prompt_cache_key: true
models:
  - name: grok-4.6
    alias: test-grok-responses
"#,
        server.url
    );
    let provider: ProviderConfig = serde_yaml::from_str(&provider_yaml).unwrap();
    Arc::make_mut(&mut *state.config.write().await)
        .providers
        .push(provider);
    let app = app(state);
    let request = Request::builder()
        .uri("/v1/messages")
        .method("POST")
        .header("content-type", "application/json")
        .header("x-claude-code-session-id", "sess-abc-123")
        .body(Body::from(
            serde_json::to_vec(&json!({
                "model": "test-grok-responses",
                "max_tokens": 64,
                "stream": false,
                "messages": [{"role": "user", "content": "hi"}]
            }))
            .unwrap(),
        ))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();

    assert_eq!(status, StatusCode::OK);
    let ua = captured.header("user-agent").unwrap_or_default();
    assert!(ua.starts_with("grok-pager/1.0.46 grok-shell/1.0.46 ("));
    assert_eq!(
        captured.header("x-xai-token-auth").as_deref(),
        Some("xai-grok-cli")
    );
    assert_eq!(
        captured.header("x-grok-doom-loop-check").as_deref(),
        Some("1024")
    );
    assert_eq!(
        captured.header("x-grok-conv-id").as_deref(),
        Some("sess-abc-123")
    );
    assert!(!captured.has_header("x-grok-req-id"));
    assert!(!captured.has_header("x-grok-session-id"));
    assert!(!captured.has_header("x-grok-agent-id"));
    assert!(!captured.has_header("x-grok-turn-idx"));
    assert_eq!(captured.body()["prompt_cache_key"], "sess-abc-123");
}

#[tokio::test]
async fn test_chat_payload_override_gpt_to_grok_uses_outbound_model() {
    let (server, captured) = spawn_captured_server(
        "/chat/completions",
        StatusCode::OK,
        json!({
            "id": "chatcmpl_override",
            "model": "grok-4.6",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "ok"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        })
        .to_string(),
    )
    .await;

    let state = mock_state();
    let provider_yaml = format!(
        r#"
name: test-gpt-to-grok
protocol: openai_chat
base_url: "{}"
key: sk-test
proxy_url: "direct"
prompt_cache_key: true
models:
  - name: gpt-4
    alias: test-gpt-to-grok
"#,
        server.url
    );
    let provider: ProviderConfig = serde_yaml::from_str(&provider_yaml).unwrap();
    Arc::make_mut(&mut *state.config.write().await)
        .providers
        .push(provider);
    let mut params = serde_json::Map::new();
    params.insert("model".into(), json!("grok-4.6"));
    Arc::make_mut(&mut *state.config.write().await)
        .payload_rules
        .push(PayloadRule {
            models: vec!["test-gpt-to-grok".into()],
            protocol: Some(Protocol::OpenAiChat),
            params,
        });
    let app = app(state);
    let request = Request::builder()
        .uri("/v1/messages")
        .method("POST")
        .header("content-type", "application/json")
        .header("x-claude-code-session-id", "sess-abc-123")
        .body(Body::from(
            serde_json::to_vec(&json!({
                "model": "test-gpt-to-grok",
                "max_tokens": 64,
                "stream": false,
                "messages": [{"role": "user", "content": "hi"}]
            }))
            .unwrap(),
        ))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();

    assert_eq!(status, StatusCode::OK);
    assert_eq!(captured.body()["model"], "grok-4.6");
    let ua = captured.header("user-agent").unwrap_or_default();
    assert!(
        ua.starts_with("grok-pager/1.0.46 grok-shell/1.0.46 ("),
        "UA 应为交互式 Grok CLI,实际 {ua}"
    );
    assert_eq!(
        captured.header("x-grok-model-override").as_deref(),
        Some("grok-4.6")
    );
    assert_eq!(
        captured.header("x-grok-conv-id").as_deref(),
        Some("sess-abc-123")
    );
    assert!(
        captured.body().get("prompt_cache_key").is_none(),
        "payload 改 grok 后 chat 不注入"
    );
}

#[tokio::test]
async fn test_chat_grok_preserves_inbound_prompt_cache_key() {
    let (server, captured) = spawn_captured_server(
        "/chat/completions",
        StatusCode::OK,
        json!({
            "id": "chatcmpl_key",
            "model": "grok-4.6",
            "choices": [{
                "index": 0,
                "message": {"role": "assistant", "content": "ok"},
                "finish_reason": "stop"
            }],
            "usage": {"prompt_tokens": 1, "completion_tokens": 1}
        })
        .to_string(),
    )
    .await;

    let state = mock_state();
    let provider_yaml = format!(
        r#"
name: test-grok-chat-key
protocol: openai_chat
base_url: "{}"
key: sk-test
proxy_url: "direct"
prompt_cache_key: true
models:
  - name: grok-4.6
    alias: test-grok-chat-key
"#,
        server.url
    );
    let provider: ProviderConfig = serde_yaml::from_str(&provider_yaml).unwrap();
    Arc::make_mut(&mut *state.config.write().await)
        .providers
        .push(provider);
    let app = app(state);
    let request = Request::builder()
        .uri("/v1/messages")
        .method("POST")
        .header("content-type", "application/json")
        .header("x-claude-code-session-id", "sess-abc-123")
        .body(Body::from(
            serde_json::to_vec(&json!({
                "model": "test-grok-chat-key",
                "max_tokens": 64,
                "stream": false,
                "prompt_cache_key": "user-key",
                "messages": [{"role": "user", "content": "hi"}]
            }))
            .unwrap(),
        ))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();

    assert_eq!(status, StatusCode::OK);
    assert_eq!(captured.body()["prompt_cache_key"], "user-key");
}

#[tokio::test]
async fn test_responses_preserves_inbound_prompt_cache_key() {
    let (server, captured) = spawn_captured_server(
        "/responses",
        StatusCode::OK,
        json!({
            "type": "response.completed",
            "response": {
                "id": "resp_key",
                "model": "grok-4.6",
                "output": [{
                    "type": "message",
                    "content": [{"type": "output_text", "text": "ok"}]
                }],
                "usage": {"input_tokens": 1, "output_tokens": 1}
            }
        })
        .to_string(),
    )
    .await;

    let state = mock_state();
    let provider_yaml = format!(
        r#"
name: test-grok-responses-key
protocol: openai_responses
base_url: "{}"
key: sk-test
proxy_url: "direct"
prompt_cache_key: true
models:
  - name: grok-4.6
    alias: test-grok-responses-key
"#,
        server.url
    );
    let provider: ProviderConfig = serde_yaml::from_str(&provider_yaml).unwrap();
    Arc::make_mut(&mut *state.config.write().await)
        .providers
        .push(provider);
    let app = app(state);
    let request = Request::builder()
        .uri("/v1/messages")
        .method("POST")
        .header("content-type", "application/json")
        .header("x-claude-code-session-id", "sess-abc-123")
        .body(Body::from(
            serde_json::to_vec(&json!({
                "model": "test-grok-responses-key",
                "max_tokens": 64,
                "stream": false,
                "prompt_cache_key": "user-key",
                "messages": [{"role": "user", "content": "hi"}]
            }))
            .unwrap(),
        ))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    let status = response.status();

    assert_eq!(status, StatusCode::OK);
    assert_eq!(
        captured.body()["prompt_cache_key"],
        "user-key",
        "入站 key 不改写成 session"
    );
}

#[test]
fn test_cf_retry_after_keeps_jitter_above_general_cap() {
    let started = std::time::Instant::now();
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(reqwest::header::RETRY_AFTER, "2".parse().unwrap());
    let status = reqwest::StatusCode::from_u16(522).unwrap();

    let delay = compute_retry_delay(0, started, &headers, Some(status)).unwrap();

    assert!(
        delay > RETRY_MAX_DELAY,
        "52x Retry-After 不得被通用上限截断"
    );
    assert!(delay >= std::time::Duration::from_millis(1600));
    assert!(delay <= std::time::Duration::from_millis(2400));
}

#[test]
fn test_retry_delay_respects_retry_after_and_budget() {
    let started = std::time::Instant::now();
    let mut headers = reqwest::header::HeaderMap::new();
    headers.insert(reqwest::header::RETRY_AFTER, "5".parse().unwrap());
    // 5xx 带 Retry-After: 钳位 1.5s 后 ±20% jitter
    let d = compute_retry_delay(
        0,
        started,
        &headers,
        Some(reqwest::StatusCode::SERVICE_UNAVAILABLE),
    )
    .unwrap();
    assert!(d >= std::time::Duration::from_millis(1200));
    assert!(d <= std::time::Duration::from_millis(1800));

    // 无头: 基础 300ms 经 jitter 在 [240ms, 360ms] 范围
    let d2 = compute_retry_delay(0, started, &reqwest::header::HeaderMap::new(), None).unwrap();
    assert!(d2 >= std::time::Duration::from_millis(240));
    assert!(d2 <= std::time::Duration::from_millis(360));

    // 总预算耗尽 → 不再重试
    std::thread::sleep(std::time::Duration::from_millis(20));
    let exhausted_start = started - (RETRY_TOTAL_BUDGET + std::time::Duration::from_secs(1));
    assert!(compute_retry_delay(0, exhausted_start, &headers, None).is_none());
}

/// 上游 429:不进退避重试(对齐 codex 传输层 retry_429: false),单次命中即返回;
/// Retry-After 头透传给客户端,由客户端按上游声明退避。
#[tokio::test]
async fn test_upstream_429_fails_fast_passes_retry_after() {
    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let handler_attempts = Arc::clone(&attempts);
    let upstream = Router::new().route(
        "/chat/completions",
        post(move || {
            let attempts = Arc::clone(&handler_attempts);
            async move {
                attempts.fetch_add(1, Ordering::SeqCst);
                (
                    axum::http::StatusCode::TOO_MANY_REQUESTS,
                    [
                        (header::CONTENT_TYPE, "application/json"),
                        (header::RETRY_AFTER, "7"),
                    ],
                    r#"{"error":{"type":"rate_limit_error","message":"rate limited"}}"#,
                )
            }
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });

    let state = mock_state();
    let provider: ProviderConfig = serde_yaml::from_str(&format!(
            "name: test-429\nprotocol: openai_chat\nbase_url: http://{}\nkey: sk-test\nproxy_url: direct\nmodels:\n  - name: gpt-4\n    alias: test-429\n",
            upstream_addr
        ))
        .unwrap();
    Arc::make_mut(&mut *state.config.write().await)
        .providers
        .push(provider);
    let app = app(state);
    let request = Request::builder()
        .uri("/v1/messages")
        .method("POST")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&json!({
                "model": "test-429", "max_tokens": 64, "stream": false,
                "messages": [{"role": "user", "content": "hi"}]
            }))
            .unwrap(),
        ))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    server.abort();

    // 单次命中,无退避重试
    assert_eq!(attempts.load(Ordering::SeqCst), 1);
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok()),
        Some("7")
    );
    let body = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let err: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(err["type"], "error");
    assert_eq!(err["error"]["type"], "rate_limit_error");
}

/// 多 base_url 全部 5xx:每个地址各试一次,快速失败返回末次上游错误。
#[tokio::test]
async fn test_upstream_all_base_urls_5xx_returns_last_error() {
    use crate::test_support::TestServer;
    let first = TestServer::reply(
        StatusCode::SERVICE_UNAVAILABLE,
        r#"{"error":{"type":"api_error","message":"first down"}}"#,
    )
    .await;
    let last = TestServer::reply(
        StatusCode::INTERNAL_SERVER_ERROR,
        r#"{"error":{"type":"api_error","message":"last down"}}"#,
    )
    .await;

    let state = mock_state();
    let provider: ProviderConfig = serde_yaml::from_str(&format!(
        "name: test-all-5xx\nprotocol: openai_chat\nbase_url: [{}, {}]\nkey: sk-test\nproxy_url: direct\nmodels:\n  - name: gpt-4\n    alias: test-all-5xx\n",
        first.url, last.url
    ))
    .unwrap();
    Arc::make_mut(&mut *state.config.write().await)
        .providers
        .push(provider);
    let attempts = state.config.read().await.runtime.upstream.attempts.clone();
    let response = post_test_request(state, "/v1/messages", "test-all-5xx").await;
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();

    // 末次上游错误决定状态;每个地址只打一次,不本地退避
    assert_eq!(status, StatusCode::INTERNAL_SERVER_ERROR);
    assert_eq!(
        *attempts.lock().unwrap(),
        [first.url.clone(), last.url.clone()]
    );
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["type"], "error");
    assert!(v["error"]["message"]
        .as_str()
        .unwrap()
        .contains("last down"));
}

/// 空 base_url 列表:prepare 阶段拒绝,返回 anthropic error 而非 panic。
#[tokio::test]
async fn test_empty_base_url_list_rejected() {
    let state = mock_state();
    let provider: ProviderConfig = serde_yaml::from_str(
        "name: test-empty-url\nprotocol: openai_chat\nbase_url: []\nkey: sk-test\nproxy_url: direct\nmodels:\n  - name: gpt-4\n    alias: test-empty-url\n",
    )
    .unwrap();
    Arc::make_mut(&mut *state.config.write().await)
        .providers
        .push(provider);
    let response = post_test_request(state, "/v1/messages", "test-empty-url").await;
    assert_eq!(response.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["type"], "error");
    assert!(v["error"]["message"].as_str().unwrap().contains("base_url"));
}

async fn post_test_request(state: AppState, path: &str, model: &str) -> axum::response::Response {
    app(state)
        .oneshot(
            Request::builder()
                .uri(path)
                .method("POST")
                .header("content-type", "application/json")
                .body(Body::from(
                    json!({"model": model, "max_tokens": 64, "stream": false,
            "messages": [{"role": "user", "content": "hi"}]})
                    .to_string(),
                ))
                .unwrap(),
        )
        .await
        .unwrap()
}

#[tokio::test]
async fn fallback_attempts_each_url_once() {
    use crate::test_support::TestServer;
    let good = TestServer::reply(StatusCode::OK,
        r#"{"id":"c1","choices":[{"message":{"role":"assistant","content":"fallback ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1}}"#).await;
    let limited = TestServer::reply(StatusCode::TOO_MANY_REQUESTS, "rate limited").await;
    let broken = TestServer::reply(StatusCode::INTERNAL_SERVER_ERROR, "boom").await;
    // 网络错误(拒连)、429、5xx 都在同轮轮转到下一个 base_url,各地址只试一次
    for first_url in [None, Some(limited.url.clone()), Some(broken.url.clone())] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let first =
            first_url.unwrap_or_else(|| format!("http://{}", listener.local_addr().unwrap()));
        let state = mock_state();
        let provider: ProviderConfig = serde_yaml::from_str(&format!(
            "name: fallback\nprotocol: openai_chat\nbase_url: [{first}, {}]\nkey: sk-test\nproxy_url: direct\nmodels:\n  - name: gpt-4\n    alias: fallback\n", good.url)).unwrap();
        Arc::make_mut(&mut *state.config.write().await)
            .providers
            .push(provider);
        let attempts = state.config.read().await.runtime.upstream.attempts.clone();
        drop(listener);
        let response = post_test_request(state, "/v1/messages", "fallback").await;
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(*attempts.lock().unwrap(), [first, good.url.clone()]);
    }
}

#[tokio::test]
async fn test_codex_provider_sends_account_id_header() {
    let (server, captured) = spawn_captured_server(
        "/responses",
        StatusCode::OK,
        json!({
            "type": "response.completed",
            "response": {
                "id": "resp_codex",
                "model": "gpt-5.6-terra",
                "output": [{
                    "type": "message",
                    "content": [{"type": "output_text", "text": "ok"}]
                }],
                "usage": {"input_tokens": 1, "output_tokens": 1}
            }
        })
        .to_string(),
    )
    .await;

    // fresh 凭证:运行时刷新直接命中,不触发网络
    let dir = tempfile::tempdir().unwrap();
    let cred = crate::codex::CodexCredential {
        r#type: "codex".to_string(),
        id_token: String::new(),
        access_token: "codex-access-token".to_string(),
        refresh_token: "codex-refresh-token".to_string(),
        account_id: "acct-123".to_string(),
        last_refresh: String::new(),
        email: "user@example.com".to_string(),
        plan_type: "pro".to_string(),
        expired: "2999-01-01T00:00:00Z".to_string(),
        disabled: false,
    };
    crate::codex::store::save(dir.path(), &cred).unwrap();

    let state = mock_state();
    let mut metadata = std::collections::HashMap::new();
    metadata.insert(
        "auth_dir".to_string(),
        dir.path().to_string_lossy().to_string(),
    );
    metadata.insert("email".to_string(), "user@example.com".to_string());
    metadata.insert("account_id".to_string(), "acct-123".to_string());
    metadata.insert("provider_type".to_string(), "codex".to_string());
    let provider = ProviderConfig::new(
        "codex-user".to_string(),
        Protocol::OpenAiResponses,
        vec![server.url.clone()],
        "stale-key".to_string(),
        Some("direct".to_string()),
        false,
        vec![ccextra_core::route::ModelConfig {
            name: "gpt-5.6-terra".to_string(),
            alias: "codex-test".to_string(),
            max_input_tokens: None,
            max_tokens: None,
        }],
    )
    .with_metadata(metadata);
    Arc::make_mut(&mut *state.config.write().await)
        .providers
        .push(provider);
    let app = app(state);
    let request = Request::builder()
        .uri("/v1/messages")
        .method("POST")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&json!({
                "model": "codex-test",
                "max_tokens": 64,
                "stream": false,
                "messages": [{"role": "user", "content": "hi"}]
            }))
            .unwrap(),
        ))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(
        captured.header("chatgpt-account-id").as_deref(),
        Some("acct-123")
    );
    // 运行时刷新后 key 来自凭证而非静态 provider key
    assert_eq!(
        captured.header("authorization").as_deref(),
        Some("Bearer codex-access-token")
    );
    assert_eq!(
        captured.header("originator").as_deref(),
        Some("codex_cli_rs")
    );
    let ua = captured.header("user-agent").unwrap_or_default();
    assert!(
        ua.starts_with("codex_cli_rs/"),
        "UA 应为 codex CLI,实际 {ua}"
    );
}

#[tokio::test]
async fn test_responses_payload_tier_is_serialized_before_input() {
    let (server, captured) = spawn_captured_server(
        "/responses",
        StatusCode::OK,
        json!({
            "id": "resp_routing", "model": "gpt-5",
            "output": [{"type": "message", "content": [{"type": "output_text", "text": "ok"}]}],
            "usage": {"input_tokens": 1, "output_tokens": 1}
        })
        .to_string(),
    )
    .await;
    let state = mock_state();
    let provider: ProviderConfig = serde_yaml::from_str(&format!(
        "name: routing-test\nprotocol: openai_responses\nbase_url: '{}'\nkey: sk-test\nproxy_url: direct\nmodels:\n  - name: gpt-5\n    alias: routing-test\n", server.url
    )).unwrap();
    {
        let mut config = state.config.write().await;
        let snapshot = Arc::make_mut(&mut config);
        snapshot.providers.push(provider);
        snapshot.payload_rules = serde_yaml::from_str(
            "- models: ['*']\n  protocol: openai_responses\n  params:\n    service_tier: priority\n"
        ).unwrap();
    }
    let request = Request::builder()
        .uri("/v1/messages")
        .method("POST")
        .header("content-type", "application/json")
        .body(Body::from(
            json!({
                "model": "routing-test", "max_tokens": 64, "stream": false,
                "messages": [{"role": "user", "content": "hi"}]
            })
            .to_string(),
        ))
        .unwrap();
    let response = app(state).oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let raw = captured.raw_body.lock().unwrap().clone().unwrap();
    assert!(std::str::from_utf8(&raw).unwrap().starts_with(
        r#"{"model":"gpt-5","stream":false,"service_tier":"priority","instructions":"#
    ));
    assert_eq!(captured.body()["service_tier"], "priority");
}

#[tokio::test]
async fn test_codex_provider_compresses_request_body() {
    let (server, captured) = spawn_captured_server(
        "/responses",
        StatusCode::OK,
        json!({
            "type": "response.completed",
            "response": {
                "id": "resp_codex_zstd",
                "model": "gpt-5.6-terra",
                "output": [{
                    "type": "message",
                    "content": [{"type": "output_text", "text": "ok"}]
                }],
                "usage": {"input_tokens": 1, "output_tokens": 1}
            }
        })
        .to_string(),
    )
    .await;

    // fresh 凭证:运行时刷新直接命中,不触发网络
    let dir = tempfile::tempdir().unwrap();
    let cred = crate::codex::CodexCredential {
        r#type: "codex".to_string(),
        id_token: String::new(),
        access_token: "codex-access-token".to_string(),
        refresh_token: "codex-refresh-token".to_string(),
        account_id: "acct-123".to_string(),
        last_refresh: String::new(),
        email: "user@example.com".to_string(),
        plan_type: "pro".to_string(),
        expired: "2999-01-01T00:00:00Z".to_string(),
        disabled: false,
    };
    crate::codex::store::save(dir.path(), &cred).unwrap();

    let state = mock_state();
    let mut metadata = std::collections::HashMap::new();
    metadata.insert(
        "auth_dir".to_string(),
        dir.path().to_string_lossy().to_string(),
    );
    metadata.insert("email".to_string(), "user@example.com".to_string());
    metadata.insert("account_id".to_string(), "acct-123".to_string());
    metadata.insert("provider_type".to_string(), "codex".to_string());
    let provider = ProviderConfig::new(
        "codex-user".to_string(),
        Protocol::OpenAiResponses,
        vec![server.url.clone()],
        "stale-key".to_string(),
        Some("direct".to_string()),
        false,
        vec![ccextra_core::route::ModelConfig {
            name: "gpt-5.6-terra".to_string(),
            alias: "codex-test".to_string(),
            max_input_tokens: None,
            max_tokens: None,
        }],
    )
    .with_metadata(metadata);
    Arc::make_mut(&mut *state.config.write().await)
        .providers
        .push(provider);
    let app = app(state);
    let request = Request::builder()
        .uri("/v1/messages")
        .method("POST")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&json!({
                "model": "codex-test",
                "max_tokens": 64,
                "stream": false,
                "messages": [{"role": "user", "content": "hi"}]
            }))
            .unwrap(),
        ))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    // codex 订阅请求:声明 zstd 编码,线上字节解压后与出站 JSON 等价
    assert_eq!(captured.header("content-encoding").as_deref(), Some("zstd"));
    // 压缩 body 不影响路由提示:hint 从压缩前的最终 body 构造
    assert_eq!(
        captured.header("x-codex-routing-hint").as_deref(),
        Some("model=gpt-5.6-terra")
    );
    let raw = captured
        .raw_body
        .lock()
        .unwrap()
        .clone()
        .expect("应捕获原始请求字节");
    let decompressed = zstd::stream::decode_all(&raw[..]).unwrap();
    let body: serde_json::Value = serde_json::from_slice(&decompressed).unwrap();
    assert_eq!(body["model"], "gpt-5.6-terra");
    // GPT 上游首条消息转 developer 适配块;解压后结构完整即证明压缩无损
    assert_eq!(body["input"][0]["role"], "developer");
}

#[tokio::test]
async fn test_static_gpt_provider_omits_account_id_header() {
    let (server, captured) = spawn_captured_server(
        "/responses",
        StatusCode::OK,
        json!({
            "type": "response.completed",
            "response": {
                "id": "resp_static",
                "model": "gpt-5.6-terra",
                "output": [{
                    "type": "message",
                    "content": [{"type": "output_text", "text": "ok"}]
                }],
                "usage": {"input_tokens": 1, "output_tokens": 1}
            }
        })
        .to_string(),
    )
    .await;

    let state = mock_state();
    let provider: ProviderConfig = serde_yaml::from_str(&format!(
        r#"
name: static-gpt
protocol: openai_responses
base_url: "{}"
key: sk-static
proxy_url: "direct"
models:
  - name: gpt-5.6-terra
    alias: static-gpt-test
"#,
        server.url
    ))
    .unwrap();
    Arc::make_mut(&mut *state.config.write().await)
        .providers
        .push(provider);
    let app = app(state);
    let request = Request::builder()
        .uri("/v1/messages")
        .method("POST")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&json!({
                "model": "static-gpt-test",
                "max_tokens": 64,
                "stream": false,
                "messages": [{"role": "user", "content": "hi"}]
            }))
            .unwrap(),
        ))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();

    assert_eq!(response.status(), StatusCode::OK);
    // API key 静态 provider 无 codex metadata,不发订阅身份头,也不压缩
    assert!(!captured.has_header("chatgpt-account-id"));
    assert!(!captured.has_header("content-encoding"));
    // 路由提示仅 codex OAuth 订阅请求携带(对齐 CPA applyCodexRoutingHint)
    assert!(!captured.has_header("x-codex-routing-hint"));
    assert_eq!(
        captured.header("authorization").as_deref(),
        Some("Bearer sk-static")
    );
}

// ===== Cursor SDK sidecar 集成 =====

/// 构造 Cursor 测试状态:cursor provider + for_test sidecar + 临时凭证目录
async fn cursor_test_state(sidecar_url: String, auth_dir: &std::path::Path) -> AppState {
    let state = mock_state();
    let provider_yaml = r#"
name: cursor
protocol: cursor_sdk
base_url: ""
key: managed
models:
  - name: auto
    alias: cursor-auto
"#;
    let provider: ProviderConfig = serde_yaml::from_str(provider_yaml).unwrap();
    Arc::make_mut(&mut *state.config.write().await)
        .providers
        .push(provider);
    let sidecar = crate::cursor::CursorSidecar::for_test(
        sidecar_url,
        "test-token".into(),
        auth_dir.to_path_buf(),
    );
    let runtime = crate::cursor::CursorRuntime {
        sidecar,
        config: tokio::sync::RwLock::new(crate::cursor::CursorConfig {
            auth_dir: auth_dir.to_path_buf(),
            models: vec![],
            idle_secs: 1800,
            max_agents: 16,
            workspace_dir: std::path::PathBuf::from("/tmp"),
        }),
        vocab: tokio::sync::RwLock::new(std::collections::HashMap::new()),
    };
    *state.cursor.write().unwrap() = Some(Arc::new(runtime));
    state
}

/// 写入新鲜测试凭证(expires_at = now + 1h)
fn write_cursor_credential(auth_dir: &std::path::Path) {
    std::fs::create_dir_all(auth_dir).unwrap();
    let expires_at = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_secs() as i64
        + 3600;
    let credential = json!({
        "accessToken": "test-access-token",
        "refreshToken": "test-refresh-token",
        "sub": "test-sub",
        "expires_at": expires_at,
    });
    std::fs::write(
        auth_dir.join("cursor.json"),
        serde_json::to_vec(&credential).unwrap(),
    )
    .unwrap();
}

/// sidecar mock /run:校验 Bearer token 与 apiKey,回固定 SSE 帧
fn cursor_sidecar_router(frames: &'static str) -> Router {
    Router::new().route(
        "/run",
        post(move |headers: HeaderMap, body: bytes::Bytes| {
            let frames = frames;
            async move {
                assert_eq!(
                    headers.get("authorization").and_then(|v| v.to_str().ok()),
                    Some("Bearer test-token")
                );
                let payload: Value = serde_json::from_slice(&body).unwrap();
                assert_eq!(payload["apiKey"], "test-access-token");
                assert_eq!(payload["model"], "auto");
                (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "text/event-stream")],
                    frames,
                )
            }
        }),
    )
}

#[tokio::test]
async fn test_cursor_sdk_stream_relay_end_to_end() {
    let frames = concat!(
        "data: {\"type\":\"text_delta\",\"text\":\"hello\"}\n\n",
        "data: {\"type\":\"usage\",\"input_tokens\":42,\"output_tokens\":7}\n\n",
        "data: {\"type\":\"turn_end\",\"stop_reason\":\"end_turn\"}\n\n",
    );
    let sidecar = crate::test_support::TestServer::spawn(cursor_sidecar_router(frames)).await;
    let auth_dir = tempfile::tempdir().unwrap();
    write_cursor_credential(auth_dir.path());
    let state = cursor_test_state(sidecar.url.clone(), auth_dir.path()).await;
    let app = app(state.clone());

    let request = Request::builder()
        .uri("/v1/messages")
        .method("POST")
        .header("content-type", "application/json")
        .header("x-claude-code-session-id", "cursor-sess-1")
        .body(Body::from(
            serde_json::to_vec(&json!({
                "model": "cursor-auto",
                "max_tokens": 64,
                "stream": true,
                "messages": [{"role": "user", "content": "hi"}]
            }))
            .unwrap(),
        ))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let text = String::from_utf8_lossy(&body);
    assert!(text.contains("event: message_start"));
    assert!(text.contains("hello"));
    assert!(text.contains("\"input_tokens\":42"));
    assert!(text.contains("event: message_stop"));
    // usage 写入 session cache
    assert_eq!(
        state.last_input_tokens.lock().unwrap().get("cursor-sess-1"),
        Some(42)
    );
}

#[tokio::test]
async fn test_cursor_sdk_non_stream_aggregates_anthropic_json() {
    let frames = concat!(
        "data: {\"type\":\"thinking_delta\",\"text\":\"ponder\"}\n\n",
        "data: {\"type\":\"text_delta\",\"text\":\"hello\"}\n\n",
        "data: {\"type\":\"usage\",\"input_tokens\":10,\"output_tokens\":5}\n\n",
        "data: {\"type\":\"turn_end\",\"stop_reason\":\"end_turn\"}\n\n",
    );
    let sidecar = crate::test_support::TestServer::spawn(cursor_sidecar_router(frames)).await;
    let auth_dir = tempfile::tempdir().unwrap();
    write_cursor_credential(auth_dir.path());
    let state = cursor_test_state(sidecar.url.clone(), auth_dir.path()).await;
    let app = app(state.clone());

    let request = Request::builder()
        .uri("/v1/messages")
        .method("POST")
        .header("content-type", "application/json")
        .header("x-claude-code-session-id", "cursor-sess-2")
        .body(Body::from(
            serde_json::to_vec(&json!({
                "model": "cursor-auto",
                "max_tokens": 64,
                "messages": [{"role": "user", "content": "hi"}]
            }))
            .unwrap(),
        ))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let message: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(message["type"], "message");
    assert_eq!(message["model"], "auto");
    assert_eq!(message["stop_reason"], "end_turn");
    let content = message["content"].as_array().unwrap();
    assert_eq!(content[0]["type"], "thinking");
    assert_eq!(content[0]["thinking"], "ponder");
    assert_eq!(content[1]["type"], "text");
    assert_eq!(content[1]["text"], "hello");
    assert_eq!(message["usage"]["input_tokens"], 10);
    // 非流聚合也写 session cache
    assert_eq!(
        state.last_input_tokens.lock().unwrap().get("cursor-sess-2"),
        Some(10)
    );
}

#[tokio::test]
async fn test_cursor_sdk_sidecar_503_maps_anthropic_error_with_retry_after() {
    // sidecar 503 + Retry-After:不提交 200,Anthropic error 形状透传
    let sidecar = Router::new().route(
        "/run",
        post(|| async {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                [(header::RETRY_AFTER, "7")],
                "cursor sidecar busy",
            )
        }),
    );
    let server = crate::test_support::TestServer::spawn(sidecar).await;
    let auth_dir = tempfile::tempdir().unwrap();
    write_cursor_credential(auth_dir.path());
    let state = cursor_test_state(server.url.clone(), auth_dir.path()).await;
    let app = app(state);

    let request = Request::builder()
        .uri("/v1/messages")
        .method("POST")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&json!({
                "model": "cursor-auto",
                "max_tokens": 64,
                "stream": true,
                "messages": [{"role": "user", "content": "hi"}]
            }))
            .unwrap(),
        ))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(
        response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok()),
        Some("7")
    );
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let error: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(error["type"], "error");
    assert!(error["error"]["message"].as_str().unwrap().contains("busy"));
}

#[tokio::test]
async fn test_cursor_sdk_first_frame_error_returns_502() {
    // sidecar 200 但首业务帧是 error:不提交 200,返回 502
    let frames = "data: {\"type\":\"error\",\"code\":\"x\",\"message\":\"sidecar boom\"}\n\n";
    let sidecar = crate::test_support::TestServer::spawn(cursor_sidecar_router(frames)).await;
    let auth_dir = tempfile::tempdir().unwrap();
    write_cursor_credential(auth_dir.path());
    let state = cursor_test_state(sidecar.url.clone(), auth_dir.path()).await;
    let app = app(state);

    let request = Request::builder()
        .uri("/v1/messages")
        .method("POST")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&json!({
                "model": "cursor-auto",
                "max_tokens": 64,
                "stream": true,
                "messages": [{"role": "user", "content": "hi"}]
            }))
            .unwrap(),
        ))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let error: Value = serde_json::from_slice(&body).unwrap();
    assert!(error["error"]["message"]
        .as_str()
        .unwrap()
        .contains("sidecar boom"));
}

#[tokio::test]
async fn test_cursor_sdk_count_tokens_reuses_cache_and_defaults_zero() {
    let frames = concat!(
        "data: {\"type\":\"text_delta\",\"text\":\"hi\"}\n\n",
        "data: {\"type\":\"usage\",\"input_tokens\":33,\"output_tokens\":1}\n\n",
        "data: {\"type\":\"turn_end\",\"stop_reason\":\"end_turn\"}\n\n",
    );
    let sidecar = crate::test_support::TestServer::spawn(cursor_sidecar_router(frames)).await;
    let auth_dir = tempfile::tempdir().unwrap();
    write_cursor_credential(auth_dir.path());
    let state = cursor_test_state(sidecar.url.clone(), auth_dir.path()).await;
    let app = app(state.clone());

    // 先跑一轮非流,写入 usage cache
    let request = Request::builder()
        .uri("/v1/messages")
        .method("POST")
        .header("content-type", "application/json")
        .header("x-claude-code-session-id", "cursor-count")
        .body(Body::from(
            serde_json::to_vec(&json!({
                "model": "cursor-auto",
                "max_tokens": 64,
                "messages": [{"role": "user", "content": "hi"}]
            }))
            .unwrap(),
        ))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);

    // count_tokens:同 session 命中 cache
    let request = Request::builder()
        .uri("/v1/messages/count_tokens")
        .method("POST")
        .header("content-type", "application/json")
        .header("x-claude-code-session-id", "cursor-count")
        .body(Body::from(
            serde_json::to_vec(&json!({
                "model": "cursor-auto",
                "messages": [{"role": "user", "content": "hi"}]
            }))
            .unwrap(),
        ))
        .unwrap();
    let response = app.clone().oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    assert_eq!(body, bytes::Bytes::from_static(br#"{"input_tokens":33}"#));

    // 未知 session:返回 0,不请求 sidecar
    let request = Request::builder()
        .uri("/v1/messages/count_tokens")
        .method("POST")
        .header("content-type", "application/json")
        .header("x-claude-code-session-id", "cursor-unknown")
        .body(Body::from(
            serde_json::to_vec(&json!({
                "model": "cursor-auto",
                "messages": [{"role": "user", "content": "hi"}]
            }))
            .unwrap(),
        ))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    assert_eq!(body, bytes::Bytes::from_static(br#"{"input_tokens":0}"#));
}

/// Cursor 请求不得触碰 generic upstream:同 snapshot 内并存 openai_chat
/// provider 时,cursor 路由命中 sidecar,generic upstream 调用次数为 0
#[tokio::test]
async fn test_cursor_sdk_never_calls_generic_upstream() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let counter = std::sync::Arc::new(AtomicUsize::new(0));
    let captured = counter.clone();
    let generic = Router::new().route(
        "/v1/chat/completions",
        post(move |_body: bytes::Bytes| {
            let captured = captured.clone();
            async move {
                captured.fetch_add(1, Ordering::SeqCst);
                (
                    StatusCode::OK,
                    bytes::Bytes::from_static(br#"{"choices":[{"message":{"content":"x"}}]}"#),
                )
            }
        }),
    );
    let generic_server = crate::test_support::TestServer::spawn(generic).await;

    let frames = concat!(
        "data: {\"type\":\"text_delta\",\"text\":\"hello\"}\n\n",
        "data: {\"type\":\"turn_end\",\"stop_reason\":\"end_turn\"}\n\n",
    );
    let sidecar = crate::test_support::TestServer::spawn(cursor_sidecar_router(frames)).await;
    let auth_dir = tempfile::tempdir().unwrap();
    write_cursor_credential(auth_dir.path());
    let state = cursor_test_state(sidecar.url.clone(), auth_dir.path()).await;
    let generic_provider: ProviderConfig = serde_yaml::from_str(&format!(
        r#"
name: generic
protocol: openai_chat
base_url: {}
key: sk-test
models:
  - name: gpt-test
    alias: generic-gpt
"#,
        generic_server.url
    ))
    .unwrap();
    Arc::make_mut(&mut *state.config.write().await)
        .providers
        .push(generic_provider);
    let app = app(state);

    let request = Request::builder()
        .uri("/v1/messages")
        .method("POST")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&json!({
                "model": "cursor-auto",
                "max_tokens": 64,
                "stream": true,
                "messages": [{"role": "user", "content": "hi"}]
            }))
            .unwrap(),
        ))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(counter.load(Ordering::SeqCst), 0);
}

/// sidecar 429:Anthropic error 形状 + Retry-After 透传
#[tokio::test]
async fn test_cursor_sdk_sidecar_429_maps_anthropic_error_with_retry_after() {
    let sidecar = Router::new().route(
        "/run",
        post(|| async {
            (
                StatusCode::TOO_MANY_REQUESTS,
                [(header::RETRY_AFTER, "9")],
                "rate limited",
            )
        }),
    );
    let server = crate::test_support::TestServer::spawn(sidecar).await;
    let auth_dir = tempfile::tempdir().unwrap();
    write_cursor_credential(auth_dir.path());
    let state = cursor_test_state(server.url.clone(), auth_dir.path()).await;
    let app = app(state);

    let request = Request::builder()
        .uri("/v1/messages")
        .method("POST")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&json!({
                "model": "cursor-auto",
                "max_tokens": 64,
                "stream": true,
                "messages": [{"role": "user", "content": "hi"}]
            }))
            .unwrap(),
        ))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::TOO_MANY_REQUESTS);
    assert_eq!(
        response
            .headers()
            .get("retry-after")
            .and_then(|v| v.to_str().ok()),
        Some("9")
    );
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    let error: Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(error["type"], "error");
}

/// 图片输入:Cursor SDK sidecar 接收 base64 图片并返回正常响应
#[tokio::test]
async fn test_cursor_sdk_image_input_is_forwarded() {
    let frames = "data: {\"type\":\"turn_end\",\"stop_reason\":\"end_turn\"}\n\n";
    let sidecar = crate::test_support::TestServer::spawn(cursor_sidecar_router(frames)).await;
    let auth_dir = tempfile::tempdir().unwrap();
    write_cursor_credential(auth_dir.path());
    let state = cursor_test_state(sidecar.url.clone(), auth_dir.path()).await;
    let app = app(state);

    let request = Request::builder()
        .uri("/v1/messages")
        .method("POST")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&json!({
                "model": "cursor-auto",
                "max_tokens": 64,
                "messages": [{
                    "role": "user",
                    "content": [
                        {"type": "image", "source": {
                            "type": "base64",
                            "media_type": "image/png",
                            "data": "aGk="
                        }}
                    ]
                }]
            }))
            .unwrap(),
        ))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    assert!(!body.is_empty());
}

/// 首帧 error:不本地重试(D11 退避交客户端),/run 恰好 1 次,返回 502
#[tokio::test]
async fn test_cursor_sdk_first_frame_error_fails_fast_without_retry() {
    use std::sync::atomic::{AtomicUsize, Ordering};
    let counter = std::sync::Arc::new(AtomicUsize::new(0));
    let captured = counter.clone();
    let router = Router::new().route(
        "/run",
        post(move |_headers: HeaderMap, _body: bytes::Bytes| {
            let captured = captured.clone();
            async move {
                captured.fetch_add(1, Ordering::SeqCst);
                (
                    StatusCode::OK,
                    [(header::CONTENT_TYPE, "text/event-stream")],
                    "data: {\"type\":\"error\",\"code\":\"x\",\"message\":\"boom\"}\n\n",
                )
            }
        }),
    );
    let sidecar = crate::test_support::TestServer::spawn(router).await;
    let auth_dir = tempfile::tempdir().unwrap();
    write_cursor_credential(auth_dir.path());
    let state = cursor_test_state(sidecar.url.clone(), auth_dir.path()).await;
    let app = app(state);

    let request = Request::builder()
        .uri("/v1/messages")
        .method("POST")
        .header("content-type", "application/json")
        .body(Body::from(
            serde_json::to_vec(&json!({
                "model": "cursor-auto",
                "max_tokens": 64,
                "stream": true,
                "messages": [{"role": "user", "content": "hi"}]
            }))
            .unwrap(),
        ))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::BAD_GATEWAY);
    // D11:Cursor 路径不本地重试,首帧 error 快速失败
    assert_eq!(counter.load(Ordering::SeqCst), 1);
}

/// /reload 禁用 Cursor:sidecar 停止,运行时清空
#[tokio::test]
async fn test_reload_disables_cursor_runtime() {
    let auth_dir = tempfile::tempdir().unwrap();
    let server = crate::test_support::TestServer::spawn(cursor_sidecar_router("")).await;
    let state = cursor_test_state(server.url.clone(), auth_dir.path()).await;
    let runtime = state.cursor.read().unwrap().clone().unwrap();

    crate::http::handlers::reload::reconcile_cursor_runtime(&state, None).await;

    assert!(state.cursor.read().unwrap().is_none());
    // sidecar 已 shutdown:后续 run 返回 NotReady,不再发请求
    let error = runtime
        .sidecar
        .run(&json!({"model": "auto"}))
        .await
        .unwrap_err();
    assert!(matches!(error, crate::cursor::CursorSidecarError::NotReady));
}

/// /reload 保留启用:配置原子更新,catalog 按白名单重新合成并发布
#[tokio::test]
async fn test_reload_reconciles_cursor_config_and_refreshes_catalog() {
    let auth_dir = tempfile::tempdir().unwrap();
    write_cursor_credential(auth_dir.path());
    let router = Router::new().route(
        "/models",
        post(|headers: HeaderMap, body: bytes::Bytes| async move {
            assert_eq!(
                headers.get("authorization").and_then(|v| v.to_str().ok()),
                Some("Bearer test-token")
            );
            let payload: Value = serde_json::from_slice(&body).unwrap();
            assert_eq!(payload["apiKey"], "test-access-token");
            (
                StatusCode::OK,
                [(header::CONTENT_TYPE, "application/json")],
                bytes::Bytes::from(
                    serde_json::to_vec(
                        &json!({"models": [{"id": "auto"}, {"id": "composer-2.5"}]}),
                    )
                    .unwrap(),
                ),
            )
        }),
    );
    let server = crate::test_support::TestServer::spawn(router).await;
    let state = cursor_test_state(server.url.clone(), auth_dir.path()).await;

    let desired = crate::cursor::CursorConfig {
        auth_dir: auth_dir.path().to_path_buf(),
        models: vec!["default".to_string()],
        idle_secs: 60,
        max_agents: 2,
        workspace_dir: std::path::PathBuf::from("/tmp"),
    };
    crate::http::handlers::reload::reconcile_cursor_runtime(&state, Some(desired)).await;

    // 运行时配置已原子更新
    let runtime = state.cursor.read().unwrap().clone().unwrap();
    let config = runtime.config.read().await;
    assert_eq!(config.idle_secs, 60);
    assert_eq!(config.max_agents, 2);
    drop(config);

    // catalog 已发布:cursor provider 模型集按白名单过滤,alias 归一 default
    let snapshot = state.config.read().await.clone();
    let cursor = snapshot
        .providers
        .iter()
        .find(|p| p.name == "cursor")
        .expect("cursor provider 已发布");
    assert_eq!(cursor.models.len(), 1);
    assert_eq!(cursor.models[0].name, "auto");
    assert_eq!(cursor.models[0].alias, "default");
}

/// /reload 目录拉取失败:保留快照中现有 cursor provider,不发布空集
#[tokio::test]
async fn test_reload_cursor_catalog_failure_retains_provider() {
    let auth_dir = tempfile::tempdir().unwrap();
    write_cursor_credential(auth_dir.path());
    let router = Router::new().route(
        "/models",
        post(|| async {
            (
                StatusCode::SERVICE_UNAVAILABLE,
                [(header::CONTENT_TYPE, "application/json")],
                bytes::Bytes::from_static(br#"{"error":"upstream down"}"#),
            )
        }),
    );
    let server = crate::test_support::TestServer::spawn(router).await;
    let state = cursor_test_state(server.url.clone(), auth_dir.path()).await;
    let before = state.config.read().await.clone();

    let desired = crate::cursor::CursorConfig {
        auth_dir: auth_dir.path().to_path_buf(),
        models: vec![],
        idle_secs: 1800,
        max_agents: 16,
        workspace_dir: std::path::PathBuf::from("/tmp"),
    };
    crate::http::handlers::reload::reconcile_cursor_runtime(&state, Some(desired)).await;

    // 拉取失败:快照整体未变,原 cursor provider(model auto/alias cursor-auto)保留
    let after = state.config.read().await.clone();
    assert_eq!(after.version, before.version);
    let cursor = after
        .providers
        .iter()
        .find(|p| p.name == "cursor")
        .expect("cursor provider 保留");
    assert_eq!(cursor.models[0].alias, "cursor-auto");
}

/// node >= 24 且 sidecar 依赖已装(真实 spawn 测试守卫)
fn cursor_sidecar_prerequisites() -> bool {
    let sidecar_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("sidecar")
        .join("cursor");
    let sdk_installed = sidecar_dir
        .join("node_modules")
        .join("@cursor")
        .join("sdk")
        .join("package.json")
        .exists();
    let node_ok = std::process::Command::new("node")
        .arg("--version")
        .output()
        .map(|out| {
            let version = String::from_utf8_lossy(&out.stdout);
            version
                .trim_start_matches('v')
                .split('.')
                .next()
                .and_then(|major| major.parse::<u32>().ok())
                >= Some(24)
        })
        .unwrap_or(false);
    sdk_installed && node_ok
}

/// /reload 从禁用到启用:sidecar 真实启动,运行时注入,catalog 失败不回滚启用
#[tokio::test]
async fn test_reload_enables_cursor_runtime_from_disabled() {
    if !cursor_sidecar_prerequisites() {
        eprintln!("skipping: sidecar node_modules or node >= 24 unavailable");
        return;
    }
    let state = mock_state();
    assert!(state.cursor.read().unwrap().is_none());
    let auth_dir = tempfile::tempdir().unwrap();
    write_cursor_credential(auth_dir.path());

    let desired = crate::cursor::CursorConfig {
        auth_dir: auth_dir.path().to_path_buf(),
        models: vec![],
        idle_secs: 1800,
        max_agents: 16,
        workspace_dir: std::path::PathBuf::from("/tmp"),
    };
    crate::http::handlers::reload::reconcile_cursor_runtime(&state, Some(desired)).await;

    // sidecar 已启动且健康;catalog 拉取失败(测试凭证无法过真实 SDK)不撤销启用
    let runtime = state
        .cursor
        .read()
        .unwrap()
        .clone()
        .expect("cursor runtime enabled");
    assert!(runtime.sidecar.health().await);
    runtime.sidecar.shutdown().await;
}

/// /reload auth_dir 变化:sidecar 受控重启指向新目录,配置原子更新
#[tokio::test]
async fn test_reload_cursor_auth_dir_change_restarts_sidecar() {
    if !cursor_sidecar_prerequisites() {
        eprintln!("skipping: sidecar node_modules or node >= 24 unavailable");
        return;
    }
    let server = crate::test_support::TestServer::spawn(cursor_sidecar_router("")).await;
    let dir_a = tempfile::tempdir().unwrap();
    write_cursor_credential(dir_a.path());
    let state = cursor_test_state(server.url.clone(), dir_a.path()).await;
    let dir_b = tempfile::tempdir().unwrap();
    write_cursor_credential(dir_b.path());

    let desired = crate::cursor::CursorConfig {
        auth_dir: dir_b.path().to_path_buf(),
        models: vec![],
        idle_secs: 1800,
        max_agents: 16,
        workspace_dir: std::path::PathBuf::from("/tmp"),
    };
    crate::http::handlers::reload::reconcile_cursor_runtime(&state, Some(desired)).await;

    // sidecar 重启后 auth_dir 指向新目录;运行时配置同步
    let runtime = state.cursor.read().unwrap().clone().unwrap();
    assert_eq!(runtime.sidecar.auth_dir().await, dir_b.path());
    let config = runtime.config.read().await;
    assert_eq!(config.auth_dir, dir_b.path());
    drop(config);
    runtime.sidecar.shutdown().await;
}

#[tokio::test]
async fn serve_shutdown_stops_cursor_sidecar() {
    let sidecar = crate::test_support::TestServer::spawn(cursor_sidecar_router("")).await;
    let auth_dir = tempfile::tempdir().unwrap();
    let state = cursor_test_state(sidecar.url.clone(), auth_dir.path()).await;
    let runtime = state.cursor.read().unwrap().clone().unwrap();
    let (shutdown_tx, shutdown_rx) = tokio::sync::oneshot::channel();
    shutdown_tx.send(()).unwrap();

    crate::http::serve_with_shutdown("127.0.0.1:0", state, async move {
        shutdown_rx.await.unwrap();
    })
    .await
    .unwrap();

    assert!(!runtime.sidecar.health().await);
}
