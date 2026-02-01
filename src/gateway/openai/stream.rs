use super::types::{ChatCompletion, Choice, Delta, ToolCall, Usage};
use crate::util::{id, model as modelutil};
use crate::vertex::types::StreamDataPart;
use chrono::Utc;

/// OpenAI SSE: 写入 `{"error":{"message":...,"type":"server_error"}}` 事件并结束。
pub fn sse_error_events(msg: &str) -> Vec<String> {
    let encoded = sonic_rs::to_string(msg).unwrap_or_else(|_| "\"\"".to_string());
    let json = format!("{{\"error\":{{\"message\":{encoded},\"type\":\"server_error\"}}}}");
    vec![json, "[DONE]".to_string()]
}

fn image_signature_key(
    inline: &crate::vertex::types::InlineData,
    is_gemini_pro_image: bool,
) -> String {
    if !is_gemini_pro_image {
        return inline.signature_key();
    }

    let s = inline.data.as_str();
    if s.is_empty() {
        return String::new();
    }
    let n = s.len().min(100);
    s[..n].to_string()
}

#[derive(Debug)]
pub struct SignatureSave {
    pub request_id: String,
    pub tool_call_id: String,
    pub is_image_key: bool,
    pub signature: String,
    pub reasoning: String,
    pub model: String,
}

pub struct StreamWriter {
    id: String,
    created: i64,
    model: String,
    request_id: String,

    sent_role: bool,
    content_buf: Vec<u8>,
    reasoning_buf: Vec<u8>,

    pending_reasoning: String,
    tool_calls: Vec<ToolCall>,

    pending_sig: String,
    is_claude_thinking: bool,
    is_gemini_pro_image: bool,

    log_enabled: bool,
    log_events: Vec<sonic_rs::Value>,
    log_pending_content: String,
    log_pending_reasoning: String,
}

impl StreamWriter {
    pub fn new(
        id: String,
        created: i64,
        model: String,
        request_id: String,
        log_enabled: bool,
    ) -> Self {
        let is_claude_thinking = modelutil::is_claude_thinking(&model);
        let is_gemini_pro_image = modelutil::is_gemini_pro_image(&model);
        Self {
            id,
            created,
            model,
            request_id,
            sent_role: false,
            content_buf: Vec::new(),
            reasoning_buf: Vec::new(),
            pending_reasoning: String::new(),
            tool_calls: Vec::new(),
            pending_sig: String::new(),
            is_claude_thinking,
            is_gemini_pro_image,

            log_enabled,
            log_events: Vec::new(),
            log_pending_content: String::new(),
            log_pending_reasoning: String::new(),
        }
    }

    pub fn process_part(&mut self, part: &StreamDataPart) -> (Vec<String>, Vec<SignatureSave>) {
        let mut saves: Vec<SignatureSave> = Vec::new();

        // Claude thinking：把签名绑定到后续第一个 tool call。
        if self.is_claude_thinking && part.thought && !part.thought_signature.is_empty() {
            self.pending_sig = part.thought_signature.clone();
        }

        if part.thought {
            self.pending_reasoning.push_str(&part.text);
            return (self.write_reasoning(&part.text), saves);
        }

        if !part.text.is_empty() {
            return (self.write_content(&part.text), saves);
        }

        if let Some(inline) = &part.inline_data {
            let image_key = image_signature_key(inline, self.is_gemini_pro_image);
            if !part.thought_signature.is_empty() && !image_key.is_empty() {
                saves.push(SignatureSave {
                    request_id: self.request_id.clone(),
                    tool_call_id: image_key,
                    is_image_key: true,
                    signature: part.thought_signature.clone(),
                    reasoning: std::mem::take(&mut self.pending_reasoning),
                    model: self.model.clone(),
                });
            }

            let data = inline.data.as_str();
            let mut sb = String::with_capacity(10 + inline.mime_type.len() + data.len());
            sb.push_str("![image](data:");
            sb.push_str(&inline.mime_type);
            sb.push_str(";base64,");
            sb.push_str(data);
            sb.push(')');
            return (self.write_content(&sb), saves);
        }

        if let Some(fc) = &part.function_call {
            let tool_call_id = if fc.id.is_empty() {
                id::tool_call_id()
            } else {
                fc.id.clone()
            };

            let mut signature_to_save: Option<String> = None;
            if self.is_claude_thinking {
                if !self.pending_sig.is_empty() {
                    signature_to_save = Some(std::mem::take(&mut self.pending_sig));
                } else if !part.thought_signature.is_empty() {
                    signature_to_save = Some(part.thought_signature.clone());
                }
            } else if !part.thought_signature.is_empty() {
                signature_to_save = Some(part.thought_signature.clone());
            }

            if let Some(sig) = signature_to_save {
                saves.push(SignatureSave {
                    request_id: self.request_id.clone(),
                    tool_call_id: tool_call_id.clone(),
                    is_image_key: false,
                    signature: sig,
                    reasoning: std::mem::take(&mut self.pending_reasoning),
                    model: self.model.clone(),
                });
            }

            let args = if fc.args.is_empty() {
                "{}".to_string()
            } else {
                sonic_rs::to_string(&fc.args).unwrap_or_else(|_| "{}".to_string())
            };

            let idx = self.tool_calls.len() as i32;
            self.tool_calls.push(ToolCall {
                index: Some(idx),
                id: tool_call_id,
                typ: "function".to_string(),
                function: super::types::FunctionCall {
                    name: fc.name.clone(),
                    arguments: args,
                },
            });
        }

        (Vec::new(), saves)
    }

    pub fn flush_tool_calls(&mut self) -> Vec<String> {
        if self.tool_calls.is_empty() {
            return Vec::new();
        }
        let calls = std::mem::take(&mut self.tool_calls);
        self.write_tool_calls(&calls)
    }

    pub fn finish_events(&mut self, finish_reason: &str, usage: Option<Usage>) -> Vec<String> {
        let mut out = Vec::new();
        out.extend(self.write_role());
        out.extend(self.write_chunk(
            Delta {
                role: String::new(),
                content: String::new(),
                tool_calls: Vec::new(),
                reasoning: String::new(),
            },
            Some(finish_reason.to_string()),
            usage,
        ));
        out.push("[DONE]".to_string());
        out
    }

    fn write_role(&mut self) -> Vec<String> {
        if self.sent_role {
            return Vec::new();
        }
        self.sent_role = true;
        self.write_chunk(
            Delta {
                role: "assistant".to_string(),
                content: String::new(),
                tool_calls: Vec::new(),
                reasoning: String::new(),
            },
            None,
            None,
        )
    }

    fn write_content(&mut self, s: &str) -> Vec<String> {
        let mut out = Vec::new();
        out.extend(self.write_role());
        self.content_buf.extend_from_slice(s.as_bytes());
        if let Some(valid) = take_valid_utf8(&mut self.content_buf)
            && !valid.is_empty()
        {
            out.extend(self.write_chunk(
                Delta {
                    role: String::new(),
                    content: valid,
                    tool_calls: Vec::new(),
                    reasoning: String::new(),
                },
                None,
                None,
            ));
        }
        out
    }

    fn write_reasoning(&mut self, s: &str) -> Vec<String> {
        let mut out = Vec::new();
        out.extend(self.write_role());
        self.reasoning_buf.extend_from_slice(s.as_bytes());
        if let Some(valid) = take_valid_utf8(&mut self.reasoning_buf)
            && !valid.is_empty()
        {
            out.extend(self.write_chunk(
                Delta {
                    role: String::new(),
                    content: String::new(),
                    tool_calls: Vec::new(),
                    reasoning: valid,
                },
                None,
                None,
            ));
        }
        out
    }

    fn write_tool_calls(&mut self, calls: &[ToolCall]) -> Vec<String> {
        let mut out = Vec::new();
        out.extend(self.write_role());
        out.extend(self.write_chunk(
            Delta {
                role: String::new(),
                content: String::new(),
                tool_calls: calls.to_vec(),
                reasoning: String::new(),
            },
            None,
            None,
        ));
        out
    }

    fn write_chunk(
        &mut self,
        delta: Delta,
        finish_reason: Option<String>,
        usage: Option<Usage>,
    ) -> Vec<String> {
        let chunk = ChatCompletion {
            id: self.id.clone(),
            object: "chat.completion.chunk".to_string(),
            created: self.created,
            model: self.model.clone(),
            choices: vec![Choice {
                index: 0,
                message: None,
                delta: Some(delta),
                finish_reason,
            }],
            usage,
        };

        if self.log_enabled {
            self.collect_chunk_for_log(&chunk);
        }

        match sonic_rs::to_string(&chunk) {
            Ok(s) => vec![s],
            Err(_) => Vec::new(),
        }
    }

    pub fn take_merged_events_for_log(&mut self) -> Vec<sonic_rs::Value> {
        if !self.log_enabled {
            return Vec::new();
        }
        self.flush_pending_log();
        std::mem::take(&mut self.log_events)
    }

    fn flush_pending_log(&mut self) {
        if self.log_pending_reasoning.is_empty() && self.log_pending_content.is_empty() {
            return;
        }

        // Go 版顺序：先 reasoning，后 content
        if !self.log_pending_reasoning.is_empty() {
            let reasoning = std::mem::take(&mut self.log_pending_reasoning);
            if let Ok(v) = sonic_rs::to_value(&ChatCompletion {
                id: self.id.clone(),
                object: "chat.completion.chunk".to_string(),
                created: self.created,
                model: self.model.clone(),
                choices: vec![Choice {
                    index: 0,
                    message: None,
                    delta: Some(Delta {
                        role: String::new(),
                        content: String::new(),
                        tool_calls: Vec::new(),
                        reasoning,
                    }),
                    finish_reason: None,
                }],
                usage: None,
            }) {
                self.log_events.push(v);
            }
        }

        if !self.log_pending_content.is_empty() {
            let content = std::mem::take(&mut self.log_pending_content);
            if let Ok(v) = sonic_rs::to_value(&ChatCompletion {
                id: self.id.clone(),
                object: "chat.completion.chunk".to_string(),
                created: self.created,
                model: self.model.clone(),
                choices: vec![Choice {
                    index: 0,
                    message: None,
                    delta: Some(Delta {
                        role: String::new(),
                        content,
                        tool_calls: Vec::new(),
                        reasoning: String::new(),
                    }),
                    finish_reason: None,
                }],
                usage: None,
            }) {
                self.log_events.push(v);
            }
        }
    }

    fn collect_chunk_for_log(&mut self, chunk: &ChatCompletion) {
        let Some(choice) = chunk.choices.first() else {
            self.flush_pending_log();
            if let Ok(v) = sonic_rs::to_value(chunk) {
                self.log_events.push(v);
            }
            return;
        };

        let Some(delta) = choice.delta.as_ref() else {
            self.flush_pending_log();
            if let Ok(v) = sonic_rs::to_value(chunk) {
                self.log_events.push(v);
            }
            return;
        };

        if !delta.content.is_empty() {
            if !self.log_pending_reasoning.is_empty() {
                self.flush_pending_log();
            }
            self.log_pending_content.push_str(&delta.content);
            return;
        }

        if !delta.reasoning.is_empty() {
            if !self.log_pending_content.is_empty() {
                self.flush_pending_log();
            }
            self.log_pending_reasoning.push_str(&delta.reasoning);
            return;
        }

        self.flush_pending_log();
        if let Ok(v) = sonic_rs::to_value(chunk) {
            self.log_events.push(v);
        }
    }
}

fn take_valid_utf8(buf: &mut Vec<u8>) -> Option<String> {
    if buf.is_empty() {
        return None;
    }
    match std::str::from_utf8(buf) {
        Ok(s) => {
            let out = s.to_string();
            buf.clear();
            Some(out)
        }
        Err(e) => {
            let valid_up_to = e.valid_up_to();
            if valid_up_to == 0 {
                return None;
            }
            let rest = buf.split_off(valid_up_to);
            let valid_bytes = std::mem::take(buf);
            *buf = rest;
            Some(String::from_utf8_lossy(&valid_bytes).to_string())
        }
    }
}

pub fn now_unix() -> i64 {
    Utc::now().timestamp()
}
