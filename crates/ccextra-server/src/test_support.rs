use axum::{body::Body, http::StatusCode, Router};
use bytes::Bytes;

pub(crate) struct TestServer {
    pub url: String,
    task: tokio::task::JoinHandle<()>,
}

impl TestServer {
    pub async fn spawn(router: Router) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let task = tokio::spawn(async move { axum::serve(listener, router).await.unwrap() });
        Self { url, task }
    }

    pub async fn reply(status: StatusCode, body: impl Into<Bytes>) -> Self {
        let body = body.into();
        Self::spawn(Router::new().fallback(move || {
            let body = body.clone();
            async move { (status, body) }
        }))
        .await
    }
}

impl Drop for TestServer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

// 合法 JSON 后补空白,超限时不能靠解析失败误打误撞通过。
pub(crate) fn padded_json(json: &str, len: usize) -> Bytes {
    serde_json::from_str::<serde_json::Value>(json).unwrap();
    let mut body = json.as_bytes().to_vec();
    body.resize(len.max(body.len()), b' ');
    body.into()
}

pub(crate) fn response(status: StatusCode, body: impl Into<reqwest::Body>) -> reqwest::Response {
    axum::http::Response::builder()
        .status(status)
        .body(body.into())
        .unwrap()
        .into()
}

pub(crate) fn stalled_body() -> Body {
    use futures::StreamExt;
    Body::from_stream(
        futures::stream::iter([Ok::<_, std::io::Error>(Bytes::from_static(b"{"))])
            .chain(futures::stream::pending()),
    )
}
