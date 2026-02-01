use crate::signature::cache::SignatureCache;
use crate::signature::store::Store;
use crate::signature::types::{Entry, EntryIndex, FALLBACK_SIGNATURE};
use chrono::Utc;
use std::sync::Arc;
use tokio::time::{Duration, interval};

/// 思维签名管理器：负责热写入、LRU 索引缓存与落盘队列的编排。
#[derive(Clone)]
pub struct Manager {
    cache: SignatureCache,
    store: Arc<Store>,
}

impl Manager {
    /// 创建 Manager，并尝试加载最近 `3` 天的索引到内存缓存（best-effort，与 Go 行为一致）。
    pub async fn new(data_dir: &str) -> anyhow::Result<Self> {
        let cache = SignatureCache::new();
        let store = Store::new(data_dir, cache.clone())?;

        // Go 版本 LoadRecent 不返回错误；这里也采用 best-effort。
        let _ = store.load_recent(3).await;

        spawn_daily_cache_cleanup_task(data_dir.to_string());

        Ok(Self { cache, store })
    }

    pub fn cache(&self) -> &SignatureCache {
        &self.cache
    }

    pub fn store(&self) -> &Arc<Store> {
        &self.store
    }

    pub async fn save(
        &self,
        request_id: &str,
        tool_call_id: &str,
        signature: &str,
        reasoning: &str,
        model: &str,
    ) {
        self.save_owned(
            request_id.to_string(),
            tool_call_id.to_string(),
            signature.to_string(),
            reasoning.to_string(),
            model.to_string(),
        )
        .await;
    }

    pub async fn save_owned(
        &self,
        request_id: String,
        tool_call_id: String,
        signature: String,
        reasoning: String,
        model: String,
    ) {
        self.save_owned_with_kind(
            request_id,
            tool_call_id,
            false,
            signature,
            reasoning,
            model,
        )
        .await;
    }

    pub async fn save_image_key(
        &self,
        request_id: String,
        image_key: String,
        signature: String,
        reasoning: String,
        model: String,
    ) {
        self.save_owned_with_kind(
            request_id,
            image_key,
            true,
            signature,
            reasoning,
            model,
        )
        .await;
    }

    async fn save_owned_with_kind(
        &self,
        request_id: String,
        tool_call_id: String,
        is_image_key: bool,
        signature: String,
        reasoning: String,
        model: String,
    ) {
        if request_id.is_empty() || tool_call_id.is_empty() || signature.is_empty() {
            return;
        }

        let now = Utc::now();

        let e = Arc::new(Entry {
            signature,
            reasoning,
            request_id: request_id.clone(),
            tool_call_id: tool_call_id.clone(),
            is_image_key,
            model: model.clone(),
            created_at: now,
            last_access: now,
        });

        self.store.put_hot(e.clone()).await;
        self.cache
            .put(EntryIndex {
                request_id,
                tool_call_id,
                model,
                created_at: Some(now),
                last_access: Some(now),
                date: String::new(),
            })
            .await;
        self.store.enqueue(e).await;
    }

    pub async fn lookup(&self, request_id: &str, tool_call_id: &str) -> Option<Entry> {
        let idx = self.cache.get(request_id, tool_call_id).await?;
        match self.store.load_by_index(&idx).await {
            Some(mut e) => {
                if e.signature.is_empty() {
                    e.signature = FALLBACK_SIGNATURE.to_string();
                }
                Some(e)
            }
            None => Some(fallback_entry_from_index(&idx)),
        }
    }

    pub async fn lookup_by_tool_call_id(&self, tool_call_id: &str) -> Option<Entry> {
        let idx = self.cache.get_by_tool_call_id(tool_call_id).await?;
        match self.store.load_by_index(&idx).await {
            Some(mut e) => {
                if e.signature.is_empty() {
                    e.signature = FALLBACK_SIGNATURE.to_string();
                }
                Some(e)
            }
            None => Some(fallback_entry_from_index(&idx)),
        }
    }

    pub async fn lookup_by_image_key(&self, image_key: &str) -> Option<Entry> {
        if image_key.trim().is_empty() {
            return None;
        }
        self.lookup_by_tool_call_id(image_key).await
    }
}

fn fallback_entry_from_index(idx: &EntryIndex) -> Entry {
    let now = Utc::now();
    Entry {
        signature: FALLBACK_SIGNATURE.to_string(),
        reasoning: String::new(),
        request_id: idx.request_id.clone(),
        tool_call_id: idx.tool_call_id.clone(),
        is_image_key: false,
        model: idx.model.clone(),
        created_at: idx.created_at.unwrap_or(now),
        last_access: idx.last_access.unwrap_or(now),
    }
}

fn spawn_daily_cache_cleanup_task(data_dir: String) {
    tokio::spawn(async move {
        let mut tick = interval(Duration::from_secs(24 * 60 * 60));
        loop {
            tick.tick().await;
            let days = crate::runtime_config::get().cache_retention_days;
            match crate::signature::store::cleanup_signature_cache_files(&data_dir, days).await {
                Ok(_) => {}
                Err(e) => tracing::warn!("清理签名缓存失败: {e:#}"),
            }
        }
    });
}
