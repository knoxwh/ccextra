// sidecar.rs:Cursor SDK sidecar 进程管理
//
// 职责:spawn node 子进程、READY 握手、健康巡检、退避重启、配置重载。
// token 在 start 内随机生成,只经环境变量传给子进程,不进 config/log/journal。
// 进程状态放 Arc 管理器,不进入不可变配置快照。

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{anyhow, Context, Result};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{Mutex, RwLock};
use tokio::task::JoinHandle;

use super::client::{CursorModelEntry, CursorSidecarError, SidecarClient};

/// READY 握手超时
const READY_TIMEOUT: Duration = Duration::from_secs(10);
/// 生产 sidecar 固定监听端口
pub const DEFAULT_SIDECAR_PORT: u16 = 8223;
/// 退避序列起点(1/2/4…,封顶 restart_backoff)
const BACKOFF_START: Duration = Duration::from_secs(1);

/// sidecar 进程配置(运行时可重载的部分)
#[derive(Debug, Clone)]
pub struct CursorSidecarConfig {
    /// sidecar 目录(含 main.mjs / package.json)
    pub sidecar_dir: PathBuf,
    /// Cursor 凭证目录(绝对路径,注入 CCEXTRA_CURSOR_AUTH_DIR)
    pub auth_dir: PathBuf,
    /// sidecar HTTP 端口;0 仅用于测试时让操作系统分配端口
    pub port: u16,
    /// 健康巡检间隔(默认 5 秒)
    pub health_interval: Duration,
    /// 重启退避上限(默认 60 秒)
    pub restart_backoff: Duration,
}

impl Default for CursorSidecarConfig {
    fn default() -> Self {
        Self {
            sidecar_dir: PathBuf::new(),
            auth_dir: PathBuf::new(),
            port: {
                #[cfg(test)]
                {
                    0
                }
                #[cfg(not(test))]
                {
                    DEFAULT_SIDECAR_PORT
                }
            },
            health_interval: Duration::from_secs(5),
            restart_backoff: Duration::from_secs(60),
        }
    }
}

/// 解析 sidecar READY 行:严格 JSON `{ "event": "ready", "port": N }`
pub fn parse_ready_line(line: &str) -> Result<u16> {
    let value: serde_json::Value = serde_json::from_str(line.trim())
        .with_context(|| format!("invalid sidecar READY line: {line:?}"))?;
    if value.get("event").and_then(|e| e.as_str()) != Some("ready") {
        return Err(anyhow!("invalid sidecar READY line: {line:?}"));
    }
    let port = value
        .get("port")
        .and_then(|p| p.as_u64())
        .and_then(|p| u16::try_from(p).ok())
        .ok_or_else(|| anyhow!("invalid sidecar READY port: {line:?}"))?;
    if port == 0 {
        return Err(anyhow!("invalid sidecar READY port: {line:?}"));
    }
    Ok(port)
}

/// 解析 `node --version` 输出,校验 major >= 24
fn parse_node_major(version: &str) -> Result<u32> {
    let trimmed = version.trim().trim_start_matches('v');
    let major = trimmed
        .split('.')
        .next()
        .and_then(|m| m.parse::<u32>().ok())
        .ok_or_else(|| anyhow!("cannot parse node version: {version:?}"))?;
    Ok(major)
}

/// 定位 sidecar 目录:优先可执行文件同级,回退仓库根
///
/// 两处都要求 package.json 与 main.mjs 齐备,否则明确报错
pub fn resolve_sidecar_dir(current_exe: &Path, repository_root: &Path) -> Result<PathBuf> {
    let exe_sibling = current_exe
        .parent()
        .map(|dir| dir.join("sidecar").join("cursor"))
        .ok_or_else(|| anyhow!("cannot derive exe parent: {current_exe:?}"))?;
    let repo_root = repository_root.join("sidecar").join("cursor");
    for candidate in [exe_sibling, repo_root] {
        if candidate.join("package.json").is_file() && candidate.join("main.mjs").is_file() {
            return Ok(candidate);
        }
    }
    Err(anyhow!(
        "cursor sidecar directory not found (need package.json + main.mjs): \
         exe sibling or repository root sidecar/cursor"
    ))
}

/// sidecar 进程管理器:client 就绪即可用,child 由 monitor 巡检保活
pub struct CursorSidecar {
    inner: Arc<Inner>,
}

struct Inner {
    config: RwLock<CursorSidecarConfig>,
    /// 就绪客户端;None = 未 ready,/run 返回 NotReady(503)
    client: RwLock<Option<SidecarClient>>,
    child: Mutex<Option<Child>>,
    shutdown: AtomicBool,
    monitor: Mutex<Option<JoinHandle<()>>>,
}

impl CursorSidecar {
    /// 启动 sidecar:node 版本校验 → spawn → READY 握手 → 健康巡检
    pub async fn start(config: CursorSidecarConfig) -> Result<Arc<Self>> {
        let sidecar = Self::spawn_sidecar(&config).await?;
        let inner = Arc::new(Inner {
            config: RwLock::new(config),
            client: RwLock::new(Some(sidecar.client)),
            child: Mutex::new(Some(sidecar.child)),
            shutdown: AtomicBool::new(false),
            monitor: Mutex::new(None),
        });
        let monitor = tokio::spawn(monitor_loop(inner.clone()));
        *inner.monitor.lock().await = Some(monitor);
        Ok(Arc::new(Self { inner }))
    }

    /// 测试注入:不启动子进程,client 直指 mock 服务;auth_dir 供
    /// dispatch 侧 ensure_credential_fresh 读取测试凭证
    #[cfg(test)]
    pub fn for_test(base_url: String, token: String, auth_dir: PathBuf) -> Arc<Self> {
        let client = SidecarClient::new(reqwest::Client::new(), base_url, token);
        let config = CursorSidecarConfig {
            auth_dir,
            ..CursorSidecarConfig::default()
        };
        let inner = Arc::new(Inner {
            config: RwLock::new(config),
            client: RwLock::new(Some(client)),
            child: Mutex::new(None),
            shutdown: AtomicBool::new(false),
            monitor: Mutex::new(None),
        });
        Arc::new(Self { inner })
    }

    /// POST /run:未 ready 返回 NotReady(映射 503)
    pub async fn run(
        &self,
        body: &serde_json::Value,
    ) -> std::result::Result<reqwest::Response, CursorSidecarError> {
        match self.inner.client.read().await.as_ref() {
            Some(client) => client.run(body).await,
            None => Err(CursorSidecarError::NotReady),
        }
    }

    /// POST /models:发现账户模型目录
    pub async fn models(
        &self,
        api_key: &str,
    ) -> std::result::Result<Vec<CursorModelEntry>, CursorSidecarError> {
        match self.inner.client.read().await.as_ref() {
            Some(client) => client.models(api_key).await,
            None => Err(CursorSidecarError::NotReady),
        }
    }

    /// GET /health:未 ready 或巡检失败均为 false
    pub async fn health(&self) -> bool {
        match self.inner.client.read().await.as_ref() {
            Some(client) => client.health().await,
            None => false,
        }
    }

    /// 当前凭证目录(dispatch 侧 ensure_credential_fresh 用)
    pub async fn auth_dir(&self) -> PathBuf {
        self.inner.config.read().await.auth_dir.clone()
    }

    /// 重载配置:目录变化重启子进程,其余字段原子更新
    pub async fn reconfigure(&self, config: CursorSidecarConfig) -> Result<()> {
        let dir_changed = {
            let current = self.inner.config.read().await;
            current.sidecar_dir != config.sidecar_dir || current.auth_dir != config.auth_dir
        };
        if dir_changed {
            self.stop_child().await;
            let sidecar = Self::spawn_sidecar(&config).await?;
            *self.inner.client.write().await = Some(sidecar.client);
            *self.inner.child.lock().await = Some(sidecar.child);
        }
        *self.inner.config.write().await = config;
        Ok(())
    }

    /// 停止子进程与巡检(测试与正常退出共用)
    pub async fn shutdown(&self) {
        self.inner.shutdown.store(true, Ordering::SeqCst);
        self.stop_child().await;
        *self.inner.client.write().await = None;
        if let Some(monitor) = self.inner.monitor.lock().await.take() {
            monitor.abort();
        }
    }

    async fn stop_child(&self) {
        if let Some(mut child) = self.inner.child.lock().await.take() {
            let _ = child.kill().await;
        }
    }

    /// 测试辅助:当前子进程 pid(kill 重启集成测试用)
    #[cfg(test)]
    pub(crate) async fn child_pid(&self) -> Option<u32> {
        self.inner
            .child
            .lock()
            .await
            .as_ref()
            .and_then(|child| child.id())
    }
}

struct SpawnedSidecar {
    client: SidecarClient,
    child: Child,
}

impl CursorSidecar {
    /// spawn 子进程并完成 READY 握手
    async fn spawn_sidecar(config: &CursorSidecarConfig) -> Result<SpawnedSidecar> {
        // node 版本门槛:SDK 要求 >= 24
        let version = Command::new("node")
            .arg("--version")
            .output()
            .await
            .context("cannot run node --version")?;
        let major = parse_node_major(&String::from_utf8_lossy(&version.stdout))?;
        if major < 24 {
            return Err(anyhow!(
                "cursor sidecar requires node >= 24, found v{major}"
            ));
        }

        // token 只经环境变量传递,不落盘不打日志
        let token = generate_token()?;
        // 相对路径锚定进程 cwd(std::path::absolute 需 1.79,MSRV 1.75)
        let auth_dir = if config.auth_dir.is_absolute() {
            config.auth_dir.clone()
        } else {
            std::env::current_dir()
                .context("cannot resolve relative auth_dir")?
                .join(&config.auth_dir)
        };

        let mut child = Command::new("node")
            .arg("main.mjs")
            .current_dir(&config.sidecar_dir)
            .env("CCEXTRA_CURSOR_TOKEN", &token)
            .env("CCEXTRA_CURSOR_AUTH_DIR", &auth_dir)
            .env("CCEXTRA_CURSOR_PORT", config.port.to_string())
            // Node fetch 默认忽略代理环境变量;开启后 SDK 出站走
            // http_proxy/https_proxy(无代理环境变量时无副作用)
            .env("NODE_USE_ENV_PROXY", "1")
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .with_context(|| format!("cannot spawn node in {:?}", config.sidecar_dir))?;

        // READY 握手:stdout 只读第一行,严格解析
        let stdout = child.stdout.take().context("sidecar stdout not captured")?;
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        let read = tokio::time::timeout(READY_TIMEOUT, reader.read_line(&mut line))
            .await
            .map_err(|_| anyhow!("sidecar READY handshake timed out after 10s"))?
            .context("cannot read sidecar stdout")?;
        if read == 0 {
            return Err(anyhow!("sidecar exited before READY"));
        }
        let port = parse_ready_line(&line)?;
        if config.port != 0 && port != config.port {
            let _ = child.kill().await;
            return Err(anyhow!(
                "sidecar READY port {port} does not match configured port {}",
                config.port
            ));
        }
        if let Some(status) = child.try_wait()? {
            return Err(anyhow!("sidecar exited early with {status}"));
        }

        // stderr 持续转 tracing
        if let Some(stderr) = child.stderr.take() {
            spawn_stderr_pump(stderr);
        }

        let client = SidecarClient::new(
            reqwest::Client::new(),
            format!("http://127.0.0.1:{port}"),
            token,
        );
        Ok(SpawnedSidecar { client, child })
    }
}

/// 随机 sidecar token:32 字节 hex
fn generate_token() -> Result<String> {
    let mut bytes = [0u8; 32];
    getrandom::getrandom(&mut bytes).context("generate sidecar token")?;
    Ok(hex::encode(bytes))
}

/// stderr 逐行转 tracing
fn spawn_stderr_pump(stderr: tokio::process::ChildStderr) {
    tokio::spawn(async move {
        let mut lines = BufReader::new(stderr).lines();
        while let Ok(Some(line)) = lines.next_line().await {
            tracing::warn!("cursor sidecar stderr: {line}");
        }
    });
}

/// 健康巡检:每 health_interval 探测一次;不健康时按 1/2/4…退避重启,
/// 成功后退避归零;shutdown 置位即退出
async fn monitor_loop(inner: Arc<Inner>) {
    let mut backoff = BACKOFF_START;
    let mut next_attempt = tokio::time::Instant::now();
    loop {
        let interval = inner.config.read().await.health_interval;
        tokio::time::sleep(interval).await;
        if inner.shutdown.load(Ordering::SeqCst) {
            break;
        }
        let healthy = match inner.client.read().await.as_ref() {
            Some(client) => client.health().await,
            None => false,
        };
        if healthy {
            backoff = BACKOFF_START;
            continue;
        }
        if tokio::time::Instant::now() < next_attempt {
            continue;
        }
        let config = inner.config.read().await.clone();
        match CursorSidecar::spawn_sidecar(&config).await {
            Ok(sidecar) => {
                if let Some(mut old) = inner.child.lock().await.take() {
                    let _ = old.kill().await;
                }
                *inner.client.write().await = Some(sidecar.client);
                *inner.child.lock().await = Some(sidecar.child);
                backoff = BACKOFF_START;
                next_attempt = tokio::time::Instant::now();
            }
            Err(error) => {
                tracing::warn!("cursor sidecar restart failed: {error:#}");
                next_attempt = tokio::time::Instant::now() + backoff;
                let cap = inner.config.read().await.restart_backoff;
                backoff = backoff.saturating_mul(2).min(cap);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ready_parser_accepts_only_ready_json() {
        assert_eq!(
            parse_ready_line(r#"{"event":"ready","port":8223}"#).unwrap(),
            8223
        );
        assert!(parse_ready_line("sidecar log").is_err());
        assert!(parse_ready_line(r#"{"event":"started","port":8223}"#).is_err());
        assert!(parse_ready_line(r#"{"event":"ready","port":0}"#).is_err());
        assert!(parse_ready_line(r#"{"event":"ready","port":70000}"#).is_err());
        assert!(parse_ready_line("").is_err());
    }

    #[test]
    fn default_sidecar_port_is_8223() {
        assert_eq!(DEFAULT_SIDECAR_PORT, 8223);
    }

    #[test]
    fn node_major_parses_and_rejects_garbage() {
        assert_eq!(parse_node_major("v24.1.0\n").unwrap(), 24);
        assert_eq!(parse_node_major("v28.0.1").unwrap(), 28);
        assert!(parse_node_major("not-a-version").is_err());
    }

    #[test]
    fn resolve_sidecar_dir_requires_package_and_entry() {
        let root = tempfile::tempdir().unwrap();
        let sidecar = root.path().join("sidecar").join("cursor");
        std::fs::create_dir_all(&sidecar).unwrap();
        let exe = root.path().join("ccextra");

        // 缺 package.json + main.mjs:报错
        assert!(resolve_sidecar_dir(&exe, root.path()).is_err());

        std::fs::write(sidecar.join("package.json"), "{}").unwrap();
        std::fs::write(sidecar.join("main.mjs"), "").unwrap();
        // exe 同级无 sidecar,回退仓库根
        let found = resolve_sidecar_dir(&exe, root.path()).unwrap();
        assert_eq!(found, sidecar);

        // exe 同级优先
        let exe_dir = root.path().join("bin");
        std::fs::create_dir_all(&exe_dir).unwrap();
        let sibling = exe_dir.join("sidecar").join("cursor");
        std::fs::create_dir_all(&sibling).unwrap();
        std::fs::write(sibling.join("package.json"), "{}").unwrap();
        std::fs::write(sibling.join("main.mjs"), "").unwrap();
        let found = resolve_sidecar_dir(&exe_dir.join("ccextra"), root.path()).unwrap();
        assert_eq!(found, sibling);
    }

    #[tokio::test]
    async fn for_test_client_routes_run_models_health() {
        use crate::test_support::TestServer;
        use axum::http::StatusCode;
        use bytes::Bytes;

        let router = axum::Router::new()
            .route(
                "/run",
                axum::routing::post(|| async {
                    (
                        StatusCode::OK,
                        [(axum::http::header::CONTENT_TYPE, "text/event-stream")],
                        Bytes::from_static(b"data: {}\n\n"),
                    )
                }),
            )
            .route(
                "/models",
                axum::routing::post(|| async {
                    (
                        StatusCode::OK,
                        Bytes::from_static(br#"{"models":[{"id":"auto"}]}"#),
                    )
                }),
            )
            .route(
                "/health",
                axum::routing::get(|| async { (StatusCode::OK, "ok") }),
            );
        let server = TestServer::spawn(router).await;
        let sidecar = CursorSidecar::for_test(
            server.url.clone(),
            "test-token".into(),
            PathBuf::from("/tmp"),
        );

        assert!(sidecar.health().await);
        let response = sidecar
            .run(&serde_json::json!({ "model": "auto" }))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let models = sidecar.models("key").await.unwrap();
        assert_eq!(models.len(), 1);
        assert_eq!(models[0].id, "auto");
        assert!(models[0].parameters.is_empty());
    }

    #[tokio::test]
    async fn shutdown_clears_client_and_run_returns_not_ready() {
        use crate::test_support::TestServer;
        use axum::http::StatusCode;

        let server = TestServer::reply(StatusCode::OK, bytes::Bytes::from_static(b"ok")).await;
        let sidecar = CursorSidecar::for_test(
            server.url.clone(),
            "test-token".into(),
            PathBuf::from("/tmp"),
        );
        assert!(sidecar.health().await);

        sidecar.shutdown().await;
        assert!(!sidecar.health().await);
        let err = sidecar.run(&serde_json::json!({})).await.unwrap_err();
        assert!(matches!(err, CursorSidecarError::NotReady));
        let err = sidecar.models("key").await.unwrap_err();
        assert!(matches!(err, CursorSidecarError::NotReady));
    }

    /// 真实 spawn 集成:READY 握手、鉴权 /health、kill 后 monitor 自动重启、
    /// shutdown 后 /run 返回 NotReady。node 或依赖缺失时跳过(打印提示)。
    #[tokio::test]
    async fn real_sidecar_ready_handshake_and_restart_after_kill() {
        let repo_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .canonicalize()
            .unwrap();
        let sidecar_dir = repo_root.join("sidecar").join("cursor");
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
                parse_node_major(&String::from_utf8_lossy(&out.stdout))
                    .map(|major| major >= 24)
                    .unwrap_or(false)
            })
            .unwrap_or(false);
        if !sdk_installed || !node_ok {
            eprintln!("skipping: sidecar node_modules or node >= 24 unavailable");
            return;
        }

        let auth = tempfile::tempdir().unwrap();
        let config = CursorSidecarConfig {
            sidecar_dir,
            auth_dir: auth.path().to_path_buf(),
            port: 8223,
            // 缩短巡检间隔,加速 kill 重启断言
            health_interval: Duration::from_millis(200),
            restart_backoff: Duration::from_secs(1),
        };
        let sidecar = CursorSidecar::start(config).await.unwrap();
        // READY 握手成功后 /health 可用
        assert!(sidecar.health().await);
        assert!(tokio::net::TcpStream::connect(("127.0.0.1", 8223))
            .await
            .is_ok());

        // kill 子进程:monitor 巡检发现不健康并自动重启
        let pid = sidecar.child_pid().await.unwrap();
        let _ = std::process::Command::new("kill")
            .arg("-9")
            .arg(pid.to_string())
            .status();
        let mut restarted = false;
        for _ in 0..100 {
            tokio::time::sleep(Duration::from_millis(200)).await;
            if sidecar.health().await {
                restarted = true;
                break;
            }
        }
        assert!(restarted, "sidecar did not restart after kill");
        let new_pid = sidecar.child_pid().await.unwrap();
        assert_ne!(pid, new_pid, "monitor must spawn a new child process");

        // shutdown 后 /run 返回 NotReady(映射 503)
        sidecar.shutdown().await;
        assert!(!sidecar.health().await);
        assert!(tokio::time::timeout(
            Duration::from_secs(1),
            tokio::net::TcpStream::connect(("127.0.0.1", 8223)),
        )
        .await
        .is_ok_and(|result| result.is_err()));
        let err = sidecar.run(&serde_json::json!({})).await.unwrap_err();
        assert!(matches!(err, CursorSidecarError::NotReady));
    }
}
