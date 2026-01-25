pub mod config;
pub mod credential;
pub mod error;
pub mod gateway;
pub mod logging;
pub mod signature;
pub mod util;
pub mod vertex;

use anyhow::Context;
use axum::Router;
use axum::routing::{get, post};
use std::net::SocketAddr;
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

const DEFAULT_BACKEND_HOST: &str = "daily-cloudcode-pa.sandbox.googleapis.com";

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = config::Config::load();

    init_tracing(&cfg);

    let store = Arc::new(credential::store::Store::new(cfg.clone()));
    if let Err(e) = store.load().await {
        tracing::warn!("加载 accounts.json 失败: {e:#}");
    }

    let sig_mgr = signature::manager::Manager::new(&cfg.data_dir)
        .await
        .context("初始化 signature manager 失败")?;

    let endpoint = vertex::client::Endpoint {
        key: cfg.endpoint_mode.clone(),
        host: DEFAULT_BACKEND_HOST.to_string(),
    };
    let vertex = vertex::client::VertexClient::new(&cfg).context("初始化 VertexClient 失败")?;

    let openai_state = Arc::new(gateway::openai::handler::OpenAIState {
        cfg: cfg.clone(),
        endpoint,
        vertex,
        store,
        sig_mgr,
    });

    let app = Router::new()
        .route("/health", get(handle_health))
        .route(
            "/v1/models",
            get(gateway::openai::handler::handle_list_models),
        )
        .route(
            "/v1/chat/completions",
            post(gateway::openai::handler::handle_chat_completions),
        )
        // 兼容 Go ServeMux：允许尾随斜杠的同一路径
        .route(
            "/v1/chat/completions/",
            post(gateway::openai::handler::handle_chat_completions),
        )
        .with_state(openai_state);

    let addr: SocketAddr = format!("{}:{}", cfg.host, cfg.port)
        .parse()
        .unwrap_or_else(|_| SocketAddr::from(([0, 0, 0, 0], cfg.port)));

    tracing::info!("Server listening on http://{addr}");

    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .context("绑定监听端口失败")?;

    axum::serve(listener, app)
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("服务异常退出")?;

    Ok(())
}

async fn handle_health() -> &'static str {
    "ok"
}

fn init_tracing(cfg: &config::Config) {
    // Go 版的 DEBUG=low/high 主要控制“客户端/后端详细日志块”。
    // 这里默认把依赖库日志控制在 warn（避免噪声），但确保本项目自身日志至少为 info，
    // 以免环境中预设的 RUST_LOG=warn 把关键调试日志过滤掉。
    let debug = cfg.debug.trim().to_lowercase();
    let filter = if debug == "off" {
        EnvFilter::new("off")
    } else {
        let env = std::env::var("RUST_LOG").unwrap_or_default();
        let env = env.trim();
        if env.is_empty() {
            EnvFilter::new("warn,ant2api=info")
        } else if env.contains("ant2api") {
            EnvFilter::new(env)
        } else {
            EnvFilter::new(format!("{env},ant2api=info"))
        }
    };

    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .with_level(true)
        .try_init();
}

async fn shutdown_signal() {
    let _ = tokio::signal::ctrl_c().await;
    tracing::info!("收到退出信号，准备关闭服务...");
}
