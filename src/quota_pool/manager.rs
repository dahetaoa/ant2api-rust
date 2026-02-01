use crate::gateway::manager::quota::{
    QUOTA_GROUP_CLAUDE_GPT, QUOTA_GROUP_GEMINI3_FLASH, QUOTA_GROUP_GEMINI3_PRO,
    QUOTA_GROUP_GEMINI3_PRO_IMAGE, QUOTA_GROUP_GEMINI25, QuotaGroup,
};
use crate::quota_pool::selector;
use crate::quota_pool::types::{PoolEntry, QuotaPool};
use chrono::{DateTime, Utc};
use std::collections::{HashMap, HashSet};
use std::time::Instant;
use tokio::sync::RwLock;

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
        for name in [
            QUOTA_GROUP_CLAUDE_GPT,
            QUOTA_GROUP_GEMINI3_PRO,
            QUOTA_GROUP_GEMINI3_FLASH,
            QUOTA_GROUP_GEMINI3_PRO_IMAGE,
            QUOTA_GROUP_GEMINI25,
        ] {
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
