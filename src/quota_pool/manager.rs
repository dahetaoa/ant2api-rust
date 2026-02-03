use crate::quota_pool::selector;
use crate::quota_pool::types::{PoolEntry, QuotaPool};
use chrono::{DateTime, Utc};
use sonic_rs::{JsonContainerTrait, JsonValueTrait};
use std::collections::{HashMap, HashSet};
use std::time::Instant;
use tokio::sync::RwLock;

/// 配额分组键常量（同时也是池名）。
pub(crate) const QUOTA_GROUP_CLAUDE_GPT: &str = "Claude/GPT";
pub(crate) const QUOTA_GROUP_GEMINI3_PRO: &str = "Gemini 3 Pro";
pub(crate) const QUOTA_GROUP_GEMINI3_FLASH: &str = "Gemini 3 Flash";
pub(crate) const QUOTA_GROUP_GEMINI3_PRO_IMAGE: &str = "Gemini 3 Pro Image";
pub(crate) const QUOTA_GROUP_GEMINI25: &str = "Gemini 2.5 Pro/Flash/Lite";

/// 预定义分组的标准展示/路由顺序（必须与路由选择逻辑保持一致）。
pub(crate) const QUOTA_GROUP_ORDER: [&str; 5] = [
    QUOTA_GROUP_CLAUDE_GPT,
    QUOTA_GROUP_GEMINI3_PRO,
    QUOTA_GROUP_GEMINI3_FLASH,
    QUOTA_GROUP_GEMINI3_PRO_IMAGE,
    QUOTA_GROUP_GEMINI25,
];

/// 配额分组（用于后台刷新入池，以及 WebUI 展示）。
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

/// 账号配额快照（用于 WebUI/API 输出）。
#[derive(Debug, Clone, serde::Serialize)]
#[serde(rename_all = "camelCase")]
pub struct AccountQuota {
    pub session_id: String,
    pub groups: Vec<QuotaGroup>,
    pub fetched_at: DateTime<Utc>,
}

/// 配额池管理器：集中维护所有分组（pool）的账号配额视图，并提供按模型分组的账号选择。
#[derive(Debug)]
pub struct QuotaPoolManager {
    inner: RwLock<Inner>,
}

#[derive(Debug)]
struct Inner {
    pools: HashMap<String, QuotaPool>,
}

impl QuotaPoolManager {
    pub fn new() -> Self {
        let mut pools = HashMap::new();
        for name in QUOTA_GROUP_ORDER {
            pools.insert(name.to_string(), QuotaPool::new(name));
        }
        Self {
            inner: RwLock::new(Inner { pools }),
        }
    }

    /// 根据一次配额查询结果更新池状态。
    ///
    /// - remainingFraction 存在：加入/更新 active
    /// - remainingFraction 不存在但 resetTime 存在：加入 cooldown
    pub async fn update_from_quota(&self, session_id: &str, groups: &[QuotaGroup]) {
        let session_id = session_id.trim();
        if session_id.is_empty() {
            return;
        }

        let now = Utc::now();
        let now_instant = Instant::now();

        let mut inner = self.inner.write().await;
        for g in groups {
            let pool_name = g.group_name.trim();
            if pool_name.is_empty() {
                continue;
            }
            let pool = inner
                .pools
                .entry(pool_name.to_string())
                .or_insert_with(|| QuotaPool::new(pool_name));

            let reset_dt = parse_reset_time(g.reset_time.as_deref());

            match g.remaining_fraction {
                Some(frac) => {
                    let frac = if frac.is_finite() {
                        frac.clamp(0.0, 1.0)
                    } else {
                        0.0
                    };

                    // remainingFraction=0 且 resetTime 在未来：更符合“冷却”语义，避免被频繁选中。
                    let should_cooldown =
                        frac <= 0.0 && reset_dt.as_ref().is_some_and(|rt| *rt > now);
                    if should_cooldown {
                        pool.active.remove(session_id);
                        if let Some(rt) = reset_dt {
                            pool.cooldown.insert(session_id.to_string(), rt);
                        }
                        continue;
                    }

                    pool.cooldown.remove(session_id);
                    pool.active.insert(
                        session_id.to_string(),
                        PoolEntry {
                            remaining_fraction: frac,
                            reset_time: reset_dt,
                            last_updated: now_instant,
                        },
                    );
                }
                None => {
                    if let Some(rt) = reset_dt {
                        pool.active.remove(session_id);
                        pool.cooldown.insert(session_id.to_string(), rt);
                    }
                }
            }
        }
    }

    /// 从指定 pool 里选择一个账号（sessionId），并跳过 exclude。
    pub async fn get_account_for_pool_excluding(
        &self,
        pool_name: &str,
        exclude: &HashSet<String>,
    ) -> Option<String> {
        let pool_name = pool_name.trim();
        if pool_name.is_empty() {
            return None;
        }
        let inner = self.inner.read().await;
        let pool = inner.pools.get(pool_name)?;
        selector::select_weighted_excluding(&pool.active, exclude)
    }

    /// 移除指定 sessionId 在所有池中的状态（用于账号删除/禁用后的清理）。
    pub async fn remove_session(&self, session_id: &str) {
        let session_id = session_id.trim();
        if session_id.is_empty() {
            return;
        }

        let mut inner = self.inner.write().await;
        for pool in inner.pools.values_mut() {
            pool.active.remove(session_id);
            pool.cooldown.remove(session_id);
        }
    }

    /// 与 Store 同步：只保留 valid_sessions 内的条目（用于清理被删除/禁用的账号）。
    pub async fn sync_valid_sessions(&self, valid_sessions: &HashSet<String>) {
        let mut inner = self.inner.write().await;
        for pool in inner.pools.values_mut() {
            pool.active.retain(|sid, _| valid_sessions.contains(sid));
            pool.cooldown.retain(|sid, _| valid_sessions.contains(sid));
        }
    }

    /// 找出所有已到达 resetTime 的冷却账号（去重后返回 sessionId 列表）。
    pub async fn due_cooldown_sessions(&self) -> Vec<String> {
        let now = Utc::now();
        let inner = self.inner.read().await;
        let mut out: HashSet<String> = HashSet::new();
        for pool in inner.pools.values() {
            for (sid, rt) in &pool.cooldown {
                if *rt <= now {
                    out.insert(sid.clone());
                }
            }
        }
        out.into_iter().collect()
    }

    /// 获取指定账号在所有预定义分组下的配额快照（用于 WebUI 展示）。
    ///
    /// 规则：
    /// - 若在 active：返回当前 remaining_fraction（并尽量附带 reset_time）
    /// - 若在 cooldown：remaining_fraction 固定为 0.0，并附带 reset_time
    /// - 若缺失：remaining_fraction 固定为 0.0
    ///
    /// 注意：为保证 UI 稳定性，未知/未入池的 sessionId 也会返回“全 0 分组”而非 404。
    pub async fn get_session_quota_groups(&self, session_id: &str) -> Vec<QuotaGroup> {
        let session_id = session_id.trim();
        let inner = self.inner.read().await;

        let mut out = Vec::with_capacity(QUOTA_GROUP_ORDER.len());
        for pool_name in QUOTA_GROUP_ORDER {
            let mut g = QuotaGroup {
                group_name: pool_name.to_string(),
                remaining_fraction: Some(0.0),
                reset_time: None,
                model_list: Vec::new(),
            };

            let Some(pool) = inner.pools.get(pool_name) else {
                out.push(g);
                continue;
            };

            if let Some(e) = pool.active.get(session_id) {
                g.remaining_fraction = Some(e.remaining_fraction);
                g.reset_time = e.reset_time.as_ref().map(|rt| rt.to_rfc3339());
            } else if let Some(rt) = pool.cooldown.get(session_id) {
                g.remaining_fraction = Some(0.0);
                g.reset_time = Some(rt.to_rfc3339());
            }

            out.push(g);
        }

        out
    }
}

impl Default for QuotaPoolManager {
    fn default() -> Self {
        Self::new()
    }
}

fn parse_reset_time(v: Option<&str>) -> Option<DateTime<Utc>> {
    let s = v?.trim();
    if s.is_empty() {
        return None;
    }
    chrono::DateTime::parse_from_rfc3339(s)
        .ok()
        .map(|dt| dt.with_timezone(&Utc))
}

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
    let mut out = Vec::new();

    for name in QUOTA_GROUP_ORDER {
        if let Some(mut g) = groups.remove(name) {
            g.model_list.sort();
            out.push(g);
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
