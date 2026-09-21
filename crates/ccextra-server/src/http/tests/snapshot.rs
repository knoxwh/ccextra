use super::*;
use axum::extract::State;
use std::sync::atomic::AtomicUsize;
use std::task::Poll;
use tokio::sync::{Notify, Semaphore};

#[tokio::test]
async fn concurrent_reload_serializes_loading_and_publication() {
    let mut state = mock_state();
    let entered = Arc::new(Notify::new());
    let release = Arc::new(Semaphore::new(0));
    let calls = Arc::new(AtomicUsize::new(0));
    state.reload = {
        let entered = entered.clone();
        let release = release.clone();
        let calls = calls.clone();
        Arc::new(move || {
            let entered = entered.clone();
            let release = release.clone();
            let index = calls.fetch_add(1, Ordering::SeqCst);
            Box::pin(async move {
                if index == 0 {
                    entered.notify_one();
                    release.acquire().await.unwrap().forget();
                }
                reload_returning_secret(Some(format!("secret-{index}")))().await
            })
        })
    };
    let mut tasks = tokio::task::JoinSet::new();
    let first_state = state.clone();
    tasks.spawn(async move { handle_reload(State(first_state)).await.unwrap() });
    tokio::time::timeout(std::time::Duration::from_secs(2), entered.notified())
        .await
        .unwrap();
    let second = handle_reload(State(state.clone()));
    tokio::pin!(second);
    // 第一个加载尚未完成，第二个不能提前加载和发布旧文件内容。
    let mut context = std::task::Context::from_waker(std::task::Waker::noop());
    assert!(matches!(second.as_mut().poll(&mut context), Poll::Pending));
    assert_eq!(calls.load(Ordering::SeqCst), 1);
    release.add_permits(1);
    tasks.join_next().await.unwrap().unwrap();
    second.await.unwrap();
    let snapshot = state.config.read().await;
    assert_eq!(snapshot.version, 3);
    assert_eq!(snapshot.runtime.secret.as_deref(), Some("secret-1"));
}

// 捕获真实出站 body 和头，验证入站 body 等待期间 reload 不会切换请求版本。
async fn request_retains_snapshot_across_reload(endpoint: &'static str) {
    let (server, captured) = crate::test_support::spawn_captured_server(
        endpoint,
        StatusCode::OK,
        "{\"input_tokens\":42}",
    )
    .await;
    let mut state = mock_state();
    {
        let mut config = state.config.write().await;
        let snapshot = Arc::make_mut(&mut config);
        snapshot.providers[0].set_base_url_for_test(server.url.clone());
        snapshot.runtime.secret = Some("old-key".into());
        snapshot.runtime.user_agents.claude_cli = Arc::new("snapshot-old-agent".into());
        snapshot.payload_rules.push(PayloadRule {
            models: vec!["test-opus".into()],
            protocol: Some(Protocol::Claude),
            params: serde_json::from_value(json!({"max_tokens": 17})).unwrap(),
        });
    }
    state.reload = reload_returning_secret(Some("new-key".into()));
    let router = app(state);
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (release_tx, release_rx) = tokio::sync::oneshot::channel();
    let body = Body::from_stream(futures::stream::once(async move {
        entered_tx.send(()).unwrap();
        release_rx.await.unwrap();
        Ok::<_, std::io::Error>(bytes::Bytes::from_static(
            br#"{"model":"test-opus","stream":false,"messages":[{"role":"user","content":"hi"}]}"#,
        ))
    }));
    let request = Request::builder()
        .method("POST")
        .uri(endpoint)
        .header("x-api-key", "old-key")
        .body(body)
        .unwrap();
    let inflight = router.clone();
    let mut tasks = tokio::task::JoinSet::new();
    tasks.spawn(async move { inflight.oneshot(request).await.unwrap() });
    tokio::time::timeout(std::time::Duration::from_secs(2), entered_rx)
        .await
        .unwrap()
        .unwrap();
    let reload = Request::builder()
        .method("POST")
        .uri("/reload")
        .body(Body::empty())
        .unwrap();
    let response = tokio::time::timeout(
        std::time::Duration::from_secs(2),
        router.clone().oneshot(reload),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    release_tx.send(()).unwrap();
    let response = tokio::time::timeout(std::time::Duration::from_secs(2), tasks.join_next())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "旧请求继续使用已删除的旧路由"
    );
    assert_eq!(
        captured.header("user-agent").as_deref(),
        Some("snapshot-old-agent")
    );
    if endpoint == "/v1/messages" {
        assert_eq!(captured.body()["model"], "claude-opus-5");
        assert_eq!(captured.body()["max_tokens"], 17);
    } else {
        assert_eq!(captured.body()["model"], "test-opus");
    }
    let old_key = Request::builder()
        .uri("/v1/models")
        .header("x-api-key", "old-key")
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        router.clone().oneshot(old_key).await.unwrap().status(),
        StatusCode::UNAUTHORIZED
    );
    let new_key = Request::builder()
        .uri("/v1/models")
        .header("x-api-key", "new-key")
        .body(Body::empty())
        .unwrap();
    let response = router.oneshot(new_key).await.unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body = to_bytes(response.into_body(), usize::MAX).await.unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&body).unwrap()["data"],
        json!([])
    );
}

#[tokio::test]
async fn messages_retains_snapshot_across_reload() {
    request_retains_snapshot_across_reload("/v1/messages").await;
}

#[tokio::test]
async fn count_tokens_retains_snapshot_across_reload() {
    request_retains_snapshot_across_reload("/v1/messages/count_tokens").await;
}

#[tokio::test]
async fn failed_reload_or_refresh_preserves_the_entire_snapshot() {
    let mut state = mock_state();
    let old = Arc::clone(&*state.config.read().await);
    let mut conflicting = old.providers.clone();
    conflicting[1].models[0].alias = "test-opus".into();
    assert!(
        publish_refreshed_providers(&state.config, old.version, conflicting.clone())
            .await
            .is_err()
    );
    assert!(Arc::ptr_eq(&old, &*state.config.read().await));
    state.reload = Arc::new(move || {
        let providers = conflicting.clone();
        Box::pin(async move {
            let mut data = reload_returning_secret(Some("must-not-publish".into()))().await?;
            data.providers = providers;
            Ok(data)
        })
    });
    assert!(handle_reload(State(state.clone())).await.is_err());
    assert!(Arc::ptr_eq(&old, &*state.config.read().await));
    state.reload = Arc::new(|| Box::pin(async { anyhow::bail!("invalid configuration") }));
    assert!(handle_reload(State(state.clone())).await.is_err());
    assert!(Arc::ptr_eq(&old, &*state.config.read().await));
    // 失败不能占用版本，也不能让后续合法 reload 永久等待。
    state.reload = reload_returning_secret(None);
    tokio::time::timeout(
        std::time::Duration::from_secs(2),
        handle_reload(State(state.clone())),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(state.config.read().await.version, 2);
}

#[tokio::test]
async fn unchanged_refresh_does_not_publish_a_new_version() {
    let state = mock_state();
    let old = Arc::clone(&*state.config.read().await);
    assert!(
        !publish_refreshed_providers(&state.config, old.version, old.providers.clone())
            .await
            .unwrap()
    );
    assert!(Arc::ptr_eq(&old, &*state.config.read().await));
}

#[tokio::test]
async fn valid_refresh_changes_only_providers_and_version() {
    let state = mock_state();
    let old = Arc::clone(&*state.config.read().await);
    let mut providers = old.providers.clone();
    providers[0].models[0].alias = "refreshed-opus".into();
    assert!(
        publish_refreshed_providers(&state.config, old.version, providers)
            .await
            .unwrap()
    );
    let current = state.config.read().await;
    assert_eq!(current.version, 2);
    assert_eq!(current.providers[0].models[0].alias, "refreshed-opus");
    assert_eq!(old.providers[0].models[0].alias, "test-opus");
    assert!(Arc::ptr_eq(
        &old.runtime.thinking_registry,
        &current.runtime.thinking_registry
    ));
    assert_eq!(current.runtime.secret, old.runtime.secret);
    assert_eq!(
        current.refresh.static_providers,
        old.refresh.static_providers
    );
}
