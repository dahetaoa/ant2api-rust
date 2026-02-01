//! 配额缓存模块，与 Go 版本语义对齐。
//!
//! - 成功缓存 TTL: 2 分钟
//! - 错误缓存 TTL: 30 秒
//! - 请求超时: 20 秒
//! - 最大并发: 4
//! - 按 sessionId 去重 inflight 请求

use sonic_rs::{JsonContainerTrait, JsonValueTrait};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{Mutex, Semaphore};

use crate::credential::types::Account;
use crate::vertex::client::{Endpoint, VertexClient};

/// 缓存 TTL 常量
const QUOTA_CACHE_TTL: Duration = Duration::from_secs(2 * 60);
const QUOTA_ERROR_CACHE_TTL: Duration = Duration::from_secs(30);
const QUOTA_FETCH_TIMEOUT: Duration = Duration::from_secs(20);
const QUOTA_MAX_CONCURRENCY: usize = 4;

/// 配额分组
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct QuotaGroup {
    pub group_name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remaining_fraction: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reset_time: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub model_list: Vec<String>,
}

/// 账号配额信息
#[derive(Debug, Clone)]
pub struct AccountQuota {
    pub session_id: String,
    pub groups: Vec<QuotaGroup>,
    pub fetched_at: chrono::DateTime<chrono::Utc>,
}

/// 缓存条目
struct CacheEntry {
    quota: Option<AccountQuota>,
    error: Option<String>,
    expires_at: Instant,
}

/// Inflight 请求状态
struct InflightRequest {
    done: tokio::sync::broadcast::Sender<()>,
    result: Arc<Mutex<Option<Result<AccountQuota, String>>>>,
}

/// 配额缓存
pub struct QuotaCache {
    cache: Mutex<HashMap<String, CacheEntry>>,
    inflight: Mutex<HashMap<String, Arc<InflightRequest>>>,
    semaphore: Semaphore,
}

impl QuotaCache {
    pub fn new() -> Self {
        Self {
            cache: Mutex::new(HashMap::new()),
            inflight: Mutex::new(HashMap::new()),
            semaphore: Semaphore::new(QUOTA_MAX_CONCURRENCY),
        }
    }

    /// 使指定 session 的缓存失效。
    pub async fn invalidate(&self, session_id: &str) {
        let session_id = session_id.trim();
        if session_id.is_empty() {
            return;
        }
        let mut cache = self.cache.lock().await;
        cache.remove(session_id);
    }

    /// 获取账号配额（带缓存）。
    /// 返回 (quota, cached, error_message)
    pub async fn get_quota(
        &self,
        account: &Account,
        endpoint: &Endpoint,
        vertex: &VertexClient,
        force: bool,
    ) -> (Option<AccountQuota>, bool, Option<String>) {
        let session_id = account.session_id.trim();
        if session_id.is_empty() {
            // 无 session_id，直接获取不缓存
            return self.fetch_quota_once(account, endpoint, vertex).await;
        }

        let now = Instant::now();

        // 检查缓存
        if !force {
            let cache = self.cache.lock().await;
            if let Some(entry) = cache.get(session_id) && now < entry.expires_at {
                return (entry.quota.clone(), true, entry.error.clone());
            }
        }

        // 检查是否有 inflight 请求
        {
            let inflight = self.inflight.lock().await;
            if let Some(req) = inflight.get(session_id) {
                let req = req.clone();
                drop(inflight);

                // 等待 inflight 请求完成
                let mut rx = req.done.subscribe();
                let _ = rx.recv().await;

                let result = req.result.lock().await;
                if let Some(ref res) = *result {
                    match res {
                        Ok(q) => return (Some(q.clone()), false, None),
                        Err(e) => return (None, false, Some(e.clone())),
                    }
                }
            }
        }

        // 创建新的 inflight 请求
        let (tx, _rx) = tokio::sync::broadcast::channel(1);
        let inflight_req = Arc::new(InflightRequest {
            done: tx.clone(),
            result: Arc::new(Mutex::new(None)),
        });

        {
            let mut inflight = self.inflight.lock().await;
            inflight.insert(session_id.to_string(), inflight_req.clone());
        }

        // 执行获取
        let (quota, _cached, error) = self.fetch_quota_once(account, endpoint, vertex).await;

        // 更新缓存
        {
            let mut cache = self.cache.lock().await;
            let ttl = if error.is_some() {
                QUOTA_ERROR_CACHE_TTL
            } else {
                QUOTA_CACHE_TTL
            };
            cache.insert(
                session_id.to_string(),
                CacheEntry {
                    quota: quota.clone(),
                    error: error.clone(),
                    expires_at: Instant::now() + ttl,
                },
            );
        }

        // 更新 inflight 结果并通知等待者
        {
            let mut result = inflight_req.result.lock().await;
            *result = Some(if let Some(q) = quota.clone() {
                Ok(q)
            } else {
                Err(error.clone().unwrap_or_default())
            });
        }
        let _ = tx.send(());

        // 移除 inflight
        {
            let mut inflight = self.inflight.lock().await;
            inflight.remove(session_id);
        }

        (quota, false, error)
    }

    /// 直接获取配额（不走缓存）。
    async fn fetch_quota_once(
        &self,
        account: &Account,
        endpoint: &Endpoint,
        vertex: &VertexClient,
    ) -> (Option<AccountQuota>, bool, Option<String>) {
        // 获取并发许可
        let _permit =
            match tokio::time::timeout(QUOTA_FETCH_TIMEOUT, self.semaphore.acquire()).await {
                Ok(Ok(permit)) => permit,
                Ok(Err(_)) => return (None, false, Some("内部错误".to_string())),
                Err(_) => return (None, false, Some("请求超时，无法获取配额".to_string())),
            };

        let project_id = account.project_id.trim();
        let project_id = if project_id.is_empty() {
            crate::util::id::project_id()
        } else {
            project_id.to_string()
        };

        let access_token = account.access_token.trim();
        if access_token.is_empty() {
            return (None, false, Some("缺少 access_token".to_string()));
        }

        // 带超时获取
        let result = tokio::time::timeout(
            QUOTA_FETCH_TIMEOUT,
            vertex.fetch_available_models(endpoint, &project_id, access_token, &account.email),
        )
        .await;

        match result {
            Ok(Ok(resp)) => {
                let groups = group_quota_groups(&resp.models);
                let quota = AccountQuota {
                    session_id: account.session_id.clone(),
                    groups,
                    fetched_at: chrono::Utc::now(),
                };
                (Some(quota), false, None)
            }
            Ok(Err(e)) => {
                let msg = quota_error_message(&e);
                (None, false, Some(msg))
            }
            Err(_) => (None, false, Some("请求超时，无法获取配额".to_string())),
        }
    }
}

impl Default for QuotaCache {
    fn default() -> Self {
        Self::new()
    }
}

/// 配额分组键常量
pub(crate) const QUOTA_GROUP_CLAUDE_GPT: &str = "Claude/GPT";
pub(crate) const QUOTA_GROUP_GEMINI3_PRO: &str = "Gemini 3 Pro";
pub(crate) const QUOTA_GROUP_GEMINI3_FLASH: &str = "Gemini 3 Flash";
pub(crate) const QUOTA_GROUP_GEMINI3_PRO_IMAGE: &str = "Gemini 3 Pro Image";
pub(crate) const QUOTA_GROUP_GEMINI25: &str = "Gemini 2.5 Pro/Flash/Lite";

/// 根据模型 ID 确定分组键。
pub(crate) fn group_quota_key(model_id: &str) -> &'static str {
    let m = crate::util::model::canonical_model_id(model_id).to_lowercase();
    if m.starts_with("claude-") || m.starts_with("gpt-") {
        QUOTA_GROUP_CLAUDE_GPT
    } else if m.starts_with("gemini-3-pro-high") {
        QUOTA_GROUP_GEMINI3_PRO
    } else if m.starts_with("gemini-3-flash") {
        QUOTA_GROUP_GEMINI3_FLASH
    } else if m.starts_with("gemini-3-pro-image") {
        QUOTA_GROUP_GEMINI3_PRO_IMAGE
    } else {
        QUOTA_GROUP_GEMINI25
    }
}

/// 将模型响应分组为配额组。
pub(crate) fn group_quota_groups(models: &HashMap<String, sonic_rs::Value>) -> Vec<QuotaGroup> {
    use std::collections::BTreeMap;

    let mut groups: BTreeMap<&str, QuotaGroup> = BTreeMap::new();

    for (model_id, model_data) in models {
        let model_id = model_id.trim();
        if model_id.is_empty() {
            continue;
        }

        let group_name = group_quota_key(model_id);
        let group = groups.entry(group_name).or_insert_with(|| QuotaGroup {
            group_name: group_name.to_string(),
            remaining_fraction: None,
            reset_time: None,
            model_list: Vec::new(),
        });

        group
            .model_list
            .push(crate::util::model::canonical_model_id(model_id));

        let mq = parse_model_quota(model_data);
        if group.remaining_fraction.is_none() && mq.remaining_fraction.is_some() {
            group.remaining_fraction = mq.remaining_fraction;
        }
        if group.reset_time.is_none() && mq.reset_time.is_some() {
            group.reset_time = mq.reset_time;
        }
    }

    // 按预定义顺序排序
    let order = [
        QUOTA_GROUP_CLAUDE_GPT,
        QUOTA_GROUP_GEMINI3_PRO,
        QUOTA_GROUP_GEMINI3_FLASH,
        QUOTA_GROUP_GEMINI3_PRO_IMAGE,
        QUOTA_GROUP_GEMINI25,
    ];

    let mut out = Vec::new();
    let mut seen = std::collections::HashSet::new();

    for name in order {
        if let Some(mut g) = groups.remove(name) {
            g.model_list.sort();
            out.push(g);
            seen.insert(name);
        }
    }

    // 添加未知分组（按名称排序）
    let mut rest: Vec<_> = groups.into_iter().collect();
    rest.sort_by_key(|(k, _)| *k);
    for (_, mut g) in rest {
        g.model_list.sort();
        out.push(g);
    }

    out
}

/// 模型配额解析结果
struct ModelQuota {
    remaining_fraction: Option<f64>,
    reset_time: Option<String>,
}

/// 解析模型配额数据。
fn parse_model_quota(value: &sonic_rs::Value) -> ModelQuota {
    let Some(obj) = value.as_object() else {
        return ModelQuota {
            remaining_fraction: None,
            reset_time: None,
        };
    };

    // 尝试直接解析
    if let Some(mq) = parse_model_quota_map(obj)
        && (mq.remaining_fraction.is_some() || mq.reset_time.is_some())
    {
        return mq;
    }

    // 尝试 quotaInfo
    let quota_info_key = "quotaInfo".to_string();
    if let Some(qi) = obj.get(&quota_info_key).and_then(|v| v.as_object())
        && let Some(mq) = parse_model_quota_map(qi)
        && (mq.remaining_fraction.is_some() || mq.reset_time.is_some())
    {
        return mq;
    }

    // 尝试 quota
    let quota_key = "quota".to_string();
    if let Some(q) = obj.get(&quota_key).and_then(|v| v.as_object())
        && let Some(mq) = parse_model_quota_map(q)
    {
        return mq;
    }

    ModelQuota {
        remaining_fraction: None,
        reset_time: None,
    }
}

/// 从对象解析配额字段。
fn parse_model_quota_map(obj: &sonic_rs::Object) -> Option<ModelQuota> {
    let remaining_fraction_key = "remainingFraction".to_string();
    let reset_time_key = "resetTime".to_string();

    let has_remaining_fraction = obj.contains_key(&remaining_fraction_key);
    let remaining_fraction = obj
        .get(&remaining_fraction_key)
        .and_then(any_to_float64)
        .map(clamp01);

    let reset_time = obj
        .get(&reset_time_key)
        .and_then(|v| v.as_str())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty());

    // 兼容后端在配额完全耗尽时不返回 remainingFraction 的情况：
    // 仅返回 quotaInfo.resetTime，此时 remainingFraction 语义应视为 0。
    let remaining_fraction =
        if remaining_fraction.is_none() && !has_remaining_fraction && reset_time.is_some() {
            Some(0.0)
        } else {
            remaining_fraction
        };

    Some(ModelQuota {
        remaining_fraction,
        reset_time,
    })
}

/// 将任意值转换为 f64。
fn any_to_float64(v: &sonic_rs::Value) -> Option<f64> {
    if let Some(n) = v.as_f64() {
        return Some(n);
    }
    if let Some(n) = v.as_i64() {
        return Some(n as f64);
    }
    if let Some(n) = v.as_u64() {
        return Some(n as f64);
    }
    if let Some(s) = v.as_str() {
        let s = s.trim();
        if s.is_empty() {
            return None;
        }
        return s.parse().ok();
    }
    None
}

/// 将值限制在 [0, 1] 范围内。
fn clamp01(v: f64) -> f64 {
    v.clamp(0.0, 1.0)
}

/// 将 API 错误转换为用户友好的中文消息。
pub fn quota_error_message(err: &crate::vertex::client::ApiError) -> String {
    use crate::vertex::client::ApiError;

    match err {
        ApiError::Http {
            status, message, ..
        } => {
            if *status == 401 {
                "Token 已失效或无权限，无法获取配额".to_string()
            } else if *status == 429 {
                "请求过于频繁，请稍后重试".to_string()
            } else if !message.is_empty() {
                format!("无法获取配额：{}", message)
            } else {
                format!("无法获取配额：{}", err)
            }
        }
        ApiError::Transport(e) => {
            if e.is_timeout() {
                "请求超时，无法获取配额".to_string()
            } else {
                format!("无法获取配额：{}", e)
            }
        }
        ApiError::Json(e) => format!("无法获取配额：{}", e),
    }
}
