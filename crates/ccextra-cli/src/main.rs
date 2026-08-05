use anyhow::Result;
use ccextra_core::cache_stabilization::drift_detector::DriftState;
use ccextra_server::http::{AppState, ReloadData};
use ccextra_server::serve;
use ccextra_server::upstream::UpstreamClient;
use clap::Parser;
use std::sync::Arc;
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
    #[arg(short, long, default_value = "config.yaml")]
    config: String,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

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
        })
    });
    let state = AppState {
        providers: Arc::new(RwLock::new(config.providers)),
        payload_rules: Arc::new(RwLock::new(config.payload.unwrap_or_default())),
        normalize: config.normalize,
        logging: config.logging,
        upstream: UpstreamClient::new(config.server.proxy_url),
        reload,
        secret: config.secret_key,
        drift: DriftState::new(1000),
    };

    // 启动 HTTP 服务
    let addr = format!("{}:{}", config.server.host, config.server.port);
    tracing::info!("ccextra 启动中...");

    serve(&addr, state).await?;

    Ok(())
}
