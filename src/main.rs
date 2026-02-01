// === Jemalloc 全局分配器配置（用于内存分析）===
#[cfg(not(target_env = "msvc"))]
#[global_allocator]
static ALLOC: tikv_jemallocator::Jemalloc = tikv_jemallocator::Jemalloc;

pub mod config;
pub mod credential;
pub mod error;
pub mod gateway;
pub mod logging;
pub mod quota_pool;
pub mod runtime_config;
pub mod signature;
pub mod util;
pub mod vertex;

use anyhow::Context;
use axum::routing::{get, post};
use axum::{Router, middleware};
use std::net::SocketAddr;
use std::sync::Arc;
use tracing_subscriber::EnvFilter;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cfg = config::Config::load();

    init_tracing(&cfg);

    // 初始化运行时配置
    runtime_config::init(&cfg);

    let store = Arc::new(credential::store::Store::new(cfg.clone()));
    if let Err(e) = store.load().await {
        tracing::warn!("加载 accounts.json 失败: {e:#}");
    }
    // 启动时先刷新所有账号 token，避免 WebUI 首次加载配额时出现大量 401/未知状态。
    // 刷新失败不阻塞启动：保留原始账号信息，后续按需再刷新。
    let account_count = store.count().await;
    if account_count > 0 {
        tracing::info!("启动刷新所有账号 token（共 {account_count} 个）...");
        match store.refresh_all().await {
            Ok((success, failed)) => {
                tracing::info!("启动刷新完成：成功 {success}，失败 {failed}");
            }
            Err(e) => {
                tracing::warn!("启动刷新账号失败：{e:#}");
            }
        }
    }

    // 后台主动刷新：定期刷新即将过期的 token，尽量避免请求路径遭遇 401。
    credential::refresh_task::spawn_token_refresh_task(store.clone(), cfg.clone());

    let sig_mgr = signature::manager::Manager::new(&cfg.data_dir)
        .await
        .context("初始化 signature manager 失败")?;

    let vertex =
        Arc::new(vertex::client::VertexClient::new(&cfg).context("初始化 VertexClient 失败")?);

    // 配额池：后台刷新各账号配额，并用于按模型分组选择更“有余额”的账号。
    let quota_pool = Arc::new(quota_pool::QuotaPoolManager::new());
    quota_pool::spawn_refresh_task(
        store.clone(),
        cfg.clone(),
        (*vertex).clone(),
        quota_pool.clone(),
    );

    // API 网关状态（OpenAI/Claude 共用同一份字段集合，便于注册多套路由）。
    let api_state = Arc::new(gateway::claude::ClaudeState {
        cfg: cfg.clone(),
        vertex: (*vertex).clone(),
        store: store.clone(),
        quota_pool: quota_pool.clone(),
        sig_mgr,
    });

    // Manager WebUI 状态
    let manager_state = Arc::new(gateway::manager::ManagerState {
        store: store.clone(),
        vertex: vertex.clone(),
        quota_cache: gateway::manager::QuotaCache::new(),
        quota_pool: quota_pool.clone(),
    });

    // === 公开路由（不需要认证）===
    let public_routes = Router::new()
        .route("/health", get(handle_health))
        .route("/debug/pprof/heap", get(handle_pprof_heap))
        .route("/login", get(gateway::manager::handle_login_view))
        .route("/login", post(gateway::manager::handle_login))
        .route("/logout", get(gateway::manager::handle_logout));

    // === API 路由 ===
    let api_routes = Router::new()
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
        .route("/v1/messages", post(gateway::claude::handle_messages))
        // 兼容 Go ServeMux：允许尾随斜杠的同一路径
        .route("/v1/messages/", post(gateway::claude::handle_messages))
        .with_state(api_state);

    // === Manager API 路由（需要认证）===
    let manager_api_routes = Router::new()
        .route("/manager/api/stats", get(gateway::manager::handle_stats))
        .route("/manager/api/list", get(gateway::manager::handle_list))
        .route("/manager/api/delete", post(gateway::manager::handle_delete))
        .route("/manager/api/toggle", post(gateway::manager::handle_toggle))
        .route(
            "/manager/api/refresh",
            post(gateway::manager::handle_refresh),
        )
        .route(
            "/manager/api/refresh_all",
            post(gateway::manager::handle_refresh_all),
        )
        .route("/manager/api/quota", get(gateway::manager::handle_quota))
        .route(
            "/manager/api/quota/all",
            post(gateway::manager::handle_quota_all),
        )
        .route(
            "/manager/api/oauth/url",
            get(gateway::manager::handle_oauth_url),
        )
        .route(
            "/manager/api/oauth/parse-url",
            post(gateway::manager::handle_oauth_parse_url),
        )
        .route(
            "/manager/api/settings",
            get(gateway::manager::handle_settings_get),
        )
        .route(
            "/manager/api/settings",
            post(gateway::manager::handle_settings_post),
        )
        .with_state(manager_state.clone());

    // === Dashboard 路由（需要认证）===
    let dashboard_routes = Router::new()
        .route("/", get(gateway::manager::handle_dashboard))
        // 捕获 /oauth-callback 等路径，也显示 dashboard
        .fallback(gateway::manager::handle_dashboard)
        .with_state(manager_state.clone());

    // 受保护路由（需要认证）
    let protected_routes = Router::new()
        .merge(manager_api_routes)
        .merge(dashboard_routes)
        .layer(middleware::from_fn(
            gateway::manager::manager_auth_middleware,
        ));

    // 组合所有路由
    let app = Router::new()
        .merge(public_routes)
        .merge(api_routes)
        .merge(protected_routes);

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
    // Go 版的 DEBUG=off/low/high 主要控制"客户端/后端详细日志块"。
    // Rust 版扩展：DEBUG=medium（旧 high：格式化/脱敏），DEBUG=high（raw：完全原始流式输出）。
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

/// 导出 jemalloc 堆内存分析数据（pprof 格式）
/// 使用方式：
/// 1. curl http://localhost:PORT/debug/pprof/heap > heap.pb.gz
/// 2. go tool pprof -http=:8080 heap.pb.gz
#[cfg(not(target_env = "msvc"))]
async fn handle_pprof_heap() -> impl axum::response::IntoResponse {
    use axum::http::{StatusCode, header};
    use axum::response::Response;

    match jemalloc_pprof::PROF_CTL.as_ref() {
        Some(prof_ctl) => {
            let mut prof = prof_ctl.lock().await;
            match prof.dump_pprof() {
                Ok(pprof_data) => Response::builder()
                    .status(StatusCode::OK)
                    .header(header::CONTENT_TYPE, "application/octet-stream")
                    .header(
                        header::CONTENT_DISPOSITION,
                        "attachment; filename=\"heap.pb.gz\"",
                    )
                    .body(axum::body::Body::from(pprof_data))
                    .unwrap(),
                Err(e) => Response::builder()
                    .status(StatusCode::INTERNAL_SERVER_ERROR)
                    .body(axum::body::Body::from(format!("Failed to dump pprof: {e}")))
                    .unwrap(),
            }
        }
        None => Response::builder()
            .status(StatusCode::SERVICE_UNAVAILABLE)
            .body(axum::body::Body::from(
                "jemalloc profiling not enabled. Set MALLOC_CONF=prof:true before starting.",
            ))
            .unwrap(),
    }
}

#[cfg(target_env = "msvc")]
async fn handle_pprof_heap() -> &'static str {
    "jemalloc profiling is not supported on MSVC targets"
}
