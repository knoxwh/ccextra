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
const TEST_GROK_VERSION: &str = "1.0.5";
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

/// /reload 清空 bcrypt 校验缓存:旧 secret 的 hash 命中过缓存后,
/// 重载换新 secret,旧明文 key 不得再通过。
#[tokio::test]
async fn test_reload_clears_auth_cache() {
    let old_hash = bcrypt::hash("sk-old", 4).unwrap();
    let new_hash = bcrypt::hash("sk-new", 4).unwrap();
    let mut state = mock_state();
    Arc::make_mut(&mut *state.config.write().await)
        .runtime
        .secret = Some(old_hash.clone());
    state.reload = reload_returning_secret(Some(new_hash));
    let app = app(state);

    let with_key = |k: &str| {
        Request::builder()
            .uri("/v1/models")
            .header("x-api-key", k)
            .body(Body::empty())
            .unwrap()
    };
    // 先命中一次,把 sk-old→true 写进 AUTH_CACHE
    let r = app.clone().oneshot(with_key("sk-old")).await.unwrap();
    assert_eq!(r.status(), StatusCode::OK);
    assert!(
        crate::http::auth::auth_cache_contains(&old_hash, "sk-old"),
        "reload 前全局缓存应含旧 hash 条目"
    );

    let reload = Request::builder()
        .uri("/reload")
        .method("POST")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        app.clone().oneshot(reload).await.unwrap().status(),
        StatusCode::OK
    );
    // reload 必须实际清掉该条目(删掉 handle_reload 的 clear 则此处失败)
    assert!(
        !crate::http::auth::auth_cache_contains(&old_hash, "sk-old"),
        "reload 应清掉旧 hash 的缓存条目"
    );

    let stale = app.clone().oneshot(with_key("sk-old")).await.unwrap();
    assert_eq!(
        stale.status(),
        StatusCode::UNAUTHORIZED,
        "旧 key 的缓存结果应随 /reload 作废"
    );
    let fresh = app.oneshot(with_key("sk-new")).await.unwrap();
    assert_eq!(fresh.status(), StatusCode::OK, "新 secret 应校验通过");
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
        ua.starts_with("grok-shell/1.0.5 ("),
        "UA 应为 grok-shell,实际 {ua}"
    );
    assert_eq!(
        captured.header("x-xai-token-auth").as_deref(),
        Some("xai-grok-cli")
    );
    assert_eq!(
        captured.header("x-grok-client-version").as_deref(),
        Some("1.0.5")
    );
    assert_eq!(
        captured.header("x-grok-client-identifier").as_deref(),
        Some("grok-shell")
    );
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
    assert!(ua.starts_with("grok-shell/1.0.5 ("));
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
        ua.starts_with("grok-shell/1.0.5 ("),
        "UA 应为 grok-shell,实际 {ua}"
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

/// 上游 500:按退避重试后成功。
#[tokio::test]
async fn test_upstream_500_retries_with_backoff_then_succeeds() {
    let attempts = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let handler_attempts = Arc::clone(&attempts);
    let upstream = Router::new().route(
            "/chat/completions",
            post(move || {
                let attempts = Arc::clone(&handler_attempts);
                async move {
                    if attempts.fetch_add(1, Ordering::SeqCst) < 2 {
                        (
                            axum::http::StatusCode::INTERNAL_SERVER_ERROR,
                            [(header::CONTENT_TYPE, "application/json")],
                            r#"{"error":{"message":"overloaded"}}"#,
                        )
                    } else {
                        (
                            axum::http::StatusCode::OK,
                            [(header::CONTENT_TYPE, "application/json")],
                            r#"{"id":"c1","object":"chat.completion","model":"gpt-4","choices":[{"index":0,"message":{"role":"assistant","content":"retry ok"},"finish_reason":"stop"}],"usage":{"prompt_tokens":1,"completion_tokens":1}}"#,
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
            "name: test-500-retry\nprotocol: openai_chat\nbase_url: http://{}\nkey: sk-test\nproxy_url: direct\nmodels:\n  - name: gpt-4\n    alias: test-500-retry\n",
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
                "model": "test-500-retry", "max_tokens": 64, "stream": false,
                "messages": [{"role": "user", "content": "hi"}]
            }))
            .unwrap(),
        ))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    server.abort();

    assert_eq!(response.status(), StatusCode::OK);
    assert_eq!(attempts.load(Ordering::SeqCst), 3);
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

/// 上游持续 503 耗尽退避预算:末次错误响应转 anthropic error 返回,
/// 不返回 500 内部错误,客户端能拿到上游原始错误信息。
#[tokio::test]
async fn test_upstream_persistent_503_exhausts_budget_returns_error_shape() {
    let upstream = Router::new().route(
        "/chat/completions",
        post(|| async {
            (
                axum::http::StatusCode::SERVICE_UNAVAILABLE,
                [(header::CONTENT_TYPE, "application/json")],
                r#"{"error":{"message":"down"}}"#,
            )
        }),
    );
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let upstream_addr = listener.local_addr().unwrap();
    let server = tokio::spawn(async move { axum::serve(listener, upstream).await.unwrap() });

    let state = mock_state();
    let provider: ProviderConfig = serde_yaml::from_str(&format!(
            "name: test-503-exhaust\nprotocol: openai_chat\nbase_url: http://{}\nkey: sk-test\nproxy_url: direct\nmodels:\n  - name: gpt-4\n    alias: test-503-exhaust\n",
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
                "model": "test-503-exhaust", "max_tokens": 64, "stream": false,
                "messages": [{"role": "user", "content": "hi"}]
            }))
            .unwrap(),
        ))
        .unwrap();
    let response = app.oneshot(request).await.unwrap();
    server.abort();

    assert_eq!(
        response.status(),
        axum::http::StatusCode::SERVICE_UNAVAILABLE
    );
    let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
        .await
        .unwrap();
    let v: Value = serde_json::from_slice(&bytes).unwrap();
    assert_eq!(v["type"], "error", "错误必须转 anthropic error 形状");
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
    for refused in [true, false] {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let first = if refused {
            format!("http://{}", listener.local_addr().unwrap())
        } else {
            limited.url.clone()
        };
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
    assert_eq!(
        captured.header("authorization").as_deref(),
        Some("Bearer sk-static")
    );
}
