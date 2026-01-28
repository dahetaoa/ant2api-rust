use crate::util::{id, model as modelutil};
use crate::vertex::types::StreamDataPart;
use serde::Serialize;

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
            // tool_use：关闭当前块，发送完整 tool_use 块
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

        let json = match typ {
            BlockType::Thinking => {
                #[derive(Serialize)]
                struct ThinkingBlock<'a> {
                    #[serde(rename = "type")]
                    typ: &'a str,
                    thinking: &'a str,
                }
                #[derive(Serialize)]
                struct Data<'a> {
                    #[serde(rename = "type")]
                    typ: &'a str,
                    index: i32,
                    content_block: ThinkingBlock<'a>,
                }
                Data {
                    typ: "content_block_start",
                    index: idx,
                    content_block: ThinkingBlock {
                        typ: "thinking",
                        thinking: "",
                    },
                }
                .to_value()
            }
            BlockType::Text => {
                #[derive(Serialize)]
                struct TextBlock<'a> {
                    #[serde(rename = "type")]
                    typ: &'a str,
                    text: &'a str,
                }
                #[derive(Serialize)]
                struct Data<'a> {
                    #[serde(rename = "type")]
                    typ: &'a str,
                    index: i32,
                    content_block: TextBlock<'a>,
                }
                Data {
                    typ: "content_block_start",
                    index: idx,
                    content_block: TextBlock { typ: "text", text: "" },
                }
                .to_value()
            }
        };

        self.collect_plain_event_for_log(json.clone());
        ("content_block_start", to_json_string(&json))
    }

    fn close_current_block(&mut self) -> Vec<(&'static str, String)> {
        let Some((idx, _)) = self.current_block.take() else {
            return Vec::new();
        };

        #[derive(Serialize)]
        struct Data<'a> {
            #[serde(rename = "type")]
            typ: &'a str,
            index: i32,
        }

        let json = Data {
            typ: "content_block_stop",
            index: idx,
        }
        .to_value();

        self.collect_plain_event_for_log(json.clone());
        vec![("content_block_stop", to_json_string(&json))]
    }

    fn emit_message_start(&mut self) -> (&'static str, String) {
        let msg_id = format!("msg_{}", self.request_id);

        let mut usage = sonic_rs::Object::new();
        usage.insert("input_tokens", self.input_tokens.max(0));
        usage.insert("output_tokens", 0);

        let mut message = sonic_rs::Object::new();
        message.insert("id", msg_id.as_str());
        message.insert("type", "message");
        message.insert("role", "assistant");
        message.insert("model", self.model.as_str());
        message.insert("content", Vec::<sonic_rs::Value>::new());
        message.insert("stop_reason", sonic_rs::Value::new());
        message.insert("stop_sequence", sonic_rs::Value::new());
        message.insert("usage", usage);

        let mut outer = sonic_rs::Object::new();
        outer.insert("type", "message_start");
        outer.insert("message", message);

        let json = outer.into_value();
        self.collect_plain_event_for_log(json.clone());
        ("message_start", to_json_string(&json))
    }

    fn emit_thinking_delta(&mut self, text: &str) -> (&'static str, String) {
        let idx = self
            .current_block
            .map(|(i, _)| i)
            .unwrap_or_else(|| 0);

        #[derive(Serialize)]
        struct Delta<'a> {
            #[serde(rename = "type")]
            typ: &'a str,
            thinking: &'a str,
        }

        #[derive(Serialize)]
        struct Data<'a> {
            #[serde(rename = "type")]
            typ: &'a str,
            index: i32,
            delta: Delta<'a>,
        }

        let json = Data {
            typ: "content_block_delta",
            index: idx,
            delta: Delta {
                typ: "thinking_delta",
                thinking: text,
            },
        }
        .to_value();

        self.collect_delta_for_log(BlockType::Thinking, idx, text);
        ("content_block_delta", to_json_string(&json))
    }

    fn emit_text_delta(&mut self, text: &str) -> (&'static str, String) {
        let idx = self
            .current_block
            .map(|(i, _)| i)
            .unwrap_or_else(|| 0);

        #[derive(Serialize)]
        struct Delta<'a> {
            #[serde(rename = "type")]
            typ: &'a str,
            text: &'a str,
        }

        #[derive(Serialize)]
        struct Data<'a> {
            #[serde(rename = "type")]
            typ: &'a str,
            index: i32,
            delta: Delta<'a>,
        }

        let json = Data {
            typ: "content_block_delta",
            index: idx,
            delta: Delta {
                typ: "text_delta",
                text,
            },
        }
        .to_value();

        self.collect_delta_for_log(BlockType::Text, idx, text);
        ("content_block_delta", to_json_string(&json))
    }

    fn emit_signature_delta(&mut self, index: i32, signature: &str) -> (&'static str, String) {
        #[derive(Serialize)]
        struct Delta<'a> {
            #[serde(rename = "type")]
            typ: &'a str,
            signature: &'a str,
        }

        #[derive(Serialize)]
        struct Data<'a> {
            #[serde(rename = "type")]
            typ: &'a str,
            index: i32,
            delta: Delta<'a>,
        }

        let json = Data {
            typ: "content_block_delta",
            index,
            delta: Delta {
                typ: "signature_delta",
                signature,
            },
        }
        .to_value();

        self.collect_plain_event_for_log(json.clone());
        ("content_block_delta", to_json_string(&json))
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
        struct Stop<'a> {
            #[serde(rename = "type")]
            typ: &'a str,
            index: i32,
        }

        let input = sonic_rs::to_value(&fc.args).unwrap_or_default();
        let start_json = Start {
            typ: "content_block_start",
            index: idx,
            content_block: ToolUse {
                typ: "tool_use",
                id: &tool_id,
                name: fc.name.as_str(),
                input,
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
        self.collect_plain_event_for_log(stop_json.clone());

        let mut events = Vec::with_capacity(2);
        events.push(("content_block_start", to_json_string(&start_json)));
        events.push(("content_block_stop", to_json_string(&stop_json)));

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

    fn emit_message_delta(&mut self, output_tokens: i32, stop_reason: &str) -> (&'static str, String) {
        let mut usage = sonic_rs::Object::new();
        usage.insert("output_tokens", output_tokens.max(0));

        let mut delta = sonic_rs::Object::new();
        delta.insert("stop_reason", stop_reason);
        delta.insert("stop_sequence", sonic_rs::Value::new());

        let mut outer = sonic_rs::Object::new();
        outer.insert("type", "message_delta");
        outer.insert("delta", delta);
        outer.insert("usage", usage);

        let json = outer.into_value();
        self.collect_plain_event_for_log(json.clone());
        ("message_delta", to_json_string(&json))
    }

    fn emit_message_stop(&mut self) -> (&'static str, String) {
        let mut outer = sonic_rs::Object::new();
        outer.insert("type", "message_stop");
        let json = outer.into_value();
        self.collect_plain_event_for_log(json.clone());
        ("message_stop", to_json_string(&json))
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

fn to_json_string(v: &sonic_rs::Value) -> String {
    // sonic_rs 的 Object 序列化不保证字段顺序稳定（每次构造的 HashBuilder 种子不同），
    // 这里转为 serde_json::Value（默认 BTreeMap）以输出确定性的 key 顺序。
    let raw = sonic_rs::to_string(v).unwrap_or_else(|_| "{}".to_string());
    match serde_json::from_str::<serde_json::Value>(&raw) {
        Ok(v) => serde_json::to_string(&v).unwrap_or(raw),
        Err(_) => raw,
    }
}

#[cfg(test)]
mod tests {
    use super::to_json_string;

    #[test]
    fn to_json_string_orders_claude_delta_fields() {
        let mut delta = sonic_rs::Object::new();
        delta.insert("type", "thinking_delta");
        delta.insert("thinking", "x");

        let mut outer = sonic_rs::Object::new();
        outer.insert("type", "content_block_delta");
        outer.insert("index", 0);
        outer.insert("delta", delta);

        let s = to_json_string(&outer.into_value());
        assert_eq!(
            s,
            r#"{"delta":{"thinking":"x","type":"thinking_delta"},"index":0,"type":"content_block_delta"}"#
        );
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
    let mut delta = sonic_rs::Object::new();
    delta.insert("type", delta_type);
    delta.insert(field, text);

    let mut outer = sonic_rs::Object::new();
    outer.insert("type", "content_block_delta");
    outer.insert("index", index);
    outer.insert("delta", delta);
    outer.into_value()
}
