use crate::util::{id, model as modelutil};
use crate::vertex::types::StreamDataPart;
use serde::Serialize;

#[derive(Serialize)]
struct MessageStartUsage {
    input_tokens: i32,
    output_tokens: i32,
}

#[derive(Serialize)]
struct MessageStartMessage<'a> {
    content: Vec<sonic_rs::Value>,
    id: &'a str,
    model: &'a str,
    role: &'a str,
    stop_reason: Option<()>,
    stop_sequence: Option<()>,
    #[serde(rename = "type")]
    typ: &'a str,
    usage: MessageStartUsage,
}

#[derive(Serialize)]
struct MessageStartEvent<'a> {
    message: MessageStartMessage<'a>,
    #[serde(rename = "type")]
    typ: &'a str,
}

#[derive(Serialize)]
struct MessageDeltaDelta<'a> {
    stop_reason: &'a str,
    stop_sequence: Option<()>,
}

#[derive(Serialize)]
struct MessageDeltaUsage {
    output_tokens: i32,
}

#[derive(Serialize)]
struct MessageDeltaEvent<'a> {
    delta: MessageDeltaDelta<'a>,
    #[serde(rename = "type")]
    typ: &'a str,
    usage: MessageDeltaUsage,
}

#[derive(Serialize)]
struct MessageStopEvent<'a> {
    #[serde(rename = "type")]
    typ: &'a str,
}

#[derive(Serialize)]
struct LogDeltaInner<'a> {
    #[serde(skip_serializing_if = "Option::is_none")]
    text: Option<&'a str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    thinking: Option<&'a str>,
    #[serde(rename = "type")]
    typ: &'a str,
}

#[derive(Serialize)]
struct LogDeltaEvent<'a> {
    delta: LogDeltaInner<'a>,
    index: i32,
    #[serde(rename = "type")]
    typ: &'a str,
}

/// Claude SSE: 写入 `{"type":"error","error":{"type":"api_error","message":...}}` 事件并结束。
pub fn sse_error_events(msg: &str) -> Vec<(&'static str, String)> {
    // 保持字段顺序稳定（对齐 Claude 官方输出），避免某些严格客户端按文本匹配解析失败。
    let json = serde_json::json!({
        "type": "error",
        "error": {
            "type": "api_error",
            "message": msg,
        }
    });
    let json = serde_json::to_string(&json).unwrap_or_else(|_| "{\"type\":\"error\"}".to_string());
    vec![
        ("error", json),
        ("message_stop", "{\"type\":\"message_stop\"}".to_string()),
    ]
}

#[derive(Debug)]
pub struct SignatureSave {
    pub request_id: String,
    pub tool_call_id: String,
    pub signature: String,
    pub reasoning: String,
    pub model: String,
}

#[derive(Clone, Copy, PartialEq)]
enum BlockType {
    Thinking,
    Text,
}

/// Claude SSE 流写入器。
///
/// 核心方法 `process_part()` 是同步的，每次调用立即返回要发送的事件。
pub struct ClaudeStreamWriter {
    request_id: String,
    model: String,
    input_tokens: i32,

    // 块索引追踪
    next_index: i32,
    started: bool,

    // 当前打开的块 (索引, 类型)
    current_block: Option<(i32, BlockType)>,

    // 签名处理
    pending_signature: String,
    pending_thinking_text: String,
    signature_emitted: bool,
    enable_signature: bool, // true for Claude models only

    // 仅用于客户端流式日志（合并连续 delta，避免刷屏）
    log_enabled: bool,
    log_events: Vec<sonic_rs::Value>,
    log_pending_thinking: String,
    log_pending_text: String,
    log_pending_index: i32,
    log_pending_kind: Option<BlockType>,
}

impl ClaudeStreamWriter {
    pub fn new(request_id: String, model: String) -> Self {
        Self {
            request_id,
            model: model.clone(),
            input_tokens: 0,
            next_index: 0,
            started: false,
            current_block: None,
            pending_signature: String::new(),
            pending_thinking_text: String::new(),
            signature_emitted: false,
            enable_signature: modelutil::is_claude(&model),

            log_enabled: false,
            log_events: Vec::new(),
            log_pending_thinking: String::new(),
            log_pending_text: String::new(),
            log_pending_index: 0,
            log_pending_kind: None,
        }
    }

    pub fn set_input_tokens(&mut self, input_tokens: i32) {
        let v = input_tokens.max(0);
        if v == 0 {
            return;
        }
        // 仅在流开始前设置，避免客户端已收到 message_start 后出现“前后不一致”的错觉。
        if self.started {
            return;
        }
        self.input_tokens = v;
    }

    pub fn set_log_enabled(&mut self, enabled: bool) {
        self.log_enabled = enabled;
    }

    pub fn take_merged_events_for_log(&mut self) -> Vec<sonic_rs::Value> {
        if !self.log_enabled {
            return Vec::new();
        }
        self.flush_pending_log();
        std::mem::take(&mut self.log_events)
    }

    /// 处理一个 Part，立即返回要发送的 SSE 事件。
    pub fn process_part(
        &mut self,
        part: &StreamDataPart,
    ) -> (Vec<(&'static str, String)>, Vec<SignatureSave>) {
        let mut events = Vec::new();
        let mut saves = Vec::new();

        // 首次调用：发送 message_start
        if !self.started {
            events.push(self.emit_message_start());
            self.started = true;
        }

        // 缓存 thinking 签名（后续绑定到 tool_use）
        if self.enable_signature && part.thought && !part.thought_signature.trim().is_empty() {
            self.pending_signature = part.thought_signature.trim().to_string();
            self.signature_emitted = false;
        }

        if part.thought {
            // 确保 thinking 块打开
            if self.current_block.map(|(_, t)| t) != Some(BlockType::Thinking) {
                events.extend(self.close_current_block());
                events.push(self.open_block(BlockType::Thinking));
            }
            if !part.text.is_empty() {
                events.push(self.emit_thinking_delta(&part.text));
                self.pending_thinking_text.push_str(&part.text);
            }
        } else if !part.text.is_empty() {
            // 切换到 text 块时，先刷新 signature 到 thinking 块
            if self.current_block.map(|(_, t)| t) != Some(BlockType::Text) {
                events.extend(self.flush_signature_to_current_block());
                events.extend(self.close_current_block());
                events.push(self.open_block(BlockType::Text));
            }
            events.push(self.emit_text_delta(&part.text));
        } else if let Some(fc) = &part.function_call {
            // tool_use：关闭当前块，按 Claude SSE 规范输出 tool_use，并通过 input_json_delta 传输 input
            events.extend(self.close_current_block());
            let (tool_events, save) = self.emit_tool_use(fc, &part.thought_signature);
            events.extend(tool_events);
            if let Some(s) = save {
                saves.push(s);
            }
        }

        (events, saves)
    }

    /// 流结束时调用。
    pub fn finish(&mut self, output_tokens: i32, stop_reason: &str) -> Vec<(&'static str, String)> {
        let mut events = Vec::new();

        // 若 thinking 块仍打开，刷新 signature
        if let Some((_, BlockType::Thinking)) = self.current_block {
            events.extend(self.flush_signature_to_current_block());
        }
        events.extend(self.close_current_block());

        // message_delta
        events.push(self.emit_message_delta(output_tokens, stop_reason));
        // message_stop
        events.push(self.emit_message_stop());

        events
    }

    fn open_block(&mut self, typ: BlockType) -> (&'static str, String) {
        let idx = self.next_index;
        self.next_index += 1;
        self.current_block = Some((idx, typ));

        if typ == BlockType::Thinking {
            self.pending_thinking_text.clear();
            // 新块：允许再次输出 signature_delta（同一签名也可能需要绑定到新块）。
            self.signature_emitted = false;
        }

        let (json, out) = match typ {
            BlockType::Thinking => {
                #[derive(Serialize)]
                struct ThinkingBlock<'a> {
                    thinking: &'a str,
                    #[serde(rename = "type")]
                    typ: &'a str,
                }
                #[derive(Serialize)]
                struct Data<'a> {
                    content_block: ThinkingBlock<'a>,
                    index: i32,
                    #[serde(rename = "type")]
                    typ: &'a str,
                }
                let event = Data {
                    content_block: ThinkingBlock {
                        thinking: "",
                        typ: "thinking",
                    },
                    index: idx,
                    typ: "content_block_start",
                };
                let out = serde_json::to_string(&event)
                    .unwrap_or_else(|_| "{\"type\":\"content_block_start\"}".to_string());
                (event.to_value(), out)
            }
            BlockType::Text => {
                #[derive(Serialize)]
                struct TextBlock<'a> {
                    text: &'a str,
                    #[serde(rename = "type")]
                    typ: &'a str,
                }
                #[derive(Serialize)]
                struct Data<'a> {
                    content_block: TextBlock<'a>,
                    index: i32,
                    #[serde(rename = "type")]
                    typ: &'a str,
                }
                let event = Data {
                    content_block: TextBlock {
                        text: "",
                        typ: "text",
                    },
                    index: idx,
                    typ: "content_block_start",
                };
                let out = serde_json::to_string(&event)
                    .unwrap_or_else(|_| "{\"type\":\"content_block_start\"}".to_string());
                (event.to_value(), out)
            }
        };

        self.collect_plain_event_for_log(json);
        ("content_block_start", out)
    }

    fn close_current_block(&mut self) -> Vec<(&'static str, String)> {
        let Some((idx, _)) = self.current_block.take() else {
            return Vec::new();
        };

        #[derive(Serialize)]
        struct Data<'a> {
            index: i32,
            #[serde(rename = "type")]
            typ: &'a str,
        }

        let event = Data {
            index: idx,
            typ: "content_block_stop",
        };
        let out = serde_json::to_string(&event)
            .unwrap_or_else(|_| "{\"type\":\"content_block_stop\"}".to_string());
        let json = event.to_value();

        self.collect_plain_event_for_log(json);
        vec![("content_block_stop", out)]
    }

    fn emit_message_start(&mut self) -> (&'static str, String) {
        let msg_id = format!("msg_{}", self.request_id);

        let event = MessageStartEvent {
            message: MessageStartMessage {
                content: Vec::new(),
                id: msg_id.as_str(),
                model: self.model.as_str(),
                role: "assistant",
                stop_reason: None,
                stop_sequence: None,
                typ: "message",
                usage: MessageStartUsage {
                    input_tokens: self.input_tokens.max(0),
                    output_tokens: 0,
                },
            },
            typ: "message_start",
        };
        let out = serde_json::to_string(&event)
            .unwrap_or_else(|_| "{\"type\":\"message_start\"}".to_string());
        let json = event.to_value();
        self.collect_plain_event_for_log(json);
        ("message_start", out)
    }

    fn emit_thinking_delta(&mut self, text: &str) -> (&'static str, String) {
        let idx = self.current_block.map(|(i, _)| i).unwrap_or_else(|| 0);

        #[derive(Serialize)]
        struct Delta<'a> {
            thinking: &'a str,
            #[serde(rename = "type")]
            typ: &'a str,
        }

        #[derive(Serialize)]
        struct Data<'a> {
            delta: Delta<'a>,
            index: i32,
            #[serde(rename = "type")]
            typ: &'a str,
        }

        let event = Data {
            delta: Delta {
                thinking: text,
                typ: "thinking_delta",
            },
            index: idx,
            typ: "content_block_delta",
        };

        self.collect_delta_for_log(BlockType::Thinking, idx, text);
        let out = serde_json::to_string(&event)
            .unwrap_or_else(|_| "{\"type\":\"content_block_delta\"}".to_string());
        ("content_block_delta", out)
    }

    fn emit_text_delta(&mut self, text: &str) -> (&'static str, String) {
        let idx = self.current_block.map(|(i, _)| i).unwrap_or_else(|| 0);

        #[derive(Serialize)]
        struct Delta<'a> {
            text: &'a str,
            #[serde(rename = "type")]
            typ: &'a str,
        }

        #[derive(Serialize)]
        struct Data<'a> {
            delta: Delta<'a>,
            index: i32,
            #[serde(rename = "type")]
            typ: &'a str,
        }

        let event = Data {
            delta: Delta {
                text,
                typ: "text_delta",
            },
            index: idx,
            typ: "content_block_delta",
        };

        self.collect_delta_for_log(BlockType::Text, idx, text);
        let out = serde_json::to_string(&event)
            .unwrap_or_else(|_| "{\"type\":\"content_block_delta\"}".to_string());
        ("content_block_delta", out)
    }

    fn emit_signature_delta(&mut self, index: i32, signature: &str) -> (&'static str, String) {
        #[derive(Serialize)]
        struct Delta<'a> {
            signature: &'a str,
            #[serde(rename = "type")]
            typ: &'a str,
        }

        #[derive(Serialize)]
        struct Data<'a> {
            delta: Delta<'a>,
            index: i32,
            #[serde(rename = "type")]
            typ: &'a str,
        }

        let event = Data {
            delta: Delta {
                signature,
                typ: "signature_delta",
            },
            index,
            typ: "content_block_delta",
        };
        let out = serde_json::to_string(&event)
            .unwrap_or_else(|_| "{\"type\":\"content_block_delta\"}".to_string());
        let json = event.to_value();

        self.collect_plain_event_for_log(json);
        ("content_block_delta", out)
    }

    fn flush_signature_to_current_block(&mut self) -> Vec<(&'static str, String)> {
        let Some((idx, BlockType::Thinking)) = self.current_block else {
            return Vec::new();
        };
        if !self.enable_signature {
            return Vec::new();
        }

        let sig = self.pending_signature.trim().to_string();
        if sig.is_empty() || self.signature_emitted {
            return Vec::new();
        }

        let mut events = Vec::new();

        // Edge case：只有签名没有 thinking 文本时，注入占位符，避免客户端/后端无法重建。
        if self.pending_thinking_text.trim().is_empty() {
            let placeholder = "[missing thought text]";
            events.push(self.emit_thinking_delta(placeholder));
            self.pending_thinking_text.push_str(placeholder);
        }

        // 先 flush delta 的日志，再输出 signature_delta，保证顺序一致。
        if self.log_enabled {
            self.flush_pending_log();
        }
        events.push(self.emit_signature_delta(idx, &sig));
        self.signature_emitted = true;
        events
    }

    fn emit_tool_use(
        &mut self,
        fc: &crate::vertex::types::FunctionCall,
        part_signature: &str,
    ) -> (Vec<(&'static str, String)>, Option<SignatureSave>) {
        let idx = self.next_index;
        self.next_index += 1;

        let tool_id = if fc.id.trim().is_empty() {
            format!("toolu_{}", id::request_id())
        } else {
            fc.id.trim().to_string()
        };

        #[derive(Serialize)]
        struct ToolUse<'a> {
            #[serde(rename = "type")]
            typ: &'a str,
            id: &'a str,
            name: &'a str,
            input: sonic_rs::Value,
        }

        #[derive(Serialize)]
        struct Start<'a> {
            #[serde(rename = "type")]
            typ: &'a str,
            index: i32,
            content_block: ToolUse<'a>,
        }

        #[derive(Serialize)]
        struct InputJsonDelta<'a> {
            #[serde(rename = "type")]
            typ: &'a str,
            partial_json: &'a str,
        }

        #[derive(Serialize)]
        struct Delta<'a> {
            #[serde(rename = "type")]
            typ: &'a str,
            index: i32,
            delta: InputJsonDelta<'a>,
        }

        #[derive(Serialize)]
        struct Stop<'a> {
            #[serde(rename = "type")]
            typ: &'a str,
            index: i32,
        }

        // Claude 官方 SSE：content_block_start 的 tool_use.input 为空对象，真实 input 通过 input_json_delta 流式传输。
        let input_json = serde_json::to_value(&fc.args)
            .ok()
            .and_then(|v| serde_json::to_string(&v).ok())
            .unwrap_or_else(|| "{}".to_string());
        let empty_input = sonic_rs::Object::new().into_value();

        #[derive(Serialize)]
        struct ToolUseOrdered<'a> {
            id: &'a str,
            input: sonic_rs::Value,
            name: &'a str,
            #[serde(rename = "type")]
            typ: &'a str,
        }

        #[derive(Serialize)]
        struct StartOrdered<'a> {
            content_block: ToolUseOrdered<'a>,
            index: i32,
            #[serde(rename = "type")]
            typ: &'a str,
        }

        #[derive(Serialize)]
        struct InputJsonDeltaOrdered<'a> {
            partial_json: &'a str,
            #[serde(rename = "type")]
            typ: &'a str,
        }

        #[derive(Serialize)]
        struct DeltaOrdered<'a> {
            delta: InputJsonDeltaOrdered<'a>,
            index: i32,
            #[serde(rename = "type")]
            typ: &'a str,
        }

        #[derive(Serialize)]
        struct StopOrdered<'a> {
            index: i32,
            #[serde(rename = "type")]
            typ: &'a str,
        }

        let start_json = Start {
            typ: "content_block_start",
            index: idx,
            content_block: ToolUse {
                typ: "tool_use",
                id: &tool_id,
                name: fc.name.as_str(),
                input: empty_input,
            },
        }
        .to_value();
        let delta_json = Delta {
            typ: "content_block_delta",
            index: idx,
            delta: InputJsonDelta {
                typ: "input_json_delta",
                partial_json: input_json.as_str(),
            },
        }
        .to_value();
        let stop_json = Stop {
            typ: "content_block_stop",
            index: idx,
        }
        .to_value();

        // 事件优先：先把 tool_use 发出去，签名保存交给 handler 在后面异步处理。
        self.collect_plain_event_for_log(start_json.clone());
        self.collect_plain_event_for_log(delta_json.clone());
        self.collect_plain_event_for_log(stop_json.clone());

        let mut events = Vec::with_capacity(3);
        let start_out = serde_json::to_string(&StartOrdered {
            content_block: ToolUseOrdered {
                id: &tool_id,
                input: sonic_rs::Object::new().into_value(),
                name: fc.name.as_str(),
                typ: "tool_use",
            },
            index: idx,
            typ: "content_block_start",
        })
        .unwrap_or_else(|_| "{\"type\":\"content_block_start\"}".to_string());
        let delta_out = serde_json::to_string(&DeltaOrdered {
            delta: InputJsonDeltaOrdered {
                partial_json: input_json.as_str(),
                typ: "input_json_delta",
            },
            index: idx,
            typ: "content_block_delta",
        })
        .unwrap_or_else(|_| "{\"type\":\"content_block_delta\"}".to_string());
        let stop_out = serde_json::to_string(&StopOrdered {
            index: idx,
            typ: "content_block_stop",
        })
        .unwrap_or_else(|_| "{\"type\":\"content_block_stop\"}".to_string());
        events.push(("content_block_start", start_out));
        events.push(("content_block_delta", delta_out));
        events.push(("content_block_stop", stop_out));

        let part_sig = part_signature.trim();
        let mut signature_to_save = String::new();
        let mut consumed_pending = false;

        if !part_sig.is_empty() {
            signature_to_save = part_sig.to_string();
        } else if self.enable_signature && !self.pending_signature.trim().is_empty() {
            signature_to_save = self.pending_signature.trim().to_string();
            consumed_pending = true;
        }

        let save = if signature_to_save.is_empty() {
            None
        } else {
            let mut reasoning = self.pending_thinking_text.trim().to_string();
            if reasoning.is_empty() {
                reasoning = "[missing thought text]".to_string();
            }
            Some(SignatureSave {
                request_id: self.request_id.clone(),
                tool_call_id: tool_id.clone(),
                signature: signature_to_save,
                reasoning,
                model: self.model.clone(),
            })
        };

        // Edge case：多 tool_use 时，只把 thinking 的签名绑定到第一个 tool_use。
        if consumed_pending {
            self.pending_signature.clear();
        }

        (events, save)
    }

    fn emit_message_delta(
        &mut self,
        output_tokens: i32,
        stop_reason: &str,
    ) -> (&'static str, String) {
        let event = MessageDeltaEvent {
            delta: MessageDeltaDelta {
                stop_reason,
                stop_sequence: None,
            },
            typ: "message_delta",
            usage: MessageDeltaUsage {
                output_tokens: output_tokens.max(0),
            },
        };
        let out = serde_json::to_string(&event)
            .unwrap_or_else(|_| "{\"type\":\"message_delta\"}".to_string());
        let json = event.to_value();
        self.collect_plain_event_for_log(json);
        ("message_delta", out)
    }

    fn emit_message_stop(&mut self) -> (&'static str, String) {
        let event = MessageStopEvent {
            typ: "message_stop",
        };
        let out = serde_json::to_string(&event)
            .unwrap_or_else(|_| "{\"type\":\"message_stop\"}".to_string());
        let json = event.to_value();
        self.collect_plain_event_for_log(json);
        ("message_stop", out)
    }

    fn collect_delta_for_log(&mut self, kind: BlockType, index: i32, text: &str) {
        if !self.log_enabled || text.is_empty() {
            return;
        }
        if self.log_pending_kind != Some(kind) || self.log_pending_index != index {
            self.flush_pending_log();
            self.log_pending_kind = Some(kind);
            self.log_pending_index = index;
        }
        match kind {
            BlockType::Thinking => self.log_pending_thinking.push_str(text),
            BlockType::Text => self.log_pending_text.push_str(text),
        }
    }

    fn collect_plain_event_for_log(&mut self, v: sonic_rs::Value) {
        if !self.log_enabled {
            return;
        }
        self.flush_pending_log();
        self.log_events.push(v);
    }

    fn flush_pending_log(&mut self) {
        if !self.log_enabled {
            return;
        }

        if let Some(kind) = self.log_pending_kind {
            let idx = self.log_pending_index;
            match kind {
                BlockType::Thinking => {
                    if self.log_pending_thinking.is_empty() {
                        self.log_pending_kind = None;
                        return;
                    }
                    let thinking = std::mem::take(&mut self.log_pending_thinking);
                    let json = build_delta_value(idx, "thinking_delta", "thinking", &thinking);
                    self.log_events.push(json);
                }
                BlockType::Text => {
                    if self.log_pending_text.is_empty() {
                        self.log_pending_kind = None;
                        return;
                    }
                    let text = std::mem::take(&mut self.log_pending_text);
                    let json = build_delta_value(idx, "text_delta", "text", &text);
                    self.log_events.push(json);
                }
            }
            self.log_pending_kind = None;
        }
    }
}

trait ToValue {
    fn to_value(self) -> sonic_rs::Value;
}

impl<T> ToValue for T
where
    T: Serialize,
{
    fn to_value(self) -> sonic_rs::Value {
        sonic_rs::to_value(&self).unwrap_or_default()
    }
}

fn build_delta_value(index: i32, delta_type: &str, field: &str, text: &str) -> sonic_rs::Value {
    let (thinking, text) = match field {
        "thinking" => (Some(text), None),
        "text" => (None, Some(text)),
        _ => (None, None),
    };
    LogDeltaEvent {
        delta: LogDeltaInner {
            text,
            thinking,
            typ: delta_type,
        },
        index,
        typ: "content_block_delta",
    }
    .to_value()
}

#[cfg(test)]
mod tests {
    #[test]
    fn thinking_delta_sse_field_order_stable() {
        use super::ClaudeStreamWriter;

        let mut writer =
            ClaudeStreamWriter::new("req_test".to_string(), "claude-3-opus-20240229".to_string());
        let (_event, s) = writer.emit_thinking_delta("x");
        assert_eq!(
            s,
            r#"{"delta":{"thinking":"x","type":"thinking_delta"},"index":0,"type":"content_block_delta"}"#
        );
    }

    #[test]
    fn tool_use_sse_sends_input_via_input_json_delta() {
        use super::ClaudeStreamWriter;
        use crate::vertex::types::{FunctionCall, StreamDataPart};
        use std::collections::HashMap;

        let mut args = HashMap::new();
        args.insert("command".to_string(), sonic_rs::to_value("ls -la").unwrap());
        args.insert(
            "description".to_string(),
            sonic_rs::to_value("列出当前目录下的所有文件").unwrap(),
        );

        let part = StreamDataPart {
            text: String::new(),
            function_call: Some(FunctionCall {
                id: "toolu_test".to_string(),
                name: "Bash".to_string(),
                args,
            }),
            inline_data: None,
            thought: false,
            thought_signature: String::new(),
        };

        let mut writer =
            ClaudeStreamWriter::new("req_test".to_string(), "claude-3-opus-20240229".to_string());
        let (events, _saves) = writer.process_part(&part);

        assert_eq!(events.len(), 4);
        assert_eq!(events[0].0, "message_start");
        assert_eq!(events[1].0, "content_block_start");
        assert_eq!(events[2].0, "content_block_delta");
        assert_eq!(events[3].0, "content_block_stop");

        let start = serde_json::from_str::<serde_json::Value>(&events[1].1).unwrap();
        assert_eq!(start["type"], "content_block_start");
        assert_eq!(start["content_block"]["type"], "tool_use");
        assert_eq!(start["content_block"]["name"], "Bash");
        assert_eq!(start["content_block"]["id"], "toolu_test");
        assert!(start["content_block"]["input"].is_object());
        assert_eq!(
            start["content_block"]["input"].as_object().unwrap().len(),
            0
        );

        let delta = serde_json::from_str::<serde_json::Value>(&events[2].1).unwrap();
        assert_eq!(delta["type"], "content_block_delta");
        assert_eq!(delta["delta"]["type"], "input_json_delta");
        let partial = delta["delta"]["partial_json"].as_str().unwrap();
        assert!(partial.contains("ls -la"));
        assert!(partial.contains("列出当前目录下的所有文件"));
    }
}
