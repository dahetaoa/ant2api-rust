use serde::{Deserialize, Deserializer, Serialize, Serializer};
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Request {
    pub project: String,
    pub model: String,
    pub request_id: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub request_type: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub user_agent: String,
    pub request: InnerReq,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct InnerReq {
    pub contents: Vec<Content>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub system_instruction: Option<SystemInstruction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub generation_config: Option<GenerationConfig>,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub tools: Vec<Tool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_config: Option<ToolConfig>,
    pub session_id: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Content {
    pub role: String,
    pub parts: Vec<Part>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct Part {
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function_call: Option<FunctionCall>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function_response: Option<FunctionResponse>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inline_data: Option<InlineData>,
    #[serde(skip_serializing_if = "is_false", default)]
    pub thought: bool,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub thought_signature: String,
}

fn is_false(v: &bool) -> bool {
    !*v
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FunctionCall {
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub id: String,
    pub name: String,
    pub args: HashMap<String, sonic_rs::Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FunctionResponse {
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub id: String,
    pub name: String,
    pub response: HashMap<String, sonic_rs::Value>,
}

#[derive(Debug, Clone)]
pub struct InlineData {
    pub mime_type: String,
    pub data: Base64Text,
}

impl InlineData {
    pub fn new(mime_type: impl Into<String>, data: impl Into<String>) -> Self {
        Self {
            mime_type: mime_type.into(),
            data: Base64Text::from_owned_string(data.into()),
        }
    }

    pub fn signature_key(&self) -> String {
        let s = self.data.as_str();
        if s.is_empty() {
            return String::new();
        }
        if s.len() > 50 {
            // 复制前 50 字节，避免保留整段大 base64 字符串的引用。
            return s[..50].to_string();
        }
        s.to_string()
    }
}

impl<'de> Deserialize<'de> for InlineData {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        #[derive(Deserialize)]
        #[serde(rename_all = "camelCase")]
        struct Wire {
            mime_type: String,
            data: String,
        }

        let w = Wire::deserialize(deserializer)?;
        Ok(Self {
            mime_type: w.mime_type,
            data: Base64Text::from_owned_string(w.data),
        })
    }
}

impl Serialize for InlineData {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Wire<'a> {
            mime_type: &'a str,
            #[serde(rename = "data")]
            data: &'a Base64Text,
        }

        Wire {
            mime_type: &self.mime_type,
            data: &self.data,
        }
        .serialize(serializer)
    }
}

/// 以“base64 字符串”作为 JSON 字段的数据承载体。
/// 这里不做解码/编码，仅用于显式隔离（后续可扩展为零拷贝/延迟序列化）。
#[derive(Debug, Clone, Default)]
pub struct Base64Text {
    inner: String,
}

impl Base64Text {
    pub fn as_str(&self) -> &str {
        &self.inner
    }

    fn from_owned_string(s: String) -> Self {
        Self { inner: s }
    }
}

impl Serialize for Base64Text {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.inner)
    }
}

impl<'de> Deserialize<'de> for Base64Text {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let s = String::deserialize(deserializer)?;
        Ok(Self { inner: s })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SystemInstruction {
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub role: String,
    pub parts: Vec<Part>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Tool {
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub function_declarations: Vec<FunctionDeclaration>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FunctionDeclaration {
    pub name: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub description: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub parameters: Option<HashMap<String, sonic_rs::Value>>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ToolConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub function_calling_config: Option<FunctionCallingConfig>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct FunctionCallingConfig {
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub mode: String,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub allowed_function_names: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GenerationConfig {
    #[serde(skip_serializing_if = "is_zero_i32", default)]
    pub candidate_count: i32,
    #[serde(skip_serializing_if = "Vec::is_empty", default)]
    pub stop_sequences: Vec<String>,
    #[serde(skip_serializing_if = "is_zero_i32", default)]
    pub max_output_tokens: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub temperature: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub top_p: Option<f64>,
    #[serde(skip_serializing_if = "is_zero_i32", default)]
    pub top_k: i32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub thinking_config: Option<ThinkingConfig>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub image_config: Option<ImageConfig>,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub media_resolution: String,
}

fn is_zero_i32(v: &i32) -> bool {
    *v == 0
}

#[derive(Debug, Clone, Deserialize, Default)]
#[serde(rename_all = "camelCase")]
pub struct ThinkingConfig {
    pub include_thoughts: bool,
    #[serde(default)]
    pub thinking_budget: i32,
    #[serde(default)]
    pub thinking_level: String,
}

impl Serialize for ThinkingConfig {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        // Go 版本兼容：当 thinkingLevel 为空时允许输出 thinkingBudget=0（例如 gemini-3-flash）。
        #[derive(Serialize)]
        #[serde(rename_all = "camelCase")]
        struct Wire<'a> {
            include_thoughts: bool,
            #[serde(skip_serializing_if = "Option::is_none")]
            thinking_budget: Option<i32>,
            #[serde(skip_serializing_if = "str::is_empty")]
            thinking_level: &'a str,
        }

        let thinking_budget = if self.thinking_budget != 0 || self.thinking_level.is_empty() {
            Some(self.thinking_budget)
        } else {
            None
        };

        Wire {
            include_thoughts: self.include_thoughts,
            thinking_budget,
            thinking_level: &self.thinking_level,
        }
        .serialize(serializer)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ImageConfig {
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub aspect_ratio: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub image_size: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Response {
    pub response: ResponseInner,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ResponseInner {
    pub candidates: Vec<Candidate>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub usage_metadata: Option<UsageMetadata>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Candidate {
    pub content: Content,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub finish_reason: String,
    #[serde(default)]
    pub index: i32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct UsageMetadata {
    #[serde(default)]
    pub prompt_token_count: i32,
    #[serde(default)]
    pub candidates_token_count: i32,
    #[serde(default)]
    pub total_token_count: i32,
    #[serde(skip_serializing_if = "is_zero_i32", default)]
    pub thoughts_token_count: i32,
}

impl Response {
    /// 清理大字段，帮助尽快释放内存（对应 Go 的 ClearLargeData）。
    pub fn clear_large_data(&mut self) {
        for cand in &mut self.response.candidates {
            for part in &mut cand.content.parts {
                if let Some(inline) = &mut part.inline_data {
                    inline.data = Base64Text::default();
                }
                part.text.clear();
                part.thought_signature.clear();
            }
        }
    }
}

// ===== Stream 相关轻量结构（用于 SSE chunk 反序列化）=====

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamData {
    pub response: StreamDataResponse,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamDataResponse {
    pub candidates: Vec<StreamDataCandidate>,
    #[serde(default)]
    pub usage_metadata: Option<UsageMetadata>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamDataCandidate {
    pub content: StreamDataContent,
    #[serde(default)]
    pub finish_reason: String,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamDataContent {
    pub parts: Vec<StreamDataPart>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StreamDataPart {
    #[serde(default)]
    pub text: String,
    #[serde(default)]
    pub function_call: Option<FunctionCall>,
    #[serde(default)]
    pub inline_data: Option<InlineData>,
    #[serde(default)]
    pub thought: bool,
    #[serde(default)]
    pub thought_signature: String,
}

#[derive(Debug, Clone)]
pub struct ToolCallInfo {
    pub id: String,
    pub name: String,
    pub args: HashMap<String, sonic_rs::Value>,
    pub thought_signature: String,
}

#[derive(Debug, Default, Clone)]
pub struct StreamResult {
    pub raw_chunks: Vec<sonic_rs::Value>,
    pub merged_response: Option<sonic_rs::Value>,
    pub text: String,
    pub thinking: String,
    pub finish_reason: String,
    pub usage: Option<UsageMetadata>,
    pub tool_calls: Vec<ToolCallInfo>,
    pub thought_signature: String,
}
