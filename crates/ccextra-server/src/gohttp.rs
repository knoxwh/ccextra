//! Antigravity 数据面可选出口:同目录 `ccextra-gohttp`(Go `net/http`)Unix-socket 转发器。
//!
//! 开关 `antigravity.gohttp`(默认 false = reqwest)。打开后由 ccextra 自己拉起 sidecar、
//! 自己收掉(SIGTERM + 删 socket);/reload 关掉开关后旧 client 释放即停,打开即拉起。
//! 协议:向 socket 发绝对形请求行 `POST https://host/path HTTP/1.1`,sidecar 不跟随 3xx。
//! 代理:已解析的非 direct 代理经 [`PROXY_HEADER`] 逐请求传给 sidecar(它剥掉不外发);direct 不加头。

use std::os::unix::fs::DirBuilderExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, Weak};
use std::time::Duration;

use axum::http::{header, HeaderMap, HeaderValue};
use bytes::Bytes;

pub(crate) const BINARY_NAME: &str = "ccextra-gohttp";
/// 逐请求代理头(值可能含凭证:不记日志、不进错误信息)
const PROXY_HEADER: &str = "x-ccextra-proxy";
/// sidecar 刚拉起时 socket 可能尚未 listen,首连最多等这么久
const STARTUP_WAIT: Duration = Duration::from_secs(2);

/// 回程剥掉的 hop-by-hop 头(与 sidecar 去程 hopHeaders 同集)
const HOP_HEADERS: [&str; 8] = [
    "connection",
    "keep-alive",
    "proxy-connection",
    "proxy-authorization",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// 进程内当前 sidecar;强引用由 UpstreamClient / 在途响应体持有,全部释放即 SIGTERM。
static CURRENT: Mutex<Weak<Sidecar>> = Mutex::new(Weak::new());
static SEQ: AtomicU64 = AtomicU64::new(0);

#[derive(Debug)]
pub struct Sidecar {
    socket: PathBuf,
    /// None:已收掉,或测试接入的外部 socket(不由我们拉起/删除)
    child: Mutex<Option<Child>>,
}

/// 取当前存活的 sidecar,没有则拉起。`socket_dir` 为配置文件旁 `.cache`。
/// 找不到二进制时返回明确错误(调用方不得回退 reqwest)。
pub(crate) fn acquire(socket_dir: Option<&Path>) -> anyhow::Result<Arc<Sidecar>> {
    let mut current = CURRENT.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(s) = current.upgrade() {
        if s.is_running() {
            return Ok(s);
        }
    }
    let exe = std::env::current_exe()
        .map_err(|e| anyhow::anyhow!("antigravity.gohttp: 无法定位当前可执行文件: {e}"))?;
    let bin = exe
        .parent()
        .map(|d| d.join(BINARY_NAME))
        .ok_or_else(|| anyhow::anyhow!("antigravity.gohttp: 可执行文件无父目录"))?;
    let dir =
        socket_dir.ok_or_else(|| anyhow::anyhow!("antigravity.gohttp: 未设置 socket 目录"))?;
    let s = Arc::new(spawn_at(&bin, dir)?);
    *current = Arc::downgrade(&s);
    Ok(s)
}

type Launch = Box<dyn Fn(Option<&Path>) -> anyhow::Result<Arc<Sidecar>> + Send + Sync>;

/// 一个 UpstreamClient 持有的 sidecar 引用。请求前发现子进程已退出(崩溃 / 被杀)
/// 或首次拉起失败,就在该次请求里重新 [`acquire`] 一次,不必等 /reload;仍失败原样报错(不回退)。
pub(crate) struct Lane {
    dir: Option<PathBuf>,
    launch: Launch,
    current: Mutex<Result<Arc<Sidecar>, String>>,
}

impl std::fmt::Debug for Lane {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Lane")
            .field("dir", &self.dir)
            .finish_non_exhaustive()
    }
}

impl Lane {
    pub(crate) fn new(socket_dir: Option<&Path>) -> Self {
        Self::with_launch(socket_dir, Box::new(acquire))
    }

    fn with_launch(socket_dir: Option<&Path>, launch: Launch) -> Self {
        let lane = Self {
            dir: socket_dir.map(Path::to_path_buf),
            launch,
            current: Mutex::new(Err(String::new())),
        };
        *lane.current.lock().unwrap_or_else(|p| p.into_inner()) = lane.launch_once();
        lane
    }

    fn launch_once(&self) -> Result<Arc<Sidecar>, String> {
        (self.launch)(self.dir.as_deref()).map_err(|e| {
            tracing::error!("{e}");
            e.to_string()
        })
    }

    /// 取可用 sidecar:当前的仍在跑就复用,否则本次请求重拉一次(acquire 幂等)。
    /// 换下的旧 Arc 释放时 terminate 只删它自己的 socket(pid+seq 命名),已回收的 pid 不 kill。
    pub(crate) fn get(&self) -> anyhow::Result<Arc<Sidecar>> {
        let mut current = self.current.lock().unwrap_or_else(|p| p.into_inner());
        if let Ok(s) = &*current {
            if s.child_alive_or_external() {
                return Ok(Arc::clone(s));
            }
            tracing::warn!(socket = %s.socket.display(), "ccextra-gohttp 已退出,重新拉起");
        }
        *current = self.launch_once();
        current
            .as_ref()
            .map(Arc::clone)
            .map_err(|e| anyhow::anyhow!("{e}"))
    }

    #[cfg(test)]
    pub(crate) fn fixed_for_test(lane: Result<Arc<Sidecar>, String>) -> Self {
        Self {
            dir: None,
            launch: Box::new(|_| anyhow::bail!("test lane: no relaunch")),
            current: Mutex::new(lane),
        }
    }
}

/// 进程退出前收掉 sidecar(在途 Arc 仍在也立即停)。
pub fn shutdown() {
    let current = CURRENT.lock().unwrap_or_else(|p| p.into_inner());
    if let Some(s) = current.upgrade() {
        s.terminate();
    }
}

pub(crate) fn spawn_at(bin: &Path, dir: &Path) -> anyhow::Result<Sidecar> {
    if !bin.is_file() {
        anyhow::bail!(
            "antigravity.gohttp 已开启,但找不到 {}(需与 ccextra 同目录);不回退 reqwest",
            bin.display()
        );
    }
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(|e| anyhow::anyhow!("antigravity.gohttp: 创建 {} 失败: {e}", dir.display()))?;
    // 每次拉起用独立文件名:热切换时旧进程收尾删自己的 socket,不误删新进程的
    let socket = dir.join(format!(
        "{BINARY_NAME}-{}-{}.sock",
        std::process::id(),
        SEQ.fetch_add(1, Ordering::Relaxed)
    ));
    let child = Command::new(bin)
        .arg("-socket")
        .arg(&socket)
        .stdin(Stdio::null())
        .spawn()
        .map_err(|e| anyhow::anyhow!("antigravity.gohttp: 拉起 {} 失败: {e}", bin.display()))?;
    tracing::info!(pid = child.id(), socket = %socket.display(), "ccextra-gohttp 已拉起");
    Ok(Sidecar {
        socket,
        child: Mutex::new(Some(child)),
    })
}

impl Sidecar {
    #[cfg(test)]
    pub(crate) fn attach_for_test(socket: PathBuf) -> Self {
        Self {
            socket,
            child: Mutex::new(None),
        }
    }

    /// 模拟崩溃:SIGKILL 自己拉起的子进程并回收(Child 留在原处,之后的 terminate 不得再 kill)
    #[cfg(test)]
    fn crash_for_test(&self) {
        let mut child = self.child.lock().unwrap();
        let c = child.as_mut().expect("self-spawned sidecar");
        c.kill().unwrap();
        c.wait().unwrap();
    }

    fn is_running(&self) -> bool {
        let mut child = self.child.lock().unwrap_or_else(|p| p.into_inner());
        matches!(child.as_mut().map(Child::try_wait), Some(Ok(None)))
    }

    /// SIGTERM + 删 socket;回收放后台线程,不阻塞调用方。幂等。
    fn terminate(&self) {
        let Some(mut child) = self.child.lock().unwrap_or_else(|p| p.into_inner()).take() else {
            return;
        };
        sigterm_if_running(&mut child, |pid| {
            // SAFETY: kill(2) 只读参数;pid 来自我们自己拉起、且刚确认尚未回收的子进程
            unsafe {
                libc::kill(pid, libc::SIGTERM);
            }
        });
        std::thread::spawn(move || {
            let _ = child.wait();
        });
        let _ = std::fs::remove_file(&self.socket);
        tracing::info!(socket = %self.socket.display(), "ccextra-gohttp 已停止");
    }

    async fn connect(&self) -> anyhow::Result<tokio::net::UnixStream> {
        let deadline = tokio::time::Instant::now() + STARTUP_WAIT;
        loop {
            match tokio::net::UnixStream::connect(&self.socket).await {
                Ok(s) => return Ok(s),
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::NotFound | std::io::ErrorKind::ConnectionRefused
                    ) && tokio::time::Instant::now() < deadline
                        && self.child_alive_or_external() =>
                {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                Err(e) => {
                    anyhow::bail!("连接 ccextra-gohttp {} 失败: {e}", self.socket.display())
                }
            }
        }
    }

    /// 外部接入(测试)视同存活;自拉起的子进程已退出则不再等 socket
    fn child_alive_or_external(&self) -> bool {
        let mut child = self.child.lock().unwrap_or_else(|p| p.into_inner());
        child
            .as_mut()
            .is_none_or(|c| matches!(c.try_wait(), Ok(None)))
    }

    /// 经 sidecar 发一次请求:绝对形 URI + Host 头,头/正文原样;等到响应头受 `wait` 约束。
    /// `proxy`:已解析的代理 URL(None = 直连,不加代理头)。
    pub(crate) async fn send(
        self: &Arc<Self>,
        request: reqwest::Request,
        proxy: Option<&str>,
        body: Bytes,
        wait: Duration,
    ) -> anyhow::Result<(reqwest::StatusCode, GoHttpBody)> {
        let url = request.url();
        let mut req = axum::http::Request::builder()
            .method(request.method().clone())
            .uri(url.as_str())
            .body(http_body_util::Full::new(body))
            .map_err(|e| anyhow::anyhow!("构建 gohttp 请求失败: {e}"))?;
        *req.headers_mut() = request.headers().clone();
        req.headers_mut().insert(
            header::HOST,
            HeaderValue::from_str(url.authority())
                .map_err(|e| anyhow::anyhow!("非法 Host: {e}"))?,
        );
        // 只认 ccextra 自己解析的代理:先删掉可能透传进来的同名头
        req.headers_mut().remove(PROXY_HEADER);
        if let Some(proxy) = proxy {
            req.headers_mut().insert(
                PROXY_HEADER,
                HeaderValue::from_str(proxy)
                    .map_err(|_| anyhow::anyhow!("antigravity.gohttp: 代理 URL 含非法字符"))?,
            );
        }
        let send = async {
            let stream = self.connect().await?;
            let (mut sender, conn) =
                hyper::client::conn::http1::handshake(hyper_util::rt::TokioIo::new(stream)).await?;
            tokio::spawn(async move {
                let _ = conn.await;
            });
            anyhow::Ok(sender.send_request(req).await?)
        };
        let resp = match tokio::time::timeout(wait, send).await {
            Ok(r) => r?,
            Err(_) => anyhow::bail!("上游请求超时 ({}s)", wait.as_secs()),
        };
        let (mut parts, body) = resp.into_parts();
        strip_hop_headers(&mut parts.headers);
        Ok((
            parts.status,
            GoHttpBody {
                headers: parts.headers,
                body,
                _sidecar: Arc::clone(self),
            },
        ))
    }
}

/// 仅在子进程仍未被回收时发信号。`is_running`/`child_alive_or_external` 的 `try_wait`
/// 可能已回收崩溃的 sidecar,其 pid 随后可被系统复用;此时不得再 kill(同 `Child::kill` 的守卫)。
fn sigterm_if_running(child: &mut Child, send: impl FnOnce(i32)) -> bool {
    if !matches!(child.try_wait(), Ok(None)) {
        return false;
    }
    let Ok(pid) = i32::try_from(child.id()) else {
        return false;
    };
    send(pid);
    true
}

impl Drop for Sidecar {
    fn drop(&mut self) {
        self.terminate();
    }
}

fn strip_hop_headers(headers: &mut HeaderMap) {
    for name in HOP_HEADERS {
        headers.remove(name);
    }
}

/// sidecar 响应体;持有 sidecar 强引用,流式响应读完前不会被收掉。
#[derive(Debug)]
pub struct GoHttpBody {
    pub(crate) headers: HeaderMap,
    pub(crate) body: hyper::body::Incoming,
    _sidecar: Arc<Sidecar>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::upstream::{AntigravityConfig, UpstreamClient};
    use ccextra_core::route::Protocol;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn uas() -> crate::http::UserAgentSet {
        crate::http::UserAgentSet {
            claude_cli: Arc::new("claude-cli/test".into()),
            codex_tui: Arc::new("codex/test".into()),
            grok_version: Arc::new("1.0.0".into()),
            antigravity: Arc::new("antigravity/hub/test".into()),
        }
    }

    async fn ag_request(
        client: &UpstreamClient,
        protocol: Protocol,
        base_url: &str,
        proxy: &str,
    ) -> anyhow::Result<crate::upstream::UpstreamResponse> {
        client
            .request(
                base_url,
                "tok",
                protocol,
                Some(proxy),
                &serde_json::json!({"model": "m", "x": 1}),
                false,
                None,
                None,
                &HeaderMap::new(),
                &uas(),
                None,
                None,
                None,
            )
            .await
    }

    /// 假 sidecar:收一条请求,回 SSE 风格分块响应
    fn fake_sidecar(dir: &Path) -> (PathBuf, tokio::task::JoinHandle<String>) {
        let sock = dir.join("fake.sock");
        let listener = tokio::net::UnixListener::bind(&sock).unwrap();
        let task = tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            loop {
                let n = s.read(&mut chunk).await.unwrap();
                buf.extend_from_slice(&chunk[..n]);
                let text = String::from_utf8_lossy(&buf);
                if let Some(end) = text.find("\r\n\r\n") {
                    let len: usize = text[..end]
                        .lines()
                        .find_map(|l| {
                            l.to_ascii_lowercase()
                                .strip_prefix("content-length: ")
                                .map(|v| v.trim().parse().unwrap())
                        })
                        .unwrap_or(0);
                    if buf.len() >= end + 4 + len || n == 0 {
                        break;
                    }
                }
            }
            s.write_all(
                b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nConnection: keep-alive\r\nTransfer-Encoding: chunked\r\n\r\n7\r\ndata: 1\r\n0\r\n\r\n",
            )
            .await
            .unwrap();
            String::from_utf8_lossy(&buf).into_owned()
        });
        (sock, task)
    }

    fn client_with_fake(sock: PathBuf) -> UpstreamClient {
        UpstreamClient::new(Some("direct".into())).with_gohttp_lane(Arc::new(Lane::fixed_for_test(
            Ok(Arc::new(Sidecar::attach_for_test(sock))),
        )))
    }

    #[tokio::test]
    async fn gohttp_direct_sends_absolute_form_over_unix_socket() {
        let dir = tempfile::tempdir().unwrap();
        let (sock, task) = fake_sidecar(dir.path());
        let client = client_with_fake(sock);
        let resp = ag_request(
            &client,
            Protocol::Antigravity,
            "https://cloudcode-pa.googleapis.com",
            "direct",
        )
        .await
        .unwrap();
        assert_eq!(resp.status, 200);
        assert!(resp.body.headers().get("connection").is_none());
        assert!(resp.body.headers().get("transfer-encoding").is_none());
        use futures::StreamExt;
        let body: Vec<_> = resp.body.bytes_stream().collect().await;
        let body: Vec<u8> = body.into_iter().flat_map(|c| c.unwrap()).collect();
        assert_eq!(body, b"data: 1");
        let wire = task.await.unwrap();
        assert!(
            wire.starts_with(
                "POST https://cloudcode-pa.googleapis.com/v1internal:generateContent HTTP/1.1\r\n"
            ),
            "{wire}"
        );
        let lower = wire.to_ascii_lowercase();
        assert!(
            lower.contains("\r\nhost: cloudcode-pa.googleapis.com\r\n"),
            "{wire}"
        );
        assert!(
            lower.contains("\r\nuser-agent: antigravity/hub/test\r\n"),
            "{wire}"
        );
        assert!(
            lower.contains("\r\nauthorization: bearer tok\r\n"),
            "{wire}"
        );
        // Accept-Encoding 交给 Go Transport 自报 gzip 并解压(对齐官方 language_server)
        assert!(!lower.contains("\r\naccept-encoding:"), "{wire}");
        assert!(
            !lower.contains("\r\nx-ccextra-proxy:"),
            "direct 不加代理头: {wire}"
        );
        assert!(wire.ends_with(r#"{"model":"m","x":1}"#), "{wire}");
    }

    #[tokio::test]
    async fn gohttp_passes_resolved_proxy_header() {
        for (configured, expected) in [
            (
                "http://user:secret@127.0.0.1:7897",
                "http://user:secret@127.0.0.1:7897",
            ),
            ("socks5://127.0.0.1:7897", "socks5h://127.0.0.1:7897"),
            ("socks5h://127.0.0.1:7897", "socks5h://127.0.0.1:7897"),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let (sock, task) = fake_sidecar(dir.path());
            let client = client_with_fake(sock);
            let resp = ag_request(
                &client,
                Protocol::Antigravity,
                "https://cloudcode-pa.googleapis.com",
                configured,
            )
            .await
            .unwrap();
            assert_eq!(resp.status, 200);
            let wire = task.await.unwrap();
            let lines: Vec<_> = wire
                .lines()
                .filter(|l| l.to_ascii_lowercase().starts_with("x-ccextra-proxy:"))
                .collect();
            assert_eq!(lines, [format!("x-ccextra-proxy: {expected}")]);
        }
    }

    /// 入站透传的同名头不得选代理:direct 不出现该头,代理时只有解析后的那一个值
    #[tokio::test]
    async fn gohttp_ignores_forged_inbound_proxy_header() {
        let mut forged = HeaderMap::new();
        forged.insert(PROXY_HEADER, HeaderValue::from_static("http://evil:1"));
        for (configured, expected) in [
            ("direct", None),
            ("socks5://127.0.0.1:7897", Some("socks5h://127.0.0.1:7897")),
        ] {
            let dir = tempfile::tempdir().unwrap();
            let (sock, task) = fake_sidecar(dir.path());
            let client = client_with_fake(sock);
            let resp = client
                .request(
                    "https://cloudcode-pa.googleapis.com",
                    "tok",
                    Protocol::Antigravity,
                    Some(configured),
                    &serde_json::json!({"model": "m"}),
                    false,
                    None,
                    None,
                    &forged,
                    &uas(),
                    None,
                    None,
                    None,
                )
                .await
                .unwrap();
            assert_eq!(resp.status, 200);
            let wire = task.await.unwrap();
            let lines: Vec<_> = wire
                .lines()
                .filter(|l| l.to_ascii_lowercase().starts_with("x-ccextra-proxy:"))
                .collect();
            let want: Vec<String> = expected
                .map(|v| format!("x-ccextra-proxy: {v}"))
                .into_iter()
                .collect();
            assert_eq!(lines, want, "{configured}");
            assert!(!wire.contains("evil"), "{configured}: {wire}");
        }
    }

    #[tokio::test]
    async fn gohttp_invalid_proxy_errors_before_connecting() {
        for bad in ["ftp://127.0.0.1:21", "127.0.0.1:7897", ""] {
            let dir = tempfile::tempdir().unwrap();
            let (sock, task) = fake_sidecar(dir.path());
            let client = client_with_fake(sock);
            let err = ag_request(
                &client,
                Protocol::Antigravity,
                "https://cloudcode-pa.googleapis.com",
                bad,
            )
            .await
            .unwrap_err()
            .to_string();
            assert!(err.contains("proxy_url"), "{err}");
            assert!(!task.is_finished(), "非法代理不得连 sidecar");
            task.abort();
        }
    }

    #[tokio::test]
    async fn gohttp_missing_binary_errors_without_fallback() {
        let dir = tempfile::tempdir().unwrap();
        let cfg = AntigravityConfig {
            gohttp: true,
            gohttp_socket_dir: Some(dir.path().to_path_buf()),
            ..Default::default()
        };
        // 测试二进制所在 target/*/deps 下没有 ccextra-gohttp
        let client = UpstreamClient::with_ant_pool(Some("direct".into()), Some(&cfg));
        let err = ag_request(
            &client,
            Protocol::Antigravity,
            "http://127.0.0.1:9",
            "direct",
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains(BINARY_NAME) && err.contains("不回退"), "{err}");
    }

    #[tokio::test]
    async fn gohttp_only_affects_antigravity() {
        let dir = tempfile::tempdir().unwrap();
        let (sock, task) = fake_sidecar(dir.path());
        let client = client_with_fake(sock);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut s, _) = listener.accept().await.unwrap();
            let mut chunk = [0u8; 4096];
            let _ = s.read(&mut chunk).await.unwrap();
            s.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}")
                .await
                .unwrap();
        });
        let resp = ag_request(
            &client,
            Protocol::OpenAiChat,
            &format!("http://{addr}"),
            "direct",
        )
        .await
        .unwrap();
        assert_eq!(resp.status, 200);
        server.await.unwrap();
        assert!(!task.is_finished(), "非 AG 协议不得经 sidecar");
        task.abort();
    }

    #[test]
    fn sigterm_skipped_after_child_reaped() {
        // 已退出且被 try_wait 回收:pid 可能被复用,不得发信号
        let mut done = Command::new("true").spawn().unwrap();
        while done.try_wait().unwrap().is_none() {
            std::thread::sleep(Duration::from_millis(5));
        }
        let mut sent = false;
        assert!(!sigterm_if_running(&mut done, |_| sent = true));
        assert!(!sent);
        // 仍在跑:发给它自己的 pid
        let mut live = Command::new("sleep").arg("30").spawn().unwrap();
        let mut got = None;
        assert!(sigterm_if_running(&mut live, |pid| got = Some(pid)));
        assert_eq!(got, i32::try_from(live.id()).ok());
        live.kill().unwrap();
        live.wait().unwrap();
    }

    #[test]
    fn gohttp_flag_defaults_false() {
        let cfg: AntigravityConfig = yaml_serde::from_str("connection-pool: {}").unwrap();
        assert!(!cfg.gohttp);
        let cfg: AntigravityConfig = yaml_serde::from_str("gohttp: true").unwrap();
        assert!(cfg.gohttp);
        assert!(!UpstreamClient::with_ant_pool(Some("direct".into()), None).gohttp_enabled());
    }

    /// 真二进制回环:拉起、socket 0600、经 socket 转发(目标拒连 → sidecar 502)、SIGTERM 收掉
    #[tokio::test]
    async fn gohttp_real_binary_lifecycle() {
        let bin = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(BINARY_NAME);
        if !bin.is_file() {
            eprintln!("skip: {} 不存在", bin.display());
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let sidecar = Arc::new(spawn_at(&bin, &dir.path().join("c")).unwrap());
        let mut req = reqwest::Request::new(
            reqwest::Method::POST,
            "https://127.0.0.1:9/x".parse().unwrap(),
        );
        req.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        let (status, _body) = sidecar
            .send(
                req,
                None,
                Bytes::from_static(b"{}"),
                Duration::from_secs(10),
            )
            .await
            .unwrap();
        assert_eq!(status, 502);
        use std::os::unix::fs::PermissionsExt;
        let mode = std::fs::metadata(&sidecar.socket)
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(mode & 0o777, 0o600);
        let dmode = std::fs::metadata(dir.path().join("c"))
            .unwrap()
            .permissions()
            .mode();
        assert_eq!(dmode & 0o777, 0o700);
        let socket = sidecar.socket.clone();
        drop(_body);
        drop(sidecar);
        assert!(!socket.exists());
    }

    /// 真二进制:代理不可达 → sidecar 502,错误里不带代理凭证,也不偷偷直连
    #[tokio::test]
    async fn gohttp_real_binary_proxy_error_hides_credentials() {
        let bin = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(BINARY_NAME);
        if !bin.is_file() {
            eprintln!("skip: {} 不存在", bin.display());
            return;
        }
        // 目标在本地监听:若 sidecar 绕过代理直连,这里会收到连接
        let target = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        target.set_nonblocking(true).unwrap();
        let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let dead_addr = dead.local_addr().unwrap();
        drop(dead);
        let dir = tempfile::tempdir().unwrap();
        let sidecar = Arc::new(spawn_at(&bin, &dir.path().join("c")).unwrap());
        for scheme in ["http", "socks5h"] {
            let req = reqwest::Request::new(
                reqwest::Method::POST,
                format!("https://{}/x", target.local_addr().unwrap())
                    .parse()
                    .unwrap(),
            );
            let proxy = format!("{scheme}://user:s3cret@{dead_addr}");
            let (status, body) = sidecar
                .send(
                    req,
                    Some(&proxy),
                    Bytes::from_static(b"{}"),
                    Duration::from_secs(10),
                )
                .await
                .unwrap();
            assert_eq!(status, 502, "{scheme}");
            use http_body_util::BodyExt;
            let text = body.body.collect().await.unwrap().to_bytes();
            let text = String::from_utf8_lossy(&text);
            assert!(
                !text.contains("s3cret") && !text.contains("user:"),
                "{text}"
            );
        }
        assert!(target.accept().is_err(), "代理失败不得回退直连");
    }

    fn real_bin() -> Option<PathBuf> {
        let bin = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join(BINARY_NAME);
        if bin.is_file() {
            Some(bin)
        } else {
            eprintln!("skip: {} 不存在", bin.display());
            None
        }
    }

    /// 真二进制:sidecar 崩溃(已被回收)后,下一次 AG 请求在请求路径里重拉新进程,不等 /reload
    #[tokio::test]
    async fn gohttp_relaunches_crashed_sidecar_on_next_request() {
        let Some(bin) = real_bin() else { return };
        let dir = tempfile::tempdir().unwrap();
        let sock_dir = dir.path().join("c");
        let launches = Arc::new(AtomicU64::new(0));
        let counted = Arc::clone(&launches);
        let lane = Arc::new(Lane::with_launch(
            Some(&sock_dir),
            Box::new(move |d| {
                counted.fetch_add(1, Ordering::SeqCst);
                spawn_at(&bin, d.unwrap()).map(Arc::new)
            }),
        ));
        let client = UpstreamClient::new(Some("direct".into())).with_gohttp_lane(Arc::clone(&lane));
        // 目标端口已关:sidecar 活着就回 502;sidecar 死了则是连 socket 失败(Err)
        let dead = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let target = format!("https://{}", dead.local_addr().unwrap());
        drop(dead);

        let first = lane.get().unwrap();
        assert!(
            Arc::ptr_eq(&first, &lane.get().unwrap()),
            "存活时复用,不重拉"
        );
        let resp = ag_request(&client, Protocol::Antigravity, &target, "direct")
            .await
            .unwrap();
        assert_eq!(resp.status, 502);
        drop(resp);

        first.crash_for_test();
        let old_socket = first.socket.clone();
        // 对照:旧进程确实不可用(不是假阳性)
        let err = first
            .send(
                reqwest::Request::new(reqwest::Method::POST, target.parse().unwrap()),
                None,
                Bytes::new(),
                Duration::from_secs(5),
            )
            .await
            .unwrap_err();
        assert!(err.to_string().contains("连接 ccextra-gohttp"), "{err}");
        drop(first);
        assert_eq!(launches.load(Ordering::SeqCst), 1);

        let resp = ag_request(&client, Protocol::Antigravity, &target, "direct")
            .await
            .unwrap();
        assert_eq!(resp.status, 502, "新进程应接手");
        drop(resp);
        assert_eq!(launches.load(Ordering::SeqCst), 2, "一次请求只重拉一次");
        let second = lane.get().unwrap();
        assert_ne!(second.socket, old_socket);
        assert!(second.is_running());
        assert!(second.socket.exists());
        // 旧 Arc 已释放:只删了自己的 socket,没碰新的
        assert!(!old_socket.exists());
        assert_eq!(launches.load(Ordering::SeqCst), 2);
    }

    /// 测试里手动拉起的 sidecar:panic 时也 SIGKILL 并回收,避免泄漏进程。
    struct KillOnDrop(Child);
    impl Drop for KillOnDrop {
        fn drop(&mut self) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }

    /// 自签证书(SAN localhost/127.0.0.1),经 SSL_CERT_FILE 只让测试 sidecar 信任(Go 1.27 在 macOS 也生效)
    fn test_cert(dir: &Path) -> (PathBuf, PathBuf) {
        let (cert, key) = (dir.join("cert.pem"), dir.join("key.pem"));
        let ok = Command::new("openssl")
            .args([
                "req", "-x509", "-newkey", "rsa:2048", "-nodes", "-days", "1",
            ])
            .args(["-subj", "/CN=localhost"])
            .args(["-addext", "subjectAltName=DNS:localhost,IP:127.0.0.1"])
            .arg("-keyout")
            .arg(&key)
            .arg("-out")
            .arg(&cert)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("openssl")
            .success();
        assert!(ok, "openssl req 失败");
        (cert, key)
    }

    /// 真二进制端到端:上游回 gzip 压缩的 SSE;Rust 不发 Accept-Encoding,Go 自报 gzip 并透明解压,
    /// 事件逐块流回(第二个事件在 Rust 收到第一个之后才发,整包等待会超时失败)
    #[tokio::test]
    async fn gohttp_real_binary_gzip_sse_streams_decompressed() {
        use futures::StreamExt;
        use std::io::Write;
        use tokio_rustls::rustls::{self, pki_types::pem::PemObject};
        let Some(bin) = real_bin() else { return };
        let dir = tempfile::tempdir().unwrap();
        let (cert, key) = test_cert(dir.path());
        let tls = rustls::ServerConfig::builder_with_provider(Arc::new(
            rustls::crypto::aws_lc_rs::default_provider(),
        ))
        .with_safe_default_protocol_versions()
        .unwrap()
        .with_no_client_auth()
        .with_single_cert(
            vec![rustls::pki_types::CertificateDer::from_pem_file(&cert).unwrap()],
            rustls::pki_types::PrivateKeyDer::from_pem_file(&key).unwrap(),
        )
        .unwrap();
        let acceptor = tokio_rustls::TlsAcceptor::from(Arc::new(tls));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let port = listener.local_addr().unwrap().port();
        let (go_on, wait_go_on) = tokio::sync::oneshot::channel::<()>();
        let upstream = tokio::spawn(async move {
            let (tcp, _) = listener.accept().await.unwrap();
            let mut s = acceptor.accept(tcp).await.unwrap();
            let mut buf = Vec::new();
            let mut chunk = [0u8; 4096];
            let head_end = loop {
                let n = s.read(&mut chunk).await.unwrap();
                assert!(n > 0, "请求不完整");
                buf.extend_from_slice(&chunk[..n]);
                if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                    break i;
                }
            };
            let head = String::from_utf8_lossy(&buf[..head_end]).to_ascii_lowercase();
            let chunked = |data: &[u8]| {
                let mut out = format!("{:x}\r\n", data.len()).into_bytes();
                out.extend_from_slice(data);
                out.extend_from_slice(b"\r\n");
                out
            };
            let mut gz = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
            s.write_all(b"HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-encoding: gzip\r\ntransfer-encoding: chunked\r\n\r\n").await.unwrap();
            gz.write_all(b"data: 1\n\n").unwrap();
            gz.flush().unwrap(); // sync flush:第一个事件可独立解压
            s.write_all(&chunked(&std::mem::take(gz.get_mut())))
                .await
                .unwrap();
            s.flush().await.unwrap();
            wait_go_on.await.unwrap();
            gz.write_all(b"data: 2\n\n").unwrap();
            let mut out = chunked(&gz.finish().unwrap());
            out.extend_from_slice(b"0\r\n\r\n");
            s.write_all(&out).await.unwrap();
            s.shutdown().await.ok();
            head
        });

        let sock = dir.path().join("s").join("gohttp.sock");
        let _child = KillOnDrop(
            Command::new(&bin)
                .arg("-socket")
                .arg(&sock)
                .env("SSL_CERT_FILE", &cert)
                .stdin(Stdio::null())
                .spawn()
                .unwrap(),
        );
        let client = UpstreamClient::new(Some("direct".into())).with_gohttp_lane(Arc::new(
            Lane::fixed_for_test(Ok(Arc::new(Sidecar::attach_for_test(sock)))),
        ));
        let resp = client
            .request(
                &format!("https://localhost:{port}"),
                "tok",
                Protocol::Antigravity,
                Some("direct"),
                &serde_json::json!({"model": "m"}),
                true,
                None,
                None,
                &HeaderMap::new(),
                &uas(),
                None,
                None,
                None,
            )
            .await;
        let resp = resp.unwrap();
        assert_eq!(resp.status, 200);
        assert!(
            resp.body.headers().get("content-encoding").is_none(),
            "Go 已解压"
        );
        let mut stream = resp.body.bytes_stream();
        let mut got = Vec::new();
        while !got.ends_with(b"data: 1\n\n") {
            let next = tokio::time::timeout(Duration::from_secs(5), stream.next())
                .await
                .expect("第一个事件未在第二个发送前流回(整包等待?)")
                .expect("流提前结束")
                .unwrap();
            got.extend_from_slice(&next);
        }
        assert_eq!(got, b"data: 1\n\n");
        go_on.send(()).unwrap();
        while let Some(next) = tokio::time::timeout(Duration::from_secs(5), stream.next())
            .await
            .unwrap()
        {
            got.extend_from_slice(&next.unwrap());
        }
        assert_eq!(got, b"data: 1\n\ndata: 2\n\n");
        let head = upstream.await.unwrap();
        assert!(head.contains("\r\naccept-encoding: gzip"), "{head}");
    }
}
