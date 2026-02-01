use crate::config::Config;
use crate::gateway::common::extract::{extract_claude_system_text, extract_text_from_content};
use crate::gateway::common::{AccountContext, find_function_name};
use crate::signature::manager::Manager as SignatureManager;
use crate::util::{id, model as modelutil};
use crate::vertex::sanitize::{
    inject_agent_system_prompt, sanitize_contents, sanitize_function_parameters_schema,
};
use crate::vertex::types::{
    Content, FunctionCall as VFunctionCall, FunctionCallingConfig, FunctionDeclaration,
    FunctionResponse, GenerationConfig, ImageConfig, InnerReq, Part, Request, SystemInstruction,
    Tool as VTool, ToolConfig,
};
use sonic_rs::prelude::*;
use std::collections::HashMap;

use super::types::{Message, MessagesRequest, Tool};

pub async fn to_vertex_request(
    cfg: &Config,
    sig_mgr: &SignatureManager,
    req: &MessagesRequest,
    account: &AccountContext,
) -> anyhow::Result<(Request, String)> {
    if req.messages.is_empty() {
        anyhow::bail!("messages 是必填字段");
    }

    let model_name = req.model.clone();
    let model = req.model.trim();
    let is_claude_model = modelutil::is_claude(model);
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

    if let Some(sys) = req.system.as_ref() {
        let sys = extract_claude_system_text(sys);
        if !sys.is_empty() {
            vreq.request.system_instruction = Some(SystemInstruction {
                role: "user".to_string(),
                parts: vec![Part {
                    text: sys,
                    ..Part::default()
                }],
            });
        }
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
    let contents = to_vertex_contents(sig_mgr, &req.messages, is_claude_model).await?;
    vreq.request.contents = sanitize_contents(contents);

    let should_skip_system_prompt = is_image_model || is_gemini3_flash;
    if !should_skip_system_prompt {
        let sys = vreq.request.system_instruction.take();
        vreq.request.system_instruction = Some(inject_agent_system_prompt(sys));
    }

    Ok((vreq, request_id))
}

fn build_generation_config(cfg: &Config, req: &MessagesRequest) -> GenerationConfig {
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

    // Claude：maxOutputTokens 固定为 64000；Gemini：固定为 65535。
    if is_claude {
        out.max_output_tokens = modelutil::CLAUDE_MAX_OUTPUT_TOKENS;
    } else if is_gemini {
        out.max_output_tokens = modelutil::GEMINI_MAX_OUTPUT_TOKENS;
    } else if req.max_tokens > 0 {
        out.max_output_tokens = req.max_tokens;
    } else {
        out.max_output_tokens = 8192;
    }

    if let Some(v) = req.temperature {
        out.temperature = Some(v);
    }
    if let Some(v) = req.top_p {
        out.top_p = Some(v);
    }
    if !req.stop_sequences.is_empty() {
        out.stop_sequences = req.stop_sequences.clone();
    }

    if let Some(thinking) = req.thinking.as_ref() {
        let thinking_type = thinking.typ.trim();
        let budget = thinking.budget.unwrap_or(0);
        let budget_tokens = thinking.budget_tokens.unwrap_or(0);
        out.thinking_config =
            modelutil::thinking_config_from_claude(model, thinking_type, budget, budget_tokens);
    } else {
        // 允许由模型名强制启用 thinking（例如 gemini-3-flash / claude 4.5）。
        out.thinking_config = modelutil::forced_thinking_config(model);
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
        let mut params = sanitize_function_parameters_schema(&t.input_schema);
        if params.is_empty() {
            // 兼容部分客户端：tools.input_schema 可能缺失或解析失败，做最小兜底避免空 schema。
            params.insert(
                "type".to_string(),
                sonic_rs::to_value("OBJECT").unwrap_or_default(),
            );
        }
        out.push(VTool {
            function_declarations: vec![FunctionDeclaration {
                name: t.name.clone(),
                description: t.description.clone(),
                parameters: Some(params),
            }],
        });
    }
    out
}

async fn to_vertex_contents(
    sig_mgr: &SignatureManager,
    messages: &[Message],
    is_claude_model: bool,
) -> anyhow::Result<Vec<Content>> {
    let mut out: Vec<Content> = Vec::new();

    for m in messages {
        match m.role.as_str() {
            "user" => {
                let parts =
                    extract_content_parts(sig_mgr, &m.content, &out, is_claude_model).await?;
                if !parts.is_empty() {
                    out.push(Content {
                        role: "user".to_string(),
                        parts,
                    });
                }
            }
            "assistant" => {
                let parts =
                    extract_content_parts(sig_mgr, &m.content, &out, is_claude_model).await?;
                if !parts.is_empty() {
                    out.push(Content {
                        role: "model".to_string(),
                        parts,
                    });
                }
            }
            _ => {}
        }
    }

    Ok(out)
}

async fn extract_content_parts(
    sig_mgr: &SignatureManager,
    content: &sonic_rs::Value,
    contents_so_far: &[Content],
    is_claude_model: bool,
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

    let Some(arr) = content.as_array() else {
        return Ok(out);
    };

    for i in 0..arr.len() {
        let Some(obj) = arr[i].as_object() else {
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
            "thinking" => {
                let mut thinking = obj
                    .get(&"thinking")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let mut signature = obj
                    .get(&"signature")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string();

                if is_claude_model {
                    // 部分客户端不会持久化 signature，尝试用后续 tool_use 的 id 从缓存中恢复。
                    if let Some(tool_use_id) = lookahead_tool_use_id(arr, i + 1) {
                        if signature.is_empty() {
                            if let Some(e) = sig_mgr.lookup_by_tool_call_id(&tool_use_id).await {
                                signature = e.signature.trim().to_string();
                            }
                        } else if signature.len() <= 50
                            && let Some(e) = sig_mgr
                                .lookup_by_tool_call_id_and_signature_prefix(
                                    &tool_use_id,
                                    &signature,
                                )
                                .await
                        {
                            signature = e.signature.trim().to_string();
                        }
                    }

                    // 仍无法恢复：跳过该块，避免发送无效 extended thinking 历史。
                    if signature.is_empty() {
                        continue;
                    }

                    // Edge case：只有签名没有 thinking 文本时注入占位符。
                    if thinking.trim().is_empty() {
                        thinking = "[missing thought text]".to_string();
                    }

                    out.push(Part {
                        text: thinking,
                        thought: true,
                        thought_signature: signature,
                        ..Part::default()
                    });
                    continue;
                }

                if !thinking.is_empty() {
                    out.push(Part {
                        text: thinking,
                        thought: true,
                        ..Part::default()
                    });
                }
            }
            "redacted_thinking" => {
                let mut data = obj
                    .get(&"data")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim()
                    .to_string();

                if is_claude_model {
                    // 部分客户端会丢失 opaque payload，尝试用后续 tool_use 的 id 从缓存中恢复。
                    if let Some(tool_use_id) = lookahead_tool_use_id(arr, i + 1) {
                        if data.is_empty() {
                            if let Some(e) = sig_mgr.lookup_by_tool_call_id(&tool_use_id).await {
                                data = e.signature.trim().to_string();
                            }
                        } else if data.len() <= 50
                            && let Some(e) = sig_mgr
                                .lookup_by_tool_call_id_and_signature_prefix(&tool_use_id, &data)
                                .await
                        {
                            data = e.signature.trim().to_string();
                        }
                    }
                    if data.is_empty() {
                        continue;
                    }
                    // Cloud Code 使用 thoughtSignature 作为 opaque 校验字段；text 置空。
                    out.push(Part {
                        text: String::new(),
                        thought: true,
                        thought_signature: data,
                        ..Part::default()
                    });
                    continue;
                }

                out.push(Part {
                    text: String::new(),
                    thought: true,
                    ..Part::default()
                });
            }
            "tool_use" => {
                let idv = obj.get(&"id").and_then(|v| v.as_str()).unwrap_or("").trim();
                let tool_call_id = if idv.is_empty() {
                    id::tool_call_id()
                } else {
                    idv.to_string()
                };

                let name = obj.get(&"name").and_then(|v| v.as_str()).unwrap_or("");

                // 正确提取 input 对象：sonic_rs::from_value::<HashMap<..>>() 可能失败（保持与 schema 清洗一致的手动拷贝策略）。
                let args: HashMap<String, sonic_rs::Value> = obj
                    .get(&"input")
                    .and_then(|v| v.as_object())
                    .map(|o| {
                        o.iter()
                            .map(|(k, v)| (k.to_string(), v.to_owned()))
                            .collect()
                    })
                    .unwrap_or_default();

                // Claude 模型：签名只出现在 thinking block；非 Claude：把签名放在 functionCall 上。
                let sig = if is_claude_model {
                    String::new()
                } else {
                    sig_mgr
                        .lookup_by_tool_call_id(&tool_call_id)
                        .await
                        .map(|e| e.signature.trim().to_string())
                        .unwrap_or_default()
                };

                out.push(Part {
                    function_call: Some(VFunctionCall {
                        id: tool_call_id,
                        name: name.to_string(),
                        args,
                    }),
                    thought_signature: sig,
                    ..Part::default()
                });
            }
            "tool_result" => {
                let tool_use_id = obj
                    .get(&"tool_use_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .trim();
                if tool_use_id.is_empty() {
                    // 保持请求语义：tool_result 必须引用先前的 tool_use。
                    continue;
                }

                let func_name = find_function_name(contents_so_far, tool_use_id)
                    .trim()
                    .to_string();
                if func_name.is_empty() {
                    continue;
                }

                let content_value = obj.get(&"content").cloned().unwrap_or_default();
                let output = extract_text_from_content(&content_value, "", false);

                let mut resp: HashMap<String, sonic_rs::Value> = HashMap::new();
                resp.insert(
                    "output".to_string(),
                    sonic_rs::to_value(&output).unwrap_or_default(),
                );

                out.push(Part {
                    function_response: Some(FunctionResponse {
                        id: tool_use_id.to_string(),
                        name: func_name,
                        response: resp,
                    }),
                    ..Part::default()
                });
            }
            _ => {}
        }
    }

    Ok(out)
}

fn lookahead_tool_use_id(blocks: &[sonic_rs::Value], start: usize) -> Option<String> {
    for block in blocks.iter().skip(start) {
        let Some(obj) = block.as_object() else {
            continue;
        };
        if obj.get(&"type").and_then(|v| v.as_str()) != Some("tool_use") {
            continue;
        }
        let idv = obj.get(&"id").and_then(|v| v.as_str()).unwrap_or("").trim();
        if !idv.is_empty() {
            return Some(idv.to_string());
        }
    }
    None
}
