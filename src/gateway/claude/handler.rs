use super::convert::to_vertex_request;
use super::response::to_messages_response;
use super::stream::{sse_error_events, ClaudeStreamWriter};
use super::types::MessagesRequest;
use crate::credential::store::Store as CredentialStore;
use crate::gateway::common::retry::should_retry_with_next_token;
use crate::gateway::common::AccountContext;
use crate::logging;
use crate::quota_pool::QuotaPoolManager;
use crate::signature::manager::Manager as SignatureManager;
use crate::util::{id, model as modelutil};
use crate::vertex::client::{ApiError, Endpoint, VertexClient};
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
    pub endpoint: Endpoint,
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
    if state.cfg.client_log_enabled() {
        logging::client_request(method.as_str(), uri.0.path(), &headers, &[]);
    }

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
                if state.cfg.client_log_enabled() {
                    let err = claude_error_value(&e.to_string());
                    logging::client_response(status.as_u16(), start.elapsed(), Some(&err));
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
            .fetch_available_models(&state.endpoint, &project_id, &acc.access_token, &acc.email)
            .await
        {
            Ok(v) => {
                models = Some(v.models);
                last_err = None;
                break;
            }
            Err(e) => {
                tracing::warn!(error = ?e, "fetchAvailableModels 失败");
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
        if state.cfg.client_log_enabled() {
            let err = claude_error_value(&msg);
            logging::client_response(status.as_u16(), start.elapsed(), Some(&err));
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
    if state.cfg.client_log_enabled()
        && let Ok(v) = sonic_rs::to_value(&out)
    {
        logging::client_response(StatusCode::OK.as_u16(), start.elapsed(), Some(&v));
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
    if state.cfg.client_log_enabled() {
        logging::client_request(method.as_str(), uri.0.path(), &headers, body.as_ref());
    }

    let req: MessagesRequest = match sonic_rs::from_slice(body.as_ref()) {
        Ok(v) => v,
        Err(_) => {
            if state.cfg.client_log_enabled() {
                let err = claude_error_value("请求 JSON 解析失败，请检查请求体格式。");
                logging::client_response(
                    StatusCode::BAD_REQUEST.as_u16(),
                    start.elapsed(),
                    Some(&err),
                );
            }
            return claude_error(
                StatusCode::BAD_REQUEST,
                "请求 JSON 解析失败，请检查请求体格式。",
            );
        }
    };

    let placeholder = AccountContext {
        project_id: id::project_id(),
        session_id: id::session_id(),
        access_token: String::new(),
        email: String::new(),
    };

    let (mut vreq, request_id) = match to_vertex_request(&state.cfg, &state.sig_mgr, &req, &placeholder).await {
        Ok(v) => v,
        Err(e) => {
            if state.cfg.client_log_enabled() {
                let err = claude_error_value(&e.to_string());
                logging::client_response(
                    StatusCode::BAD_REQUEST.as_u16(),
                    start.elapsed(),
                    Some(&err),
                );
            }
            return claude_error(StatusCode::BAD_REQUEST, &e.to_string());
        }
    };

    let model = req.model.clone();
    let is_stream = req.stream;
    drop(req);

    let mut attempts = state.store.enabled_count().await;
    if attempts < 1 {
        attempts = 1;
    }

    if is_stream {
        return handle_stream_with_retry(
            state,
            vreq,
            request_id,
            model,
            attempts,
            start,
        )
        .await;
    }

    let mut last_err: Option<ApiError> = None;
    let mut vresp = None;
    let mut used_sessions: HashSet<String> = HashSet::new();

    for _ in 0..attempts {
        let acc = match state
            .store
            .get_token_for_model_excluding(&model, &state.quota_pool, &used_sessions)
            .await
        {
            Ok(v) => v,
            Err(e) => {
                if state.cfg.client_log_enabled() {
                    let err = claude_error_value(&e.to_string());
                    logging::client_response(
                        StatusCode::SERVICE_UNAVAILABLE.as_u16(),
                        start.elapsed(),
                        Some(&err),
                    );
                }
                return claude_error(StatusCode::SERVICE_UNAVAILABLE, &e.to_string());
            }
        };
        used_sessions.insert(acc.session_id.clone());
        let project_id = if acc.project_id.is_empty() {
            id::project_id()
        } else {
            acc.project_id.clone()
        };

        vreq.project = project_id;
        vreq.request.session_id = acc.session_id;

        match state
            .vertex
            .generate_content(&state.endpoint, &acc.access_token, &vreq, &acc.email)
            .await
        {
            Ok(v) => {
                vresp = Some(v);
                last_err = None;
                break;
            }
            Err(e) => {
                let retry = should_retry_with_next_token(&e);
                last_err = Some(e);
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
        let msg = last_err
            .as_ref()
            .map(|e| e.to_string())
            .unwrap_or_else(|| "后端请求失败".to_string());
        if state.cfg.client_log_enabled() {
            let err = claude_error_value(&msg);
            logging::client_response(status.as_u16(), start.elapsed(), Some(&err));
        }
        return claude_error(status, &msg);
    };

    let out = to_messages_response(&vresp, &request_id, &model, &state.sig_mgr).await;
    if state.cfg.client_log_enabled()
        && let Ok(v) = sonic_rs::to_value(&out)
    {
        logging::client_response(StatusCode::OK.as_u16(), start.elapsed(), Some(&v));
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

    tokio::spawn(async move {
        let client_log = state.cfg.client_log_enabled();
        let backend_log = state.cfg.backend_log_enabled();

        let mut last_err: Option<ApiError> = None;
        let mut resp = None;
        let mut used_sessions: HashSet<String> = HashSet::new();

        for _ in 0..attempts {
            let acc = match state
                .store
                .get_token_for_model_excluding(&model, &state.quota_pool, &used_sessions)
                .await
            {
                Ok(v) => v,
                Err(e) => {
                    if client_log {
                        let err = claude_error_value(&e.to_string());
                        logging::client_stream_response(
                            StatusCode::OK.as_u16(),
                            started_at.elapsed(),
                            &[err],
                        );
                    }
                    send_sse_error(&tx, &e.to_string()).await;
                    return;
                }
            };
            used_sessions.insert(acc.session_id.clone());

            let project_id = if acc.project_id.is_empty() {
                id::project_id()
            } else {
                acc.project_id.clone()
            };
            vreq.project = project_id;
            vreq.request.session_id = acc.session_id;

            match state
                .vertex
                .generate_content_stream(&state.endpoint, &acc.access_token, &vreq, &acc.email)
                .await
            {
                Ok(r) => {
                    resp = Some(r);
                    last_err = None;
                    break;
                }
                Err(e) => {
                    let retry = should_retry_with_next_token(&e);
                    last_err = Some(e);
                    if !retry {
                        break;
                    }
                }
            }
        }

        let Some(resp) = resp else {
            let msg = last_err
                .as_ref()
                .map(|e| e.to_string())
                .unwrap_or_else(|| "后端请求失败".to_string());
            if client_log {
                let err = claude_error_value(&msg);
                logging::client_stream_response(
                    StatusCode::OK.as_u16(),
                    started_at.elapsed(),
                    &[err],
                );
            }
            send_sse_error(&tx, &msg).await;
            return;
        };

        let mut writer = ClaudeStreamWriter::new(request_id.clone(), model.clone());
        writer.set_log_enabled(client_log);

        let build_merged = backend_log;
        let parse_res = crate::vertex::stream::parse_stream_with_result(
            resp,
            |data| {
                let mut events: Vec<String> = Vec::new();
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
                async move {
                    // 先发事件（低延迟），再写签名缓存（不影响首包）。
                    for ev in events {
                        let name = claude_sse_event_name(&ev);
                        if tx
                            .send(Ok(Event::default().event(name).data(ev)))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                    for s in saves {
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
                    Ok(())
                }
            },
            build_merged,
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
        for ev in writer.finish(output_tokens, stop_reason) {
            if client_disconnected {
                continue;
            }
            let name = claude_sse_event_name(&ev);
            if tx
                .send(Ok(Event::default().event(name).data(ev)))
                .await
                .is_err()
            {
                client_disconnected = true;
            }
        }

        let duration = started_at.elapsed();
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
    });

    Sse::new(ReceiverStream::new(rx)).into_response()
}

async fn send_sse_error(tx: &mpsc::Sender<Result<Event, Infallible>>, msg: &str) {
    for ev in sse_error_events(msg) {
        let name = claude_sse_event_name(&ev);
        let _ = tx.send(Ok(Event::default().event(name).data(ev))).await;
    }
}

fn claude_sse_event_name(json: &str) -> &'static str {
    const PREFIX: &str = "{\"type\":\"";
    let Some(rest) = json.strip_prefix(PREFIX) else {
        return "message";
    };
    let Some(end) = rest.find('"') else {
        return "message";
    };
    match &rest[..end] {
        "message_start" => "message_start",
        "content_block_start" => "content_block_start",
        "content_block_delta" => "content_block_delta",
        "content_block_stop" => "content_block_stop",
        "message_delta" => "message_delta",
        "message_stop" => "message_stop",
        "error" => "error",
        _ => "message",
    }
}

fn claude_error(status: StatusCode, msg: &str) -> Response {
    let encoded = sonic_rs::to_string(msg).unwrap_or_else(|_| "\"\"".to_string());
    let body = format!("{{\"type\":\"error\",\"error\":{{\"type\":\"api_error\",\"message\":{encoded}}}}}");
    (
        status,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response()
}

fn claude_error_value(msg: &str) -> sonic_rs::Value {
    let mut inner = sonic_rs::Object::new();
    inner.insert("type", "api_error");
    inner.insert("message", msg);

    let mut outer = sonic_rs::Object::new();
    outer.insert("type", "error");
    outer.insert("error", inner);
    outer.into_value()
}
