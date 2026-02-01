//! Manager WebUI 处理器。
//!
//! 实现与 Go 版本完全一致的 WebUI 功能。

use axum::{
    Form, Json,
    extract::{Query, State},
    http::{HeaderMap, StatusCode, header},
    response::{Html, IntoResponse, Redirect, Response},
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;

use crate::credential::oauth;
use crate::credential::store::Store;
use crate::credential::types::Account;
use crate::gateway::manager::quota::{AccountQuota, QuotaCache, QuotaGroup};
use crate::gateway::manager::templates::{self, ViewAccount, ViewQuotaGroup, to_view_accounts};
use crate::quota_pool::QuotaPoolManager;
use crate::runtime_config::{self, WebUISettings};
use crate::signature;
use crate::util::id;
use crate::vertex::client::VertexClient;

use askama::Template;

/// Manager 应用状态
pub struct ManagerState {
    pub store: Arc<Store>,
    pub vertex: Arc<VertexClient>,
    pub quota_cache: QuotaCache,
    pub quota_pool: Arc<QuotaPoolManager>,
    pub data_dir: String,
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

    let mut headers = HeaderMap::new();
    headers.insert("HX-Trigger", "refreshQuota".parse().unwrap());

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
        state.quota_cache.invalidate(&query.id).await;
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

                let mut headers = HeaderMap::new();
                headers.insert("HX-Trigger", "refreshQuota".parse().unwrap());

                return (headers, Html(html)).into_response();
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
        if let Err(e) = state.store.refresh_account(idx).await {
            tracing::error!("刷新账号失败: {e:#}");
            toast_type = "error";
            toast_msg = "凭证刷新失败";
        }

        // 使配额缓存失效
        state.quota_cache.invalidate(&query.id).await;

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
            let trigger = serde_json::json!({
                "refreshQuota": true,
                "showMessage": { "message": toast_msg, "type": toast_type }
            })
            .to_string();
            headers.insert("HX-Trigger", trigger.parse().unwrap());

            return (headers, Html(html)).into_response();
        }
    }

    "".into_response()
}

/// POST /manager/api/refresh_all - 刷新所有账号
pub async fn handle_refresh_all(State(state): State<Arc<ManagerState>>) -> Response {
    let _ = state.store.refresh_all().await;

    let mut headers = HeaderMap::new();
    let trigger = serde_json::json!({
        "refreshStats": true,
        "refreshList": true,
        "refreshQuota": true,
        "showMessage": { "message": "所有账号信息已刷新", "type": "success" }
    })
    .to_string();
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
    #[serde(default)]
    force: String,
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
    let force = query.force.trim() == "1";

    if session_id.is_empty() {
        return render_quota_error(&headers, "", "缺少 id 参数");
    }

    let idx = find_index_by_session_id(&state.store, session_id).await;
    let Some(idx) = idx else {
        return render_quota_error(&headers, session_id, "未找到对应账号");
    };

    let accounts = state.store.get_all().await;
    if idx >= accounts.len() {
        return render_quota_error(&headers, session_id, "未找到对应账号");
    }

    let endpoint = runtime_config::current_endpoint();
    let (quota, cached, error) = state
        .quota_cache
        .get_quota(&accounts[idx], &endpoint, &state.vertex, force)
        .await;

    if let Some(err_msg) = error {
        return render_quota_error(&headers, session_id, &err_msg);
    }

    let quota = quota.unwrap_or(AccountQuota {
        session_id: session_id.to_string(),
        groups: Vec::new(),
        fetched_at: chrono::Utc::now(),
    });

    if is_htmx(&headers) {
        let groups: Vec<ViewQuotaGroup> = quota
            .groups
            .iter()
            .map(ViewQuotaGroup::from_quota_group)
            .collect();

        let tmpl = templates::QuotaContentTemplate {
            session_id: session_id.to_string(),
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
        session_id: session_id.to_string(),
        groups: quota.groups,
        error: None,
        cached,
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

/// 强制查询参数
#[derive(Deserialize, Default)]
pub struct ForceQuery {
    #[serde(default)]
    force: String,
}

/// POST /manager/api/quota/all - 获取所有账号配额
pub async fn handle_quota_all(
    State(state): State<Arc<ManagerState>>,
    headers: HeaderMap,
    Query(query): Query<ForceQuery>,
) -> Response {
    let force = query.force.trim() == "1";
    let accounts = state.store.get_all().await;

    if accounts.is_empty() {
        if is_htmx(&headers) {
            return Html("").into_response();
        }
        return Json(serde_json::json!({"accounts": []})).into_response();
    }

    let endpoint = runtime_config::current_endpoint();
    // 并发获取所有配额
    let mut handles = Vec::with_capacity(accounts.len());

    for acc in accounts.iter() {
        let acc = acc.clone();
        let endpoint = endpoint.clone();
        let vertex = state.vertex.clone();
        let quota_cache = &state.quota_cache;

        handles.push(async move {
            let (quota, cached, error) =
                quota_cache.get_quota(&acc, &endpoint, &vertex, force).await;
            (acc.session_id.clone(), quota, cached, error)
        });
    }

    let results: Vec<_> = futures::future::join_all(handles).await;

    if is_htmx(&headers) {
        let mut html = String::new();
        for (session_id, quota, _cached, error) in results {
            let groups: Vec<ViewQuotaGroup> = quota
                .map(|q| {
                    q.groups
                        .iter()
                        .map(ViewQuotaGroup::from_quota_group)
                        .collect()
                })
                .unwrap_or_default();

            let tmpl = templates::QuotaSwapOOBTemplate {
                session_id: session_id.clone(),
                groups,
                error_msg: error.unwrap_or_default(),
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
        .map(|(session_id, quota, cached, error)| QuotaApiResponse {
            session_id,
            groups: quota.map(|q| q.groups).unwrap_or_default(),
            error,
            cached,
            fetched_at: None,
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
pub async fn handle_oauth_url() -> Response {
    let state = match oauth::generate_state().await {
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

    let cfg = crate::config::Config::load();
    let url = match oauth::build_auth_url(&cfg, &redirect_uri, &state) {
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
    let cfg = crate::config::Config::load();

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
