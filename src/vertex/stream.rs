use crate::vertex::types::{StreamData, StreamResult, ToolCallInfo};
use sonic_rs::prelude::*;
use thiserror::Error;
use tokio_stream::StreamExt;

#[derive(Debug, Error)]
#[error("{source}")]
pub struct StreamParseError {
    pub result: StreamResult,
    #[source]
    pub source: anyhow::Error,
}

pub async fn parse_stream_with_result<F, Fut, L>(
    resp: reqwest::Response,
    mut receiver: F,
    build_merged: bool,
    mut raw_logger: L,
) -> Result<StreamResult, StreamParseError>
where
    F: FnMut(&StreamData) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
    L: FnMut(&[u8]),
{
    let mut result = StreamResult::default();
    let mut text = String::new();
    let mut thinking = String::new();

    let mut merged_parts: Vec<sonic_rs::Value> = Vec::new();
    let mut last_finish_reason = String::new();
    let mut last_usage_raw: Option<sonic_rs::Value> = None;

    let mut buf: Vec<u8> = Vec::with_capacity(4 * 1024);
    let mut processed: usize = 0;

    let mut stream = resp.bytes_stream();
    while let Some(item) = stream.next().await {
        let chunk = match item {
            Ok(c) => c,
            Err(e) => {
                result.text = text;
                result.thinking = thinking;
                return Err(StreamParseError {
                    result,
                    source: anyhow::Error::new(e),
                });
            }
        };
        buf.extend_from_slice(chunk.as_ref());

        while let Some(nl_rel) = buf[processed..].iter().position(|&b| b == b'\n') {
            let nl = processed + nl_rel;
            let line_raw = &buf[processed..nl];
            raw_logger(line_raw);

            let mut line = line_raw;
            if line.ends_with(b"\r") {
                line = &line[..line.len() - 1];
            }
            processed = nl + 1;

            if !line.starts_with(b"data: ") {
                continue;
            }
            let json_bytes = &line[6..];
            if json_bytes == b"[DONE]" {
                // 清理剩余缓冲并结束。
                buf.drain(..processed);
                result.text = text;
                result.thinking = thinking;
                if build_merged {
                    result.merged_response = build_merged_response(
                        &merged_parts,
                        &last_finish_reason,
                        last_usage_raw.as_ref(),
                    )
                    .ok();
                }
                return Ok(result);
            }

            if build_merged && let Ok(raw) = sonic_rs::from_slice::<sonic_rs::Value>(json_bytes) {
                if let Some(usage) = raw
                    .get("response")
                    .and_then(|v| v.get("usageMetadata"))
                    .map(|v| v.to_owned())
                {
                    last_usage_raw = Some(usage);
                }
                if let Some(fr) = raw
                    .get("response")
                    .and_then(|v| v.get("candidates"))
                    .and_then(|v| v.as_array())
                    .and_then(|a| a.first())
                    .and_then(|v| v.get("finishReason"))
                    .and_then(|v| v.as_str())
                    && !fr.is_empty()
                {
                    last_finish_reason = fr.to_string();
                }
                if let Some(parts) = raw
                    .get("response")
                    .and_then(|v| v.get("candidates"))
                    .and_then(|v| v.as_array())
                    .and_then(|a| a.first())
                    .and_then(|v| v.get("content"))
                    .and_then(|v| v.get("parts"))
                    .and_then(|v| v.as_array())
                {
                    for p in parts.iter() {
                        merged_parts.push(p.to_owned());
                    }
                }
            }

            let data = match sonic_rs::from_slice::<StreamData>(json_bytes) {
                Ok(v) => v,
                Err(_) => continue,
            };

            if let Some(usage) = data.response.usage_metadata.clone() {
                result.usage = Some(usage);
            }

            if let Some(cand) = data.response.candidates.first() {
                if !cand.finish_reason.is_empty() {
                    result.finish_reason = cand.finish_reason.clone();
                }
                for part in &cand.content.parts {
                    if !part.thought_signature.is_empty() {
                        result.thought_signature = part.thought_signature.clone();
                    }
                    if part.thought {
                        thinking.push_str(&part.text);
                        continue;
                    }
                    if !part.text.is_empty() {
                        text.push_str(&part.text);
                        continue;
                    }
                    if let Some(fc) = &part.function_call {
                        result.tool_calls.push(ToolCallInfo {
                            id: fc.id.clone(),
                            name: fc.name.clone(),
                            args: fc.args.clone(),
                            thought_signature: part.thought_signature.clone(),
                        });
                    }
                }
            }

            if let Err(e) = receiver(&data).await {
                result.text = text;
                result.thinking = thinking;
                return Err(StreamParseError { result, source: e });
            }
        }

        // 释放已处理的前缀，避免 buffer 无限增长。
        if processed > 0 {
            buf.drain(..processed);
            processed = 0;
        }
    }

    result.text = text;
    result.thinking = thinking;
    if build_merged {
        result.merged_response =
            build_merged_response(&merged_parts, &last_finish_reason, last_usage_raw.as_ref()).ok();
    }
    Ok(result)
}

fn build_merged_response(
    merged_parts: &[sonic_rs::Value],
    finish_reason: &str,
    usage_metadata: Option<&sonic_rs::Value>,
) -> Result<sonic_rs::Value, sonic_rs::Error> {
    #[derive(serde::Serialize)]
    #[serde(rename_all = "camelCase")]
    struct Merged<'a> {
        response: MergedResp<'a>,
    }

    #[derive(serde::Serialize)]
    #[serde(rename_all = "camelCase")]
    struct MergedResp<'a> {
        candidates: Vec<MergedCand<'a>>,
        #[serde(skip_serializing_if = "Option::is_none")]
        usage_metadata: Option<&'a sonic_rs::Value>,
    }

    #[derive(serde::Serialize)]
    #[serde(rename_all = "camelCase")]
    struct MergedCand<'a> {
        content: MergedContent<'a>,
        #[serde(rename = "finishReason")]
        finish_reason: &'a str,
    }

    #[derive(serde::Serialize)]
    #[serde(rename_all = "camelCase")]
    struct MergedContent<'a> {
        role: &'a str,
        parts: Vec<sonic_rs::Value>,
    }

    let parts = merge_parts(merged_parts);
    let merged = Merged {
        response: MergedResp {
            candidates: vec![MergedCand {
                content: MergedContent {
                    role: "model",
                    parts,
                },
                finish_reason,
            }],
            usage_metadata,
        },
    };
    sonic_rs::to_value(&merged)
}

fn merge_parts(parts: &[sonic_rs::Value]) -> Vec<sonic_rs::Value> {
    if parts.is_empty() {
        return Vec::new();
    }

    let mut merged: Vec<sonic_rs::Value> = Vec::new();
    let mut text = String::new();
    let mut thinking = String::new();
    let mut text_extra: Option<std::collections::HashMap<String, sonic_rs::Value>> = None;
    let mut thinking_extra: Option<std::collections::HashMap<String, sonic_rs::Value>> = None;

    let flush =
        |merged: &mut Vec<sonic_rs::Value>,
         buf: &mut String,
         thought: bool,
         extra: &mut Option<std::collections::HashMap<String, sonic_rs::Value>>| {
            if buf.is_empty() {
                return;
            }
            let mut obj = sonic_rs::Object::new();
            let text_value = std::mem::take(buf);
            obj.insert(&"text", text_value.as_str());
            if thought {
                obj.insert(&"thought", true);
            }
            if let Some(extra_fields) = extra.take() {
                for (k, v) in extra_fields {
                    obj.insert(&k, v);
                }
            }
            merged.push(obj.into_value());
        };

    for p in parts {
        let Some(obj) = p.as_object() else {
            flush(&mut merged, &mut text, false, &mut text_extra);
            flush(&mut merged, &mut thinking, true, &mut thinking_extra);
            merged.push(p.to_owned());
            continue;
        };

        let txt = p.get("text").and_then(|v| v.as_str()).unwrap_or("");
        if !txt.is_empty() {
            let is_thought = p.get("thought").and_then(|v| v.as_bool()).unwrap_or(false);

            let extra = extract_extra_fields(obj);
            if is_thought {
                flush(&mut merged, &mut text, false, &mut text_extra);
                thinking.push_str(txt);
                merge_extra(&mut thinking_extra, extra);
            } else {
                flush(&mut merged, &mut thinking, true, &mut thinking_extra);
                text.push_str(txt);
                merge_extra(&mut text_extra, extra);
            }
            continue;
        }

        flush(&mut merged, &mut text, false, &mut text_extra);
        flush(&mut merged, &mut thinking, true, &mut thinking_extra);
        merged.push(p.to_owned());
    }

    flush(&mut merged, &mut thinking, true, &mut thinking_extra);
    flush(&mut merged, &mut text, false, &mut text_extra);

    merged
}

fn extract_extra_fields(
    obj: &sonic_rs::Object,
) -> Option<std::collections::HashMap<String, sonic_rs::Value>> {
    let mut extra = std::collections::HashMap::new();
    for (k, v) in obj.iter() {
        if k == "text" || k == "thought" {
            continue;
        }
        extra.insert(k.to_string(), v.to_owned());
    }
    if extra.is_empty() { None } else { Some(extra) }
}

fn merge_extra(
    existing: &mut Option<std::collections::HashMap<String, sonic_rs::Value>>,
    new_fields: Option<std::collections::HashMap<String, sonic_rs::Value>>,
) {
    let Some(new_fields) = new_fields else {
        return;
    };
    let dst = existing.get_or_insert_with(std::collections::HashMap::new);
    for (k, v) in new_fields {
        dst.insert(k, v);
    }
}
