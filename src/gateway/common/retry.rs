use crate::vertex::client::ApiError;

pub fn should_retry_with_next_token(err: &ApiError) -> bool {
    matches!(err.status(), Some(429 | 401 | 403))
}
