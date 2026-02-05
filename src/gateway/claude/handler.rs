use super::convert::to_vertex_request;
use super::response::to_messages_response;
use super::stream::{ClaudeStreamWriter, sse_error_events};
use super::types::MessagesRequest;
use crate::credential::store::Store as CredentialStore;
use crate::gateway::common::AccountContext;
use crate::gateway::common::auth_retry::is_auth_failure;
use crate::gateway::common::retry::{
    MODEL_CAPACITY_EXHAUSTED_CLIENT_MESSAGE, MODEL_CAPACITY_EXHAUSTED_MAX_RETRIES,
    should_retry_with_next_token,
};
use crate::logging;
use crate::quota_pool::QuotaPoolManager;
use crate::runtime_config;
use crate::signature::manager::Manager as SignatureManager;
use crate::util::{id, model as modelutil};
use crate::vertex::client::{ApiError, VertexClient};
use axum::Json;
use axum::body::Bytes;
use axum::extract::OriginalUri;
use axum::extract::State;
use axum::http::StatusCode;
use axum::http::{HeaderMap, Method};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use serde::Serialize;
use std::collections::HashSet;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

#[derive(Clone)]
pub struct ClaudeState {
    pub cfg: crate::config::Config,
    pub vertex: VertexClient,
    pub store: Arc<CredentialStore>,
    pub quota_pool: Arc<QuotaPoolManager>,
    pub sig_mgr: SignatureManager,
}

pub async fn handle_list_models(
    State(state): State<Arc<ClaudeState>>,
    method: Method,
    uri: OriginalUri,
    headers: HeaderMap,
) -> Response {
    let start = Instant::now();
    let log_level = state.cfg.log_level();
    if log_level.client_enabled() {
        if log_level.raw_enabled() {
            logging::client_request_raw(method.as_str(), uri.0.path(), &headers, &[]);
        } else {
            logging::client_request(method.as_str(), uri.0.path(), &headers, &[]);
        }
    }

    let endpoint = runtime_config::current_endpoint();
    let mut attempts = state.store.enabled_count().await;
    if attempts < 1 {
        attempts = 1;
    }

    let mut last_err: Option<ApiError> = None;
    let mut models = None;

    for _ in 0..attempts {
        let acc = match state.store.get_token().await {
            Ok(v) => v,
            Err(e) => {
                let status = StatusCode::SERVICE_UNAVAILABLE;
                if log_level.client_enabled() {
                    if log_level.raw_enabled() {
                        let body = claude_error_body(&e.to_string());
                        logging::client_response_raw(
                            status.as_u16(),
                            start.elapsed(),
                            body.as_bytes(),
                        );
                    } else {
                        let err = claude_error_value(&e.to_string());
                        logging::client_response(status.as_u16(), start.elapsed(), Some(&err));
                    }
                }
                return claude_error(status, &e.to_string());
            }
        };

        let project_id = if acc.project_id.is_empty() {
            id::project_id()
        } else {
            acc.project_id.clone()
        };

        match state
            .vertex
            .fetch_available_models(&endpoint, &project_id, &acc.access_token, &acc.email)
            .await
        {
            Ok(v) => {
                models = Some(v.models);
                last_err = None;
                break;
            }
            Err(e) => {
                tracing::warn!(error = ?e, "fetchAvailableModels 失败");
                // 认证失败：立即切换到下一个凭证，同时后台触发刷新（不阻塞请求路径）。
                if is_auth_failure(&e) {
                    state
                        .store
                        .trigger_background_refresh(acc.session_id.clone(), state.cfg.clone());
                }
                let retry = should_retry_with_next_token(&e);
                last_err = Some(e);
                if !retry {
                    break;
                }
            }
        }
    }

    let Some(models) = models else {
        let status = last_err
            .as_ref()
            .and_then(|e| e.status())
            .and_then(|s| StatusCode::from_u16(s).ok())
            .unwrap_or(StatusCode::SERVICE_UNAVAILABLE);
        let msg = last_err
            .as_ref()
            .map(|e| e.to_string())
            .unwrap_or_else(|| "后端请求失败".to_string());
        if log_level.client_enabled() {
            if log_level.raw_enabled() {
                let body = claude_error_body(&msg);
                logging::client_response_raw(status.as_u16(), start.elapsed(), body.as_bytes());
            } else {
                let err = claude_error_value(&msg);
                logging::client_response(status.as_u16(), start.elapsed(), Some(&err));
            }
        }
        return claude_error(status, &msg);
    };

    #[derive(Serialize)]
    struct ModelItem {
        id: String,
        #[serde(rename = "type")]
        typ: String,
        #[serde(skip_serializing_if = "String::is_empty", default)]
        display_name: String,
    }

    #[derive(Serialize)]
    struct ModelListResponse {
        data: Vec<ModelItem>,
    }

    let ids = modelutil::build_sorted_model_ids(&models);
    let mut items: Vec<ModelItem> = Vec::with_capacity(ids.len());
    for mid in ids {
        items.push(ModelItem {
            display_name: mid.clone(),
            id: mid,
            typ: "model".to_string(),
        });
    }

    let out = ModelListResponse { data: items };
    if log_level.client_enabled() {
        if log_level.raw_enabled() {
            if let Ok(bytes) = serde_json::to_vec(&out) {
                logging::client_response_raw(StatusCode::OK.as_u16(), start.elapsed(), &bytes);
            }
        } else if let Ok(v) = sonic_rs::to_value(&out) {
            logging::client_response(StatusCode::OK.as_u16(), start.elapsed(), Some(&v));
        }
    }
    (StatusCode::OK, Json(out)).into_response()
}

pub async fn handle_messages(
    State(state): State<Arc<ClaudeState>>,
    method: Method,
    uri: OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let start = Instant::now();
    let log_level = state.cfg.log_level();
    if log_level.client_enabled() {
        if log_level.raw_enabled() {
            logging::client_request_raw(method.as_str(), uri.0.path(), &headers, body.as_ref());
        } else {
            logging::client_request(method.as_str(), uri.0.path(), &headers, body.as_ref());
        }
    }

    let endpoint = runtime_config::current_endpoint();
    let mut req: MessagesRequest = match sonic_rs::from_slice(body.as_ref()) {
        Ok(v) => v,
        Err(_) => {
            if log_level.client_enabled() {
                if log_level.raw_enabled() {
                    let msg = "请求 JSON 解析失败，请检查请求体格式。";
                    let body = claude_error_body(msg);
                    logging::client_response_raw(
                        StatusCode::BAD_REQUEST.as_u16(),
                        start.elapsed(),
                        body.as_bytes(),
                    );
                } else {
                    let err = claude_error_value("请求 JSON 解析失败，请检查请求体格式。");
                    logging::client_response(
                        StatusCode::BAD_REQUEST.as_u16(),
                        start.elapsed(),
                        Some(&err),
                    );
                }
            }
            return claude_error(
                StatusCode::BAD_REQUEST,
                "请求 JSON 解析失败，请检查请求体格式。",
            );
        }
    };

    // 模型 ID 映射：允许客户端使用自定义模型名，后端自动替换为原始模型名。
    req.model = runtime_config::map_client_model_id(&req.model);

    let placeholder = AccountContext {
        project_id: id::project_id(),
        session_id: id::session_id(),
        access_token: String::new(),
        email: String::new(),
    };

    let (mut vreq, request_id) =
        match to_vertex_request(&state.cfg, &state.sig_mgr, &req, &placeholder).await {
            Ok(v) => v,
            Err(e) => {
                if log_level.client_enabled() {
                    if log_level.raw_enabled() {
                        let body = claude_error_body(&e.to_string());
                        logging::client_response_raw(
                            StatusCode::BAD_REQUEST.as_u16(),
                            start.elapsed(),
                            body.as_bytes(),
                        );
                    } else {
                        let err = claude_error_value(&e.to_string());
                        logging::client_response(
                            StatusCode::BAD_REQUEST.as_u16(),
                            start.elapsed(),
                            Some(&err),
                        );
                    }
                }
                return claude_error(StatusCode::BAD_REQUEST, &e.to_string());
            }
        };

    let model = req.model.clone();
    let is_claude_model = modelutil::is_claude(&model);
    // Claude 模型始终走流式，避免非流式路径产生不一致行为。
    let is_stream = req.stream || is_claude_model;
    drop(req);

    let mut attempts = state.store.enabled_count().await;
    if attempts < 1 {
        attempts = 1;
    }

    if is_stream {
        return handle_stream_with_retry(state, vreq, request_id, model, attempts, start).await;
    }

    let mut last_err: Option<ApiError> = None;
    let mut vresp = None;
    let mut used_sessions: HashSet<String> = HashSet::new();
    let mut model_capacity_failures = 0usize;

    for _ in 0..attempts {
        let acc = match state
            .store
            .get_token_for_model_excluding(&model, &state.quota_pool, &used_sessions)
            .await
        {
            Ok(v) => v,
            Err(e) => {
                if log_level.client_enabled() {
                    if log_level.raw_enabled() {
                        let body = claude_error_body(&e.to_string());
                        logging::client_response_raw(
                            StatusCode::SERVICE_UNAVAILABLE.as_u16(),
                            start.elapsed(),
                            body.as_bytes(),
                        );
                    } else {
                        let err = claude_error_value(&e.to_string());
                        logging::client_response(
                            StatusCode::SERVICE_UNAVAILABLE.as_u16(),
                            start.elapsed(),
                            Some(&err),
                        );
                    }
                }
                return claude_error(StatusCode::SERVICE_UNAVAILABLE, &e.to_string());
            }
        };
        let session_id = acc.session_id.clone();
        used_sessions.insert(session_id.clone());
        let project_id = if acc.project_id.is_empty() {
            id::project_id()
        } else {
            acc.project_id.clone()
        };

        vreq.project = project_id;
        vreq.request.session_id = acc.session_id;

        match state
            .vertex
            .generate_content(&endpoint, &acc.access_token, &vreq, &acc.email)
            .await
        {
            Ok(v) => {
                vresp = Some(v);
                last_err = None;
                break;
            }
            Err(e) => {
                // 认证失败：立即切换到下一个凭证，同时后台触发刷新（不阻塞请求路径）。
                if is_auth_failure(&e) {
                    state
                        .store
                        .trigger_background_refresh(session_id.clone(), state.cfg.clone());
                }
                if e.is_model_capacity_exhausted() {
                    model_capacity_failures += 1;
                } else {
                    model_capacity_failures = 0;
                }
                let retry = should_retry_with_next_token(&e);
                last_err = Some(e);
                if model_capacity_failures >= MODEL_CAPACITY_EXHAUSTED_MAX_RETRIES {
                    break;
                }
                if !retry {
                    break;
                }
            }
        }
    }

    let Some(vresp) = vresp else {
        let status = last_err
            .as_ref()
            .and_then(|e| e.status())
            .and_then(|s| StatusCode::from_u16(s).ok())
            .unwrap_or(StatusCode::SERVICE_UNAVAILABLE);
        let mut msg = last_err
            .as_ref()
            .map(|e| e.to_string())
            .unwrap_or_else(|| "后端请求失败".to_string());
        if model_capacity_failures >= MODEL_CAPACITY_EXHAUSTED_MAX_RETRIES
            && last_err
                .as_ref()
                .is_some_and(|e| e.is_model_capacity_exhausted())
        {
            msg = MODEL_CAPACITY_EXHAUSTED_CLIENT_MESSAGE.to_string();
        }
        if log_level.client_enabled() {
            if log_level.raw_enabled() {
                let body = claude_error_body(&msg);
                logging::client_response_raw(status.as_u16(), start.elapsed(), body.as_bytes());
            } else {
                let err = claude_error_value(&msg);
                logging::client_response(status.as_u16(), start.elapsed(), Some(&err));
            }
        }
        return claude_error(status, &msg);
    };

    let out = to_messages_response(&vresp, &request_id, &model, &state.sig_mgr).await;
    if log_level.client_enabled() {
        if log_level.raw_enabled() {
            if let Ok(bytes) = serde_json::to_vec(&out) {
                logging::client_response_raw(StatusCode::OK.as_u16(), start.elapsed(), &bytes);
            }
        } else if let Ok(v) = sonic_rs::to_value(&out) {
            logging::client_response(StatusCode::OK.as_u16(), start.elapsed(), Some(&v));
        }
    }
    (StatusCode::OK, Json(out)).into_response()
}

async fn handle_stream_with_retry(
    state: Arc<ClaudeState>,
    mut vreq: crate::vertex::types::Request,
    request_id: String,
    model: String,
    attempts: usize,
    started_at: Instant,
) -> Response {
    let (tx, rx) = mpsc::channel::<Result<Event, Infallible>>(256);
    let endpoint = runtime_config::current_endpoint();

    tokio::spawn(async move {
        let log_level = state.cfg.log_level();
        let client_log = log_level.client_enabled();
        let backend_log = log_level.backend_enabled();
        let raw_log = log_level.raw_enabled();

        // RAW 模式下用于在“后端响应/客户端响应”之间切换时打印分割线。
        // 0 = none, 1 = backend, 2 = client
        let raw_section = std::sync::Arc::new(std::sync::atomic::AtomicU8::new(0));

        let mut last_err: Option<ApiError> = None;
        let mut resp = None;
        let mut used_sessions: HashSet<String> = HashSet::new();
        let mut model_capacity_failures = 0usize;

        for _ in 0..attempts {
            let acc = match state
                .store
                .get_token_for_model_excluding(&model, &state.quota_pool, &used_sessions)
                .await
            {
                Ok(v) => v,
                Err(e) => {
                    if client_log && !raw_log {
                        let err = claude_error_value(&e.to_string());
                        logging::client_stream_response(
                            StatusCode::OK.as_u16(),
                            started_at.elapsed(),
                            &[err],
                        );
                    }
                    send_sse_error(
                        &tx,
                        &e.to_string(),
                        client_log && raw_log,
                        raw_section.clone(),
                    )
                    .await;
                    return;
                }
            };
            let session_id = acc.session_id.clone();
            used_sessions.insert(session_id.clone());

            let project_id = if acc.project_id.is_empty() {
                id::project_id()
            } else {
                acc.project_id.clone()
            };
            vreq.project = project_id;
            vreq.request.session_id = acc.session_id;

            match state
                .vertex
                .generate_content_stream(&endpoint, &acc.access_token, &vreq, &acc.email)
                .await
            {
                Ok(r) => {
                    resp = Some(r);
                    last_err = None;
                    break;
                }
                Err(e) => {
                    // 认证失败：立即切换到下一个凭证，同时后台触发刷新（不阻塞请求路径）。
                    if is_auth_failure(&e) {
                        state
                            .store
                            .trigger_background_refresh(session_id.clone(), state.cfg.clone());
                    }
                    if e.is_model_capacity_exhausted() {
                        model_capacity_failures += 1;
                    } else {
                        model_capacity_failures = 0;
                    }
                    let retry = should_retry_with_next_token(&e);
                    last_err = Some(e);
                    if model_capacity_failures >= MODEL_CAPACITY_EXHAUSTED_MAX_RETRIES {
                        break;
                    }
                    if !retry {
                        break;
                    }
                }
            }
        }

        let Some(resp) = resp else {
            let mut msg = last_err
                .as_ref()
                .map(|e| e.to_string())
                .unwrap_or_else(|| "后端请求失败".to_string());
            if model_capacity_failures >= MODEL_CAPACITY_EXHAUSTED_MAX_RETRIES
                && last_err
                    .as_ref()
                    .is_some_and(|e| e.is_model_capacity_exhausted())
            {
                msg = MODEL_CAPACITY_EXHAUSTED_CLIENT_MESSAGE.to_string();
            }
            if client_log && !raw_log {
                let err = claude_error_value(&msg);
                logging::client_stream_response(
                    StatusCode::OK.as_u16(),
                    started_at.elapsed(),
                    &[err],
                );
            }
            send_sse_error(&tx, &msg, client_log && raw_log, raw_section.clone()).await;
            return;
        };

        let mut writer = ClaudeStreamWriter::new(request_id.clone(), model.clone());
        writer.set_log_enabled(client_log && !raw_log);

        let backend_raw = backend_log && raw_log;
        let build_merged = backend_log && !raw_log;
        let parse_res = crate::vertex::stream::parse_stream_with_result(
            resp,
            |data| {
                let mut events: Vec<(&'static str, String)> = Vec::new();
                let mut saves: Vec<super::stream::SignatureSave> = Vec::new();

                if let Some(usage) = data.response.usage_metadata.as_ref() {
                    writer.set_input_tokens(usage.prompt_token_count);
                }

                if let Some(cand) = data.response.candidates.first() {
                    for p in &cand.content.parts {
                        let (ev, sv) = writer.process_part(p);
                        events.extend(ev);
                        saves.extend(sv);
                    }
                }

                let tx = tx.clone();
                let sig_mgr = state.sig_mgr.clone();
                let raw_section = raw_section.clone();
                async move {
                    // 先发事件（低延迟），再写签名缓存（不影响首包）。
                    for (event_name, ev) in events {
                        if client_log && raw_log {
                            if raw_section.swap(2, std::sync::atomic::Ordering::Relaxed) != 2 {
                                logging::client_response_divider_raw();
                            }
                            logging::client_stream_event_raw(Some(event_name), &ev);
                        }
                        if tx
                            .send(Ok(Event::default().event(event_name).data(ev)))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    for s in saves {
                        if s.is_image_key {
                            sig_mgr
                                .save_image_key(
                                    s.request_id,
                                    s.tool_call_id,
                                    s.signature,
                                    s.reasoning,
                                    s.model,
                                )
                                .await;
                        } else {
                            sig_mgr
                                .save_owned(
                                    s.request_id,
                                    s.tool_call_id,
                                    s.signature,
                                    s.reasoning,
                                    s.model,
                                )
                                .await;
                        }
                    }
                    Ok(())
                }
            },
            build_merged,
            {
                let raw_section = raw_section.clone();
                move |line| {
                    if !backend_raw {
                        return;
                    }
                    if raw_section.swap(1, std::sync::atomic::Ordering::Relaxed) != 1 {
                        logging::backend_response_divider_raw();
                    }
                    if line.starts_with(b"data:") || line.starts_with(b":") {
                        logging::backend_stream_line_raw(line);
                    }
                }
            },
        )
        .await;

        let stream_result = match parse_res {
            Ok(r) => r,
            Err(e) => e.result,
        };

        let output_tokens = stream_result
            .usage
            .as_ref()
            .map(|u| u.candidates_token_count)
            .unwrap_or(0);

        let stop_reason = if !stream_result.tool_calls.is_empty() {
            "tool_use"
        } else {
            "end_turn"
        };

        let mut client_disconnected = false;
        for (event_name, ev) in writer.finish(output_tokens, stop_reason) {
            if client_disconnected {
                continue;
            }
            if client_log && raw_log {
                if raw_section.swap(2, std::sync::atomic::Ordering::Relaxed) != 2 {
                    logging::client_response_divider_raw();
                }
                logging::client_stream_event_raw(Some(event_name), &ev);
            }
            if tx
                .send(Ok(Event::default().event(event_name).data(ev)))
                .await
                .is_err()
            {
                client_disconnected = true;
            }
        }

        let duration = started_at.elapsed();
        if !raw_log {
            if backend_log {
                logging::backend_stream_response(
                    StatusCode::OK.as_u16(),
                    duration,
                    stream_result.merged_response.as_ref(),
                );
            }
            if client_log {
                let merged = writer.take_merged_events_for_log();
                logging::client_stream_response(StatusCode::OK.as_u16(), duration, &merged);
            }
        }
    });

    Sse::new(ReceiverStream::new(rx)).into_response()
}

async fn send_sse_error(
    tx: &mpsc::Sender<Result<Event, Infallible>>,
    msg: &str,
    raw_log: bool,
    raw_section: std::sync::Arc<std::sync::atomic::AtomicU8>,
) {
    for (event_name, ev) in sse_error_events(msg) {
        if raw_log {
            if raw_section.swap(2, std::sync::atomic::Ordering::Relaxed) != 2 {
                logging::client_response_divider_raw();
            }
            logging::client_stream_event_raw(Some(event_name), &ev);
        }
        let _ = tx
            .send(Ok(Event::default().event(event_name).data(ev)))
            .await;
    }
}

fn claude_error(status: StatusCode, msg: &str) -> Response {
    let body = claude_error_body(msg);
    (
        status,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response()
}

fn claude_error_body(msg: &str) -> String {
    let encoded = sonic_rs::to_string(msg).unwrap_or_else(|_| "\"\"".to_string());
    format!("{{\"type\":\"error\",\"error\":{{\"type\":\"api_error\",\"message\":{encoded}}}}}")
}

#[derive(Serialize)]
struct ClaudeErrorInner<'a> {
    message: &'a str,
    #[serde(rename = "type")]
    typ: &'a str,
}

#[derive(Serialize)]
struct ClaudeErrorEvent<'a> {
    error: ClaudeErrorInner<'a>,
    #[serde(rename = "type")]
    typ: &'a str,
}

fn claude_error_value(msg: &str) -> sonic_rs::Value {
    sonic_rs::to_value(&ClaudeErrorEvent {
        error: ClaudeErrorInner {
            message: msg,
            typ: "api_error",
        },
        typ: "error",
    })
    .unwrap_or_default()
}
