use anyhow::Result;
use ccextra_core::cache_stabilization::drift_detector::DriftState;
use ccextra_server::antigravity::{
    constants::{CALLBACK_PORT, REFRESH_SKEW_SECS},
    list, resolve_auth_dir, run_login, LoginOptions,
};
use ccextra_server::http::{AppState, ReloadData, RuntimeConfig};
use ccextra_server::serve;
use ccextra_server::upstream::UpstreamClient;
use clap::{Parser, Subcommand};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;
use time::UtcOffset;
use tokio::sync::RwLock;
use tracing_subscriber::{
    fmt::time::OffsetTime, layer::SubscriberExt, util::SubscriberInitExt, EnvFilter,
};

mod config;

use config::Config;

#[derive(Parser)]
#[command(name = "ccextra")]
#[command(about = "Claude Code 请求代理:协议转换 + 缓存优化 + 上游路由")]
struct Cli {
    /// 配置文件路径
    #[arg(short, long, default_value = "config.yaml", global = true)]
    config: String,
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    /// 浏览器登录 Antigravity 并写入凭证
    #[command(name = "antigravity-login")]
    AntigravityLogin {
        /// 凭证目录,默认配置文件旁 .cache/antigravity
        #[arg(long)]
        auth_dir: Option<String>,
        /// 不自动打开浏览器,只打印 URL
        #[arg(long)]
        no_browser: bool,
        /// 本地回调端口,默认 51121(须与 Google 桌面端 client 一致)
        #[arg(long)]
        callback_port: Option<u16>,
    },
    /// 列出已保存的 Antigravity 凭证(不打印 token)
    #[command(name = "antigravity-status")]
    AntigravityStatus {
        /// 凭证目录,默认配置文件旁 .cache/antigravity
        #[arg(long)]
        auth_dir: Option<String>,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Some(Commands::AntigravityLogin {
            auth_dir,
            no_browser,
            callback_port,
        }) => {
            return cmd_login(&cli.config, auth_dir, no_browser, callback_port).await;
        }
        Some(Commands::AntigravityStatus { auth_dir }) => {
            return cmd_status(&cli.config, auth_dir);
        }
        None => {}
    }

    // 加载配置(日志级别依赖配置,故先加载)
    let config = Config::load(&cli.config)?;

    // 初始化日志:config.logging.level 为默认,RUST_LOG 可覆盖
    // 本地时区时间戳 + 无 ANSI + 无 target,适合日志文件
    let timer_fmt = time::format_description::parse_borrowed::<2>(
        "[year]-[month]-[day] [hour]:[minute]:[second]",
    )
    .expect("valid log timestamp format");
    let timer = OffsetTime::new(
        UtcOffset::current_local_offset().unwrap_or(UtcOffset::UTC),
        timer_fmt,
    );
    let filter = EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| EnvFilter::builder().parse_lossy(&config.logging.level));
    tracing_subscriber::registry()
        .with(filter)
        .with(
            tracing_subscriber::fmt::layer()
                .compact()
                .with_ansi(false)
                .with_target(false)
                .with_thread_ids(false)
                .with_thread_names(false)
                .with_timer(timer),
        )
        .init();

    tracing::info!("配置加载成功: {} providers", config.providers.len());

    // 启动时验证配置
    ccextra_core::route::validate_providers(&config.providers)?;
    tracing::info!("配置验证通过");

    // 构建应用状态
    let config_path = cli.config.clone();
    let reload = Arc::new(move || -> anyhow::Result<ReloadData> {
        let cfg = Config::load(&config_path)?;
        Ok(ReloadData {
            providers: cfg.providers,
            payload_rules: cfg.payload.unwrap_or_default(),
            normalize: cfg.normalize,
            logging: cfg.logging,
            secret: cfg.secret_key,
            proxy_url: cfg.server.proxy_url,
        })
    });
    let state = AppState {
        providers: Arc::new(RwLock::new(config.providers)),
        payload_rules: Arc::new(RwLock::new(config.payload.unwrap_or_default())),
        runtime: Arc::new(RwLock::new(RuntimeConfig {
            normalize: config.normalize,
            logging: config.logging,
            secret: config.secret_key,
            upstream: UpstreamClient::new(config.server.proxy_url),
        })),
        reload,
        drift: DriftState::new(1000),
    };

    // 启动 HTTP 服务
    let addr = format!("{}:{}", config.server.host, config.server.port);
    tracing::info!("ccextra 启动中...");

    serve(&addr, state).await?;

    Ok(())
}

fn load_optional_config(path: &str) -> Option<Config> {
    Config::load(path).ok()
}

fn auth_dir_from(config_path: &str, override_dir: Option<String>) -> PathBuf {
    let raw = if let Some(dir) = override_dir {
        Some(dir)
    } else {
        load_optional_config(config_path).and_then(|cfg| cfg.auth_dir)
    };
    pin_auth_dir(config_path, raw.as_deref())
}

/// 相对路径钉在配置文件所在目录,不跟进程 cwd 走
fn pin_auth_dir(config_path: &str, raw: Option<&str>) -> PathBuf {
    let path = resolve_auth_dir(raw);
    if path.is_absolute() {
        return path;
    }
    let parent = PathBuf::from(config_path);
    let base = parent.parent().filter(|p| !p.as_os_str().is_empty());
    match base {
        Some(dir) => dir.join(path),
        None => path,
    }
}

async fn cmd_login(
    config_path: &str,
    auth_dir: Option<String>,
    no_browser: bool,
    callback_port: Option<u16>,
) -> Result<()> {
    let cfg = load_optional_config(config_path);
    // 登录换码/userinfo/project 走 server.proxy_url
    let proxy_url = cfg.as_ref().and_then(|c| c.server.proxy_url.clone());
    let opts = LoginOptions {
        auth_dir: auth_dir_from(config_path, auth_dir),
        no_browser,
        callback_port: callback_port.unwrap_or(CALLBACK_PORT),
        proxy_url,
    };
    run_login(opts).await?;
    Ok(())
}

fn cmd_status(config_path: &str, auth_dir: Option<String>) -> Result<()> {
    let dir = auth_dir_from(config_path, auth_dir);
    let now = SystemTime::now();
    let entries = list(&dir)?;
    if entries.is_empty() {
        println!("无 Antigravity 凭证: {}", dir.display());
        return Ok(());
    }
    println!("auth_dir: {}", dir.display());
    for (path, cred) in entries {
        let status = if cred.disabled {
            "disabled"
        } else if cred.is_fresh(now, REFRESH_SKEW_SECS) {
            "fresh"
        } else {
            "stale"
        };
        let email = if cred.email.is_empty() {
            "-"
        } else {
            cred.email.as_str()
        };
        let project = if cred.project_id.is_empty() {
            "-"
        } else {
            cred.project_id.as_str()
        };
        println!(
            "{}  email={email}  project={project}  {status}  expired={}",
            path.file_name().unwrap_or_default().to_string_lossy(),
            if cred.expired.is_empty() {
                "-"
            } else {
                cred.expired.as_str()
            }
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::pin_auth_dir;
    use std::path::PathBuf;

    #[test]
    fn pin_default_next_to_config() {
        let dir = pin_auth_dir("/tmp/proj/config.yaml", None);
        assert_eq!(dir, PathBuf::from("/tmp/proj/.cache/antigravity"));
        assert!(!dir.to_string_lossy().contains(".cli-proxy-api"));
    }

    #[test]
    fn pin_keeps_absolute_and_tilde() {
        assert_eq!(
            pin_auth_dir("/tmp/proj/config.yaml", Some("/abs/creds")),
            PathBuf::from("/abs/creds")
        );
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
            .unwrap();
        assert_eq!(
            pin_auth_dir("/tmp/proj/config.yaml", Some("~/.cli-proxy-api")),
            home.join(".cli-proxy-api")
        );
    }
}
