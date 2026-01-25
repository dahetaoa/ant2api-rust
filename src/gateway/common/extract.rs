use sonic_rs::prelude::*;

/// 从 OpenAI/Claude 常见的 content/system 字段中提取纯文本：
/// - string：直接返回
/// - array：抽取 {"type":"text","text":...} 并按 sep 连接
pub fn extract_text_from_content(content: &sonic_rs::Value, sep: &str, skip_empty: bool) -> String {
    if let Some(s) = content.as_str() {
        return s.to_string();
    }
    let Some(arr) = content.as_array() else {
        return String::new();
    };

    let mut out = String::new();
    let mut first = true;
    for it in arr {
        let Some(obj) = it.as_object() else {
            continue;
        };
        if obj.get(&"type").and_then(|v| v.as_str()) != Some("text") {
            continue;
        }
        let t = obj.get(&"text").and_then(|v| v.as_str()).unwrap_or("");
        if skip_empty && t.is_empty() {
            continue;
        }
        if !first {
            out.push_str(sep);
        }
        out.push_str(t);
        first = false;
    }
    out
}

/// 从一组消息中提取 role=="system" 的文本，并以两个换行分隔。
pub fn extract_system_from_messages<T, FRole, FContent>(
    messages: &[T],
    role: FRole,
    content: FContent,
) -> String
where
    FRole: Fn(&T) -> &str,
    FContent: Fn(&T) -> &sonic_rs::Value,
{
    let mut out = String::new();
    let mut first = true;
    for m in messages {
        if role(m) != "system" {
            continue;
        }
        let t = extract_text_from_content(content(m), "\n", false);
        if t.is_empty() {
            continue;
        }
        if !first {
            out.push_str("\n\n");
        }
        out.push_str(&t);
        first = false;
    }
    out
}

/// 提取 Claude 请求中的 system 字段文本（支持 string 与 array）。
pub fn extract_claude_system_text(system: &sonic_rs::Value) -> String {
    extract_text_from_content(system, "\n\n", true)
}
