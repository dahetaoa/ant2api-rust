use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Claude /v1/messages 请求体。
#[derive(Debug, Clone, Deserialize)]
pub struct MessagesRequest {
    pub model: String,
    #[serde(rename = "max_tokens", default)]
    pub max_tokens: i32,
    #[serde(default)]
    pub messages: Vec<Message>,
    /// system 支持 string 或 ContentBlock 数组（按原项目行为保持为动态 JSON）。
    #[serde(default)]
    pub system: Option<sonic_rs::Value>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub temperature: Option<f64>,
    #[serde(rename = "top_p", default)]
    pub top_p: Option<f64>,
    #[serde(rename = "stop_sequences", default)]
    pub stop_sequences: Vec<String>,
    #[serde(default)]
    pub tools: Vec<Tool>,
    /// 解析但不使用（保持与 Go 版一致）。
    #[serde(rename = "tool_choice", default)]
    pub tool_choice: Option<sonic_rs::Value>,
    #[serde(default)]
    pub thinking: Option<Thinking>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Message {
    pub role: String,
    /// content 支持 string 或 ContentBlock 数组（按原项目行为保持为动态 JSON）。
    #[serde(default)]
    pub content: sonic_rs::Value,
}

/// Claude content block（请求侧）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ContentBlock {
    #[serde(rename = "type")]
    pub typ: String,
    pub text: Option<String>,
    pub thinking: Option<String>,
    pub signature: Option<String>,
    /// redacted_thinking：opaque payload（用于后端校验/解密）。
    pub data: Option<String>,
    /// tool_use
    pub id: Option<String>,
    /// tool_use
    pub name: Option<String>,
    /// tool_use
    pub input: Option<sonic_rs::Value>,
    /// tool_result
    pub tool_use_id: Option<String>,
    /// tool_result：string 或 ContentBlock 数组
    pub content: Option<sonic_rs::Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Tool {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub input_schema: HashMap<String, sonic_rs::Value>,
}

#[derive(Debug, Clone, Deserialize)]
pub struct Thinking {
    #[serde(rename = "type")]
    pub typ: String,
    pub budget: Option<i32>,
    pub budget_tokens: Option<i32>,
    #[serde(rename = "thinking_level")]
    pub level: Option<String>,
}

/// Claude /v1/messages 响应体。
#[derive(Debug, Clone, Serialize)]
pub struct MessagesResponse {
    pub id: String,
    #[serde(rename = "type")]
    pub typ: String,
    pub role: String,
    pub model: String,
    pub content: Vec<ResponseContentBlock>,
    pub stop_reason: String,
    pub stop_sequence: Option<String>,
    pub usage: Usage,
}

#[derive(Debug, Clone, Serialize)]
pub struct ResponseContentBlock {
    #[serde(rename = "type")]
    pub typ: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub signature: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub input: Option<sonic_rs::Value>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Usage {
    pub input_tokens: i32,
    pub output_tokens: i32,
}
