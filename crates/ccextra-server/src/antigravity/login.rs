// 本地 OAuth 回调 + 换码落盘

use super::constants::{CALLBACK_PORT, REFRESH_SKEW_SECS};
use super::credential::AntigravityCredential;
use super::oauth::{
    build_auth_url, default_redirect_uri, exchange_code, fetch_user_email, http_client,
    refresh_token,
};
use super::project::fetch_project_id;
use super::store;
use anyhow::{anyhow, Context, Result};
use axum::extract::Query;
use axum::response::Html;
use axum::routing::get;
use axum::Router;
use reqwest::Client;
use serde::Deserialize;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::{Duration, SystemTime};
use tokio::sync::oneshot;

const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, Clone)]
pub struct LoginOptions {
    pub auth_dir: PathBuf,
    pub no_browser: bool,
    pub callback_port: u16,
    /// 空则继承环境代理
    pub proxy_url: Option<String>,
}

impl Default for LoginOptions {
    fn default() -> Self {
        Self {
            auth_dir: store::default_auth_dir(),
            no_browser: false,
            callback_port: CALLBACK_PORT,
            proxy_url: None,
        }
    }
}

#[derive(Debug, Deserialize, Default)]
struct CallbackQuery {
    #[serde(default)]
    code: String,
    #[serde(default)]
    state: String,
    #[serde(default)]
    error: String,
}

/// 浏览器登录,写 `antigravity-<email>.json`
pub async fn run_login(opts: LoginOptions) -> Result<PathBuf> {
    // 换码/userinfo/project 走配置代理
    let client = http_client(opts.proxy_url.as_deref())?;
    let state = random_state()?;
    let (port, rx) = start_callback(opts.callback_port).await?;
    let redirect = default_redirect_uri(port);
    let auth_url = build_auth_url(&state, &redirect);

    if opts.no_browser || open_browser(&auth_url).is_err() {
        println!("在浏览器打开以下地址完成登录:\n{auth_url}");
    } else {
        println!("已打开浏览器,等待 Antigravity 登录回调...");
    }

    let cb = tokio::time::timeout(LOGIN_TIMEOUT, rx)
        .await
        .map_err(|_| anyhow!("antigravity: authentication timed out"))?
        .map_err(|_| anyhow!("antigravity: callback channel closed"))?;
    finish_login(&client, &opts.auth_dir, &state, &redirect, cb).await
}

async fn finish_login(
    client: &Client,
    auth_dir: &std::path::Path,
    expected_state: &str,
    redirect: &str,
    cb: CallbackQuery,
) -> Result<PathBuf> {
    let code = cb.code.trim();
    let state = cb.state.trim();
    let error = cb.error.trim();
    if !error.is_empty() {
        return Err(anyhow!("antigravity: authentication failed: {error}"));
    }
    if state != expected_state {
        return Err(anyhow!("antigravity: invalid state"));
    }
    if code.is_empty() {
        return Err(anyhow!("antigravity: missing authorization code"));
    }
    let token = exchange_code(client, code, redirect).await?;
    let email = fetch_user_email(client, &token.access_token).await?;
    let project_id = fetch_project_id(client, &token.access_token).await?;
    if project_id.trim().is_empty() {
        return Err(anyhow!(
            "antigravity: project ID discovery returned empty project"
        ));
    }
    let cred = AntigravityCredential::new(
        token.access_token,
        token.refresh_token,
        token.expires_in,
        email.clone(),
        project_id.clone(),
    );
    let path = store::save(auth_dir, &cred)?;
    println!("Antigravity 登录成功: {email}");
    println!("GCP project: {}", hide_api_key(&project_id));
    println!("凭证: {}", path.display());
    Ok(path)
}

/// 打码过长密钥,只留头尾
fn hide_api_key(key: &str) -> String {
    let n = key.len();
    if n > 8 {
        format!("{}...{}", &key[..4], &key[n - 4..])
    } else if n > 4 {
        format!("{}...{}", &key[..2], &key[n - 2..])
    } else if n > 2 {
        format!("{}...{}", &key[..1], &key[n - 1..])
    } else {
        key.to_string()
    }
}

/// 过期则刷新并回写;project 空则补发现
pub async fn ensure_fresh(
    client: &Client,
    dir: &std::path::Path,
    cred: &mut AntigravityCredential,
) -> Result<()> {
    if !cred.is_fresh(SystemTime::now(), REFRESH_SKEW_SECS) {
        let token = refresh_token(client, &cred.refresh_token).await?;
        let new_refresh = if token.refresh_token.trim().is_empty() {
            None
        } else {
            Some(token.refresh_token)
        };
        cred.apply_tokens(token.access_token, new_refresh, token.expires_in);
    }
    if cred.project_id.trim().is_empty() {
        match fetch_project_id(client, &cred.access_token).await {
            Ok(id) if !id.is_empty() => cred.project_id = id,
            Ok(_) => {}
            Err(err) => tracing::warn!("antigravity: ensure project id failed: {err}"),
        }
    }
    store::save(dir, cred)?;
    Ok(())
}

async fn start_callback(port: u16) -> Result<(u16, oneshot::Receiver<CallbackQuery>)> {
    let (tx, rx) = oneshot::channel();
    let tx = Arc::new(std::sync::Mutex::new(Some(tx)));
    let app = Router::new().route(
        "/oauth-callback",
        get({
            let tx = tx.clone();
            move |Query(q): Query<CallbackQuery>| {
                let tx = tx.clone();
                async move {
                    let html = if !q.code.is_empty() && q.error.is_empty() {
                        "<h1>Login successful</h1><p>You can close this window.</p>"
                    } else {
                        "<h1>Login failed</h1><p>Please check the CLI output.</p>"
                    };
                    if let Ok(mut slot) = tx.lock() {
                        if let Some(sender) = slot.take() {
                            let _ = sender.send(q);
                        }
                    }
                    Html(html)
                }
            }
        }),
    );
    // 全接口监听,SSH/隧道可打进
    let addr = SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .with_context(|| format!("bind oauth callback {addr}"))?;
    let bound = listener.local_addr()?.port();
    tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    Ok((bound, rx))
}

/// 16 字节随机 hex
fn random_state() -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::getrandom(&mut bytes).context("generate oauth state")?;
    Ok(bytes.iter().map(|b| format!("{b:02x}")).collect())
}

fn open_browser(url: &str) -> Result<()> {
    let status = std::process::Command::new("open")
        .arg(url)
        .status()
        .context("open browser")?;
    if status.success() {
        Ok(())
    } else {
        Err(anyhow!("open browser failed: {status}"))
    }
}
