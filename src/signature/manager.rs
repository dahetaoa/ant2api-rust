use crate::signature::cache::SignatureCache;
use crate::signature::store::Store;
use crate::signature::types::{Entry, EntryIndex, signature_prefix};
use chrono::Utc;
use std::sync::Arc;

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
        if request_id.is_empty() || tool_call_id.is_empty() || signature.is_empty() {
            return;
        }

        let sig_prefix = signature_prefix(&signature);
        let now = Utc::now();

        let e = Arc::new(Entry {
            signature,
            reasoning,
            request_id: request_id.clone(),
            tool_call_id: tool_call_id.clone(),
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
                signature_prefix: sig_prefix,
                date: String::new(),
            })
            .await;
        self.store.enqueue(e).await;
    }

    pub async fn lookup(&self, request_id: &str, tool_call_id: &str) -> Option<Entry> {
        let idx = self.cache.get(request_id, tool_call_id).await?;
        let e = self.store.load_by_index(&idx).await?;
        if e.signature.is_empty() {
            return None;
        }
        Some(e)
    }

    pub async fn lookup_by_tool_call_id(&self, tool_call_id: &str) -> Option<Entry> {
        let idx = self.cache.get_by_tool_call_id(tool_call_id).await?;
        let e = self.store.load_by_index(&idx).await?;
        if e.signature.is_empty() {
            return None;
        }
        Some(e)
    }

    /// 将客户端持久化的短前缀（sigPrefix）扩展回完整 signature（用于减少 payload 体积）。
    pub async fn lookup_by_tool_call_id_and_signature_prefix(
        &self,
        tool_call_id: &str,
        sig_prefix: &str,
    ) -> Option<Entry> {
        let sig_prefix = sig_prefix.trim();
        if tool_call_id.is_empty() || sig_prefix.is_empty() {
            return None;
        }

        let idx = self.cache.get_by_tool_call_id(tool_call_id).await?;
        if !idx.signature_prefix.is_empty() && !idx.signature_prefix.starts_with(sig_prefix) {
            return None;
        }

        let e = self.store.load_by_index(&idx).await?;
        if e.signature.is_empty() || !e.signature.starts_with(sig_prefix) {
            return None;
        }
        Some(e)
    }
}
