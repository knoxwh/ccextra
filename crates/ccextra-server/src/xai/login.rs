// xAI Device Code 登录流程与交互

use super::credential::{parse_jwt_identity, XAICredential};
use super::oauth::{http_client, poll_for_token, start_device_flow};
use super::store;
use anyhow::{anyhow, Context, Result};
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct XAILoginOptions {
    pub auth_dir: PathBuf,
    pub no_browser: bool,
    /// 空则继承环境代理
    pub proxy_url: Option<String>,
}

impl Default for XAILoginOptions {
    fn default() -> Self {
        Self {
            auth_dir: store::default_auth_dir(),
            no_browser: false,
            proxy_url: None,
        }
    }
}

/// 执行 Device Flow 登录并写入凭证文件
pub async fn run_login(opts: XAILoginOptions) -> Result<PathBuf> {
    let client = http_client(opts.proxy_url.as_deref())?;

    println!("正在请求 xAI OAuth 设备授权代码...");
    let device = start_device_flow(&client).await?;

    let verify_url = if !device.verification_uri_complete.is_empty() {
        &device.verification_uri_complete
    } else {
        &device.verification_uri
    };

    println!("==================================================================");
    println!("🔑 xAI 用户验证代码 (User Code): {}", device.user_code);
    println!("🔗 验证页面 URL: {}", verify_url);
    println!("==================================================================");

    if !opts.no_browser {
        if let Err(e) = open_browser(verify_url) {
            tracing::debug!("自动打开浏览器失败: {}", e);
            println!("提示: 请手动在浏览器中打开上方链接并输入 User Code。");
        } else {
            println!("已自动在浏览器中打开验证页面，请完成授权...");
        }
    } else {
        println!("提示: 请在浏览器中打开上方链接并输入 User Code。");
    }

    println!(
        "等待授权完成中 (轮询超时时间: {} 秒)...",
        if device.expires_in > 0 {
            device.expires_in
        } else {
            1800
        }
    );

    let token_data = poll_for_token(&client, &device).await?;
    let (email, sub) = parse_jwt_identity(&token_data.id_token);

    let mut cred = XAICredential {
        r#type: "xai".to_string(),
        auth_kind: "oauth".to_string(),
        access_token: token_data.access_token.clone(),
        refresh_token: token_data.refresh_token.clone(),
        id_token: token_data.id_token.clone(),
        token_type: if token_data.token_type.is_empty() {
            "Bearer".to_string()
        } else {
            token_data.token_type
        },
        expires_in: token_data.expires_in,
        expired: String::new(),
        last_refresh: String::new(),
        email: email.clone(),
        sub: sub.clone(),
        base_url: super::constants::CLI_CHAT_PROXY_BASE_URL.to_string(),
        token_endpoint: device.token_endpoint,
        disabled: false,
    };

    cred.apply_tokens(
        token_data.access_token,
        if token_data.refresh_token.is_empty() {
            None
        } else {
            Some(token_data.refresh_token)
        },
        if token_data.id_token.is_empty() {
            None
        } else {
            Some(token_data.id_token)
        },
        token_data.expires_in,
    );

    let path = store::save(&opts.auth_dir, &cred)?;
    let display_id = if !email.is_empty() {
        email
    } else if !sub.is_empty() {
        sub
    } else {
        "xai-user".to_string()
    };

    println!("✅ xAI 登录成功: {}", display_id);
    println!("📁 凭证已保存至: {}", path.display());

    Ok(path)
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
