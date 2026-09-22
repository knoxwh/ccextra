// Codex PKCE + 本地回调登录流程 (对齐 CPA sdk/auth/codex.go 主路径)

use super::constants::{CALLBACK_PATH, DEFAULT_CALLBACK_PORT};
use super::credential::{parse_jwt_identity, CodexCredential};
use super::oauth::{build_auth_url, exchange_code, generate_pkce_codes, http_client};
use super::store;
use anyhow::{anyhow, Context, Result};
use axum::extract::Query;
use axum::response::Html;
use axum::routing::get;
use axum::Router;
use serde::Deserialize;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::oneshot;

const LOGIN_TIMEOUT: Duration = Duration::from_secs(300);

#[derive(Debug, Clone)]
pub struct CodexLoginOptions {
    pub auth_dir: PathBuf,
    pub no_browser: bool,
    pub callback_port: u16,
    /// 空则继承环境代理
    pub proxy_url: Option<String>,
}

impl Default for CodexLoginOptions {
    fn default() -> Self {
        Self {
            auth_dir: store::default_auth_dir(),
            no_browser: false,
            callback_port: DEFAULT_CALLBACK_PORT,
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

/// 浏览器 PKCE 登录,写 `codex-<email>.json`
pub async fn run_login(opts: CodexLoginOptions) -> Result<PathBuf> {
    let client = http_client(opts.proxy_url.as_deref())?;
    let state = random_state()?;
    let pkce = generate_pkce_codes()?;
    let (port, rx) = start_callback(opts.callback_port).await?;
    let redirect = format!("http://localhost:{port}{CALLBACK_PATH}");
    let auth_url = build_auth_url(&state, &redirect, &pkce);

    if opts.no_browser || open_browser(&auth_url).is_err() {
        println!("在浏览器打开以下地址完成登录:\n{auth_url}");
    } else {
        println!("已打开浏览器,等待 Codex 登录回调...");
    }

    let cb = tokio::time::timeout(LOGIN_TIMEOUT, rx)
        .await
        .map_err(|_| anyhow!("codex: authentication timed out"))?
        .map_err(|_| anyhow!("codex: callback channel closed"))?;
    finish_login(&client, &opts.auth_dir, &state, &redirect, cb, &pkce).await
}

async fn finish_login(
    client: &reqwest::Client,
    auth_dir: &std::path::Path,
    expected_state: &str,
    redirect: &str,
    cb: CallbackQuery,
    pkce: &super::oauth::PkceCodes,
) -> Result<PathBuf> {
    let code = cb.code.trim();
    let state = cb.state.trim();
    let error = cb.error.trim();
    if !error.is_empty() {
        return Err(anyhow!("codex: authentication failed: {error}"));
    }
    if state != expected_state {
        return Err(anyhow!("codex: invalid state"));
    }
    if code.is_empty() {
        return Err(anyhow!("codex: missing authorization code"));
    }
    let token = exchange_code(client, code, redirect, pkce).await?;
    let (email, account_id, plan_type) = parse_jwt_identity(&token.id_token);

    let mut cred = CodexCredential {
        r#type: "codex".to_string(),
        id_token: token.id_token.clone(),
        access_token: token.access_token.clone(),
        refresh_token: token.refresh_token.clone(),
        account_id: account_id.clone(),
        last_refresh: String::new(),
        email: email.clone(),
        plan_type: plan_type.clone(),
        expired: String::new(),
        disabled: false,
    };
    // 统一走 apply_tokens 计算 expired/last_refresh
    cred.apply_tokens(
        token.access_token,
        if token.refresh_token.is_empty() {
            None
        } else {
            Some(token.refresh_token)
        },
        if token.id_token.is_empty() {
            None
        } else {
            Some(token.id_token)
        },
        token.expires_in,
    );

    let path = store::save(auth_dir, &cred)?;
    let display_id = if !email.is_empty() {
        email
    } else if !account_id.is_empty() {
        account_id
    } else {
        "codex-user".to_string()
    };
    println!("✅ Codex 登录成功: {}", display_id);
    if !plan_type.is_empty() {
        println!("订阅档位: {}", plan_type);
    }
    println!("📁 凭证已保存至: {}", path.display());
    Ok(path)
}

async fn start_callback(port: u16) -> Result<(u16, oneshot::Receiver<CallbackQuery>)> {
    let (tx, rx) = oneshot::channel();
    let tx = Arc::new(std::sync::Mutex::new(Some(tx)));
    let app = Router::new().route(
        CALLBACK_PATH,
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
