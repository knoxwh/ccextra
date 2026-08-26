use anyhow::Result;
use ccextra_core::cache_stabilization::drift_detector::DriftState;
use ccextra_core::route::ProviderConfig;
use ccextra_server::antigravity::{
    constants::{CALLBACK_PORT, REFRESH_SKEW_SECS},
    list as list_antigravity, resolve_auth_dir as resolve_antigravity_auth_dir,
    run_login as run_antigravity_login, LoginOptions as AntigravityLoginOptions,
};
use ccextra_server::http::{AppState, ReloadData, RuntimeConfig, UserAgentSet};
use ccextra_server::serve;
use ccextra_server::upstream::UpstreamClient;
use ccextra_server::xai::{
    constants::REFRESH_SKEW_SECS as XAI_REFRESH_SKEW_SECS, list as list_xai,
    resolve_auth_dir as resolve_xai_auth_dir, run_login as run_xai_login, XAILoginOptions,
};
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
    /// xAI Grok 设备码授权登录并写入凭证
    #[command(name = "xai-login")]
    XaiLogin {
        /// 凭证目录,默认配置文件旁 .cache/xai
        #[arg(long)]
        auth_dir: Option<String>,
        /// 不自动打开浏览器,只打印 URL
        #[arg(long)]
        no_browser: bool,
    },
    /// 列出已保存的 xAI Grok 凭证(不打印 token)
    #[command(name = "xai-status")]
    XaiStatus {
        /// 凭证目录,默认配置文件旁 .cache/xai
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
            return cmd_antigravity_login(&cli.config, auth_dir, no_browser, callback_port).await;
        }
        Some(Commands::AntigravityStatus { auth_dir }) => {
            return cmd_antigravity_status(&cli.config, auth_dir);
        }
        Some(Commands::XaiLogin {
            auth_dir,
            no_browser,
        }) => {
            return cmd_xai_login(&cli.config, auth_dir, no_browser).await;
        }
        Some(Commands::XaiStatus { auth_dir }) => {
            return cmd_xai_status(&cli.config, auth_dir);
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

    // 动态加载 Antigravity providers
    let antigravity_auth_dir = pin_auth_dir(
        &cli.config,
        config.auth_dir.as_deref(),
        resolve_antigravity_auth_dir,
    );
    let antigravity_providers = ccextra_server::antigravity::load_antigravity_providers(
        &antigravity_auth_dir,
        config.server.proxy_url.as_deref(),
    )
    .await;
    if !antigravity_providers.is_empty() {
        tracing::info!(
            "动态加载 {} 个 Antigravity providers",
            antigravity_providers.len()
        );
    }

    // 动态加载 xAI providers
    let xai_auth_dir = pin_auth_dir(
        &cli.config,
        config.xai_auth_dir.as_deref(),
        resolve_xai_auth_dir,
    );
    let xai_providers =
        ccextra_server::xai::load_xai_providers(&xai_auth_dir, config.server.proxy_url.as_deref())
            .await;
    if !xai_providers.is_empty() {
        tracing::info!("动态加载 {} 个 xAI providers", xai_providers.len());
    }

    // 合并配置文件 providers、Antigravity providers 和 xAI providers
    let mut all_providers = merge_providers(config.providers, antigravity_providers);
    all_providers = merge_providers(all_providers, xai_providers);

    // 启动时验证配置
    ccextra_core::route::validate_providers(&all_providers)?;
    tracing::info!("配置验证通过");

    // 构建应用状态
    let config_path = cli.config.clone();
    let reload = Arc::new(move || -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<ReloadData>> + Send>> {
        let config_path = config_path.clone();
        Box::pin(async move {
            let cfg = Config::load(&config_path)?;

            // 重载时重新解析 auth_dir 并加载动态 providers
            let antigravity_auth_dir = pin_auth_dir(&config_path, cfg.auth_dir.as_deref(), resolve_antigravity_auth_dir);
            let antigravity_providers = ccextra_server::antigravity::load_antigravity_providers(
                &antigravity_auth_dir,
                cfg.server.proxy_url.as_deref(),
            )
            .await;

            let xai_auth_dir = pin_auth_dir(&config_path, cfg.xai_auth_dir.as_deref(), resolve_xai_auth_dir);
            let xai_providers = ccextra_server::xai::load_xai_providers(
                &xai_auth_dir,
                cfg.server.proxy_url.as_deref(),
            )
            .await;

            let mut providers = merge_providers(cfg.providers, antigravity_providers);
            providers = merge_providers(providers, xai_providers);

            let user_agents = build_user_agents(cfg.user_agents.as_ref());

            Ok(ReloadData {
                providers,
                payload_rules: cfg.payload.unwrap_or_default(),
                normalize: cfg.normalize,
                logging: cfg.logging,
                secret: cfg.secret_key,
                proxy_url: cfg.server.proxy_url,
                user_agents,
            })
        })
    });

    let user_agents = build_user_agents(config.user_agents.as_ref());

    let state = AppState {
        providers: Arc::new(RwLock::new(all_providers)),
        payload_rules: Arc::new(RwLock::new(config.payload.unwrap_or_default())),
        runtime: Arc::new(RwLock::new(RuntimeConfig {
            normalize: config.normalize,
            logging: config.logging,
            secret: config.secret_key,
            upstream: UpstreamClient::new(config.server.proxy_url),
            user_agents,
        })),
        reload,
        drift: DriftState::new(1000),
        replay_cache: ccextra_server::sse::replay_cache::ReplayCache::new(
            std::time::Duration::from_secs(3600),
            1024,
        ),
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

fn antigravity_auth_dir_from(config_path: &str, override_dir: Option<String>) -> PathBuf {
    let raw = if let Some(dir) = override_dir {
        Some(dir)
    } else {
        load_optional_config(config_path).and_then(|cfg| cfg.auth_dir)
    };
    pin_auth_dir(config_path, raw.as_deref(), resolve_antigravity_auth_dir)
}

fn xai_auth_dir_from(config_path: &str, override_dir: Option<String>) -> PathBuf {
    let raw = if let Some(dir) = override_dir {
        Some(dir)
    } else {
        load_optional_config(config_path).and_then(|cfg| cfg.xai_auth_dir)
    };
    pin_auth_dir(config_path, raw.as_deref(), resolve_xai_auth_dir)
}

/// 相对路径钉在配置文件所在目录,不跟进程 cwd 走
fn pin_auth_dir<F>(config_path: &str, raw: Option<&str>, resolver: F) -> PathBuf
where
    F: FnOnce(Option<&str>) -> PathBuf,
{
    let path = resolver(raw);
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

/// 合并配置文件与动态注入的 providers(多账号暴露同一批模型,
/// alias 冲突会触发启动校验失败;先到者胜出,重复 alias 丢弃并告警)
fn merge_providers(
    mut base: Vec<ProviderConfig>,
    injected: Vec<ProviderConfig>,
) -> Vec<ProviderConfig> {
    let mut seen: std::collections::HashSet<String> = base
        .iter()
        .flat_map(|p| p.models.iter().map(|m| m.alias.clone()))
        .collect();
    for mut p in injected {
        p.models.retain(|m| {
            if seen.contains(&m.alias) {
                tracing::warn!(
                    "alias 冲突,丢弃 {} 的模型 {}(alias {})",
                    p.name,
                    m.name,
                    m.alias
                );
                false
            } else {
                seen.insert(m.alias.clone());
                true
            }
        });
        base.push(p);
    }
    base
}

async fn cmd_antigravity_login(
    config_path: &str,
    auth_dir: Option<String>,
    no_browser: bool,
    callback_port: Option<u16>,
) -> Result<()> {
    let cfg = load_optional_config(config_path);
    let proxy_url = cfg.as_ref().and_then(|c| c.server.proxy_url.clone());
    let opts = AntigravityLoginOptions {
        auth_dir: antigravity_auth_dir_from(config_path, auth_dir),
        no_browser,
        callback_port: callback_port.unwrap_or(CALLBACK_PORT),
        proxy_url,
    };
    run_antigravity_login(opts).await?;
    Ok(())
}

fn cmd_antigravity_status(config_path: &str, auth_dir: Option<String>) -> Result<()> {
    let dir = antigravity_auth_dir_from(config_path, auth_dir);
    let now = SystemTime::now();
    let entries = list_antigravity(&dir)?;
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

async fn cmd_xai_login(
    config_path: &str,
    auth_dir: Option<String>,
    no_browser: bool,
) -> Result<()> {
    let cfg = load_optional_config(config_path);
    let proxy_url = cfg.as_ref().and_then(|c| c.server.proxy_url.clone());
    let opts = XAILoginOptions {
        auth_dir: xai_auth_dir_from(config_path, auth_dir),
        no_browser,
        proxy_url,
    };
    run_xai_login(opts).await?;
    Ok(())
}

fn cmd_xai_status(config_path: &str, auth_dir: Option<String>) -> Result<()> {
    let dir = xai_auth_dir_from(config_path, auth_dir);
    let now = SystemTime::now();
    let entries = list_xai(&dir)?;
    if entries.is_empty() {
        println!("无 xAI 凭证: {}", dir.display());
        return Ok(());
    }
    println!("auth_dir: {}", dir.display());
    for (path, cred) in entries {
        let status = if cred.disabled {
            "disabled"
        } else if cred.is_fresh(now, XAI_REFRESH_SKEW_SECS) {
            "fresh"
        } else {
            "stale"
        };
        let email = if cred.email.is_empty() {
            "-"
        } else {
            cred.email.as_str()
        };
        let sub = if cred.sub.is_empty() {
            "-"
        } else {
            cred.sub.as_str()
        };
        println!(
            "{}  email={email}  sub={sub}  {status}  expired={}",
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

/// 构建 UserAgentSet(从配置或使用默认值)
fn build_user_agents(config: Option<&config::UserAgents>) -> UserAgentSet {
    const DEFAULT_CLAUDE_CLI: &str = "claude-cli/2.1.246";
    const DEFAULT_CODEX_TUI: &str =
        "codex-tui/0.149.1 (Mac OS 26.6.2; arm64) ghostty/1.3.1 (codex-tui; 0.149.1)";
    const DEFAULT_GROK_VERSION: &str = "1.0.5";
    const DEFAULT_ANTIGRAVITY: &str = "antigravity/hub/2.10.0 darwin/arm64";

    UserAgentSet {
        claude_cli: Arc::new(
            config
                .and_then(|c| c.claude_cli.clone())
                .unwrap_or_else(|| DEFAULT_CLAUDE_CLI.to_string()),
        ),
        codex_tui: Arc::new(
            config
                .and_then(|c| c.codex_tui.clone())
                .unwrap_or_else(|| DEFAULT_CODEX_TUI.to_string()),
        ),
        grok_version: Arc::new(
            config
                .and_then(|c| c.grok_version.clone())
                .unwrap_or_else(|| DEFAULT_GROK_VERSION.to_string()),
        ),
        antigravity: Arc::new(
            config
                .and_then(|c| c.antigravity.clone())
                .unwrap_or_else(|| DEFAULT_ANTIGRAVITY.to_string()),
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::pin_auth_dir;
    use ccextra_server::antigravity::resolve_auth_dir as resolve_antigravity_auth_dir;
    use ccextra_server::xai::resolve_auth_dir as resolve_xai_auth_dir;
    use std::path::PathBuf;

    #[test]
    fn pin_default_next_to_config() {
        let dir = pin_auth_dir("/tmp/proj/config.yaml", None, resolve_antigravity_auth_dir);
        assert_eq!(dir, PathBuf::from("/tmp/proj/.cache/antigravity"));
        assert!(!dir.to_string_lossy().contains(".cli-proxy-api"));

        let xai_dir = pin_auth_dir("/tmp/proj/config.yaml", None, resolve_xai_auth_dir);
        assert_eq!(xai_dir, PathBuf::from("/tmp/proj/.cache/xai"));
    }

    #[test]
    fn pin_keeps_absolute_and_tilde() {
        assert_eq!(
            pin_auth_dir(
                "/tmp/proj/config.yaml",
                Some("/abs/creds"),
                resolve_antigravity_auth_dir
            ),
            PathBuf::from("/abs/creds")
        );
        let home = std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(PathBuf::from)
            .unwrap();
        assert_eq!(
            pin_auth_dir(
                "/tmp/proj/config.yaml",
                Some("~/.cli-proxy-api"),
                resolve_antigravity_auth_dir
            ),
            home.join(".cli-proxy-api")
        );
    }
}
