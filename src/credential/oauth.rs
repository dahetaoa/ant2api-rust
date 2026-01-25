use crate::config::Config;
use crate::credential::types::Account;
use anyhow::{anyhow, bail};
use base64::Engine;
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HOST, USER_AGENT};
use std::collections::HashMap;
use std::sync::OnceLock;
use std::time::{Duration, Instant};
use tokio::sync::Mutex;
use tracing::{info, warn};
use uuid::Uuid;

pub const OAUTH_SCOPES: [&str; 5] = [
    "https://www.googleapis.com/auth/cloud-platform",
    "https://www.googleapis.com/auth/userinfo.email",
    "https://www.googleapis.com/auth/userinfo.profile",
    "https://www.googleapis.com/auth/cclog",
    "https://www.googleapis.com/auth/experimentsandconfigs",
];

#[derive(Debug, Clone, serde::Deserialize)]
pub struct TokenResponse {
    pub access_token: String,
    #[serde(default)]
    pub refresh_token: String,
    pub expires_in: i32,
    #[serde(default)]
    pub token_type: String,
    #[serde(default)]
    pub scope: String,
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct UserInfo {
    pub email: String,
    #[serde(default)]
    pub name: String,
}

pub fn build_auth_url(cfg: &Config, redirect_uri: &str, state: &str) -> anyhow::Result<String> {
    let redirect_uri = redirect_uri.trim();
    let state = state.trim();
    if redirect_uri.is_empty() {
        bail!("缺少 redirect_uri");
    }
    if state.is_empty() {
        bail!("缺少 state");
    }

    let mut url = reqwest::Url::parse("https://accounts.google.com/o/oauth2/v2/auth")?;
    url.query_pairs_mut()
        .append_pair("access_type", "offline")
        .append_pair("client_id", cfg.effective_google_client_id())
        .append_pair("prompt", "consent")
        .append_pair("redirect_uri", redirect_uri)
        .append_pair("response_type", "code")
        .append_pair("scope", &OAUTH_SCOPES.join(" "))
        .append_pair("state", state);
    Ok(url.to_string())
}

pub async fn exchange_code_for_token(
    cfg: &Config,
    code: &str,
    redirect_uri: &str,
) -> anyhow::Result<TokenResponse> {
    let code = code.trim();
    let redirect_uri = redirect_uri.trim();
    if code.is_empty() {
        bail!("回调 URL 中缺少 code 参数");
    }
    if redirect_uri.is_empty() {
        bail!("缺少 redirect_uri");
    }

    let client = oauth_http_client(cfg)?;
    let resp = client
        .post("https://oauth2.googleapis.com/token")
        .header(HOST, "oauth2.googleapis.com")
        .header(USER_AGENT, cfg.api_user_agent.as_str())
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
        .form(&[
            ("code", code),
            ("client_id", cfg.effective_google_client_id()),
            ("client_secret", cfg.effective_google_client_secret()),
            ("redirect_uri", redirect_uri),
            ("grant_type", "authorization_code"),
        ])
        .send()
        .await?;

    let status = resp.status();
    let body = resp.bytes().await?;
    if body.len() > (1 << 20) {
        bail!("OAuth 响应过大");
    }

    if !status.is_success() {
        warn!(
            "OAuth 交换 token 失败（HTTP {}）：{}",
            status.as_u16(),
            String::from_utf8_lossy(&body)
        );
        bail!("交换 Token 失败：请确认授权码未过期，且 redirect_uri 与发起授权时一致");
    }

    Ok(sonic_rs::from_slice::<TokenResponse>(&body)?)
}

pub async fn refresh_token(cfg: &Config, account: &mut Account) -> anyhow::Result<()> {
    if account.refresh_token.trim().is_empty() {
        bail!("缺少 refresh_token");
    }

    let client = oauth_http_client(cfg)?;
    let resp = client
        .post("https://oauth2.googleapis.com/token")
        .header(HOST, "oauth2.googleapis.com")
        .header(USER_AGENT, cfg.api_user_agent.as_str())
        .header(CONTENT_TYPE, "application/x-www-form-urlencoded")
        .form(&[
            ("client_id", cfg.effective_google_client_id()),
            ("client_secret", cfg.effective_google_client_secret()),
            ("grant_type", "refresh_token"),
            ("refresh_token", account.refresh_token.as_str()),
        ])
        .send()
        .await?;

    let status = resp.status();
    let body = resp.bytes().await?;
    if body.len() > (1 << 20) {
        bail!("OAuth 响应过大");
    }

    if !status.is_success() {
        warn!(
            "OAuth 刷新 token 失败（HTTP {}）：{}",
            status.as_u16(),
            String::from_utf8_lossy(&body)
        );
        bail!("刷新 Token 失败");
    }

    let token = sonic_rs::from_slice::<TokenResponse>(&body)?;
    account.access_token = token.access_token;
    account.expires_in = token.expires_in;
    account.timestamp = UtcNowMs::now_ms();
    if !token.refresh_token.is_empty() {
        account.refresh_token = token.refresh_token;
    }

    info!("已刷新 Token：{}", account.email);
    Ok(())
}

pub async fn get_user_info(cfg: &Config, access_token: &str) -> anyhow::Result<UserInfo> {
    let access_token = access_token.trim();
    if access_token.is_empty() {
        bail!("缺少 access_token");
    }

    let client = oauth_http_client(cfg)?;
    let resp = client
        .get("https://www.googleapis.com/oauth2/v2/userinfo")
        .header(HOST, "www.googleapis.com")
        .header(AUTHORIZATION, format!("Bearer {access_token}"))
        .header(USER_AGENT, cfg.api_user_agent.as_str())
        .send()
        .await?;

    let status = resp.status();
    let body = resp.bytes().await?;
    if body.len() > (1 << 20) {
        bail!("用户信息响应过大");
    }

    if !status.is_success() {
        warn!(
            "获取用户信息失败（HTTP {}）：{}",
            status.as_u16(),
            String::from_utf8_lossy(&body)
        );
        bail!("获取用户信息失败");
    }

    Ok(sonic_rs::from_slice::<UserInfo>(&body)?)
}

pub fn parse_oauth_url(oauth_url: &str) -> anyhow::Result<(String, String)> {
    let u = reqwest::Url::parse(oauth_url)?;
    let mut code = String::new();
    let mut state = String::new();
    for (k, v) in u.query_pairs() {
        if k == "code" {
            code = v.to_string();
        } else if k == "state" {
            state = v.to_string();
        }
    }
    if code.trim().is_empty() {
        bail!("回调 URL 中缺少 code 参数");
    }
    Ok((code, state))
}

pub async fn fetch_project_id(cfg: &Config, access_token: &str) -> anyhow::Result<String> {
    if let Ok(pid) = fetch_project_id_from_load_code_assist(cfg, access_token).await {
        let pid = pid.trim().to_string();
        if !pid.is_empty() {
            return Ok(pid);
        }
    }

    match fetch_project_id_from_resource_manager(cfg, access_token).await {
        Ok(pid) if !pid.trim().is_empty() => Ok(pid.trim().to_string()),
        Ok(_) => Err(anyhow!("未能获取 projectId")),
        Err(e) => Err(e),
    }
}

#[derive(Debug, serde::Deserialize)]
struct LoadCodeAssistResponse {
    #[serde(rename = "cloudaicompanionProject", default)]
    cloud_ai_companion_project: String,
}

async fn fetch_project_id_from_load_code_assist(
    cfg: &Config,
    access_token: &str,
) -> anyhow::Result<String> {
    let access_token = access_token.trim();
    if access_token.is_empty() {
        bail!("缺少 access_token");
    }

    let client = oauth_http_client(cfg)?;
    let resp = client
        .post("https://daily-cloudcode-pa.sandbox.googleapis.com/v1internal:loadCodeAssist")
        .header(HOST, "daily-cloudcode-pa.sandbox.googleapis.com")
        .header(AUTHORIZATION, format!("Bearer {access_token}"))
        .header(USER_AGENT, cfg.api_user_agent.as_str())
        .header(CONTENT_TYPE, "application/json")
        .body(r#"{"metadata":{"ideType":"ANTIGRAVITY"}}"#)
        .send()
        .await?;

    if !resp.status().is_success() {
        bail!("loadCodeAssist 请求失败（HTTP {}）", resp.status().as_u16());
    }

    let body = resp.bytes().await?;
    if body.len() > (1 << 20) {
        bail!("loadCodeAssist 响应过大");
    }

    let decoded = sonic_rs::from_slice::<LoadCodeAssistResponse>(&body)?;
    Ok(decoded.cloud_ai_companion_project)
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResourceManagerProjectsResponse {
    #[serde(default)]
    projects: Vec<ResourceManagerProject>,
    #[serde(default)]
    next_page_token: String,
}

#[derive(Debug, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
struct ResourceManagerProject {
    #[serde(default)]
    project_id: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    lifecycle_state: String,
}

async fn fetch_project_id_from_resource_manager(
    cfg: &Config,
    access_token: &str,
) -> anyhow::Result<String> {
    let access_token = access_token.trim();
    if access_token.is_empty() {
        bail!("缺少 access_token");
    }

    let client = oauth_http_client(cfg)?;
    let mut page_token = String::new();

    for _ in 0..5 {
        let mut url =
            reqwest::Url::parse("https://cloudresourcemanager.googleapis.com/v1/projects")?;
        if !page_token.is_empty() {
            url.query_pairs_mut().append_pair("pageToken", &page_token);
        }

        let resp = client
            .get(url)
            .header(HOST, "cloudresourcemanager.googleapis.com")
            .header(AUTHORIZATION, format!("Bearer {access_token}"))
            .header(USER_AGENT, cfg.api_user_agent.as_str())
            .send()
            .await?;

        if !resp.status().is_success() {
            bail!(
                "Resource Manager 请求失败（HTTP {}）",
                resp.status().as_u16()
            );
        }

        let body = resp.bytes().await?;
        if body.len() > (2 << 20) {
            bail!("Resource Manager 响应过大");
        }

        let decoded = sonic_rs::from_slice::<ResourceManagerProjectsResponse>(&body)?;
        if let Some(selected) = select_project_id(&decoded.projects) {
            return Ok(selected);
        }

        if decoded.next_page_token.trim().is_empty() {
            break;
        }
        page_token = decoded.next_page_token;
    }

    bail!("未找到可用的 ACTIVE 项目")
}

fn select_project_id(projects: &[ResourceManagerProject]) -> Option<String> {
    let mut first_active = None;
    for p in projects {
        if p.lifecycle_state.trim().to_uppercase() != "ACTIVE" {
            continue;
        }
        let pid = p.project_id.trim();
        if pid.is_empty() {
            continue;
        }
        if first_active.is_none() {
            first_active = Some(pid.to_string());
        }

        let name = p.name.trim().to_lowercase();
        if name.contains("default") || pid.to_lowercase().contains("default") {
            return Some(pid.to_string());
        }
    }
    first_active
}

// ===== OAuth state（用于 callback 防 CSRF）=====

const OAUTH_STATE_TTL: Duration = Duration::from_secs(10 * 60);

struct OAuthStateManager {
    states: Mutex<HashMap<String, Instant>>,
}

static OAUTH_STATES: OnceLock<OAuthStateManager> = OnceLock::new();

pub async fn generate_state() -> anyhow::Result<String> {
    let mgr = OAUTH_STATES.get_or_init(|| OAuthStateManager {
        states: Mutex::new(HashMap::new()),
    });
    mgr.generate().await
}

pub async fn validate_state(state: &str) -> bool {
    let mgr = OAUTH_STATES.get_or_init(|| OAuthStateManager {
        states: Mutex::new(HashMap::new()),
    });
    mgr.validate(state).await
}

impl OAuthStateManager {
    async fn generate(&self) -> anyhow::Result<String> {
        let mut bytes = [0u8; 32];
        bytes[..16].copy_from_slice(Uuid::new_v4().as_bytes());
        bytes[16..].copy_from_slice(Uuid::new_v4().as_bytes());
        let state = base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(bytes);

        let now = Instant::now();
        let mut guard = self.states.lock().await;
        purge_expired(&mut guard, now);
        guard.insert(state.clone(), now + OAUTH_STATE_TTL);
        Ok(state)
    }

    async fn validate(&self, state: &str) -> bool {
        let state = state.trim();
        if state.is_empty() {
            return false;
        }

        let now = Instant::now();
        let mut guard = self.states.lock().await;
        purge_expired(&mut guard, now);
        let Some(expires_at) = guard.remove(state) else {
            return false;
        };
        now < expires_at
    }
}

fn purge_expired(states: &mut HashMap<String, Instant>, now: Instant) {
    states.retain(|_, &mut exp| now < exp);
}

// ===== HTTP client =====

static OAUTH_HTTP_CLIENT: OnceLock<reqwest::Client> = OnceLock::new();

fn oauth_http_client(cfg: &Config) -> anyhow::Result<&'static reqwest::Client> {
    if let Some(c) = OAUTH_HTTP_CLIENT.get() {
        return Ok(c);
    }

    let client = build_oauth_http_client(cfg)?;
    let _ = OAUTH_HTTP_CLIENT.set(client);
    OAUTH_HTTP_CLIENT
        .get()
        .ok_or_else(|| anyhow!("初始化 OAuth HTTP client 失败"))
}

fn build_oauth_http_client(cfg: &Config) -> anyhow::Result<reqwest::Client> {
    let mut builder = reqwest::Client::builder()
        .pool_max_idle_per_host(10)
        .pool_idle_timeout(Duration::from_secs(90))
        // 对齐 Go 版本：禁用 HTTP/2（Go 端 ForceAttemptHTTP2=false）。
        // Google OAuth 端点在 HTTP/2 下偶发返回 PROTOCOL_ERROR 导致刷新失败。
        .http1_only();

    if cfg.timeout_ms > 0 {
        builder = builder.timeout(Duration::from_millis(cfg.timeout_ms));
    }

    if !cfg.proxy.trim().is_empty() {
        builder = builder.proxy(reqwest::Proxy::all(cfg.proxy.trim())?);
    }

    Ok(builder.build()?)
}

struct UtcNowMs;

impl UtcNowMs {
    fn now_ms() -> i64 {
        chrono::Utc::now().timestamp_millis()
    }
}
