use anyhow::Result;
use ccextra_core::cache_stabilization::drift_detector::DriftState;
use ccextra_core::route::ProviderConfig;
use ccextra_server::antigravity::{
    constants::{CALLBACK_PORT, REFRESH_SKEW_SECS},
    list as list_antigravity, resolve_auth_dir as resolve_antigravity_auth_dir,
    run_login as run_antigravity_login, LoginOptions as AntigravityLoginOptions,
};
use ccextra_server::codex::{
    list as list_codex, resolve_auth_dir as resolve_codex_auth_dir,
    run_login as run_codex_login, CodexLoginOptions,
};
use ccextra_server::http::{
    publish_refreshed_providers, AppState, ConfigSnapshot, ProviderRefreshConfig, ReloadData,
    RuntimeConfig, UserAgentSet,
};
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
    /// 浏览器 PKCE 登录 Codex (OpenAI ChatGPT 订阅) 并写入凭证
    #[command(name = "codex-login")]
    CodexLogin {
        /// 凭证目录,默认配置文件旁 .cache/codex
        #[arg(long)]
        auth_dir: Option<String>,
        /// 不自动打开浏览器,只打印 URL
        #[arg(long)]
        no_browser: bool,
        /// 本地回调端口,默认 1455
        #[arg(long)]
        callback_port: Option<u16>,
    },
    /// 列出已保存的 Codex 凭证(不打印 token)
    #[command(name = "codex-status")]
    CodexStatus {
        /// 凭证目录,默认配置文件旁 .cache/codex
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
        Some(Commands::CodexLogin {
            auth_dir,
            no_browser,
            callback_port,
        }) => {
            return cmd_codex_login(&cli.config, auth_dir, no_browser, callback_port).await;
        }
        Some(Commands::CodexStatus { auth_dir }) => {
            return cmd_codex_status(&cli.config, auth_dir);
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

    let refresh = provider_refresh_config(&cli.config, &config);

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

    // 动态加载 Codex providers
    let codex_auth_dir = pin_auth_dir(
        &cli.config,
        config.codex_auth_dir.as_deref(),
        resolve_codex_auth_dir,
    );
    let codex_providers = ccextra_server::codex::load_codex_providers(
        &codex_auth_dir,
        config.server.proxy_url.as_deref(),
    )
    .await;
    if !codex_providers.is_empty() {
        tracing::info!("动态加载 {} 个 Codex providers", codex_providers.len());
    }

    // 合并配置文件 providers 和 xAI providers
    // Antigravity 模型列表需在线拉取(对齐 CPA 启动模式:已知数据先行、
    // 后台刷新、失败保旧),不阻塞监听 —— 转入 serve 之后的后台任务注入
    let mut all_providers = merge_providers(config.providers, Vec::new());
    all_providers = merge_providers(all_providers, xai_providers);
    all_providers = merge_providers(all_providers, codex_providers);

    // 启动时验证配置
    ccextra_core::route::validate_providers(&all_providers)?;
    tracing::info!("配置验证通过");

    // 构建应用状态
    let config_path = cli.config.clone();
    let reload = Arc::new(move || -> std::pin::Pin<Box<dyn std::future::Future<Output = anyhow::Result<ReloadData>> + Send>> {
        let config_path = config_path.clone();
        Box::pin(async move {
            let cfg = Config::load(&config_path)?;
            let refresh = provider_refresh_config(&config_path, &cfg);

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

            let codex_auth_dir = pin_auth_dir(&config_path, cfg.codex_auth_dir.as_deref(), resolve_codex_auth_dir);
            let codex_providers = ccextra_server::codex::load_codex_providers(
                &codex_auth_dir,
                cfg.server.proxy_url.as_deref(),
            )
            .await;

            let mut providers = merge_providers(cfg.providers, antigravity_providers);
            providers = merge_providers(providers, xai_providers);
            providers = merge_providers(providers, codex_providers);

            let user_agents = build_user_agents(cfg.user_agents.as_ref());
            let thinking_registry = load_thinking_registry(&config_path, cfg.models_file.as_deref())?;

            Ok(ReloadData {
                providers,
                payload_rules: cfg.payload.unwrap_or_default(),
                normalize: cfg.normalize,
                logging: cfg.logging,
                secret: cfg.secret_key,
                proxy_url: cfg.server.proxy_url,
                antigravity: cfg.antigravity,
                user_agents,
                thinking_registry,
                refresh,
            })
        })
    });

    let user_agents = build_user_agents(config.user_agents.as_ref());
    let thinking_registry = load_thinking_registry(&cli.config, config.models_file.as_deref())?;

    let runtime = RuntimeConfig {
        normalize: config.normalize,
        logging: config.logging,
        secret: config.secret_key,
        upstream: UpstreamClient::with_ant_pool(
            config.server.proxy_url,
            config.antigravity.as_ref(),
        ),
        user_agents,
        thinking_registry,
    };
    let payload_rules = config.payload.unwrap_or_default();
    let config_snapshot = Arc::new(ConfigSnapshot {
        version: 1,
        providers: all_providers,
        payload_rules,
        runtime,
        refresh,
    });

    let state = AppState {
        config: Arc::new(RwLock::new(config_snapshot)),
        reload_lock: Arc::new(tokio::sync::Mutex::new(())),
        reload,
        drift: DriftState::new(1000),
        replay_cache: ccextra_server::sse::replay_cache::ReplayCache::new(
            std::time::Duration::from_secs(3600),
            1024,
        ),
        last_input_tokens: Arc::new(std::sync::Mutex::new(
            ccextra_server::http::session_tokens::SessionTokenCache::new(),
        )),
    };

    // Antigravity 后台注入(对齐 CPA 启动模式:listening 不等在线模型列表;
    // 任务内部立即拉取一次,成功替换 providers,失败保旧等下轮)
    tokio::spawn(run_antigravity_injection(state.config.clone()));

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

fn codex_auth_dir_from(config_path: &str, override_dir: Option<String>) -> PathBuf {
    let raw = if let Some(dir) = override_dir {
        Some(dir)
    } else {
        load_optional_config(config_path).and_then(|cfg| cfg.codex_auth_dir)
    };
    pin_auth_dir(config_path, raw.as_deref(), resolve_codex_auth_dir)
}

/// 读用户 models.json。缺文件 = 空表不钳;解析失败则启动/reload 报错。
fn load_thinking_registry(
    config_path: &str,
    models_file: Option<&str>,
) -> anyhow::Result<Arc<Vec<ccextra_core::thinking::ModelCapability>>> {
    let path = pin_path(config_path, models_file.unwrap_or("models.json"));
    match std::fs::read_to_string(&path) {
        Ok(text) => {
            let models = ccextra_core::thinking::parse_registry(&text).map_err(|e| {
                anyhow::anyhow!("解析 reasoning 注册表失败 {}: {e}", path.display())
            })?;
            tracing::info!(
                "加载 reasoning 注册表 {} 条: {}",
                models.len(),
                path.display()
            );
            Ok(Arc::new(models))
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            tracing::info!("未找到 reasoning 注册表 {}, 不钳制 effort", path.display());
            Ok(Arc::new(Vec::new()))
        }
        Err(e) => Err(anyhow::anyhow!(
            "读取 reasoning 注册表失败 {}: {e}",
            path.display()
        )),
    }
}

/// 相对路径钉在配置文件所在目录,不跟进程 cwd 走
fn pin_path(config_path: &str, raw: &str) -> PathBuf {
    let path = PathBuf::from(raw);
    if path.is_absolute() {
        return path;
    }
    let parent = PathBuf::from(config_path);
    match parent.parent().filter(|p| !p.as_os_str().is_empty()) {
        Some(dir) => dir.join(path),
        None => path,
    }
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

/// 刷新参数随成功发布的配置一起更新,相对目录始终相对配置文件解析。
fn provider_refresh_config(config_path: &str, config: &Config) -> ProviderRefreshConfig {
    ProviderRefreshConfig {
        auth_dir: Some(pin_auth_dir(
            config_path,
            config.auth_dir.as_deref(),
            resolve_antigravity_auth_dir,
        )),
        xai_auth_dir: Some(pin_auth_dir(
            config_path,
            config.xai_auth_dir.as_deref(),
            resolve_xai_auth_dir,
        )),
        codex_auth_dir: Some(pin_auth_dir(
            config_path,
            config.codex_auth_dir.as_deref(),
            resolve_codex_auth_dir,
        )),
        proxy_url: config.server.proxy_url.clone(),
        static_providers: config.providers.clone(),
    }
}

/// 动态拉取失败时保留整个已发布集合;静态配置仅由成功的 reload 更新。
async fn load_refreshed_providers(refresh: ProviderRefreshConfig) -> Option<Vec<ProviderConfig>> {
    let auth_dir = refresh.auth_dir.as_ref()?;
    let xai = if let Some(dir) = refresh.xai_auth_dir.as_ref() {
        ccextra_server::xai::load_xai_providers(dir, refresh.proxy_url.as_deref()).await
    } else {
        Vec::new()
    };
    let codex = if let Some(dir) = refresh.codex_auth_dir.as_ref() {
        ccextra_server::codex::load_codex_providers(dir, refresh.proxy_url.as_deref()).await
    } else {
        Vec::new()
    };
    let injected = ccextra_server::antigravity::load_antigravity_providers(
        auth_dir,
        refresh.proxy_url.as_deref(),
    )
    .await;
    if injected.is_empty() {
        tracing::warn!("Antigravity 无可用模型/provider,保持现有数据");
        return None;
    }
    let set = merge_providers(refresh.static_providers, xai);
    let set = merge_providers(set, codex);
    Some(merge_providers(set, injected))
}

async fn refresh_providers<F, Fut>(
    config: &Arc<RwLock<Arc<ConfigSnapshot>>>,
    load: F,
) -> anyhow::Result<bool>
where
    F: FnOnce(ProviderRefreshConfig) -> Fut,
    Fut: std::future::Future<Output = Option<Vec<ProviderConfig>>>,
{
    let snapshot = config.read().await.clone();
    let Some(providers) = load(snapshot.refresh.clone()).await else {
        return Ok(false);
    };
    publish_refreshed_providers(config, snapshot.version, providers).await
}

/// Antigravity 注入任务:启动即拉取,成功注入后每 3 小时刷新;失败保旧。
/// 每轮从当前快照获取参数,不发布尚未成功 reload 的磁盘配置。
async fn run_antigravity_injection(config: Arc<RwLock<Arc<ConfigSnapshot>>>) {
    /// 刷新周期(对齐 CPA modelsRefreshInterval = 3h)
    const REFRESH_INTERVAL: std::time::Duration = std::time::Duration::from_secs(3 * 3600);

    let mut ticker = tokio::time::interval(REFRESH_INTERVAL);
    // 首次 tick 立即到期,启动后马上拉取一次(CPA tryStartupRefresh 同构)
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut ready = false;
    loop {
        ticker.tick().await;
        match refresh_providers(&config, load_refreshed_providers).await {
            Ok(true) => {
                if ready {
                    tracing::info!("Antigravity 周期刷新完成");
                } else {
                    ready = true;
                    tracing::info!("Antigravity providers 已就绪");
                }
            }
            Ok(false) => {}
            Err(error) => tracing::warn!("provider 刷新校验失败,保持现有数据: {error}"),
        }
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

async fn cmd_codex_login(
    config_path: &str,
    auth_dir: Option<String>,
    no_browser: bool,
    callback_port: Option<u16>,
) -> Result<()> {
    let cfg = load_optional_config(config_path);
    let proxy_url = cfg.as_ref().and_then(|c| c.server.proxy_url.clone());
    let opts = CodexLoginOptions {
        auth_dir: codex_auth_dir_from(config_path, auth_dir),
        no_browser,
        callback_port: callback_port
            .unwrap_or(ccextra_server::codex::constants::DEFAULT_CALLBACK_PORT),
        proxy_url,
    };
    run_codex_login(opts).await?;
    Ok(())
}

fn cmd_codex_status(config_path: &str, auth_dir: Option<String>) -> Result<()> {
    let dir = codex_auth_dir_from(config_path, auth_dir);
    let now = SystemTime::now();
    let entries = list_codex(&dir)?;
    if entries.is_empty() {
        println!("无 Codex 凭证: {}", dir.display());
        return Ok(());
    }
    println!("auth_dir: {}", dir.display());
    for (path, cred) in entries {
        let status = if cred.disabled {
            "disabled"
        } else if cred.is_fresh(now, ccextra_server::codex::constants::REFRESH_SKEW_SECS) {
            "fresh"
        } else {
            "stale"
        };
        let email = if cred.email.is_empty() {
            "-"
        } else {
            cred.email.as_str()
        };
        let account = if cred.account_id.is_empty() {
            "-"
        } else {
            cred.account_id.as_str()
        };
        let plan = if cred.plan_type.is_empty() {
            "-"
        } else {
            cred.plan_type.as_str()
        };
        println!(
            "{}  email={email}  account={account}  plan={plan}  {status}  expired={}",
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
    const DEFAULT_CLAUDE_CLI: &str = "claude-cli/2.1.258";
    const DEFAULT_CODEX_TUI: &str = "codex_cli_rs/0.153.3 (Mac OS 26.6.2; arm64)";
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
    use super::{
        build_user_agents, load_refreshed_providers, load_thinking_registry, pin_auth_dir,
        pin_path, provider_refresh_config, refresh_providers, Arc, Config, ProviderConfig,
        RuntimeConfig, RwLock, UpstreamClient,
    };
    use ccextra_server::antigravity::resolve_auth_dir as resolve_antigravity_auth_dir;
    use ccextra_server::xai::resolve_auth_dir as resolve_xai_auth_dir;
    use std::io::Write;
    use std::path::PathBuf;

    fn refresh_state() -> Arc<RwLock<Arc<ccextra_server::http::ConfigSnapshot>>> {
        use ccextra_server::http::{ConfigSnapshot, LoggingConfig, NormalizeConfig};
        Arc::new(RwLock::new(Arc::new(ConfigSnapshot {
            version: 1,
            providers: vec![],
            payload_rules: vec![],
            runtime: RuntimeConfig {
                normalize: NormalizeConfig {
                    enabled: false,
                    drift_detector: false,
                },
                logging: LoggingConfig {
                    level: "info".into(),
                    request_body: false,
                },
                secret: None,
                upstream: UpstreamClient::new(None),
                user_agents: build_user_agents(None),
                thinking_registry: Arc::new(vec![]),
            },
            refresh: Default::default(),
        })))
    }

    #[tokio::test]
    async fn refresh_uses_latest_published_parameters() {
        let state = refresh_state();
        {
            let mut current = state.write().await;
            let snapshot = Arc::make_mut(&mut current);
            snapshot.version = 2;
            snapshot.refresh.auth_dir = Some(PathBuf::from("/new/antigravity"));
            snapshot.refresh.xai_auth_dir = Some(PathBuf::from("/new/xai"));
            snapshot.refresh.proxy_url = Some("http://new-proxy:8080".into());
        }
        let published = refresh_providers(&state, |refresh| async move {
            assert_eq!(refresh.auth_dir, Some(PathBuf::from("/new/antigravity")));
            assert_eq!(refresh.xai_auth_dir, Some(PathBuf::from("/new/xai")));
            assert_eq!(refresh.proxy_url.as_deref(), Some("http://new-proxy:8080"));
            Some(vec![ProviderConfig::new(
                "new-account".into(),
                ccextra_core::route::Protocol::Antigravity,
                vec!["https://example.invalid".into()],
                "fixture-key".into(),
                None,
                false,
                vec![],
            )])
        })
        .await
        .unwrap();
        assert!(published);
        assert_eq!(state.read().await.providers[0].name, "new-account");
    }

    #[tokio::test]
    async fn refresh_failure_keeps_entire_published_snapshot() {
        let state = refresh_state();
        let directory = tempfile::tempdir().unwrap();
        {
            let mut current = state.write().await;
            let snapshot = Arc::make_mut(&mut current);
            snapshot.refresh.auth_dir = Some(directory.path().join("antigravity"));
            snapshot.refresh.xai_auth_dir = Some(directory.path().join("xai"));
        }
        let before = state.read().await.clone();
        assert!(!refresh_providers(&state, load_refreshed_providers)
            .await
            .unwrap());
        assert!(Arc::ptr_eq(&before, &*state.read().await));
    }

    #[tokio::test]
    async fn refresh_discards_result_after_new_configuration_is_published() {
        let state = refresh_state();
        let published = refresh_providers(&state, |_| async {
            let mut current = state.write().await;
            let snapshot = Arc::make_mut(&mut current);
            snapshot.version = 2;
            snapshot.refresh.auth_dir = Some(PathBuf::from("/new/antigravity"));
            Some(vec![ProviderConfig::new(
                "old-account".into(),
                ccextra_core::route::Protocol::Antigravity,
                vec!["https://example.invalid".into()],
                "fixture-key".into(),
                None,
                false,
                vec![],
            )])
        })
        .await
        .unwrap();
        assert!(!published);
        let snapshot = state.read().await;
        assert_eq!(snapshot.version, 2);
        assert!(snapshot.providers.is_empty());
        assert_eq!(
            snapshot.refresh.auth_dir,
            Some(PathBuf::from("/new/antigravity"))
        );
    }

    #[test]
    fn refresh_parameters_pin_dirs_and_retain_static_providers() {
        let config: Config = serde_yaml::from_str(
            r#"
server:
  host: 127.0.0.1
  port: 8222
  proxy_url: http://new-proxy:8080
providers:
  - name: static-provider
    protocol: claude
    base_url: https://example.invalid
    key: fixture-key
    models: []
normalize: { enabled: false, drift_detector: false }
logging: { level: info, request_body: false }
auth_dir: new-antigravity
xai_auth_dir: new-xai
"#,
        )
        .unwrap();
        let refresh = provider_refresh_config("/tmp/project/config.yaml", &config);
        assert_eq!(
            refresh.auth_dir,
            Some(PathBuf::from("/tmp/project/new-antigravity"))
        );
        assert_eq!(
            refresh.xai_auth_dir,
            Some(PathBuf::from("/tmp/project/new-xai"))
        );
        assert_eq!(refresh.proxy_url.as_deref(), Some("http://new-proxy:8080"));
        assert_eq!(refresh.static_providers.len(), 1);
        assert_eq!(refresh.static_providers[0].name, "static-provider");
    }

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

    #[test]
    fn pin_path_relative_next_to_config() {
        assert_eq!(
            pin_path("/tmp/proj/config.yaml", "models.json"),
            PathBuf::from("/tmp/proj/models.json")
        );
        assert_eq!(
            pin_path("/tmp/proj/config.yaml", "/abs/models.json"),
            PathBuf::from("/abs/models.json")
        );
    }

    #[test]
    fn load_thinking_registry_missing_is_empty() {
        let reg = load_thinking_registry("/tmp/does-not-exist/config.yaml", None).unwrap();
        assert!(reg.is_empty());
    }

    #[test]
    fn load_thinking_registry_parses_file() {
        let dir = tempfile::tempdir().unwrap();
        let models = dir.path().join("models.json");
        std::fs::write(
            &models,
            r#"{"models":[{"id":"gpt-6-astra","reasoning_levels":["low","medium"]}]}"#,
        )
        .unwrap();
        let cfg = dir.path().join("config.yaml");
        let mut f = std::fs::File::create(&cfg).unwrap();
        f.write_all(b"unused").unwrap();
        let reg = load_thinking_registry(cfg.to_str().unwrap(), None).unwrap();
        assert_eq!(reg.len(), 1);
        assert_eq!(reg[0].id, "gpt-6-astra");
    }
}
