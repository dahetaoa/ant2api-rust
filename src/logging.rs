use axum::http::HeaderMap;
use sonic_rs::prelude::*;
use std::borrow::Cow;
use std::time::Duration;

/// 日志等级（对齐 Go 版本行为，并扩展 raw high）：
/// - off：不输出客户端/后端的详细请求响应
/// - low：输出客户端请求/响应（格式化/脱敏）
/// - medium：输出客户端 + 后端请求/响应（格式化/脱敏；等同于旧 high）
/// - high：输出客户端 + 后端请求/响应（完全原始：不折叠/不转换/不格式化；流式逐条输出）
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum LogLevel {
    Off = 0,
    Low = 1,
    Medium = 2,
    High = 3,
}

impl LogLevel {
    pub fn parse(debug: &str) -> Self {
        match debug.trim().to_lowercase().as_str() {
            "low" | "client" => Self::Low,
            "medium" | "backend" => Self::Medium,
            "high" | "all" | "raw" => Self::High,
            _ => Self::Off,
        }
    }

    pub fn client_enabled(self) -> bool {
        self >= Self::Low
    }

    pub fn backend_enabled(self) -> bool {
        self >= Self::Medium
    }

    /// 是否启用“完全原始”日志（high）。
    pub fn raw_enabled(self) -> bool {
        self >= Self::High
    }
}

pub fn format_duration_ms(d: Duration) -> i64 {
    d.as_millis().min(i64::MAX as u128) as i64
}

pub fn backend_response_divider_raw() {
    tracing::info!("\n---------------------- 后端响应（RAW） ----------------------");
}

pub fn client_response_divider_raw() {
    tracing::info!("\n---------------------- 客户端响应（RAW） ----------------------");
}

pub fn client_request_raw(method: &str, path: &str, headers: &HeaderMap, body: &[u8]) {
    tracing::info!(
        "\n================== 客户端请求（RAW） ==================\n[客户端请求] {method} {path}\n[客户端请求头]\n{}\n[客户端请求体]\n{}\n=========================================================",
        format_headers_raw(headers),
        format_bytes_raw(body),
    );
}

pub fn client_response_raw(status: u16, duration: Duration, body: &[u8]) {
    client_response_divider_raw();
    tracing::info!(
        "\n================== 客户端响应（RAW） ==================\n[客户端响应] {} {}ms\n{}\n=========================================================",
        status,
        format_duration_ms(duration),
        format_bytes_raw(body),
    );
}

pub fn client_stream_event_raw(event_name: Option<&str>, data: &str) {
    match event_name {
        Some(name) => tracing::info!("event: {}\ndata: {}\n", name, data),
        None => tracing::info!("data: {}\n", data),
    }
}

pub fn client_request(method: &str, path: &str, headers: &HeaderMap, body: &[u8]) {
    tracing::info!(
        "\n===================== 客户端请求 ======================\n[客户端请求] {method} {path}\n[客户端请求头]\n{}\n{}\n=========================================================",
        format_headers(headers, HeaderRedact::Client),
        format_body_bytes(body)
    );
}

pub fn client_response(status: u16, duration: Duration, body: Option<&sonic_rs::Value>) {
    tracing::info!(
        "\n===================== 客户端响应 ======================\n[客户端响应] {} {}ms\n{}\n==========================================================",
        status,
        format_duration_ms(duration),
        body.map(format_body_value).unwrap_or_default()
    );
}

pub fn client_stream_response(status: u16, duration: Duration, merged_events: &[sonic_rs::Value]) {
    let body = sonic_rs::Value::from(merged_events.to_vec());
    tracing::info!(
        "\n=================== 客户端流式响应 =======================\n[客户端流式] {} {}ms\n{}\n==========================================================",
        status,
        format_duration_ms(duration),
        format_body_value(&body)
    );
}

pub fn backend_request(method: &str, url: &str, headers: &HeaderMap, body: &[u8]) {
    tracing::info!(
        "\n====================== 后端请求 ========================\n[后端请求] {method} {url}\n[后端请求头]\n{}\n{}\n==========================================================",
        format_headers(headers, HeaderRedact::Backend),
        format_body_bytes(body)
    );
}

pub fn backend_request_raw(method: &str, url: &str, headers: &HeaderMap, body: &[u8]) {
    tracing::info!(
        "\n=================== 后端请求（RAW） ===================\n[后端请求] {method} {url}\n[后端请求头]\n{}\n[后端请求体]\n{}\n=========================================================",
        format_headers_raw(headers),
        format_bytes_raw(body),
    );
}

pub fn backend_response(status: u16, duration: Duration, body: &[u8]) {
    tracing::info!(
        "\n====================== 后端响应 ========================\n[后端响应] {} {}ms\n{}\n==========================================================",
        status,
        format_duration_ms(duration),
        format_body_bytes(body)
    );
}

pub fn backend_response_raw(status: u16, duration: Duration, body: &[u8]) {
    backend_response_divider_raw();
    tracing::info!(
        "\n=================== 后端响应（RAW） ===================\n[后端响应] {} {}ms\n{}\n=========================================================",
        status,
        format_duration_ms(duration),
        format_bytes_raw(body),
    );
}

pub fn backend_stream_line_raw(line: &[u8]) {
    // 不做任何 JSON 解析/格式化；尽量原样输出（仅在非 UTF-8 时降级为 lossy）。
    tracing::info!("{}", String::from_utf8_lossy(line));
}

pub fn backend_stream_response(
    status: u16,
    duration: Duration,
    merged_response: Option<&sonic_rs::Value>,
) {
    tracing::info!(
        "\n==================== 后端流式响应 =======================\n[后端流式] {} {}ms\n{}\n==========================================================",
        status,
        format_duration_ms(duration),
        merged_response.map(format_body_value).unwrap_or_default()
    );
}

enum HeaderRedact {
    Client,
    Backend,
}

fn format_headers(headers: &HeaderMap, kind: HeaderRedact) -> String {
    let mut obj = sonic_rs::Object::new();

    for (name, value) in headers.iter() {
        let key = name.as_str();
        let key_lc = key.to_lowercase();

        let redacted = match kind {
            HeaderRedact::Client => {
                key_lc == "authorization"
                    || key_lc == "proxy-authorization"
                    || key_lc == "x-api-key"
                    || key_lc == "cookie"
            }
            HeaderRedact::Backend => key_lc == "authorization" || key_lc == "proxy-authorization",
        };

        let v = if redacted {
            sonic_rs::Value::from("Bearer ***")
        } else {
            match value.to_str() {
                Ok(s) => sonic_rs::Value::from(s),
                Err(_) => sonic_rs::Value::from("<binary>"),
            }
        };

        // HeaderMap 可能存在同名多值，统一用数组输出，避免信息丢失。
        if let Some(existing) = obj.get(&key).and_then(|v| v.as_array()) {
            let mut arr = existing.to_vec();
            arr.push(v);
            obj.insert(key, arr);
        } else {
            obj.insert(key, vec![v]);
        }
    }

    format_body_value(&obj.into_value())
}

fn format_body_value(v: &sonic_rs::Value) -> String {
    let sanitized = sanitize_json_for_log(v, false);
    match sonic_rs::to_string_pretty(&sanitized) {
        Ok(s) => s,
        Err(_) => sanitized.to_string(),
    }
}

fn format_body_bytes(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }

    // 极端大包：避免为了日志反序列化/格式化而产生巨额内存与 CPU 开销。
    const MAX_PARSE_BYTES: usize = 2 * 1024 * 1024;
    const HEAD_TAIL: usize = 16 * 1024;

    if bytes.len() > MAX_PARSE_BYTES {
        let head_len = bytes.len().min(HEAD_TAIL);
        let tail_len = bytes.len().saturating_sub(head_len).min(HEAD_TAIL);
        let head = &bytes[..head_len];
        let tail = &bytes[bytes.len() - tail_len..];
        let head_s = String::from_utf8_lossy(head);
        let tail_s = String::from_utf8_lossy(tail);
        return format!(
            "(body too large: {} bytes, showing head/tail)\n--- head ---\n{}\n--- tail ---\n{}",
            bytes.len(),
            truncate_text_for_log(&head_s),
            truncate_text_for_log(&tail_s)
        );
    }

    match sonic_rs::from_slice::<sonic_rs::Value>(bytes) {
        Ok(v) => format_body_value(&v),
        Err(_) => truncate_text_for_log(&String::from_utf8_lossy(bytes)),
    }
}

fn format_headers_raw(headers: &HeaderMap) -> String {
    let mut out = String::new();
    for (name, value) in headers.iter() {
        let key = name.as_str();
        let val = value.to_str().unwrap_or("<non-utf8>");
        out.push_str(key);
        out.push_str(": ");
        out.push_str(val);
        out.push('\n');
    }
    out
}

fn format_bytes_raw(bytes: &[u8]) -> String {
    if bytes.is_empty() {
        return String::new();
    }
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => String::from_utf8_lossy(bytes).to_string(),
    }
}

fn truncate_text_for_log(s: &str) -> String {
    const MAX_CHARS: usize = 32 * 1024;
    if s.chars().count() <= MAX_CHARS {
        return s.to_string();
    }
    let mut out = String::with_capacity(MAX_CHARS + 64);
    for (i, ch) in s.chars().enumerate() {
        if i >= MAX_CHARS {
            break;
        }
        out.push(ch);
    }
    out.push_str("...[TRUNCATED]");
    out
}

fn sanitize_json_for_log(v: &sonic_rs::Value, in_inline_data: bool) -> sonic_rs::Value {
    // 递归走 Value，避免先反序列化到强类型结构体导致字段丢失。
    if let Some(obj) = v.as_object() {
        let mut out = sonic_rs::Object::new();

        let is_base64_ctx = obj
            .get(&"type")
            .and_then(|t| t.as_str())
            .map(|t| t.trim() == "base64")
            .unwrap_or(false);

        for (key, child) in obj.iter() {
            let sanitized = match key {
                "inlineData" => sanitize_json_for_log(child, true),
                "data" if in_inline_data || is_base64_ctx => sanitize_value_force_base64(child),
                "url" => sanitize_url_field(child),
                "content" => sanitize_content_field(child),
                _ => sanitize_json_for_log(child, in_inline_data),
            };
            out.insert(key, sanitized);
        }
        return out.into_value();
    }

    if let Some(arr) = v.as_array() {
        let mut out = Vec::with_capacity(arr.len());
        for item in arr {
            out.push(sanitize_json_for_log(item, in_inline_data));
        }
        return sonic_rs::Value::from(out);
    }

    if let Some(s) = v.as_str() {
        if s.contains(";base64,") && s.len() > 100 {
            return sonic_rs::Value::from(truncate_base64_maybe(s, true).as_ref());
        }
        return sonic_rs::Value::from(truncate_base64_maybe(s, in_inline_data).as_ref());
    }

    v.to_owned()
}

fn sanitize_value_force_base64(v: &sonic_rs::Value) -> sonic_rs::Value {
    if let Some(s) = v.as_str() {
        return sonic_rs::Value::from(truncate_base64_maybe(s, true).as_ref());
    }
    sanitize_json_for_log(v, false)
}

fn sanitize_url_field(v: &sonic_rs::Value) -> sonic_rs::Value {
    if let Some(s) = v.as_str()
        && s.contains(";base64,")
        && s.len() > 100
    {
        return sonic_rs::Value::from(truncate_base64_maybe(s, true).as_ref());
    }
    sanitize_json_for_log(v, false)
}

fn sanitize_content_field(v: &sonic_rs::Value) -> sonic_rs::Value {
    if let Some(s) = v.as_str()
        && s.contains("![image](data:")
        && s.contains(";base64,")
        && s.len() > 100
    {
        return sonic_rs::Value::from(truncate_base64_maybe(s, true).as_ref());
    }
    sanitize_json_for_log(v, false)
}

fn truncate_base64_maybe(s: &str, force: bool) -> Cow<'_, str> {
    if s.len() <= 100 {
        return Cow::Borrowed(s);
    }

    const KEEP: usize = 20;

    if let Some(idx) = s.find(";base64,") {
        let prefix_end = idx + ";base64,".len();
        let prefix = &s[..prefix_end];
        let rest = &s[prefix_end..];

        let (base64_part, suffix) = match rest.find(')') {
            Some(end) => (&rest[..end], &rest[end..]),
            None => (rest, ""),
        };

        if base64_part.len() <= 100 || base64_part.len() <= KEEP * 2 {
            return Cow::Borrowed(s);
        }

        let omitted = base64_part.len().saturating_sub(KEEP * 2);
        let mut out = String::with_capacity(prefix.len() + KEEP * 2 + suffix.len() + 64);
        out.push_str(prefix);
        out.push_str(&base64_part[..KEEP]);
        out.push_str(&format!("...[TRUNCATED: {omitted} chars]..."));
        out.push_str(&base64_part[base64_part.len() - KEEP..]);
        out.push_str(suffix);
        return Cow::Owned(out);
    }

    let mut is_base64 = force;
    if !is_base64 && s.len() > 200 {
        let sample_len = s.len().min(100);
        let mut ok = true;
        for &b in s.as_bytes().iter().take(sample_len) {
            let c = b as char;
            if !matches!(c, 'A'..='Z' | 'a'..='z' | '0'..='9' | '+' | '/' | '=') {
                ok = false;
                break;
            }
        }
        if ok {
            is_base64 = true;
        }
    }

    if !is_base64 {
        for p in ["/9j/", "iVBOR", "R0lGOD", "UklGR", "Qk1", "AAAA"] {
            if s.starts_with(p) {
                is_base64 = true;
                break;
            }
        }
    }

    if !is_base64 || s.len() <= KEEP * 2 {
        return Cow::Borrowed(s);
    }

    let omitted = s.len().saturating_sub(KEEP * 2);
    let mut out = String::with_capacity(KEEP * 2 + 64);
    out.push_str(&s[..KEEP]);
    out.push_str(&format!("...[TRUNCATED: {omitted} chars]..."));
    out.push_str(&s[s.len() - KEEP..]);
    Cow::Owned(out)
}
