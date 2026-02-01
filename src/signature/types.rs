use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub const FALLBACK_SIGNATURE: &str = "context_engineering_is_the_way_to_go";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Entry {
    pub signature: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub reasoning: String,
    #[serde(rename = "requestID")]
    pub request_id: String,
    #[serde(rename = "toolCallID")]
    pub tool_call_id: String,
    #[serde(skip_serializing_if = "is_false", default)]
    pub is_image_key: bool,
    pub model: String,
    pub created_at: DateTime<Utc>,
    pub last_access: DateTime<Utc>,
}

impl Entry {
    pub fn key(&self) -> Option<String> {
        if self.request_id.is_empty() || self.tool_call_id.is_empty() {
            return None;
        }
        Some(format!("{}:{}", self.request_id, self.tool_call_id))
    }
}

/// EntryIndex 是磁盘 Entry 的轻量指针，避免将大字段（Signature/Reasoning）长期驻留内存。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct EntryIndex {
    #[serde(rename = "requestID", default)]
    pub request_id: String,
    #[serde(rename = "toolCallID", default)]
    pub tool_call_id: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub model: String,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub created_at: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub last_access: Option<DateTime<Utc>>,
    /// Date 指向存储分片（YYYY-MM-DD）；热数据（未落盘）为 ""。
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub date: String,
}

impl EntryIndex {
    pub fn key(&self) -> Option<String> {
        if self.request_id.is_empty() || self.tool_call_id.is_empty() {
            return None;
        }
        Some(format!("{}:{}", self.request_id, self.tool_call_id))
    }
}

fn is_false(v: &bool) -> bool {
    !*v
}
