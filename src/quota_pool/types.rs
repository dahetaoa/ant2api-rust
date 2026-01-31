use chrono::{DateTime, Utc};
use std::collections::HashMap;
use std::time::Instant;

/// 单个账号在某个配额分组（pool）里的状态。
#[derive(Debug, Clone)]
pub struct PoolEntry {
    /// 剩余配额比例 [0, 1]。
    pub remaining_fraction: f64,
    /// 配额重置时间（如果后端提供）。
    #[allow(dead_code)]
    pub reset_time: Option<DateTime<Utc>>,
    /// 最近一次更新该条目的时间（用于诊断/过期策略）。
    #[allow(dead_code)]
    pub last_updated: Instant,
}

/// 一个配额池，代表某类模型共享的配额分组（例如 Claude/GPT、Gemini 3 Flash 等）。
#[derive(Debug, Clone)]
pub struct QuotaPool {
    /// 可用账号：用于选择 token。
    pub active: HashMap<String, PoolEntry>,
    /// 冷却账号：配额耗尽且已知 resetTime，等待到点后再刷新。
    pub cooldown: HashMap<String, DateTime<Utc>>,
}

impl QuotaPool {
    pub fn new(_pool_name: &str) -> Self {
        Self {
            active: HashMap::new(),
            cooldown: HashMap::new(),
        }
    }
}
