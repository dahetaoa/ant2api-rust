use super::convert::{convert_usage, to_chat_completion, to_models_response, to_vertex_request};
use super::stream::{StreamWriter, now_unix, sse_error_events};
use super::types::ChatRequest;
use crate::gateway::common::AccountContext;
use crate::gateway::common::retry::should_retry_with_next_token;
use crate::logging;
use crate::runtime_config;
use crate::util::id;
use crate::vertex::client::ApiError;
use axum::Json;
use axum::body::Bytes;
use axum::extract::OriginalUri;
use axum::extract::State;
use axum::http::StatusCode;
use axum::http::{HeaderMap, Method};
use axum::response::sse::{Event, Sse};
use axum::response::{IntoResponse, Response};
use std::collections::HashSet;
use std::convert::Infallible;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

/// OpenAI 与 Claude 网关共享同一套后端转发状态（字段一致，避免路由层 state 类型冲突）。
pub type OpenAIState = crate::gateway::claude::ClaudeState;

pub async fn handle_list_models(
    State(state): State<Arc<OpenAIState>>,
    method: Method,
    uri: OriginalUri,
    headers: HeaderMap,
) -> Response {
    let start = Instant::now();
    if state.cfg.client_log_enabled() {
        logging::client_request(method.as_str(), uri.0.path(), &headers, &[]);
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
                if state.cfg.client_log_enabled() {
                    let err = openai_error_value(&e.to_string());
                    logging::client_response(status.as_u16(), start.elapsed(), Some(&err));
                }
                return openai_error(status, &e.to_string());
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
            let err = openai_error_value(&msg);
            logging::client_response(status.as_u16(), start.elapsed(), Some(&err));
        }
        return openai_error(status, &msg);
    };

    let out = to_models_response(&models);
    if state.cfg.client_log_enabled()
        && let Ok(v) = sonic_rs::to_value(&out)
    {
        logging::client_response(StatusCode::OK.as_u16(), start.elapsed(), Some(&v));
    }
    (StatusCode::OK, Json(out)).into_response()
}

pub async fn handle_chat_completions(
    State(state): State<Arc<OpenAIState>>,
    method: Method,
    uri: OriginalUri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    let start = Instant::now();
    if state.cfg.client_log_enabled() {
        logging::client_request(method.as_str(), uri.0.path(), &headers, body.as_ref());
    }

    let endpoint = runtime_config::current_endpoint();
    let mut req: ChatRequest = match sonic_rs::from_slice(body.as_ref()) {
        Ok(v) => v,
        Err(_) => {
            if state.cfg.client_log_enabled() {
                let err = openai_error_value("请求 JSON 解析失败，请检查请求体格式。");
                logging::client_response(
                    StatusCode::BAD_REQUEST.as_u16(),
                    start.elapsed(),
                    Some(&err),
                );
            }
            return openai_error(
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

    let (mut vreq, request_id) =
        match to_vertex_request(&state.cfg, &state.sig_mgr, &mut req, &placeholder).await {
            Ok(v) => v,
            Err(e) => {
                if state.cfg.client_log_enabled() {
                    let err = openai_error_value(&e.to_string());
                    logging::client_response(
                        StatusCode::BAD_REQUEST.as_u16(),
                        start.elapsed(),
                        Some(&err),
                    );
                }
                return openai_error(StatusCode::BAD_REQUEST, &e.to_string());
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
        return handle_stream_with_retry(state, vreq, request_id, model, attempts, start).await;
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
                    let err = openai_error_value(&e.to_string());
                    logging::client_response(
                        StatusCode::SERVICE_UNAVAILABLE.as_u16(),
                        start.elapsed(),
                        Some(&err),
                    );
                }
                return openai_error(StatusCode::SERVICE_UNAVAILABLE, &e.to_string());
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
            .generate_content(&endpoint, &acc.access_token, &vreq, &acc.email)
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
            let err = openai_error_value(&msg);
            logging::client_response(status.as_u16(), start.elapsed(), Some(&err));
        }
        return openai_error(status, &msg);
    };

    let out = to_chat_completion(&vresp, &model, &request_id, &state.sig_mgr).await;

    if state.cfg.client_log_enabled()
        && let Ok(v) = sonic_rs::to_value(&out)
    {
        logging::client_response(StatusCode::OK.as_u16(), start.elapsed(), Some(&v));
    }
    (StatusCode::OK, Json(out)).into_response()
}

async fn handle_stream_with_retry(
    state: Arc<OpenAIState>,
    mut vreq: crate::vertex::types::Request,
    request_id: String,
    model: String,
    attempts: usize,
    started_at: Instant,
) -> Response {
    let (tx, rx) = mpsc::channel::<Result<Event, Infallible>>(256);
    let endpoint = runtime_config::current_endpoint();

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
                        let err = openai_error_value(&e.to_string());
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
                .generate_content_stream(&endpoint, &acc.access_token, &vreq, &acc.email)
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
                let err = openai_error_value(&msg);
                logging::client_stream_response(
                    StatusCode::OK.as_u16(),
                    started_at.elapsed(),
                    &[err],
                );
            }
            send_sse_error(&tx, &msg).await;
            return;
        };

        let mut writer = StreamWriter::new(
            id::chat_completion_id(),
            now_unix(),
            model.clone(),
            request_id.clone(),
            client_log,
        );

        let build_merged = backend_log;
        let parse_res = crate::vertex::stream::parse_stream_with_result(
            resp,
            |data| {
                let mut events: Vec<String> = Vec::new();
                let mut saves: Vec<super::stream::SignatureSave> = Vec::new();

                if let Some(cand) = data.response.candidates.first() {
                    for p in &cand.content.parts {
                        let (ev, sv) = writer.process_part(p);
                        events.extend(ev);
                        saves.extend(sv);
                    }
                    if !cand.finish_reason.is_empty() {
                        events.extend(writer.flush_tool_calls());
                    }
                }

                let tx = tx.clone();
                let sig_mgr = state.sig_mgr.clone();
                async move {
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
                    for ev in events {
                        if tx.send(Ok(Event::default().data(ev))).await.is_err() {
                            break;
                        }
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

        let finish = if stream_result.finish_reason.is_empty() {
            "stop".to_string()
        } else {
            stream_result.finish_reason
        };
        let usage = convert_usage(stream_result.usage.as_ref());

        // 即使客户端已断开，也尽量生成完整的日志（调试期更重要）。
        let mut client_disconnected = false;
        for ev in writer.finish_events(&finish, usage) {
            if client_disconnected {
                continue;
            }
            if tx.send(Ok(Event::default().data(ev))).await.is_err() {
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
        let _ = tx.send(Ok(Event::default().data(ev))).await;
    }
}

fn openai_error(status: StatusCode, msg: &str) -> Response {
    let encoded = sonic_rs::to_string(msg).unwrap_or_else(|_| "\"\"".to_string());
    let body = format!("{{\"error\":{{\"message\":{encoded},\"type\":\"server_error\"}}}}");
    (
        status,
        [(axum::http::header::CONTENT_TYPE, "application/json")],
        body,
    )
        .into_response()
}

fn openai_error_value(msg: &str) -> sonic_rs::Value {
    let mut err = sonic_rs::Object::new();
    err.insert("message", msg);
    err.insert("type", "server_error");
    let mut outer = sonic_rs::Object::new();
    outer.insert("error", err);
    outer.into_value()
}
