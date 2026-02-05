use crate::vertex::client::ApiError;

pub const MODEL_CAPACITY_EXHAUSTED_MAX_RETRIES: usize = 5;
pub const MODEL_CAPACITY_EXHAUSTED_CLIENT_MESSAGE: &str = "模型已过载，请稍后再试";

pub fn should_retry_with_next_token(err: &ApiError) -> bool {
    if err.is_model_capacity_exhausted() {
        return true;
    }
    matches!(err.status(), Some(429 | 401 | 403))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn retry_with_next_token_includes_capacity_exhausted() {
        let err = ApiError::Http {
            status: 503,
            message: "x".to_string(),
            retry_delay: Duration::ZERO,
            disable_token: false,
            model_capacity_exhausted: true,
        };
        assert!(should_retry_with_next_token(&err));
    }

    #[test]
    fn retry_with_next_token_does_not_include_other_503() {
        let err = ApiError::Http {
            status: 503,
            message: "x".to_string(),
            retry_delay: Duration::ZERO,
            disable_token: false,
            model_capacity_exhausted: false,
        };
        assert!(!should_retry_with_next_token(&err));
    }
}
