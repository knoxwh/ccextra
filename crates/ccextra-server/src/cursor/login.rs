use super::constants::{POLL_MAX_ATTEMPTS, POLL_MAX_ERRORS};
use super::credential::CursorCredential;
use super::{oauth, store};
use anyhow::{anyhow, Context, Result};
use reqwest::{Client, StatusCode, Url};
use std::path::PathBuf;
use std::time::Duration;

pub struct CursorLoginOptions {
    pub auth_dir: PathBuf,
    pub no_browser: bool,
    pub proxy_url: Option<String>,
}

pub async fn run_login(options: CursorLoginOptions) -> Result<PathBuf> {
    let params = oauth::generate_auth_params()?;
    let client = oauth::http_client(options.proxy_url.as_deref())?;
    println!("Cursor 登录地址: {}", params.login_url);
    if !options.no_browser {
        if let Err(error) = open_browser(&params.login_url) {
            tracing::debug!("打开 Cursor 登录页面失败: {error}");
        }
    }
    let url = oauth::poll_url(&params.uuid, &params.verifier)?;
    let pair = poll_for_auth(&client, url).await?;
    let mut credential = CursorCredential {
        access_token: String::new(),
        refresh_token: String::new(),
        sub: String::new(),
        expires_at: None,
    };
    credential.apply_tokens(pair.access_token, pair.refresh_token);
    store::save(&options.auth_dir, &credential)?;
    println!(
        "Cursor 凭证已保存: {}",
        store::credential_path(&options.auth_dir).display()
    );
    Ok(store::credential_path(&options.auth_dir))
}

async fn poll_for_auth(client: &Client, url: Url) -> Result<oauth::TokenPair> {
    let mut delay = Duration::from_secs(1);
    let mut consecutive_errors = 0;
    for _ in 0..POLL_MAX_ATTEMPTS {
        tokio::time::sleep(delay).await;
        let response = client
            .get(url.clone())
            .timeout(Duration::from_secs(10))
            .send()
            .await;
        match response {
            Ok(resp) if resp.status() == StatusCode::NOT_FOUND => {
                consecutive_errors = 0;
            }
            Ok(resp) if resp.status().is_success() => {
                let pair: oauth::TokenPair =
                    resp.json().await.context("解析 Cursor 登录响应失败")?;
                if pair.access_token.trim().is_empty() || pair.refresh_token.trim().is_empty() {
                    return Err(anyhow!("Cursor 登录返回空 token"));
                }
                return Ok(pair);
            }
            Ok(resp) => return Err(anyhow!("Cursor 登录轮询失败: {}", resp.status())),
            Err(error) => {
                consecutive_errors += 1;
                if consecutive_errors >= POLL_MAX_ERRORS {
                    return Err(error).context("Cursor 登录轮询连续失败");
                }
            }
        }
        delay = delay.mul_f64(1.2).min(Duration::from_secs(10));
    }
    Err(anyhow!("Cursor 登录轮询超时"))
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
