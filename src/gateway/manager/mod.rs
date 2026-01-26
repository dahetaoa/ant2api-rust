//! Manager WebUI 模块。
//!
//! 提供与 Go 版本完全一致的 WebUI 功能：
//! - 登录/登出认证
//! - Dashboard 账号管理
//! - OAuth 流程支持
//! - 配额查看与缓存
//! - 系统设置管理

pub mod handler;
pub mod quota;
pub mod templates;

pub use handler::*;
pub use quota::QuotaCache;
