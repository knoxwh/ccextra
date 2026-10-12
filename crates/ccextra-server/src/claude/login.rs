// Claude PKCE + 本地回调登录流程 (对齐 CPA sdk/auth/claude.go)

use super::constants::{CALLBACK_PATH, DEFAULT_CALLBACK_PORT, REDIRECT_URI};
use super::credential::{generate_device_id, ClaudeCredential};
use super::oauth::{
    build_auth_url, exchange_code, fetch_profile, generate_pkce_codes, http_client,
};
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
pub struct ClaudeLoginOptions {
    pub auth_dir: PathBuf,
    pub no_browser: bool,
    pub callback_port: u16,
    /// 空则继承环境代理
    pub proxy_url: Option<String>,
}

impl Default for ClaudeLoginOptions {
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

/// 浏览器 PKCE 登录,写 `claude-<email>.json`
pub async fn run_login(opts: ClaudeLoginOptions) -> Result<PathBuf> {
    let client = http_client(opts.proxy_url.as_deref())?;
    let state = random_state()?;
    let pkce = generate_pkce_codes()?;
    // RedirectURI 固定 localhost:54545;端口须与 Anthropic 注册一致
    let (port, rx) = start_callback(opts.callback_port).await?;
    if port != DEFAULT_CALLBACK_PORT {
        tracing::warn!("回调端口 {port} 与固定 RedirectURI ({REDIRECT_URI}) 不一致,授权可能失败");
    }
    let auth_url = build_auth_url(&state, &pkce);

    if opts.no_browser || open_browser(&auth_url).is_err() {
        println!("在浏览器打开以下地址完成登录:\n{auth_url}");
    } else {
        println!("已打开浏览器,等待 Claude 订阅登录回调...");
    }

    let cb = tokio::time::timeout(LOGIN_TIMEOUT, rx)
        .await
        .map_err(|_| anyhow!("claude: authentication timed out"))?
        .map_err(|_| anyhow!("claude: callback channel closed"))?;
    finish_login(&client, &opts.auth_dir, &state, cb, &pkce).await
}

async fn finish_login(
    client: &reqwest::Client,
    auth_dir: &std::path::Path,
    expected_state: &str,
    cb: CallbackQuery,
    pkce: &super::oauth::PkceCodes,
) -> Result<PathBuf> {
    let code = cb.code.trim();
    let state = cb.state.trim();
    let error = cb.error.trim();
    if !error.is_empty() {
        return Err(anyhow!("claude: authentication failed: {error}"));
    }
    if !state.is_empty() && state != expected_state {
        return Err(anyhow!("claude: invalid state"));
    }
    if code.is_empty() {
        return Err(anyhow!("claude: missing authorization code"));
    }
    let token = exchange_code(client, code, expected_state, pkce).await?;

    let mut email = token.account.email_address.clone();
    let mut account_uuid = token.account.uuid.clone();
    let mut organization_uuid = token.organization.uuid.clone();
    let mut organization_name = token.organization.name.clone();

    // 可选补全 profile (失败不阻断)
    match fetch_profile(client, &token.access_token).await {
        Ok(profile) => {
            if !profile.account.email.is_empty() {
                email = profile.account.email;
            }
            if !profile.account.uuid.is_empty() {
                account_uuid = profile.account.uuid;
            }
            if !profile.organization.uuid.is_empty() {
                organization_uuid = profile.organization.uuid;
            }
            if !profile.organization.name.is_empty() {
                organization_name = profile.organization.name;
            }
        }
        Err(e) => tracing::warn!("拉取 Claude OAuth profile 失败(可忽略): {e}"),
    }

    let device_id = generate_device_id()?;
    let mut cred = ClaudeCredential {
        r#type: "claude".to_string(),
        id_token: String::new(),
        access_token: token.access_token.clone(),
        refresh_token: token.refresh_token.clone(),
        last_refresh: String::new(),
        email: email.clone(),
        account_uuid: account_uuid.clone(),
        organization_uuid,
        organization_name: organization_name.clone(),
        claude_device_ids: vec![device_id],
        expired: String::new(),
        disabled: false,
    };
    cred.apply_tokens(
        token.access_token,
        if token.refresh_token.is_empty() {
            None
        } else {
            Some(token.refresh_token)
        },
        token.expires_in,
    );

    let path = store::save(auth_dir, &cred)?;
    let display_id = if !email.is_empty() {
        email
    } else if !account_uuid.is_empty() {
        account_uuid
    } else {
        "claude-user".to_string()
    };
    println!("✅ Claude 订阅登录成功: {}", display_id);
    if !organization_name.is_empty() {
        println!("组织: {}", organization_name);
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

fn random_state() -> Result<String> {
    let mut bytes = [0u8; 16];
    getrandom::fill(&mut bytes).context("generate oauth state")?;
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
