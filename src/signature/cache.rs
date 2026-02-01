use crate::signature::types::EntryIndex;
use chrono::Utc;
use moka::future::Cache;

const DEFAULT_SIGNATURE_CACHE_CAPACITY: u64 = 50_000;

#[derive(Clone, Debug)]
pub struct SignatureCache {
    by_tool_call_id: Cache<String, EntryIndex>,
}

impl SignatureCache {
    pub fn new() -> Self {
        Self {
            by_tool_call_id: Cache::new(DEFAULT_SIGNATURE_CACHE_CAPACITY),
        }
    }

    pub async fn put(&self, idx: EntryIndex) {
        if idx.tool_call_id.is_empty() {
            return;
        }

        self.by_tool_call_id
            .insert(idx.tool_call_id.clone(), idx)
            .await;
    }

    pub async fn get(&self, request_id: &str, tool_call_id: &str) -> Option<EntryIndex> {
        if request_id.is_empty() || tool_call_id.is_empty() {
            return None;
        }
        let mut idx = self.by_tool_call_id.get(tool_call_id).await?;
        if idx.request_id != request_id {
            return None;
        }
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
}

impl Default for SignatureCache {
    fn default() -> Self {
        Self::new()
    }
}
