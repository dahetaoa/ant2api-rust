//! Manager WebUI 处理器。
//!
//! 实现与 Go 版本完全一致的 WebUI 功能。

use axum::{
    Form, Json,
    extract::{OriginalUri, Query, State},
    http::{HeaderMap, Method, StatusCode, header},
    response::sse::{Event, Sse},
    response::{Html, IntoResponse, Redirect, Response},
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use crate::config::Config;
use crate::credential::oauth;
use crate::credential::store::Store;
use crate::credential::types::Account;
use crate::gateway::manager::templates::{self, ViewAccount, ViewQuotaGroup, to_view_accounts};
use crate::logging;
use crate::quota_pool::QuotaPoolManager;
use crate::quota_pool::{AccountQuota, QuotaGroup};
use crate::runtime_config::{self, WebUISettings};
use crate::signature;
use crate::util::id;
use crate::util::model as modelutil;

use askama::Template;
use futures::StreamExt;

/// 将 JSON 字符串中的非 ASCII 字符转义为 `\\uXXXX`（用于放进 HTTP Header，避免中文乱码）。
fn json_escape_non_ascii(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if ch.is_ascii() {
            out.push(ch);
            continue;
        }
        let mut buf = [0u16; 2];
        for u in ch.encode_utf16(&mut buf) {
            use std::fmt::Write as _;
            let _ = write!(out, "\\u{:04X}", u);
        }
    }
    out
}

/// 生成 ASCII 安全的 `HX-Trigger` Header 值（避免中文消息在浏览器端乱码）。
fn hx_trigger_value(v: serde_json::Value) -> String {
    json_escape_non_ascii(&v.to_string())
}

/// Manager 应用状态
pub struct ManagerState {
    pub store: Arc<Store>,
    pub quota_pool: Arc<QuotaPoolManager>,
    pub data_dir: String,
    pub cfg: Config,
}

/// Cookie 名称
const SESSION_COOKIE_NAME: &str = "grok_admin_session";
/// Cookie 值
const SESSION_COOKIE_VALUE: &str = "authenticated";

// ============================================================================
// 认证相关
// ============================================================================

/// 检查请求是否已认证
fn is_authenticated(headers: &HeaderMap) -> bool {
    headers
        .get(header::COOKIE)
        .and_then(|v| v.to_str().ok())
        .map(|cookies| {
            cookies.split(';').any(|c| {
                let c = c.trim();
                c == format!("{}={}", SESSION_COOKIE_NAME, SESSION_COOKIE_VALUE)
                    || c.starts_with(&format!(
                        "{}={};",
                        SESSION_COOKIE_NAME, SESSION_COOKIE_VALUE
                    ))
            })
        })
        .unwrap_or(false)
}

/// 设置认证 Cookie
fn set_auth_cookie() -> String {
    let expires = chrono::Utc::now() + chrono::Duration::hours(24);
    let expires_str = expires.format("%a, %d %b %Y %H:%M:%S GMT").to_string();
    format!(
        "{}={}; Path=/; HttpOnly; Expires={}",
        SESSION_COOKIE_NAME, SESSION_COOKIE_VALUE, expires_str
    )
}

/// 清除认证 Cookie
fn clear_auth_cookie() -> String {
    format!("{}=; Path=/; HttpOnly; Max-Age=0", SESSION_COOKIE_NAME)
}

// ============================================================================
// 登录/登出处理器
// ============================================================================

/// GET /login - 显示登录页面
pub async fn handle_login_view(headers: HeaderMap) -> Response {
    if is_authenticated(&headers) {
        return Redirect::to("/").into_response();
    }

    let tmpl = templates::LoginTemplate {
        error_msg: String::new(),
    };
    Html(tmpl.render().unwrap_or_default()).into_response()
}

/// 登录表单
#[derive(Deserialize)]
pub struct LoginForm {
    password: String,
}

/// POST /login - 处理登录
pub async fn handle_login(Form(form): Form<LoginForm>) -> Response {
    let settings = runtime_config::get();

    // 检查密码是否配置
    if settings.webui_password.is_empty() {
        let tmpl = templates::LoginTemplate {
            error_msg: "管理密码未配置，请设置 WEBUI_PASSWORD 环境变量".to_string(),
        };
        return Html(tmpl.render().unwrap_or_default()).into_response();
    }

    // 验证密码
    if form.password == settings.webui_password {
        let mut headers = HeaderMap::new();
        headers.insert(header::SET_COOKIE, set_auth_cookie().parse().unwrap());
        headers.insert("HX-Redirect", "/".parse().unwrap());

        (headers, "登录成功").into_response()
    } else {
        let tmpl = templates::LoginTemplate {
            error_msg: "密码错误".to_string(),
        };
        Html(tmpl.render().unwrap_or_default()).into_response()
    }
}

/// GET /logout - 登出
pub async fn handle_logout() -> Response {
    let mut headers = HeaderMap::new();
    headers.insert(header::SET_COOKIE, clear_auth_cookie().parse().unwrap());

    (headers, Redirect::to("/login")).into_response()
}

// ============================================================================
// Dashboard 处理器
// ============================================================================

/// GET / - Dashboard 主页面
pub async fn handle_dashboard(State(state): State<Arc<ManagerState>>) -> Response {
    let mut accounts = state.store.get_all().await;
    sort_accounts_by_created_at_desc(&mut accounts);
    let stats = templates::calculate_stats(&accounts);
    let view_accounts = to_view_accounts(&accounts);

    let tmpl = templates::DashboardTemplate {
        accounts: view_accounts,
        stats,
    };

    Html(tmpl.render().unwrap_or_default()).into_response()
}

/// GET /manager/api/stats - 统计卡片片段
pub async fn handle_stats(State(state): State<Arc<ManagerState>>) -> Response {
    let accounts = state.store.get_all().await;
    let stats = templates::calculate_stats(&accounts);

    let tmpl = templates::StatsCardsTemplate { stats };
    Html(tmpl.render().unwrap_or_default()).into_response()
}

/// 列表查询参数
#[derive(Deserialize, Default)]
pub struct ListQuery {
    #[serde(default)]
    status: String,
}

/// GET /manager/api/list - 账号列表片段
pub async fn handle_list(
    State(state): State<Arc<ManagerState>>,
    Query(query): Query<ListQuery>,
) -> Response {
    let accounts = state.store.get_all().await;
    let now = chrono::Utc::now().timestamp_millis();

    let filtered: Vec<Account> = accounts
        .into_iter()
        .filter(|acc| {
            let status = query.status.trim();
            if status.is_empty() || status == "all" {
                return true;
            }

            let is_expired = acc.is_expired(now);
            match status {
                "active" => acc.enable && !is_expired,
                "expired" => is_expired,
                "disabled" => !acc.enable,
                _ => true,
            }
        })
        .collect();

    let mut sorted = filtered;
    sort_accounts_by_created_at_desc(&mut sorted);
    let view_accounts = to_view_accounts(&sorted);

    let tmpl = templates::TokenListTemplate {
        accounts: view_accounts,
    };
    let html = tmpl.render().unwrap_or_default();

    // 列表刷新后立刻触发一次配额刷新（避免等待 30s 轮询，提升交互反馈）。
    let mut headers = HeaderMap::new();
    headers.insert(
        "HX-Trigger-After-Settle",
        hx_trigger_value(serde_json::json!({ "refreshQuota": true }))
            .parse()
            .unwrap(),
    );
    (headers, Html(html)).into_response()
}

/// 按创建时间降序排序账号
fn sort_accounts_by_created_at_desc(accounts: &mut [Account]) {
    accounts.sort_by(|a, b| b.created_at.cmp(&a.created_at));
}

// ============================================================================
// 账号操作处理器
// ============================================================================

/// 通用 ID 查询参数
#[derive(Deserialize)]
pub struct IdQuery {
    id: String,
}

/// POST /manager/api/delete - 删除账号
pub async fn handle_delete(
    State(state): State<Arc<ManagerState>>,
    Query(query): Query<IdQuery>,
) -> Response {
    let idx = find_index_by_session_id(&state.store, &query.id).await;

    if let Some(idx) = idx
        && state.store.delete(idx).await.is_ok()
    {
        state.quota_pool.remove_session(&query.id).await;
        return "".into_response();
    }

    (StatusCode::NOT_FOUND, "未找到").into_response()
}

/// POST /manager/api/toggle - 切换启用/禁用
pub async fn handle_toggle(
    State(state): State<Arc<ManagerState>>,
    Query(query): Query<IdQuery>,
) -> Response {
    let idx = find_index_by_session_id(&state.store, &query.id).await;

    if let Some(idx) = idx {
        let accounts = state.store.get_all().await;
        if idx < accounts.len() {
            let new_state = !accounts[idx].enable;
            let _ = state.store.set_enable(idx, new_state).await;
            if !new_state {
                // 账号禁用后立即从配额池移除，避免仍被网关选择。
                state.quota_pool.remove_session(&query.id).await;
            }

            // 重新获取更新后的账号
            let accounts = state.store.get_all().await;
            if idx < accounts.len() {
                let view_account = ViewAccount::from_account(&accounts[idx]);
                let tmpl = templates::TokenCardTemplate {
                    account: view_account,
                    quota_open: false,
                };
                let html = tmpl.render().unwrap_or_default();
                return Html(html).into_response();
            }
        }
    }

    "".into_response()
}

/// 刷新表单
#[derive(Deserialize, Default)]
pub struct RefreshForm {
    #[serde(default, rename = "quotaOpen")]
    quota_open: String,
}

/// POST /manager/api/refresh - 刷新单个账号
pub async fn handle_refresh(
    State(state): State<Arc<ManagerState>>,
    Query(query): Query<IdQuery>,
    Form(form): Form<RefreshForm>,
) -> Response {
    let quota_open = form.quota_open.trim() == "1";
    let idx = find_index_by_session_id(&state.store, &query.id).await;

    if let Some(idx) = idx {
        // 刷新账号
        let mut toast_type = "success";
        let mut toast_msg = "凭证刷新成功";
        match state.store.refresh_account(idx).await {
            Ok(()) => {
                tracing::info!(session_id = %query.id, "手动刷新凭证成功");
            }
            Err(e) => {
                tracing::warn!(session_id = %query.id, error = ?e, "手动刷新凭证失败");
                toast_type = "error";
                toast_msg = "凭证刷新失败";
            }
        }

        // 返回更新后的卡片
        let accounts = state.store.get_all().await;
        if idx < accounts.len() {
            let view_account = ViewAccount::from_account(&accounts[idx]);
            let tmpl = templates::TokenCardTemplate {
                account: view_account,
                quota_open,
            };
            let html = tmpl.render().unwrap_or_default();

            let mut headers = HeaderMap::new();
            let trigger = hx_trigger_value(serde_json::json!({
                "showMessage": { "message": toast_msg, "type": toast_type }
            }));
            headers.insert("HX-Trigger", trigger.parse().unwrap());

            // 卡片替换完成后，立即触发一次配额刷新，避免等待 30s 轮询。
            headers.insert(
                "HX-Trigger-After-Settle",
                hx_trigger_value(serde_json::json!({ "refreshQuota": true }))
                    .parse()
                    .unwrap(),
            );

            return (headers, Html(html)).into_response();
        }
    }

    "".into_response()
}

/// POST /manager/api/refresh_all - 刷新所有账号
pub async fn handle_refresh_all(State(state): State<Arc<ManagerState>>) -> Response {
    let _ = state.store.refresh_all().await;

    let mut headers = HeaderMap::new();
    let trigger = hx_trigger_value(serde_json::json!({
        "refreshStats": true,
        "refreshList": true,
        "showMessage": { "message": "所有账号信息已刷新", "type": "success" }
    }));
    headers.insert("HX-Trigger", trigger.parse().unwrap());

    (headers, "").into_response()
}

/// 查找账号索引
async fn find_index_by_session_id(store: &Store, session_id: &str) -> Option<usize> {
    let accounts = store.get_all().await;
    accounts.iter().position(|acc| acc.session_id == session_id)
}

// ============================================================================
// 配额处理器
// ============================================================================

/// 配额查询参数
#[derive(Deserialize, Default)]
pub struct QuotaQuery {
    id: String,
}

/// 判断是否为 HTMX 请求
fn is_htmx(headers: &HeaderMap) -> bool {
    headers
        .get("HX-Request")
        .and_then(|v| v.to_str().ok())
        .map(|v| v.eq_ignore_ascii_case("true"))
        .unwrap_or(false)
}

/// 配额 API 响应
#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaApiResponse {
    pub session_id: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<QuotaGroup>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub cached: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub fetched_at: Option<chrono::DateTime<chrono::Utc>>,
}

/// GET /manager/api/quota - 获取单个账号配额
pub async fn handle_quota(
    State(state): State<Arc<ManagerState>>,
    headers: HeaderMap,
    Query(query): Query<QuotaQuery>,
) -> Response {
    let session_id = query.id.trim();

    if session_id.is_empty() {
        return render_quota_error(&headers, "", "缺少 id 参数");
    }

    let quota = AccountQuota {
        session_id: session_id.to_string(),
        groups: state.quota_pool.get_session_quota_groups(session_id).await,
        fetched_at: chrono::Utc::now(),
    };

    if is_htmx(&headers) {
        let groups: Vec<ViewQuotaGroup> = quota
            .groups
            .iter()
            .map(ViewQuotaGroup::from_quota_group)
            .collect();

        let tmpl = templates::QuotaContentTemplate {
            session_id: quota.session_id.clone(),
            groups,
            error_msg: String::new(),
        };

        let mut resp_headers = HeaderMap::new();
        resp_headers.insert(
            header::CONTENT_TYPE,
            "text/html; charset=utf-8".parse().unwrap(),
        );

        return (resp_headers, Html(tmpl.render().unwrap_or_default())).into_response();
    }

    Json(QuotaApiResponse {
        session_id: quota.session_id,
        groups: quota.groups,
        error: None,
        cached: false,
        fetched_at: Some(quota.fetched_at),
    })
    .into_response()
}

/// 渲染配额错误
fn render_quota_error(headers: &HeaderMap, session_id: &str, msg: &str) -> Response {
    if is_htmx(headers) {
        let tmpl = templates::QuotaContentTemplate {
            session_id: session_id.to_string(),
            groups: Vec::new(),
            error_msg: msg.to_string(),
        };

        let mut resp_headers = HeaderMap::new();
        resp_headers.insert(
            header::CONTENT_TYPE,
            "text/html; charset=utf-8".parse().unwrap(),
        );

        (resp_headers, Html(tmpl.render().unwrap_or_default())).into_response()
    } else {
        Json(QuotaApiResponse {
            session_id: session_id.to_string(),
            groups: Vec::new(),
            error: Some(msg.to_string()),
            cached: false,
            fetched_at: None,
        })
        .into_response()
    }
}

/// POST /manager/api/quota/all - 获取所有账号配额
pub async fn handle_quota_all(
    State(state): State<Arc<ManagerState>>,
    headers: HeaderMap,
) -> Response {
    let accounts = state.store.get_all().await;

    if accounts.is_empty() {
        if is_htmx(&headers) {
            return Html("").into_response();
        }
        return Json(serde_json::json!({"accounts": []})).into_response();
    }

    let fetched_at = chrono::Utc::now();
    let mut results = Vec::with_capacity(accounts.len());
    for acc in &accounts {
        let sid = acc.session_id.clone();
        let groups = state.quota_pool.get_session_quota_groups(&sid).await;
        results.push(AccountQuota {
            session_id: sid,
            groups,
            fetched_at,
        });
    }

    if is_htmx(&headers) {
        let mut html = String::new();
        html.push_str(
            r#"<div id="quota-poller" class="hidden" hx-post="/manager/api/quota/all" hx-trigger="every 30s, refreshQuota from:body" hx-swap="outerHTML"></div>"#,
        );
        for quota in results {
            let AccountQuota {
                session_id,
                groups,
                fetched_at: _,
            } = quota;
            let groups: Vec<ViewQuotaGroup> = groups
                .iter()
                .map(ViewQuotaGroup::from_quota_group)
                .collect();

            let tmpl = templates::QuotaSwapOOBTemplate {
                session_id,
                groups,
                error_msg: String::new(),
            };
            html.push_str(&tmpl.render().unwrap_or_default());
        }

        let mut resp_headers = HeaderMap::new();
        resp_headers.insert(
            header::CONTENT_TYPE,
            "text/html; charset=utf-8".parse().unwrap(),
        );

        return (resp_headers, Html(html)).into_response();
    }

    let out: Vec<QuotaApiResponse> = results
        .into_iter()
        .map(|q| QuotaApiResponse {
            session_id: q.session_id,
            groups: q.groups,
            error: None,
            cached: false,
            fetched_at: Some(q.fetched_at),
        })
        .collect();

    Json(serde_json::json!({"accounts": out})).into_response()
}

// ============================================================================
// OAuth 处理器
// ============================================================================

/// OAuth URL 响应
#[derive(Serialize)]
struct OAuthUrlResponse {
    #[serde(skip_serializing_if = "Option::is_none")]
    url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// GET /manager/api/oauth/url - 生成 OAuth 授权 URL
pub async fn handle_oauth_url(State(manager_state): State<Arc<ManagerState>>) -> Response {
    let oauth_state = match oauth::generate_state().await {
        Ok(s) => s,
        Err(e) => {
            tracing::error!("生成 OAuth state 失败: {e:#}");
            return Json(OAuthUrlResponse {
                url: None,
                error: Some("生成 OAuth state 失败".to_string()),
            })
            .into_response();
        }
    };

    let settings = runtime_config::get();
    let redirect_uri = format!("http://localhost:{}/oauth-callback", settings.port);

    let cfg = manager_state.cfg.clone();
    let url = match oauth::build_auth_url(&cfg, &redirect_uri, &oauth_state) {
        Ok(u) => u,
        Err(e) => {
            tracing::error!("生成授权 URL 失败: {e:#}");
            return Json(OAuthUrlResponse {
                url: None,
                error: Some("生成授权 URL 失败".to_string()),
            })
            .into_response();
        }
    };

    Json(OAuthUrlResponse {
        url: Some(url),
        error: None,
    })
    .into_response()
}

/// OAuth 解析 URL 请求
#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct OAuthParseUrlRequest {
    url: String,
    #[serde(default)]
    custom_project_id: String,
    #[serde(default)]
    allow_random_project_id: bool,
}

/// OAuth 解析 URL 响应
#[derive(Serialize)]
struct OAuthParseUrlResponse {
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// POST /manager/api/oauth/parse-url - 解析回调 URL 并添加账号
pub async fn handle_oauth_parse_url(
    State(state): State<Arc<ManagerState>>,
    Json(req): Json<OAuthParseUrlRequest>,
) -> Response {
    let pasted_url = req.url.trim();
    if pasted_url.is_empty() {
        return Json(OAuthParseUrlResponse {
            success: false,
            error: Some("请粘贴回调 URL".to_string()),
        })
        .into_response();
    }

    // 解析 URL（使用宽松解析）
    let (code, url_state) = match parse_oauth_url_lenient(pasted_url) {
        Ok(r) => r,
        Err(e) => {
            return Json(OAuthParseUrlResponse {
                success: false,
                error: Some(e),
            })
            .into_response();
        }
    };

    // 验证 state
    if url_state.trim().is_empty() {
        return Json(OAuthParseUrlResponse {
            success: false,
            error: Some("回调 URL 中缺少 state 参数".to_string()),
        })
        .into_response();
    }

    if !oauth::validate_state(&url_state).await {
        return Json(OAuthParseUrlResponse {
            success: false,
            error: Some("state 校验失败或已过期，请重新发起 OAuth 授权".to_string()),
        })
        .into_response();
    }

    // 交换 token
    let settings = runtime_config::get();
    let redirect_uri = format!("http://localhost:{}/oauth-callback", settings.port);
    let mut cfg = state.cfg.clone();
    // WebUI 设置中允许动态修改 User-Agent，这里以运行时配置为准。
    cfg.api_user_agent = settings.api_user_agent.clone();

    tracing::info!("开始 OAuth 交换 Token...");
    let token_resp = match oauth::exchange_code_for_token(&cfg, &code, &redirect_uri).await {
        Ok(t) => t,
        Err(e) => {
            return Json(OAuthParseUrlResponse {
                success: false,
                error: Some(e.to_string()),
            })
            .into_response();
        }
    };

    // 获取用户邮箱（最佳努力）
    let email = match oauth::get_user_info(&cfg, &token_resp.access_token).await {
        Ok(ui) => ui.email,
        Err(e) => {
            tracing::warn!("获取用户邮箱失败: {e:#}");
            String::new()
        }
    };

    // 确定项目 ID
    let mut project_id = req.custom_project_id.trim().to_string();
    if !project_id.is_empty() {
        tracing::info!("使用用户自定义项目ID: {project_id}");
    } else if !token_resp.access_token.is_empty() {
        match oauth::fetch_project_id(&cfg, &token_resp.access_token).await {
            Ok(pid) => {
                project_id = pid.trim().to_string();
                if !project_id.is_empty() {
                    tracing::info!("自动获取到项目ID: {project_id}");
                }
            }
            Err(e) => {
                tracing::warn!("自动获取项目ID失败: {e:#}");
            }
        }
    }

    if project_id.is_empty() && !req.allow_random_project_id {
        return Json(OAuthParseUrlResponse {
            success: false,
            error: Some("无法自动获取 Google 项目 ID，可能会导致部分接口 403。请填写自定义项目ID，或勾选「允许使用随机项目ID」。".to_string()),
        })
        .into_response();
    }

    if project_id.is_empty() && req.allow_random_project_id {
        project_id = id::project_id();
        tracing::info!("使用随机生成的项目ID: {project_id}");
    }

    // 创建账号
    let now = chrono::Utc::now();
    let account = Account {
        session_id: String::new(), // 由 store.add 生成
        access_token: token_resp.access_token,
        refresh_token: token_resp.refresh_token,
        expires_in: token_resp.expires_in,
        timestamp: now.timestamp_millis(),
        project_id,
        email: email.clone(),
        enable: true,
        created_at: now,
    };

    if let Err(e) = state.store.add(account).await {
        tracing::error!("保存账号失败: {e:#}");
        return Json(OAuthParseUrlResponse {
            success: false,
            error: Some("保存账号失败".to_string()),
        })
        .into_response();
    }

    tracing::info!("OAuth 登录成功: {email}");
    Json(OAuthParseUrlResponse {
        success: true,
        error: None,
    })
    .into_response()
}

/// 宽松解析 OAuth 回调 URL。
/// 支持：完整 URL、无协议 URL、仅路径等格式。
fn parse_oauth_url_lenient(url: &str) -> Result<(String, String), String> {
    let url = url.trim();

    // 尝试找到查询字符串
    let query_str = if let Some(idx) = url.find('?') {
        &url[idx + 1..]
    } else {
        return Err("回调 URL 中缺少 code 参数".to_string());
    };

    // 解析查询参数
    let mut code = String::new();
    let mut state = String::new();

    for pair in query_str.split('&') {
        let mut parts = pair.splitn(2, '=');
        let key = parts.next().unwrap_or("");
        let value = parts.next().unwrap_or("");

        // URL 解码
        let decoded_value = urlencoding::decode(value)
            .map(|s| s.into_owned())
            .unwrap_or_else(|_| value.to_string());

        match key {
            "code" => code = decoded_value,
            "state" => state = decoded_value,
            _ => {}
        }
    }

    if code.is_empty() {
        return Err("回调 URL 中缺少 code 参数".to_string());
    }

    Ok((code, state))
}

// ============================================================================
// 设置处理器
// ============================================================================

/// GET /manager/api/settings - 获取设置
pub async fn handle_settings_get(headers: HeaderMap) -> Response {
    let settings = runtime_config::get();
    let webui_settings = WebUISettings::from_runtime(&settings);

    if is_htmx(&headers) {
        let tmpl = templates::SettingsTemplate {
            settings: webui_settings,
        };

        let mut resp_headers = HeaderMap::new();
        resp_headers.insert(
            header::CONTENT_TYPE,
            "text/html; charset=utf-8".parse().unwrap(),
        );

        return (resp_headers, Html(tmpl.render().unwrap_or_default())).into_response();
    }

    Json(webui_settings).into_response()
}

/// 设置保存响应
#[derive(Serialize)]
struct SettingsResponse {
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// POST /manager/api/settings - 保存设置
pub async fn handle_settings_post(Json(req): Json<WebUISettings>) -> Response {
    // 验证
    if let Err(msg) = req.validate() {
        return Json(SettingsResponse {
            success: false,
            error: Some(msg.to_string()),
        })
        .into_response();
    }

    // 持久化到 .env
    if let Err(e) = runtime_config::persist_to_dotenv(&req) {
        tracing::error!("保存设置失败: {e}");
        return Json(SettingsResponse {
            success: false,
            error: Some(format!("保存设置失败: {e}")),
        })
        .into_response();
    }

    // 更新运行时配置
    let current = runtime_config::get();
    let new_settings = req.apply_to_runtime(&current);
    runtime_config::update(new_settings.clone());

    tracing::info!(
        "设置已更新: Debug={}, UserAgent={}, EndpointMode={}",
        new_settings.debug,
        new_settings.api_user_agent,
        new_settings.endpoint_mode
    );

    Json(SettingsResponse {
        success: true,
        error: None,
    })
    .into_response()
}

/// 缓存清理响应
#[derive(Serialize)]
struct CacheCleanupResponse {
    success: bool,
    deleted: usize,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// POST /manager/api/cache/cleanup - 清理过期签名缓存文件
pub async fn handle_cache_cleanup(State(state): State<Arc<ManagerState>>) -> Response {
    let settings = runtime_config::get();
    let days = settings.cache_retention_days.max(1);

    match signature::store::cleanup_signature_cache_files(&state.data_dir, days).await {
        Ok(deleted) => Json(CacheCleanupResponse {
            success: true,
            deleted,
            error: None,
        })
        .into_response(),
        Err(e) => {
            tracing::error!("清理签名缓存失败: {e:#}");
            Json(CacheCleanupResponse {
                success: false,
                deleted: 0,
                error: Some(format!("清理失败: {e}")),
            })
            .into_response()
        }
    }
}

// ============================================================================
// 模型设置处理器
// ============================================================================

/// GET /manager/api/model-settings - 获取模型设置页面
pub async fn handle_model_settings_get(
    State(state): State<Arc<ManagerState>>,
    headers: HeaderMap,
) -> Response {
    let accounts = state.store.get_all().await;
    let now = chrono::Utc::now().timestamp_millis();

    // 只返回启用且未过期的账号
    let active_accounts: Vec<ViewAccount> = accounts
        .iter()
        .filter(|acc| acc.enable && !acc.is_expired(now))
        .map(ViewAccount::from_account)
        .collect();

    if is_htmx(&headers) {
        let tmpl = templates::ModelSettingsTemplate {
            accounts: active_accounts,
        };

        let mut resp_headers = HeaderMap::new();
        resp_headers.insert(
            header::CONTENT_TYPE,
            "text/html; charset=utf-8".parse().unwrap(),
        );

        return (resp_headers, Html(tmpl.render().unwrap_or_default())).into_response();
    }

    // JSON 响应：只返回安全字段
    let safe_accounts: Vec<serde_json::Value> = active_accounts
        .iter()
        .map(|a| {
            serde_json::json!({
                "sessionId": a.session_id,
                "displayName": a.display_name
            })
        })
        .collect();

    Json(serde_json::json!({ "accounts": safe_accounts })).into_response()
}

/// 模型 ID 映射保存响应
#[derive(Serialize)]
struct ModelIdMappingSaveResponse {
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    success: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
}

/// GET /manager/api/model-id-mapping - 获取模型 ID 映射（JSON 对象：{alias: original}）
pub async fn handle_model_id_mapping_get() -> Response {
    let mapping = runtime_config::get_model_id_mapping();
    Json(mapping.as_ref()).into_response()
}

/// POST /manager/api/model-id-mapping - 保存模型 ID 映射（JSON 对象：{alias: original}）
pub async fn handle_model_id_mapping_post(
    State(state): State<Arc<ManagerState>>,
    Json(req): Json<HashMap<String, String>>,
) -> Response {
    let normalized = match runtime_config::validate_and_normalize_model_id_mapping(req) {
        Ok(v) => v,
        Err(e) => {
            return Json(ModelIdMappingSaveResponse {
                success: false,
                error: Some(e),
            })
            .into_response();
        }
    };

    if let Err(e) =
        runtime_config::persist_model_id_mapping_to_data_dir(&state.data_dir, &normalized)
    {
        tracing::error!("保存模型 ID 映射失败: {e}");
        return Json(ModelIdMappingSaveResponse {
            success: false,
            error: Some(e),
        })
        .into_response();
    }

    runtime_config::update_model_id_mapping(normalized);

    Json(ModelIdMappingSaveResponse {
        success: true,
        error: None,
    })
    .into_response()
}

// ============================================================================
// 聊天测试处理器
// ============================================================================

/// 聊天测试请求
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ChatTestRequest {
    /// 账号 session_id
    session_id: String,
    /// 模型名称
    model: String,
    /// 接口类型：openai 或 claude
    provider: String,
    /// 测试提示词
    prompt: String,
}

/// POST /manager/api/chat/test - 聊天测试（SSE 流式输出）
pub async fn handle_chat_test(
    State(state): State<Arc<ManagerState>>,
    method: Method,
    uri: OriginalUri,
    headers: HeaderMap,
    Json(req): Json<ChatTestRequest>,
) -> Response {
    let started_at = Instant::now();
    let settings = runtime_config::get();
    let log_level = settings.log_level();

    if log_level.client_enabled() {
        let body = serde_json::to_vec(&req).unwrap_or_default();
        if log_level.raw_enabled() {
            logging::client_request_raw(method.as_str(), uri.0.path(), &headers, &body);
        } else {
            logging::client_request(method.as_str(), uri.0.path(), &headers, &body);
        }
    }

    let session_id = req.session_id.trim();
    let model_mapped = runtime_config::map_client_model_id(&req.model);
    let model = model_mapped.trim();
    let provider = req.provider.trim().to_lowercase();
    let prompt = req.prompt.trim();

    // 参数校验
    if session_id.is_empty() {
        return chat_test_error(log_level, started_at, "请选择账号");
    }
    if model.is_empty() {
        return chat_test_error(log_level, started_at, "请选择模型");
    }
    if prompt.is_empty() {
        return chat_test_error(log_level, started_at, "请输入测试提示词");
    }

    // 查找指定账号
    let accounts = state.store.get_all().await;
    let account = accounts.iter().find(|a| a.session_id == session_id);

    let Some(account) = account else {
        return chat_test_error(log_level, started_at, "未找到指定账号");
    };

    let now = chrono::Utc::now().timestamp_millis();
    if !account.enable {
        return chat_test_error(log_level, started_at, "该账号已禁用");
    }
    if account.is_expired(now) {
        return chat_test_error(log_level, started_at, "该账号已过期");
    }

    let account = account.clone();

    // 创建 SSE 通道
    let (tx, rx) = mpsc::channel::<Result<Event, Infallible>>(256);

    let store = state.store.clone();
    let cfg = state.cfg.clone();
    let model_owned = model_mapped;
    let prompt_owned = prompt.to_string();
    let provider_owned = provider.to_string();
    let started_at_inner = started_at;

    // 在后台任务中执行请求
    tokio::spawn(async move {
        let settings = runtime_config::get();
        let log_level = settings.log_level();
        let client_log = log_level.client_enabled();
        let backend_log = log_level.backend_enabled();
        let raw_log = log_level.raw_enabled();
        let endpoint = runtime_config::current_endpoint();

        let mut merged_client_events: Vec<sonic_rs::Value> = Vec::new();

        async fn emit_client_event(
            tx: &mpsc::Sender<Result<Event, Infallible>>,
            merged: &mut Vec<sonic_rs::Value>,
            client_log: bool,
            raw_log: bool,
            data: String,
        ) {
            if client_log {
                if raw_log {
                    logging::client_stream_event_raw(None, &data);
                } else if let Ok(v) = sonic_rs::from_str::<sonic_rs::Value>(&data) {
                    merged.push(v);
                }
            }
            // 始终把事件发回浏览器（SSE）。
            let _ = tx.send(Ok(Event::default().data(data))).await;
        }

        async fn emit_error_event(
            tx: &mpsc::Sender<Result<Event, Infallible>>,
            merged: &mut Vec<sonic_rs::Value>,
            client_log: bool,
            raw_log: bool,
            msg: String,
        ) {
            let data = serde_json::json!({
                "error": {
                    "message": msg,
                    "type": "server_error"
                }
            })
            .to_string();
            emit_client_event(tx, merged, client_log, raw_log, data).await;
        }

        // 选择 projectId（若 403/CONSUMER_INVALID 则尝试自动修复并落盘）。
        let mut project_id = account.project_id.trim().to_string();
        if project_id.is_empty() {
            let mut oauth_cfg = cfg.clone();
            oauth_cfg.api_user_agent = settings.api_user_agent.clone();
            match oauth::fetch_project_id(&oauth_cfg, &account.access_token).await {
                Ok(pid) if !pid.trim().is_empty() => {
                    project_id = pid.trim().to_string();
                    let _ = store
                        .update_project_id_by_session_id(&account.session_id, &project_id)
                        .await;
                }
                Ok(_) | Err(_) => {}
            }
        }

        if project_id.is_empty() {
            emit_error_event(
                &tx,
                &mut merged_client_events,
                client_log,
                raw_log,
                "缺少 projectId：请在账号管理中填写自定义项目ID（或重新 OAuth 添加账号）"
                    .to_string(),
            )
            .await;
            let _ = tx.send(Ok(Event::default().data("[DONE]"))).await;
            if client_log && !raw_log {
                logging::client_stream_response(
                    StatusCode::OK.as_u16(),
                    started_at_inner.elapsed(),
                    &merged_client_events,
                );
            }
            return;
        }

        let backend_model = modelutil::backend_model_id(&model_owned);
        let is_gemini = modelutil::is_gemini(&model_owned);
        let is_claude = modelutil::is_claude(&model_owned);

        let mut generation_config = crate::vertex::types::GenerationConfig {
            candidate_count: 1,
            stop_sequences: Vec::new(),
            max_output_tokens: if is_gemini {
                modelutil::GEMINI_MAX_OUTPUT_TOKENS
            } else if is_claude {
                modelutil::CLAUDE_MAX_OUTPUT_TOKENS
            } else {
                1024
            },
            temperature: None,
            top_p: None,
            top_k: 0,
            thinking_config: modelutil::forced_thinking_config(&model_owned),
            image_config: None,
            media_resolution: String::new(),
        };

        if modelutil::is_gemini3(&model_owned)
            && !modelutil::is_image_model(&model_owned)
            && let Some(v) = modelutil::to_api_media_resolution(&settings.gemini3_media_resolution)
            && !v.is_empty()
        {
            generation_config.media_resolution = v;
        }

        let vertex_request = crate::vertex::types::Request {
            project: project_id.clone(),
            model: backend_model,
            request_id: id::request_id(),
            request_type: "agent".to_string(),
            user_agent: "antigravity".to_string(),
            request: crate::vertex::types::InnerReq {
                contents: vec![crate::vertex::types::Content {
                    role: "user".to_string(),
                    parts: vec![crate::vertex::types::Part {
                        text: prompt_owned,
                        ..crate::vertex::types::Part::default()
                    }],
                }],
                system_instruction: None,
                generation_config: Some(generation_config),
                tools: Vec::new(),
                tool_config: None,
                session_id: account.session_id.clone(),
            },
        };

        let body = match sonic_rs::to_vec(&vertex_request) {
            Ok(b) => b,
            Err(e) => {
                emit_error_event(
                    &tx,
                    &mut merged_client_events,
                    client_log,
                    raw_log,
                    format!("序列化请求失败: {e}"),
                )
                .await;
                let _ = tx.send(Ok(Event::default().data("[DONE]"))).await;
                if client_log && !raw_log {
                    logging::client_stream_response(
                        StatusCode::OK.as_u16(),
                        started_at_inner.elapsed(),
                        &merged_client_events,
                    );
                }
                return;
            }
        };

        // 创建 HTTP 客户端
        let mut client_builder = reqwest::Client::builder()
            .pool_max_idle_per_host(10)
            .pool_idle_timeout(std::time::Duration::from_secs(90))
            // 与主转发逻辑一致：流式接口强制 HTTP/2（SSE）。
            .http2_prior_knowledge();

        if cfg.timeout_ms > 0 {
            client_builder =
                client_builder.timeout(std::time::Duration::from_millis(cfg.timeout_ms));
        }
        if !cfg.proxy.trim().is_empty() {
            match reqwest::Proxy::all(cfg.proxy.trim()) {
                Ok(p) => client_builder = client_builder.proxy(p),
                Err(e) => {
                    emit_error_event(
                        &tx,
                        &mut merged_client_events,
                        client_log,
                        raw_log,
                        format!("Proxy 配置无效: {e}"),
                    )
                    .await;
                    let _ = tx.send(Ok(Event::default().data("[DONE]"))).await;
                    if client_log && !raw_log {
                        logging::client_stream_response(
                            StatusCode::OK.as_u16(),
                            started_at_inner.elapsed(),
                            &merged_client_events,
                        );
                    }
                    return;
                }
            }
        }

        let client = match client_builder.build() {
            Ok(c) => c,
            Err(e) => {
                emit_error_event(
                    &tx,
                    &mut merged_client_events,
                    client_log,
                    raw_log,
                    format!("创建 HTTP 客户端失败: {e}"),
                )
                .await;
                return;
            }
        };

        // 使用内部 API 端点（与正常请求一致）
        let url = format!(
            "https://{}/v1internal:streamGenerateContent?alt=sse",
            endpoint.host
        );

        let mut backend_headers = HeaderMap::new();
        backend_headers.insert(
            header::USER_AGENT,
            header::HeaderValue::from_str(settings.api_user_agent.as_str())
                .unwrap_or_else(|_| header::HeaderValue::from_static("ant2api")),
        );
        backend_headers.insert(
            header::AUTHORIZATION,
            header::HeaderValue::from_str(&format!("Bearer {}", account.access_token))
                .unwrap_or_else(|_| header::HeaderValue::from_static("")),
        );
        backend_headers.insert(
            header::CONTENT_TYPE,
            header::HeaderValue::from_static("application/json"),
        );

        if backend_log {
            if raw_log {
                logging::backend_request_raw("POST", &url, &backend_headers, &body);
            } else {
                logging::backend_request("POST", &url, &backend_headers, &body);
            }
        }

        let do_request = |body: Vec<u8>| async {
            client
                .post(&url)
                .headers(backend_headers.clone())
                .body(body)
                .send()
                .await
        };

        let mut resp = match do_request(body.clone()).await {
            Ok(r) => r,
            Err(e) => {
                emit_error_event(
                    &tx,
                    &mut merged_client_events,
                    client_log,
                    raw_log,
                    format!("请求失败: {e}"),
                )
                .await;
                return;
            }
        };

        // 非 2xx：读取一次 body 用于日志/诊断；403/CONSUMER_INVALID 时尝试自动修复并重试一次。
        if !resp.status().is_success() {
            let status = resp.status();
            let bytes = resp.bytes().await.unwrap_or_default();
            if backend_log {
                if raw_log {
                    logging::backend_response_raw(
                        status.as_u16(),
                        started_at_inner.elapsed(),
                        &bytes,
                    );
                } else {
                    logging::backend_response(status.as_u16(), started_at_inner.elapsed(), &bytes);
                }
            }

            let body_text = String::from_utf8_lossy(&bytes);

            if status == StatusCode::FORBIDDEN && body_text.contains("CONSUMER_INVALID") {
                let mut oauth_cfg = cfg.clone();
                oauth_cfg.api_user_agent = settings.api_user_agent.clone();
                if let Ok(pid) = oauth::fetch_project_id(&oauth_cfg, &account.access_token).await
                    && !pid.trim().is_empty()
                    && pid.trim() != project_id.as_str()
                {
                    let new_project_id = pid.trim().to_string();
                    let _ = store
                        .update_project_id_by_session_id(&account.session_id, &new_project_id)
                        .await;

                    let mut retry_req = vertex_request.clone();
                    retry_req.project = new_project_id.clone();
                    let retry_body = sonic_rs::to_vec(&retry_req).unwrap_or_else(|_| body.clone());
                    if backend_log {
                        if raw_log {
                            logging::backend_request_raw(
                                "POST",
                                &url,
                                &backend_headers,
                                &retry_body,
                            );
                        } else {
                            logging::backend_request("POST", &url, &backend_headers, &retry_body);
                        }
                    }

                    resp = match do_request(retry_body).await {
                        Ok(r) => r,
                        Err(e) => {
                            emit_error_event(
                                &tx,
                                &mut merged_client_events,
                                client_log,
                                raw_log,
                                format!("请求失败: {e}"),
                            )
                            .await;
                            return;
                        }
                    };

                    if resp.status().is_success() {
                        // OK：继续进入流式解析
                    } else {
                        let status = resp.status();
                        let bytes = resp.bytes().await.unwrap_or_default();
                        if backend_log {
                            if raw_log {
                                logging::backend_response_raw(
                                    status.as_u16(),
                                    started_at_inner.elapsed(),
                                    &bytes,
                                );
                            } else {
                                logging::backend_response(
                                    status.as_u16(),
                                    started_at_inner.elapsed(),
                                    &bytes,
                                );
                            }
                        }

                        emit_error_event(
                            &tx,
                            &mut merged_client_events,
                            client_log,
                            raw_log,
                            format!("HTTP {}: {}", status, String::from_utf8_lossy(&bytes)),
                        )
                        .await;
                        let _ = tx.send(Ok(Event::default().data("[DONE]"))).await;
                        if client_log && !raw_log {
                            logging::client_stream_response(
                                StatusCode::OK.as_u16(),
                                started_at_inner.elapsed(),
                                &merged_client_events,
                            );
                        }
                        return;
                    }
                } else {
                    emit_error_event(
                        &tx,
                        &mut merged_client_events,
                        client_log,
                        raw_log,
                        format!("HTTP {}: {}", status, body_text),
                    )
                    .await;
                    let _ = tx.send(Ok(Event::default().data("[DONE]"))).await;
                    if client_log && !raw_log {
                        logging::client_stream_response(
                            StatusCode::OK.as_u16(),
                            started_at_inner.elapsed(),
                            &merged_client_events,
                        );
                    }
                    return;
                }
            } else {
                emit_error_event(
                    &tx,
                    &mut merged_client_events,
                    client_log,
                    raw_log,
                    format!("HTTP {}: {}", status, body_text),
                )
                .await;
                let _ = tx.send(Ok(Event::default().data("[DONE]"))).await;
                if client_log && !raw_log {
                    logging::client_stream_response(
                        StatusCode::OK.as_u16(),
                        started_at_inner.elapsed(),
                        &merged_client_events,
                    );
                }
                return;
            }
        }

        // 处理 SSE 流
        let mut stream = resp.bytes_stream();

        let mut buffer = String::new();

        while let Some(chunk) = stream.next().await {
            let chunk = match chunk {
                Ok(c) => c,
                Err(e) => {
                    emit_error_event(
                        &tx,
                        &mut merged_client_events,
                        client_log,
                        raw_log,
                        format!("读取响应失败: {e}"),
                    )
                    .await;
                    return;
                }
            };

            buffer.push_str(&String::from_utf8_lossy(&chunk));

            // 按行处理
            while let Some(pos) = buffer.find('\n') {
                let line = buffer[..pos].to_string();
                buffer = buffer[pos + 1..].to_string();

                if line.starts_with("data: ") {
                    let data = &line[6..];
                    if data == "[DONE]" {
                        continue;
                    }

                    // 解析 Vertex 内部 API 响应并转换为客户端格式
                    if let Ok(parsed) = serde_json::from_str::<serde_json::Value>(data) {
                        // 后端可能在流中返回 error 结构：直接透传给前端。
                        if parsed.get("error").is_some() {
                            emit_client_event(
                                &tx,
                                &mut merged_client_events,
                                client_log,
                                raw_log,
                                parsed.to_string(),
                            )
                            .await;
                            continue;
                        }

                        let client_event = if provider_owned == "claude" {
                            convert_vertex_to_claude_event(&parsed, &model_owned)
                        } else {
                            convert_vertex_to_openai_event(&parsed, &model_owned)
                        };

                        if let Some(event_data) = client_event {
                            emit_client_event(
                                &tx,
                                &mut merged_client_events,
                                client_log,
                                raw_log,
                                event_data,
                            )
                            .await;
                        }
                    }
                }
            }
        }

        // 发送结束标记
        let _ = tx.send(Ok(Event::default().data("[DONE]"))).await;
        if client_log && !raw_log {
            logging::client_stream_response(
                StatusCode::OK.as_u16(),
                started_at_inner.elapsed(),
                &merged_client_events,
            );
        }
    });

    // 返回 SSE 响应
    Sse::new(ReceiverStream::new(rx)).into_response()
}

/// 从 Vertex 内部 API 响应转换为 OpenAI 格式的 SSE 事件
fn convert_vertex_to_openai_event(vertex_data: &serde_json::Value, model: &str) -> Option<String> {
    // Vertex 内部 API 响应格式：
    // { "response": { "candidates": [{ "content": { "parts": [{ "text": "..." }] } }] } }
    let text = vertex_data
        .get("response")
        .and_then(|r| r.get("candidates"))
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("content"))
        .and_then(|c| c.get("parts"))
        .and_then(|p| p.get(0))
        .and_then(|p| p.get("text"))
        .and_then(|t| t.as_str());

    if let Some(text) = text {
        if !text.is_empty() {
            let event = serde_json::json!({
                "id": format!("chatcmpl-{}", id::chat_completion_id()),
                "object": "chat.completion.chunk",
                "model": model,
                "choices": [{
                    "index": 0,
                    "delta": {
                        "content": text
                    },
                    "finish_reason": null
                }]
            });
            return Some(event.to_string());
        }
    }

    None
}

/// 从 Vertex 内部 API 响应转换为 Claude 格式的 SSE 事件
fn convert_vertex_to_claude_event(vertex_data: &serde_json::Value, _model: &str) -> Option<String> {
    // Vertex 内部 API 响应格式：
    // { "response": { "candidates": [{ "content": { "parts": [{ "text": "..." }] } }] } }
    let text = vertex_data
        .get("response")
        .and_then(|r| r.get("candidates"))
        .and_then(|c| c.get(0))
        .and_then(|c| c.get("content"))
        .and_then(|c| c.get("parts"))
        .and_then(|p| p.get(0))
        .and_then(|p| p.get("text"))
        .and_then(|t| t.as_str());

    if let Some(text) = text {
        if !text.is_empty() {
            // 构造 Claude 格式的 content_block_delta 事件
            let event = serde_json::json!({
                "type": "content_block_delta",
                "index": 0,
                "delta": {
                    "type": "text_delta",
                    "text": text
                }
            });
            return Some(event.to_string());
        }
    }

    None
}

/// 返回聊天测试错误响应
fn chat_test_error(log_level: logging::LogLevel, started_at: Instant, msg: &str) -> Response {
    let body = serde_json::json!({ "error": msg }).to_string();
    if log_level.client_enabled() {
        if log_level.raw_enabled() {
            logging::client_response_raw(
                StatusCode::BAD_REQUEST.as_u16(),
                started_at.elapsed(),
                body.as_bytes(),
            );
        } else {
            match sonic_rs::from_str::<sonic_rs::Value>(&body) {
                Ok(v) => logging::client_response(
                    StatusCode::BAD_REQUEST.as_u16(),
                    started_at.elapsed(),
                    Some(&v),
                ),
                Err(_) => logging::client_response_raw(
                    StatusCode::BAD_REQUEST.as_u16(),
                    started_at.elapsed(),
                    body.as_bytes(),
                ),
            }
        }
    }
    (
        StatusCode::BAD_REQUEST,
        [(header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response()
}

// ============================================================================
// 认证中间件
// ============================================================================

use axum::extract::Request;
use axum::middleware::Next;

/// Manager 认证中间件
pub async fn manager_auth_middleware(headers: HeaderMap, request: Request, next: Next) -> Response {
    // 检查是否已认证
    if is_authenticated(&headers) {
        return next.run(request).await;
    }

    let path = request.uri().path();

    // API 路径返回 401
    if path.starts_with("/manager/api") {
        return (
            StatusCode::UNAUTHORIZED,
            "未登录或会话已过期，请先登录管理面板",
        )
            .into_response();
    }

    // 其他路径重定向到登录页
    Redirect::to("/login").into_response()
}
