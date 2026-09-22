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

#[derive(Clone, Default)]
pub(crate) struct CapturedUpstream {
    pub headers: std::sync::Arc<std::sync::Mutex<Option<axum::http::HeaderMap>>>,
    pub body: std::sync::Arc<std::sync::Mutex<Option<serde_json::Value>>>,
    /// 原始字节(压缩断言用;JSON 解析失败时 body() 为空)
    pub raw_body: std::sync::Arc<std::sync::Mutex<Option<Bytes>>>,
}

impl CapturedUpstream {
    pub fn record(&self, headers: axum::http::HeaderMap, body: Bytes) {
        *self.headers.lock().unwrap() = Some(headers);
        *self.body.lock().unwrap() = serde_json::from_slice(&body).ok();
        *self.raw_body.lock().unwrap() = Some(body);
    }

    pub fn header(&self, name: &str) -> Option<String> {
        self.headers.lock().unwrap().as_ref().and_then(|h| {
            h.get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        })
    }

    pub fn header_values(&self, name: &str) -> Vec<String> {
        self.headers
            .lock()
            .unwrap()
            .as_ref()
            .map(|headers| {
                headers
                    .get_all(name)
                    .iter()
                    .filter_map(|value| value.to_str().ok().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn has_header(&self, name: &str) -> bool {
        self.headers
            .lock()
            .unwrap()
            .as_ref()
            .map(|h| h.contains_key(name))
            .unwrap_or(false)
    }

    pub fn body(&self) -> serde_json::Value {
        self.body
            .lock()
            .unwrap()
            .clone()
            .unwrap_or(serde_json::json!({}))
    }
}

pub(crate) async fn spawn_captured_server(
    route: &str,
    status: StatusCode,
    resp_body: impl Into<Bytes>,
) -> (TestServer, CapturedUpstream) {
    let captured = CapturedUpstream::default();
    let cap = captured.clone();
    let resp_body = resp_body.into();
    let router = Router::new().route(
        route,
        axum::routing::post(move |headers: axum::http::HeaderMap, body: Bytes| {
            let cap = cap.clone();
            let resp_body = resp_body.clone();
            async move {
                cap.record(headers, body);
                (
                    status,
                    [(axum::http::header::CONTENT_TYPE, "application/json")],
                    resp_body,
                )
            }
        }),
    );
    (TestServer::spawn(router).await, captured)
}
