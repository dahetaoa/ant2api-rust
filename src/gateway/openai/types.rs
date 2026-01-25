use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Deserialize)]
pub struct ChatRequest {
    pub model: String,
    #[serde(default)]
    pub messages: Vec<Message>,
    #[serde(default)]
    pub stream: bool,
    #[serde(default)]
    pub temperature: Option<f64>,
    #[serde(rename = "top_p", default)]
    pub top_p: Option<f64>,
    #[serde(rename = "max_tokens", default)]
    pub max_tokens: i32,
    /// Stop 为 OpenAI 兼容字段：当前未映射到 Vertex generationConfig.stopSequences（保持历史行为）。
    #[serde(default)]
    pub stop: Vec<String>,
    #[serde(default)]
    pub tools: Vec<Tool>,
    /// ToolChoice 为 OpenAI 兼容字段：当前未实现 tool_choice 语义（保持历史行为）。
    #[serde(rename = "tool_choice", default)]
    pub tool_choice: Option<sonic_rs::Value>,
    #[serde(rename = "reasoning_effort", default)]
    pub reasoning_effort: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    pub role: String,
    #[serde(default)]
    pub content: sonic_rs::Value,
    #[serde(rename = "tool_calls", skip_serializing_if = "Vec::is_empty", default)]
    pub tool_calls: Vec<ToolCall>,
    #[serde(
        rename = "tool_call_id",
        skip_serializing_if = "String::is_empty",
        default
    )]
    pub tool_call_id: String,
    /// Name 为 OpenAI 兼容字段：当前未参与请求到 Vertex 的转换（保持历史行为）。
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub name: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub reasoning: String,
    /// 非标准但常用：用于跨轮次保留 Claude extended thinking。
    #[serde(
        rename = "reasoning_content",
        skip_serializing_if = "String::is_empty",
        default
    )]
    pub reasoning_content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Tool {
    #[serde(rename = "type")]
    pub typ: String,
    pub function: Function,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Function {
    pub name: String,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub parameters: std::collections::HashMap<String, sonic_rs::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    #[serde(skip_serializing_if = "Option::is_none", default)]
    pub index: Option<i32>,
    #[serde(default)]
    pub id: String,
    #[serde(rename = "type", default)]
    pub typ: String,
    pub function: FunctionCall,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    pub name: String,
    pub arguments: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ChatCompletion {
    pub id: String,
    pub object: String,
    pub created: i64,
    pub model: String,
    pub choices: Vec<Choice>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage: Option<Usage>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Choice {
    pub index: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<Message>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delta: Option<Delta>,
    pub finish_reason: Option<String>,
}

#[derive(Debug, Clone, Serialize)]
pub struct Delta {
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub role: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub content: String,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub tool_calls: Vec<ToolCall>,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub reasoning: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct Usage {
    pub prompt_tokens: i32,
    pub completion_tokens: i32,
    pub total_tokens: i32,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelsResponse {
    pub object: String,
    pub data: Vec<ModelItem>,
}

#[derive(Debug, Clone, Serialize)]
pub struct ModelItem {
    pub id: String,
    pub object: String,
    pub owned_by: String,
}
