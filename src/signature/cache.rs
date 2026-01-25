use crate::signature::types::EntryIndex;
use chrono::Utc;
use moka::future::Cache;

const DEFAULT_SIGNATURE_CACHE_CAPACITY: u64 = 50_000;

#[derive(Clone, Debug)]
pub struct SignatureCache {
    by_key: Cache<String, EntryIndex>,
    by_tool_call_id: Cache<String, EntryIndex>,
}

impl SignatureCache {
    pub fn new() -> Self {
        Self {
            by_key: Cache::new(DEFAULT_SIGNATURE_CACHE_CAPACITY),
            by_tool_call_id: Cache::new(DEFAULT_SIGNATURE_CACHE_CAPACITY),
        }
    }

    pub async fn put(&self, idx: EntryIndex) {
        let Some(key) = idx.key() else {
            return;
        };
        if idx.tool_call_id.is_empty() {
            return;
        }

        self.by_key.insert(key, idx.clone()).await;
        self.by_tool_call_id
            .insert(idx.tool_call_id.clone(), idx)
            .await;
    }

    pub async fn get(&self, request_id: &str, tool_call_id: &str) -> Option<EntryIndex> {
        if request_id.is_empty() || tool_call_id.is_empty() {
            return None;
        }
        let key = format!("{request_id}:{tool_call_id}");
        let mut idx = self.by_key.get(&key).await?;
        idx.last_access = Some(Utc::now());
        self.put(idx.clone()).await;
        Some(idx)
    }

    pub async fn get_by_tool_call_id(&self, tool_call_id: &str) -> Option<EntryIndex> {
        if tool_call_id.is_empty() {
            return None;
        }
        // moka::Cache<String, _> 支持用 &str 查询（String 等价比较）。
        let mut idx = self.by_tool_call_id.get(tool_call_id).await?;
        idx.last_access = Some(Utc::now());
        self.put(idx.clone()).await;
        Some(idx)
    }

    pub async fn get_by_tool_call_id_and_signature_prefix(
        &self,
        tool_call_id: &str,
        sig_prefix: &str,
    ) -> Option<EntryIndex> {
        let tool_call_id = tool_call_id.trim();
        let sig_prefix = sig_prefix.trim();
        if tool_call_id.is_empty() || sig_prefix.is_empty() {
            return None;
        }

        let idx = self.get_by_tool_call_id(tool_call_id).await?;
        if !idx.signature_prefix.is_empty() && !idx.signature_prefix.starts_with(sig_prefix) {
            return None;
        }
        Some(idx)
    }
}

impl Default for SignatureCache {
    fn default() -> Self {
        Self::new()
    }
}
