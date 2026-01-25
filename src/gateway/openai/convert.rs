use crate::config::Config;
use crate::gateway::common::extract::{extract_system_from_messages, extract_text_from_content};
use crate::gateway::common::{AccountContext, find_function_name};
use crate::signature::manager::Manager as SignatureManager;
use crate::util::{id, model as modelutil};
use crate::vertex::sanitize::{
    inject_agent_system_prompt, sanitize_contents, sanitize_function_parameters_schema,
};
use crate::vertex::types::{
    Content, FunctionCall as VFunctionCall, FunctionCallingConfig, FunctionDeclaration,
    FunctionResponse, GenerationConfig, ImageConfig, InnerReq, Part, Request, SystemInstruction,
    Tool as VTool, ToolConfig, UsageMetadata,
};
use chrono::Utc;
use sonic_rs::prelude::*;
use std::collections::HashMap;

use super::types::{
    ChatCompletion, ChatRequest, Choice, Message, ModelItem, ModelsResponse, Tool, ToolCall, Usage,
};

pub async fn to_vertex_request(
    cfg: &Config,
    sig_mgr: &SignatureManager,
    req: &mut ChatRequest,
    account: &AccountContext,
) -> anyhow::Result<(Request, String)> {
    let model_name = req.model.clone();
    let model = req.model.trim();
    let is_image_model = modelutil::is_image_model(model);
    let is_gemini3_flash = modelutil::is_gemini3_flash(model);

    let request_id = id::request_id();
    let vertex_model = modelutil::backend_model_id(&model_name);

    let mut vreq = Request {
        project: account.project_id.clone(),
        model: vertex_model,
        request_id: request_id.clone(),
        request_type: "agent".to_string(),
        user_agent: "antigravity".to_string(),
        request: InnerReq {
            contents: Vec::new(),
            system_instruction: None,
            generation_config: None,
            tools: Vec::new(),
            tool_config: None,
            session_id: account.session_id.clone(),
        },
    };

    let sys = extract_system_from_messages(&req.messages, |m| m.role.as_str(), |m| &m.content);
    if !sys.is_empty() {
        vreq.request.system_instruction = Some(SystemInstruction {
            role: "user".to_string(),
            parts: vec![Part {
                text: sys,
                ..Part::default()
            }],
        });
    }

    if !req.tools.is_empty() {
        vreq.request.tools = to_vertex_tools(&req.tools);
        vreq.request.tool_config = Some(ToolConfig {
            function_calling_config: Some(FunctionCallingConfig {
                mode: "AUTO".to_string(),
                allowed_function_names: Vec::new(),
            }),
        });
    }

    vreq.request.generation_config = Some(build_generation_config(cfg, req));
    let contents = to_vertex_contents(req, sig_mgr).await?;
    vreq.request.contents = sanitize_contents(contents);

    let should_skip_system_prompt = is_image_model || is_gemini3_flash;
    if !should_skip_system_prompt {
        let sys = vreq.request.system_instruction.take();
        vreq.request.system_instruction = Some(inject_agent_system_prompt(sys));
    }

    Ok((vreq, request_id))
}

async fn to_vertex_contents(
    req: &mut ChatRequest,
    sig_mgr: &SignatureManager,
) -> anyhow::Result<Vec<Content>> {
    // 特例修复：Claude/Gemini 在极少数情况下会返回“纯工具调用”（assistant 只有 tool_calls，
    // 没有正文/没有 reasoning），并且后端要求“只要出现工具调用，就必须带思维签名”。
    //
    // 我们禁止注入任何虚拟 thoughtSignature（会污染后端校验与状态），而是把这条“纯工具调用”
    // 合并到上一轮（同一次请求历史里）真实带思维签名的 assistant 轮次中，作为并行 tool_calls
    // 一并发给后端，从而通过校验并保持状态连续性。
    merge_tool_only_assistant_messages(req, sig_mgr).await;

    let mut out: Vec<Content> = Vec::new();

    let model = req.model.trim();
    let is_claude_thinking = modelutil::is_claude_thinking(model);
    let is_gemini = modelutil::is_gemini(model);

    for m in &mut req.messages {
        match m.role.as_str() {
            "system" => continue,
            "user" => {
                let parts = extract_user_parts(&mut m.content, sig_mgr).await?;
                out.push(Content {
                    role: "user".to_string(),
                    parts,
                });
            }
            "assistant" => {
                let mut parts: Vec<Part> = Vec::with_capacity(2 + m.tool_calls.len());

                let mut thinking_text = m.reasoning.trim().to_string();
                if thinking_text.is_empty() {
                    thinking_text = m.reasoning_content.trim().to_string();
                }

                let (first_tool_sig, first_tool_reasoning) = if let Some(tc0) = m.tool_calls.first()
                {
                    match sig_mgr.lookup_by_tool_call_id(&tc0.id).await {
                        Some(e) => (e.signature.trim().to_string(), e.reasoning),
                        None => (String::new(), String::new()),
                    }
                } else {
                    (String::new(), String::new())
                };

                if is_claude_thinking {
                    let mut injected_text = thinking_text;
                    if injected_text.is_empty() {
                        injected_text = first_tool_reasoning.trim().to_string();
                    }
                    let injected_sig = first_tool_sig;

                    if !injected_sig.is_empty()
                        && injected_text.is_empty()
                        && !m.tool_calls.is_empty()
                    {
                        injected_text = "[missing thought text]".to_string();
                    }
                    if !injected_sig.is_empty() && !injected_text.is_empty() {
                        parts.push(Part {
                            text: injected_text,
                            thought: true,
                            thought_signature: injected_sig,
                            ..Part::default()
                        });
                    }
                } else if !thinking_text.is_empty() {
                    parts.push(Part {
                        text: thinking_text,
                        thought: true,
                        ..Part::default()
                    });
                }

                let t = extract_text_from_content(&m.content, "\n", false);
                if !t.is_empty() {
                    let images = parse_markdown_images(&t, sig_mgr).await?;
                    if images.is_empty() {
                        parts.push(Part {
                            text: t,
                            ..Part::default()
                        });
                    } else {
                        let mut last = 0usize;
                        for img in images {
                            if img.start > last
                                && let Some(seg) = t.get(last..img.start)
                                && !seg.is_empty()
                            {
                                parts.push(Part {
                                    text: seg.to_string(),
                                    ..Part::default()
                                });
                            }
                            parts.push(Part {
                                inline_data: Some(img.inline),
                                thought_signature: img.signature,
                                ..Part::default()
                            });
                            last = img.end;
                        }
                        if last < t.len()
                            && let Some(seg) = t.get(last..)
                            && !seg.is_empty()
                        {
                            parts.push(Part {
                                text: seg.to_string(),
                                ..Part::default()
                            });
                        }
                    }
                }

                for (i, tc) in m.tool_calls.iter().enumerate() {
                    let args = parse_args(&tc.function.arguments);
                    let mut sig = String::new();
                    if is_gemini {
                        if let Some(e) = sig_mgr.lookup_by_tool_call_id(&tc.id).await {
                            sig = e.signature.trim().to_string();
                        }
                        if i != 0 {
                            sig.clear();
                        }
                    }
                    parts.push(Part {
                        function_call: Some(VFunctionCall {
                            id: tc.id.clone(),
                            name: tc.function.name.clone(),
                            args,
                        }),
                        thought_signature: sig,
                        ..Part::default()
                    });
                }

                if !parts.is_empty() {
                    out.push(Content {
                        role: "model".to_string(),
                        parts,
                    });
                }
            }
            "tool" => {
                let func_name = find_function_name(&out, &m.tool_call_id);
                let output = extract_text_from_content(&m.content, "\n", false);

                let mut resp: HashMap<String, sonic_rs::Value> = HashMap::new();
                resp.insert(
                    "output".to_string(),
                    sonic_rs::to_value(&output).unwrap_or_default(),
                );

                let p = Part {
                    function_response: Some(FunctionResponse {
                        id: m.tool_call_id.clone(),
                        name: func_name,
                        response: resp,
                    }),
                    ..Part::default()
                };
                append_function_response(&mut out, p);
            }
            _ => {}
        }
    }

    Ok(out)
}

fn is_tool_only_assistant_message(m: &Message) -> bool {
    if m.role != "assistant" || m.tool_calls.is_empty() {
        return false;
    }
    if !m.reasoning.trim().is_empty() || !m.reasoning_content.trim().is_empty() {
        return false;
    }
    match m.content.as_str() {
        Some(s) => s.trim().is_empty(),
        None => extract_text_from_content(&m.content, "\n", false)
            .trim()
            .is_empty(),
    }
}

async fn has_signature_for_first_tool_call(sig_mgr: &SignatureManager, m: &Message) -> bool {
    let Some(tc0) = m.tool_calls.first() else {
        return false;
    };
    let id = tc0.id.trim();
    if id.is_empty() {
        return false;
    }
    sig_mgr.cache().get_by_tool_call_id(id).await.is_some()
}

async fn merge_tool_only_assistant_messages(req: &mut ChatRequest, sig_mgr: &SignatureManager) {
    let mut i = 0usize;
    while i < req.messages.len() {
        if !is_tool_only_assistant_message(&req.messages[i]) {
            i += 1;
            continue;
        }

        // 仅处理“纯工具调用 + tool 结果紧随其后”的回传；避免误伤未执行工具的中间态。
        if i + 1 >= req.messages.len() || req.messages[i + 1].role != "tool" {
            i += 1;
            continue;
        }

        // 若本轮 tool_call 已有签名（服务端缓存命中），则无需合并。
        if has_signature_for_first_tool_call(sig_mgr, &req.messages[i]).await {
            i += 1;
            continue;
        }

        // 向前找“上一轮真实带签名的 assistant 工具调用”，把当前纯 tool_calls 合并进去。
        let mut anchor: Option<usize> = None;
        for j in (0..i).rev() {
            let m = &req.messages[j];
            if m.role != "assistant" || m.tool_calls.is_empty() {
                continue;
            }
            if has_signature_for_first_tool_call(sig_mgr, m).await {
                anchor = Some(j);
                break;
            }
        }

        let Some(anchor) = anchor else {
            i += 1;
            continue;
        };

        let calls = std::mem::take(&mut req.messages[i].tool_calls);
        req.messages[anchor].tool_calls.extend(calls);
        req.messages.remove(i);
    }
}

async fn extract_user_parts(
    content: &mut sonic_rs::Value,
    sig_mgr: &SignatureManager,
) -> anyhow::Result<Vec<Part>> {
    let mut out: Vec<Part> = Vec::new();

    if let Some(s) = content.as_str() {
        if !s.is_empty() {
            out.push(Part {
                text: s.to_string(),
                ..Part::default()
            });
        }
        return Ok(out);
    }

    let Some(arr) = content.as_array_mut() else {
        return Ok(out);
    };

    for it in arr.iter_mut() {
        let Some(obj) = it.as_object_mut() else {
            continue;
        };
        let typ = obj.get(&"type").and_then(|v| v.as_str()).unwrap_or("");
        match typ {
            "text" => {
                let t = obj.get(&"text").and_then(|v| v.as_str()).unwrap_or("");
                if !t.is_empty() {
                    out.push(Part {
                        text: t.to_string(),
                        ..Part::default()
                    });
                }
            }
            "image_url" => {
                let Some(img) = obj.get_mut(&"image_url").and_then(|v| v.as_object_mut()) else {
                    continue;
                };
                let url = img.get(&"url").and_then(|v| v.as_str()).unwrap_or("");
                let Some(inline) = parse_image_url(url) else {
                    continue;
                };

                let image_key = inline.signature_key();
                let sig = if image_key.is_empty() {
                    String::new()
                } else {
                    sig_mgr
                        .lookup_by_tool_call_id(&image_key)
                        .await
                        .map(|e| e.signature)
                        .unwrap_or_default()
                };

                out.push(Part {
                    inline_data: Some(inline),
                    thought_signature: sig,
                    ..Part::default()
                });

                // 帮助释放大字段：把 url 清空（与 Go 的 GC 优化等价效果）。
                if let Some(v) = img.get_mut(&"url") {
                    *v = sonic_rs::to_value("").unwrap_or_default();
                }
            }
            _ => {}
        }
    }

    Ok(out)
}

fn build_generation_config(cfg: &Config, req: &ChatRequest) -> GenerationConfig {
    let model = req.model.trim();
    let is_claude = modelutil::is_claude(model);
    let is_gemini = modelutil::is_gemini(model);
    let is_image_model = modelutil::is_image_model(model);

    let mut out = GenerationConfig {
        candidate_count: 1,
        stop_sequences: Vec::new(),
        max_output_tokens: 0,
        temperature: None,
        top_p: None,
        top_k: 0,
        thinking_config: None,
        image_config: None,
        media_resolution: String::new(),
    };

    // Gemini：maxOutputTokens 固定为 65535。
    if is_gemini {
        out.max_output_tokens = modelutil::GEMINI_MAX_OUTPUT_TOKENS;
    } else if req.max_tokens > 0 && !is_claude {
        out.max_output_tokens = req.max_tokens;
    }

    if let Some(v) = req.temperature {
        out.temperature = Some(v);
    }
    if let Some(v) = req.top_p {
        out.top_p = Some(v);
    }

    if let Some(tc) = modelutil::thinking_config_from_openai(model, &req.reasoning_effort) {
        out.thinking_config = Some(tc);
    }

    // Claude：maxOutputTokens 固定为 64000。
    if is_claude {
        out.max_output_tokens = modelutil::CLAUDE_MAX_OUTPUT_TOKENS;
    }

    // 当使用 thinkingBudget 时，确保与 maxOutputTokens 兼容（与 Go 行为一致）。
    if let Some(tc) = out.thinking_config.as_mut()
        && tc.thinking_budget > 0
    {
        if out.max_output_tokens <= 0 {
            out.max_output_tokens =
                tc.thinking_budget + modelutil::THINKING_MAX_OUTPUT_TOKENS_OVERHEAD_TOKENS;
        }
        if is_claude {
            let mut max_budget = out.max_output_tokens - modelutil::THINKING_BUDGET_HEADROOM_TOKENS;
            if max_budget < modelutil::THINKING_BUDGET_MIN_TOKENS {
                max_budget = modelutil::THINKING_BUDGET_MIN_TOKENS;
            }
            if tc.thinking_budget > max_budget {
                tc.thinking_budget = max_budget;
            }
        } else if is_gemini && out.max_output_tokens <= tc.thinking_budget {
            let mut max_budget = out.max_output_tokens - modelutil::THINKING_BUDGET_HEADROOM_TOKENS;
            if max_budget < modelutil::THINKING_BUDGET_MIN_TOKENS {
                max_budget = modelutil::THINKING_BUDGET_MIN_TOKENS;
            }
            tc.thinking_budget = max_budget;
        } else if out.max_output_tokens <= tc.thinking_budget {
            out.max_output_tokens =
                tc.thinking_budget + modelutil::THINKING_MAX_OUTPUT_TOKENS_OVERHEAD_TOKENS;
        }
    }

    // Gemini image size 虚拟模型：由 modelName 强制 imageConfig.imageSize。
    if let Some((image_size, _backend)) = modelutil::gemini_pro_image_size_config(model) {
        out.image_config = Some(ImageConfig {
            aspect_ratio: String::new(),
            image_size,
        });
    }

    // Gemini 3：应用全局 mediaResolution（非 image 模型）。
    if modelutil::is_gemini3(model)
        && !is_image_model
        && let Some(v) = modelutil::to_api_media_resolution(&cfg.gemini3_media_resolution)
        && !v.is_empty()
    {
        out.media_resolution = v;
    }

    out
}

fn to_vertex_tools(tools: &[Tool]) -> Vec<VTool> {
    let mut out: Vec<VTool> = Vec::with_capacity(tools.len());
    for t in tools {
        let mut params = sanitize_function_parameters_schema(&t.function.parameters);
        if params.is_empty() {
            // 兼容部分客户端：tools.function.parameters 可能缺失（或解析失败），
            // 但 Vertex functionDeclarations.parameters 省略后更容易诱发模型输出不合法的函数调用。
            // 这里做一个最小兜底：声明为 OBJECT，避免空 schema。
            params.insert(
                "type".to_string(),
                sonic_rs::to_value("OBJECT").unwrap_or_default(),
            );
        }
        out.push(VTool {
            function_declarations: vec![FunctionDeclaration {
                name: t.function.name.clone(),
                description: t.function.description.clone(),
                parameters: Some(params),
            }],
        });
    }
    out
}

#[derive(Debug)]
struct MarkdownImage {
    inline: crate::vertex::types::InlineData,
    signature: String,
    start: usize,
    end: usize,
}

async fn parse_markdown_images(
    content: &str,
    sig_mgr: &SignatureManager,
) -> anyhow::Result<Vec<MarkdownImage>> {
    let matches = parse_markdown_image_matches(content);
    if matches.is_empty() {
        return Ok(Vec::new());
    }

    let mut out: Vec<MarkdownImage> = Vec::with_capacity(matches.len());
    for m in matches {
        let Some(inline) = match_markdown_inline_data(&m.mime_type, &m.data) else {
            continue;
        };
        let image_key = inline.signature_key();
        let sig = if image_key.is_empty() {
            String::new()
        } else {
            sig_mgr
                .lookup_by_tool_call_id(&image_key)
                .await
                .map(|e| e.signature)
                .unwrap_or_default()
        };
        out.push(MarkdownImage {
            inline,
            signature: sig,
            start: m.start,
            end: m.end,
        });
    }
    Ok(out)
}

#[derive(Debug)]
struct MarkdownImageMatch {
    mime_type: String,
    data: String,
    start: usize,
    end: usize,
}

fn parse_markdown_image_matches(content: &str) -> Vec<MarkdownImageMatch> {
    const PREFIX: &str = "![image](data:";
    const BASE64_MARK: &str = ";base64,";

    let mut out: Vec<MarkdownImageMatch> = Vec::new();
    let mut i = 0usize;
    while let Some(pos) = content[i..].find(PREFIX) {
        let start = i + pos;
        let mut j = start + PREFIX.len();
        let Some(mark_rel) = content[j..].find(BASE64_MARK) else {
            break;
        };
        let mark = j + mark_rel;
        let mime_type = content[j..mark].to_string();
        if mime_type.is_empty() {
            i = j;
            continue;
        }
        j = mark + BASE64_MARK.len();
        let Some(end_rel) = content[j..].find(')') else {
            break;
        };
        let end = j + end_rel + 1;
        let data = content[j..(end - 1)].to_string();
        if data.is_empty() {
            i = end;
            continue;
        }
        out.push(MarkdownImageMatch {
            mime_type,
            data,
            start,
            end,
        });
        i = end;
    }
    out
}

fn match_markdown_inline_data(
    mime_type: &str,
    base64_data: &str,
) -> Option<crate::vertex::types::InlineData> {
    if mime_type.trim().is_empty() || base64_data.trim().is_empty() {
        return None;
    }
    Some(crate::vertex::types::InlineData::new(
        mime_type.to_string(),
        base64_data.to_string(),
    ))
}

fn parse_image_url(url: &str) -> Option<crate::vertex::types::InlineData> {
    const DATA_PREFIX: &str = "data:";
    const BASE64_MARK: &str = ";base64,";
    if !url.starts_with(DATA_PREFIX) || !url.starts_with("data:image/") {
        return None;
    }
    let marker = url.find(BASE64_MARK)?;
    if marker < DATA_PREFIX.len() {
        return None;
    }
    let mime_type = &url[DATA_PREFIX.len()..marker];
    let base64_data = &url[(marker + BASE64_MARK.len())..];
    if mime_type.is_empty() || base64_data.is_empty() {
        return None;
    }
    Some(crate::vertex::types::InlineData::new(
        mime_type.to_string(),
        base64_data.to_string(),
    ))
}

fn parse_args(args: &str) -> HashMap<String, sonic_rs::Value> {
    if args.is_empty() {
        return HashMap::new();
    }
    sonic_rs::from_str::<HashMap<String, sonic_rs::Value>>(args).unwrap_or_default()
}

fn append_function_response(contents: &mut Vec<Content>, part: Part) {
    if let Some(last) = contents.last_mut() {
        if last.role == "model" {
            contents.push(Content {
                role: "user".to_string(),
                parts: vec![part],
            });
            return;
        }
        if last.role == "user" {
            last.parts.push(part);
            return;
        }
    }
    contents.push(Content {
        role: "user".to_string(),
        parts: vec![part],
    });
}

pub fn convert_usage(metadata: Option<&UsageMetadata>) -> Option<Usage> {
    let m = metadata?;
    Some(Usage {
        prompt_tokens: m.prompt_token_count,
        completion_tokens: m.candidates_token_count,
        total_tokens: m.total_token_count,
    })
}

pub async fn to_chat_completion(
    resp: &crate::vertex::types::Response,
    model: &str,
    request_id: &str,
    sig_mgr: &SignatureManager,
) -> ChatCompletion {
    let created = Utc::now().timestamp();
    let mut out = ChatCompletion {
        id: id::chat_completion_id(),
        object: "chat.completion".to_string(),
        created,
        model: model.to_string(),
        choices: vec![Choice {
            index: 0,
            message: Some(Message {
                role: "assistant".to_string(),
                content: sonic_rs::to_value("").unwrap_or_default(),
                tool_calls: Vec::new(),
                tool_call_id: String::new(),
                name: String::new(),
                reasoning: String::new(),
                reasoning_content: String::new(),
            }),
            delta: None,
            finish_reason: Some("stop".to_string()),
        }],
        usage: convert_usage(resp.response.usage_metadata.as_ref()),
    };

    let Some(cand) = resp.response.candidates.first() else {
        return out;
    };

    let mut content = String::new();
    let mut reasoning = String::new();
    let mut tool_calls: Vec<ToolCall> = Vec::new();

    let is_claude_thinking = modelutil::is_claude_thinking(model);
    let mut pending_sig = String::new();
    let mut pending_reasoning = String::new();

    for p in &cand.content.parts {
        if p.thought {
            reasoning.push_str(&p.text);
            pending_reasoning.push_str(&p.text);
            if is_claude_thinking && !p.thought_signature.is_empty() {
                pending_sig = p.thought_signature.clone();
            }
            continue;
        }
        if !p.text.is_empty() {
            content.push_str(&p.text);
            continue;
        }
        if let Some(inline) = &p.inline_data {
            let image_key = inline.signature_key();
            if !p.thought_signature.is_empty() {
                sig_mgr
                    .save(
                        request_id,
                        &image_key,
                        &p.thought_signature,
                        &pending_reasoning,
                        model,
                    )
                    .await;
                pending_reasoning.clear();
            }
            let data = inline.data.as_str();
            let mut sb = String::with_capacity(10 + inline.mime_type.len() + data.len());
            sb.push_str("![image](data:");
            sb.push_str(&inline.mime_type);
            sb.push_str(";base64,");
            sb.push_str(data);
            sb.push(')');
            content.push_str(&sb);
            continue;
        }
        if let Some(fc) = &p.function_call {
            let tool_call_id = if fc.id.is_empty() {
                id::tool_call_id()
            } else {
                fc.id.clone()
            };

            let mut saved = false;
            if is_claude_thinking {
                if !pending_sig.is_empty() {
                    sig_mgr
                        .save(
                            request_id,
                            &tool_call_id,
                            &pending_sig,
                            &pending_reasoning,
                            model,
                        )
                        .await;
                    pending_sig.clear();
                    saved = true;
                } else if !p.thought_signature.is_empty() {
                    sig_mgr
                        .save(
                            request_id,
                            &tool_call_id,
                            &p.thought_signature,
                            &pending_reasoning,
                            model,
                        )
                        .await;
                    saved = true;
                }
            } else if !p.thought_signature.is_empty() {
                sig_mgr
                    .save(
                        request_id,
                        &tool_call_id,
                        &p.thought_signature,
                        &pending_reasoning,
                        model,
                    )
                    .await;
                saved = true;
            }
            if saved {
                pending_reasoning.clear();
            }

            let args = if fc.args.is_empty() {
                "{}".to_string()
            } else {
                sonic_rs::to_string(&fc.args).unwrap_or_else(|_| "{}".to_string())
            };

            tool_calls.push(ToolCall {
                index: None,
                id: tool_call_id,
                typ: "function".to_string(),
                function: super::types::FunctionCall {
                    name: fc.name.clone(),
                    arguments: args,
                },
            });
        }
    }

    let finish = if tool_calls.is_empty() {
        "stop".to_string()
    } else {
        "tool_calls".to_string()
    };
    if let Some(choice) = out.choices.first_mut() {
        choice.finish_reason = Some(finish);
        if let Some(msg) = choice.message.as_mut() {
            msg.content = sonic_rs::to_value(&content).unwrap_or_default();
            msg.reasoning = reasoning;
            msg.tool_calls = tool_calls;
        }
    }

    out
}

pub fn to_models_response(
    models: &std::collections::HashMap<String, sonic_rs::Value>,
) -> ModelsResponse {
    let ids = modelutil::build_sorted_model_ids(models);
    let mut items: Vec<ModelItem> = Vec::with_capacity(ids.len());
    for mid in ids {
        let owned_by = if mid.starts_with("claude-") {
            "anthropic"
        } else if mid.starts_with("gpt-") {
            "openai"
        } else {
            "google"
        };
        items.push(ModelItem {
            id: mid,
            object: "model".to_string(),
            owned_by: owned_by.to_string(),
        });
    }
    ModelsResponse {
        object: "list".to_string(),
        data: items,
    }
}
