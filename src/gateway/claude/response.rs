use crate::signature::manager::Manager as SignatureManager;
use crate::util::{id, model as modelutil};
use crate::vertex;

use super::types::{MessagesResponse, ResponseContentBlock, Usage};

pub async fn to_messages_response(
    resp: &vertex::types::Response,
    request_id: &str,
    model: &str,
    sig_mgr: &SignatureManager,
) -> MessagesResponse {
    let input_tokens = resp
        .response
        .usage_metadata
        .as_ref()
        .map(|u| u.prompt_token_count)
        .unwrap_or(0)
        .max(0);

    let mut out = MessagesResponse {
        id: format!("msg_{request_id}"),
        typ: "message".to_string(),
        role: "assistant".to_string(),
        model: model.to_string(),
        content: Vec::new(),
        stop_reason: "end_turn".to_string(),
        stop_sequence: None,
        usage: Usage {
            input_tokens,
            output_tokens: 0,
        },
    };

    let Some(cand) = resp.response.candidates.first() else {
        return out;
    };

    let is_claude = modelutil::is_claude(model);
    let parts = &cand.content.parts;

    let mut text = String::new();
    let mut thinking = String::new();
    let mut thinking_signature = String::new();
    let mut tool_uses: Vec<ResponseContentBlock> = Vec::new();

    for p in parts {
        if is_claude && p.thought && !p.thought_signature.trim().is_empty() {
            thinking_signature = p.thought_signature.trim().to_string();
        }

        if p.thought {
            thinking.push_str(&p.text);
            continue;
        }

        if !p.text.is_empty() {
            text.push_str(&p.text);
            continue;
        }

        let Some(fc) = &p.function_call else {
            continue;
        };

        let tool_id = if fc.id.trim().is_empty() {
            format!("toolu_{}", id::request_id())
        } else {
            fc.id.trim().to_string()
        };

        let mut sig = p.thought_signature.trim().to_string();
        if sig.is_empty() && is_claude {
            // Claude 的签名可能出现在 thinking part（而非 functionCall part）。
            sig = thinking_signature.clone();
        }
        if !sig.is_empty() {
            let mut reasoning = thinking.trim().to_string();
            if reasoning.is_empty() {
                reasoning = "[missing thought text]".to_string();
            }
            sig_mgr
                .save(request_id, &tool_id, &sig, &reasoning, model)
                .await;
        }

        tool_uses.push(ResponseContentBlock {
            typ: "tool_use".to_string(),
            text: None,
            thinking: None,
            signature: None,
            id: Some(tool_id),
            name: Some(fc.name.clone()),
            input: Some(sonic_rs::to_value(&fc.args).unwrap_or_default()),
        });
        out.stop_reason = "tool_use".to_string();
    }

    // Edge case：只有签名没有 thinking 文本时注入占位符。
    if !thinking_signature.is_empty() && thinking.trim().is_empty() {
        thinking = "[missing thought text]".to_string();
    }

    let mut blocks: Vec<ResponseContentBlock> = Vec::with_capacity(2 + tool_uses.len());

    if !thinking.is_empty() || !thinking_signature.is_empty() {
        blocks.push(ResponseContentBlock {
            typ: "thinking".to_string(),
            text: None,
            thinking: Some(thinking),
            signature: if thinking_signature.is_empty() {
                None
            } else {
                Some(thinking_signature)
            },
            id: None,
            name: None,
            input: None,
        });
    }

    if !text.is_empty() {
        blocks.push(ResponseContentBlock {
            typ: "text".to_string(),
            text: Some(text),
            thinking: None,
            signature: None,
            id: None,
            name: None,
            input: None,
        });
    }

    blocks.extend(tool_uses);
    out.content = blocks;

    if let Some(u) = resp.response.usage_metadata.as_ref() {
        out.usage.output_tokens = u.candidates_token_count.max(0);
    }

    out
}
