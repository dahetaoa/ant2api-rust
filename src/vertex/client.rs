use crate::config::Config;
use crate::logging;
use crate::vertex::types::{Request, Response};
use reqwest::header::{AUTHORIZATION, CONTENT_TYPE, HeaderMap, HeaderValue, USER_AGENT};
use sonic_rs::{JsonContainerTrait, JsonValueMutTrait, JsonValueTrait};
use std::collections::HashMap;
use std::time::Duration;
use thiserror::Error;

#[derive(Debug, Clone)]
pub struct Endpoint {
    pub key: String,
    pub host: String,
}

impl Endpoint {
    pub fn stream_url(&self) -> String {
        format!(
            "https://{}/v1internal:streamGenerateContent?alt=sse",
            self.host
        )
    }

    pub fn no_stream_url(&self) -> String {
        format!("https://{}/v1internal:generateContent", self.host)
    }

    pub fn fetch_available_models_url(&self) -> String {
        format!("https://{}/v1internal:fetchAvailableModels", self.host)
    }
}

#[derive(Debug, Error)]
pub enum ApiError {
    #[error("Vertex API 错误 {status}: {message}")]
    Http {
        status: u16,
        message: String,
        retry_delay: Duration,
        disable_token: bool,
        model_capacity_exhausted: bool,
    },

    #[error(transparent)]
    Transport(#[from] reqwest::Error),

    #[error(transparent)]
    Json(#[from] sonic_rs::Error),
}

impl ApiError {
    pub fn status(&self) -> Option<u16> {
        match self {
            Self::Http { status, .. } => Some(*status),
            _ => None,
        }
    }

    pub fn retry_delay(&self) -> Option<Duration> {
        match self {
            Self::Http { retry_delay, .. } if *retry_delay != Duration::ZERO => Some(*retry_delay),
            _ => None,
        }
    }

    pub fn disable_token(&self) -> bool {
        match self {
            Self::Http { disable_token, .. } => *disable_token,
            _ => false,
        }
    }

    pub fn is_model_capacity_exhausted(&self) -> bool {
        matches!(
            self,
            Self::Http {
                model_capacity_exhausted: true,
                ..
            }
        )
    }
}

#[derive(Debug, Clone)]
pub struct VertexClient {
    http: reqwest::Client,
    http_stream: reqwest::Client,
    retry_status_codes: Vec<u16>,
    retry_max_attempts: usize,
    user_agent: String,
    log_level: logging::LogLevel,
}

impl VertexClient {
    pub fn new(cfg: &Config) -> Result<Self, anyhow::Error> {
        // 大多数后端请求维持 HTTP/1.1（拉取模型/非流式）。
        let mut http1_builder = reqwest::Client::builder()
            .pool_max_idle_per_host(10)
            .pool_idle_timeout(Duration::from_secs(90))
            .http1_only();

        // 仅后端流式接口强制使用 HTTP/2（SSE）。
        let mut http2_stream_builder = reqwest::Client::builder()
            .pool_max_idle_per_host(10)
            .pool_idle_timeout(Duration::from_secs(90))
            .http2_prior_knowledge();

        let timeout = if cfg.timeout_ms > 0 {
            Some(Duration::from_millis(cfg.timeout_ms))
        } else {
            None
        };
        if let Some(t) = timeout {
            http1_builder = http1_builder.timeout(t);
            http2_stream_builder = http2_stream_builder.timeout(t);
        }

        if !cfg.proxy.trim().is_empty() {
            // Proxy 不保证可 Clone，这里各自构建一次避免 trait 约束。
            http1_builder = http1_builder.proxy(reqwest::Proxy::all(cfg.proxy.trim())?);
            http2_stream_builder =
                http2_stream_builder.proxy(reqwest::Proxy::all(cfg.proxy.trim())?);
        }

        let http = http1_builder.build()?;
        let http_stream = http2_stream_builder.build()?;
        Ok(Self {
            http,
            http_stream,
            retry_status_codes: cfg.retry_status_codes.clone(),
            retry_max_attempts: cfg.retry_max_attempts.max(1),
            user_agent: cfg.api_user_agent.clone(),
            log_level: cfg.log_level(),
        })
    }

    pub fn build_headers(&self, access_token: &str, _endpoint: &Endpoint) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            USER_AGENT,
            HeaderValue::from_str(&self.user_agent).unwrap_or(HeaderValue::from_static("ant2api")),
        );
        h.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {access_token}"))
                .unwrap_or(HeaderValue::from_static("")),
        );
        h.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        h.insert(
            reqwest::header::ACCEPT_ENCODING,
            HeaderValue::from_static("gzip"),
        );
        h
    }

    pub fn build_stream_headers(&self, access_token: &str, _endpoint: &Endpoint) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(
            USER_AGENT,
            HeaderValue::from_str(&self.user_agent).unwrap_or(HeaderValue::from_static("ant2api")),
        );
        h.insert(
            AUTHORIZATION,
            HeaderValue::from_str(&format!("Bearer {access_token}"))
                .unwrap_or(HeaderValue::from_static("")),
        );
        h.insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
        h
    }

    pub async fn generate_content(
        &self,
        endpoint: &Endpoint,
        access_token: &str,
        req: &Request,
        email: &str,
    ) -> Result<Response, ApiError> {
        let url = endpoint.no_stream_url();
        self.with_retry(|| async {
            let body = sonic_rs::to_vec(req)?;
            let headers = self.build_headers(access_token, endpoint);
            if self.log_level.backend_enabled() {
                if self.log_level.raw_enabled() {
                    logging::backend_request_raw("POST", &url, &headers, &body);
                } else if let Ok(mut v) = sonic_rs::from_slice::<sonic_rs::Value>(&body) {
                    if let Some(obj) = v.as_object_mut() {
                        obj.insert("account", sonic_rs::Value::from(email));
                        if let Ok(log_body) = sonic_rs::to_vec(&v) {
                            logging::backend_request("POST", &url, &headers, &log_body);
                        } else {
                            logging::backend_request("POST", &url, &headers, &body);
                        }
                    } else {
                        logging::backend_request("POST", &url, &headers, &body);
                    }
                } else {
                    logging::backend_request("POST", &url, &headers, &body);
                }
            }
            let start = std::time::Instant::now();
            let resp = self
                .http_stream
                .post(url.clone())
                .headers(headers)
                .body(body)
                .send()
                .await?;

            let status = resp.status();
            let bytes = resp.bytes().await?;
            if self.log_level.backend_enabled() {
                if self.log_level.raw_enabled() {
                    logging::backend_response_raw(status.as_u16(), start.elapsed(), &bytes);
                } else {
                    logging::backend_response(status.as_u16(), start.elapsed(), &bytes);
                }
            }
            if !status.is_success() {
                return Err(extract_error_details(status.as_u16(), &bytes));
            }
            Ok(sonic_rs::from_slice::<Response>(&bytes)?)
        })
        .await
    }

    pub async fn generate_content_stream(
        &self,
        endpoint: &Endpoint,
        access_token: &str,
        req: &Request,
        email: &str,
    ) -> Result<reqwest::Response, ApiError> {
        let url = endpoint.stream_url();
        self.with_retry(|| async {
            let body = sonic_rs::to_vec(req)?;
            let headers = self.build_stream_headers(access_token, endpoint);
            if self.log_level.backend_enabled() {
                if self.log_level.raw_enabled() {
                    logging::backend_request_raw("POST", &url, &headers, &body);
                } else if let Ok(mut v) = sonic_rs::from_slice::<sonic_rs::Value>(&body) {
                    if let Some(obj) = v.as_object_mut() {
                        obj.insert("account", sonic_rs::Value::from(email));
                        if let Ok(log_body) = sonic_rs::to_vec(&v) {
                            logging::backend_request("POST", &url, &headers, &log_body);
                        } else {
                            logging::backend_request("POST", &url, &headers, &body);
                        }
                    } else {
                        logging::backend_request("POST", &url, &headers, &body);
                    }
                } else {
                    logging::backend_request("POST", &url, &headers, &body);
                }
            }
            let start = std::time::Instant::now();
            let resp = self
                .http_stream
                .post(url.clone())
                .headers(headers)
                .body(body)
                .send()
                .await?;

            if !resp.status().is_success() {
                let status = resp.status();
                let bytes = resp.bytes().await?;
                if self.log_level.backend_enabled() {
                    if self.log_level.raw_enabled() {
                        logging::backend_response_raw(status.as_u16(), start.elapsed(), &bytes);
                    } else {
                        logging::backend_response(status.as_u16(), start.elapsed(), &bytes);
                    }
                }
                return Err(extract_error_details(status.as_u16(), &bytes));
            }
            Ok(resp)
        })
        .await
    }

    pub async fn fetch_available_models(
        &self,
        endpoint: &Endpoint,
        project: &str,
        access_token: &str,
        email: &str,
    ) -> Result<AvailableModelsResponse, ApiError> {
        let url = endpoint.fetch_available_models_url();
        let body = sonic_rs::to_vec(&serde_payload_project(project))?;
        let headers = self.build_headers(access_token, endpoint);
        let start = std::time::Instant::now();
        if self.log_level.backend_enabled() {
            if self.log_level.raw_enabled() {
                logging::backend_request_raw("POST", &url, &headers, &body);
            } else if let Ok(mut v) = sonic_rs::from_slice::<sonic_rs::Value>(&body) {
                if let Some(obj) = v.as_object_mut() {
                    obj.insert("account", sonic_rs::Value::from(email));
                    if let Ok(log_body) = sonic_rs::to_vec(&v) {
                        logging::backend_request("POST", &url, &headers, &log_body);
                    } else {
                        logging::backend_request("POST", &url, &headers, &body);
                    }
                } else {
                    logging::backend_request("POST", &url, &headers, &body);
                }
            } else {
                logging::backend_request("POST", &url, &headers, &body);
            }
        }

        let resp = self
            .http
            .post(url)
            .headers(headers)
            .body(body)
            .send()
            .await?;

        let status = resp.status();
        let bytes = resp.bytes().await?;
        if self.log_level.backend_enabled() {
            if self.log_level.raw_enabled() {
                logging::backend_response_raw(status.as_u16(), start.elapsed(), &bytes);
            } else {
                logging::backend_response(status.as_u16(), start.elapsed(), &bytes);
            }
        }
        if !status.is_success() {
            return Err(extract_error_details(status.as_u16(), &bytes));
        }
        Ok(sonic_rs::from_slice::<AvailableModelsResponse>(&bytes)?)
    }

    async fn with_retry<F, Fut, T>(&self, mut op: F) -> Result<T, ApiError>
    where
        F: FnMut() -> Fut,
        Fut: std::future::Future<Output = Result<T, ApiError>>,
    {
        let mut last_err: Option<ApiError> = None;

        for attempt in 0..self.retry_max_attempts {
            match op().await {
                Ok(v) => return Ok(v),
                Err(err) => {
                    let status = err.status();
                    // 非 API 错误（例如网络错误/JSON 错误）：直接返回。
                    if status.is_none() {
                        return Err(err);
                    }

                    // 401：不重试（与 Go 版本一致）。
                    if status == Some(401) {
                        return Err(err);
                    }

                    last_err = Some(err);

                    let Some(status) = status else {
                        break;
                    };
                    let should_retry = self.retry_status_codes.contains(&status);
                    if !should_retry || attempt + 1 == self.retry_max_attempts {
                        break;
                    }

                    let delay = last_err
                        .as_ref()
                        .and_then(|e| e.retry_delay())
                        .unwrap_or_else(|| {
                            let ms = (1_000u64 * (attempt as u64 + 1)).min(5_000);
                            Duration::from_millis(ms)
                        });
                    tokio::time::sleep(delay).await;
                }
            }
        }

        Err(last_err.unwrap_or(ApiError::Http {
            status: 500,
            message: "未知错误".to_string(),
            retry_delay: Duration::ZERO,
            disable_token: false,
            model_capacity_exhausted: false,
        }))
    }
}

#[derive(Debug, Clone, serde::Serialize)]
struct ProjectPayload<'a> {
    project: &'a str,
}

fn serde_payload_project(project: &str) -> ProjectPayload<'_> {
    ProjectPayload { project }
}

#[derive(Debug, Clone, serde::Deserialize)]
pub struct AvailableModelsResponse {
    pub models: HashMap<String, sonic_rs::Value>,
}

fn extract_error_details(status: u16, body: &[u8]) -> ApiError {
    #[derive(Debug, serde::Deserialize)]
    struct ErrResp {
        error: ErrInner,
    }

    #[derive(Debug, serde::Deserialize)]
    struct ErrInner {
        #[serde(default)]
        code: Option<sonic_rs::Value>,
        #[serde(default)]
        message: String,
        #[serde(default)]
        status: String,
        #[serde(default)]
        details: Vec<ErrDetail>,
    }

    #[derive(Debug, serde::Deserialize)]
    struct ErrDetail {
        #[serde(rename = "@type", default)]
        ty: String,
        #[serde(default)]
        retry_delay: String,
        #[serde(default)]
        reason: String,
        #[serde(default)]
        metadata: sonic_rs::Value,
    }

    let mut out_status = status;
    let mut message = "Unknown error".to_string();
    let mut retry_delay = Duration::ZERO;
    let mut disable_token = false;
    let mut model_capacity_exhausted = false;

    if let Ok(err_resp) = sonic_rs::from_slice::<ErrResp>(body) {
        let err = err_resp.error;
        message = err.message;

        if let Some(code) = err.code {
            if let Some(s) = code.as_str() {
                let up = s.to_uppercase();
                match up.as_str() {
                    "RESOURCE_EXHAUSTED" => out_status = 429,
                    "INTERNAL" => out_status = 500,
                    "UNAUTHENTICATED" => {
                        out_status = 401;
                        disable_token = true;
                    }
                    _ => {}
                }
            } else if let Some(i) = code.as_i64() {
                if i > 0 && i <= u16::MAX as i64 {
                    out_status = i as u16;
                }
            } else if let Some(f) = code.as_f64() {
                let i = f as i64;
                if i > 0 && i <= u16::MAX as i64 {
                    out_status = i as u16;
                }
            }
        }

        if out_status == 503
            && err.status == "UNAVAILABLE"
            && message.starts_with("No capacity available for model ")
        {
            for d in &err.details {
                if d.ty == "type.googleapis.com/google.rpc.ErrorInfo"
                    && d.reason == "MODEL_CAPACITY_EXHAUSTED"
                    && d.metadata
                        .as_object()
                        .and_then(|m| m.get(&"model"))
                        .and_then(|v| v.as_str())
                        .is_some()
                {
                    model_capacity_exhausted = true;
                    break;
                }
            }
        }

        for d in err.details {
            if d.ty.contains("RetryInfo")
                && let Some(delay) = parse_retry_delay_seconds(&d.retry_delay)
            {
                retry_delay = delay;
            }
        }
    }

    ApiError::Http {
        status: out_status,
        message,
        retry_delay,
        disable_token,
        model_capacity_exhausted,
    }
}

fn parse_retry_delay_seconds(s: &str) -> Option<Duration> {
    // 兼容形如 "2s" / "2.5s" / "0.123s"
    let s = s.trim();
    let s = s.strip_suffix('s')?;
    let secs: f64 = s.trim().parse().ok()?;
    if !(secs.is_finite() && secs >= 0.0) {
        return None;
    }
    Some(Duration::from_secs_f64(secs))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_error_details_marks_model_capacity_exhausted() {
        let body = r#"{
            "error": {
                "code": 503,
                "message": "No capacity available for model gemini-3-flash on the server",
                "status": "UNAVAILABLE",
                "details": [
                    {
                        "@type": "type.googleapis.com/google.rpc.ErrorInfo",
                        "reason": "MODEL_CAPACITY_EXHAUSTED",
                        "domain": "cloudcode-pa.googleapis.com",
                        "metadata": {
                            "model": "gemini-3-flash"
                        }
                    }
                ]
            }
        }"#;

        let err = extract_error_details(503, body.as_bytes());
        assert_eq!(err.status(), Some(503));
        assert!(err.is_model_capacity_exhausted());
    }

    #[test]
    fn extract_error_details_requires_full_errorinfo_match() {
        // reason 不匹配：不能被当作“模型过载”来处理。
        let body = r#"{
            "error": {
                "code": 503,
                "message": "No capacity available for model gemini-3-flash on the server",
                "status": "UNAVAILABLE",
                "details": [
                    {
                        "@type": "type.googleapis.com/google.rpc.ErrorInfo",
                        "reason": "SOME_OTHER_REASON",
                        "domain": "cloudcode-pa.googleapis.com",
                        "metadata": {
                            "model": "gemini-3-flash"
                        }
                    }
                ]
            }
        }"#;

        let err = extract_error_details(503, body.as_bytes());
        assert_eq!(err.status(), Some(503));
        assert!(!err.is_model_capacity_exhausted());
    }

    #[test]
    fn extract_error_details_allows_other_domains() {
        let body = r#"{
            "error": {
                "code": 503,
                "message": "No capacity available for model gemini-3-flash on the server",
                "status": "UNAVAILABLE",
                "details": [
                    {
                        "@type": "type.googleapis.com/google.rpc.ErrorInfo",
                        "reason": "MODEL_CAPACITY_EXHAUSTED",
                        "domain": "some.other.domain",
                        "metadata": {
                            "model": "gemini-3-flash"
                        }
                    }
                ]
            }
        }"#;

        let err = extract_error_details(503, body.as_bytes());
        assert_eq!(err.status(), Some(503));
        assert!(err.is_model_capacity_exhausted());
    }
}
