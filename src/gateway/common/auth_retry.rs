use crate::vertex::client::ApiError;

/// 检测是否为认证失败错误（需要触发后台刷新）。
///
/// 目前仅处理 401：通常代表 access_token 过期或已失效。
pub fn is_auth_failure(err: &ApiError) -> bool {
    matches!(err.status(), Some(401))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    #[test]
    fn is_auth_failure_only_matches_401() {
        let err_401 = ApiError::Http {
            status: 401,
            message: "x".to_string(),
            retry_delay: Duration::ZERO,
            disable_token: false,
            model_capacity_exhausted: false,
        };
        let err_403 = ApiError::Http {
            status: 403,
            message: "x".to_string(),
            retry_delay: Duration::ZERO,
            disable_token: false,
            model_capacity_exhausted: false,
        };

        assert!(is_auth_failure(&err_401));
        assert!(!is_auth_failure(&err_403));
    }
}
