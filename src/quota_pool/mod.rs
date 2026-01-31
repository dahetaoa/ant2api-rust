//! 配额池（Quota Pool）模块。
//!
//! 目标：在多账号场景下，根据不同模型的配额分组，选择更“有余额”的账号来承载请求，
//! 同时在缺少配额信息时保持向后兼容（退化为原有轮询策略）。

mod manager;
mod refresher;
mod selector;
mod types;

pub use manager::QuotaPoolManager;
pub use refresher::spawn_refresh_task;
