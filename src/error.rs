use axum::{
    Json,
    http::StatusCode,
    response::{IntoResponse, Response},
};
use serde::Serialize;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum AppError {
    #[error("配置错误: {0}")]
    Config(String),

    #[error("未授权: {0}")]
    Unauthorized(String),

    #[error("参数错误: {0}")]
    BadRequest(String),

    #[error("后端请求失败: {0}")]
    Backend(String),

    #[error(transparent)]
    Io(#[from] std::io::Error),

    #[error(transparent)]
    Anyhow(#[from] anyhow::Error),
}

#[derive(Debug, Serialize)]
struct ErrorBody {
    error: ErrorBodyInner,
}

#[derive(Debug, Serialize)]
struct ErrorBodyInner {
    message: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    r#type: Option<String>,
}

impl AppError {
    pub fn config(message: impl Into<String>) -> Self {
        Self::Config(message.into())
    }

    pub fn unauthorized(message: impl Into<String>) -> Self {
        Self::Unauthorized(message.into())
    }

    pub fn bad_request(message: impl Into<String>) -> Self {
        Self::BadRequest(message.into())
    }

    pub fn backend(message: impl Into<String>) -> Self {
        Self::Backend(message.into())
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        let (status, ty) = match self {
            AppError::Unauthorized(_) => {
                (StatusCode::UNAUTHORIZED, Some("unauthorized".to_string()))
            }
            AppError::BadRequest(_) => (StatusCode::BAD_REQUEST, Some("bad_request".to_string())),
            AppError::Config(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Some("config".to_string()),
            ),
            AppError::Backend(_) => (StatusCode::BAD_GATEWAY, Some("backend".to_string())),
            AppError::Io(_) => (StatusCode::INTERNAL_SERVER_ERROR, Some("io".to_string())),
            AppError::Anyhow(_) => (
                StatusCode::INTERNAL_SERVER_ERROR,
                Some("internal".to_string()),
            ),
        };

        let body = ErrorBody {
            error: ErrorBodyInner {
                message: self.to_string(),
                r#type: ty,
            },
        };

        (status, Json(body)).into_response()
    }
}
