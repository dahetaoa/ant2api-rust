//! HTML 模板渲染模块。
//!
//! 使用 askama 模板引擎，模板文件位于 rust/templates/

use askama::Template;
use chrono::DateTime;

use crate::credential::types::Account;
use crate::quota_pool::QuotaGroup;
use crate::runtime_config::WebUISettings;

/// 中国时区 (UTC+8)
fn china_tz() -> chrono::FixedOffset {
    chrono::FixedOffset::east_opt(8 * 60 * 60).unwrap()
}

/// 格式化重置时间。
pub fn format_reset_time(rt: &Option<String>) -> String {
    let Some(rt) = rt else {
        return "未知".to_string();
    };
    if rt.is_empty() {
        return "未知".to_string();
    }
    match DateTime::parse_from_rfc3339(rt) {
        Ok(dt) => dt
            .with_timezone(&china_tz())
            .format("%m/%d %H:%M")
            .to_string(),
        Err(_) => rt.clone(),
    }
}

/// 格式化百分比。
pub fn format_percent(frac: &Option<f64>) -> String {
    match frac {
        Some(f) => format!("{:.0}%", f * 100.0),
        None => "未知".to_string(),
    }
}

/// 获取进度条 CSS 类。
pub fn bar_class(frac: &Option<f64>) -> &'static str {
    match frac {
        None => "bg-slate-300 h-full rounded-full quota-bar-pop",
        Some(v) if *v <= 0.0 => {
            "bg-red-500 h-full rounded-full transition-all duration-700 ease-out quota-bar-pop"
        }
        Some(v) if *v < 0.2 => {
            "bg-amber-500 h-full rounded-full transition-all duration-700 ease-out quota-bar-pop"
        }
        Some(v) if *v < 0.5 => {
            "bg-yellow-400 h-full rounded-full transition-all duration-700 ease-out quota-bar-pop"
        }
        _ => {
            "bg-emerald-500 h-full rounded-full transition-all duration-700 ease-out quota-bar-pop"
        }
    }
}

/// 获取进度条宽度样式。
pub fn bar_width_style(frac: &Option<f64>) -> String {
    match frac {
        None => "width: 0%".to_string(),
        Some(f) => format!("width: {:.0}%", f * 100.0),
    }
}

/// 计算账号统计信息。
pub fn calculate_stats(accounts: &[Account]) -> Stats {
    let now = chrono::Utc::now().timestamp_millis();
    let total = accounts.len();
    let mut active = 0;
    let mut expired = 0;
    let mut disabled = 0;

    for acc in accounts {
        if !acc.enable {
            disabled += 1;
        } else if acc.is_expired(now) {
            expired += 1;
        } else {
            active += 1;
        }
    }

    Stats {
        total,
        active,
        expired,
        disabled,
    }
}

/// 账号统计。
#[derive(Debug, Clone)]
pub struct Stats {
    pub total: usize,
    pub active: usize,
    pub expired: usize,
    pub disabled: usize,
}

/// 视图用的配额组（带格式化方法）
#[derive(Debug, Clone)]
pub struct ViewQuotaGroup {
    pub label: String,
    pub remaining_fraction: Option<f64>,
    pub reset_time: Option<String>,
}

impl ViewQuotaGroup {
    pub fn from_quota_group(g: &QuotaGroup) -> Self {
        Self {
            label: g.group_name.clone(),
            remaining_fraction: g.remaining_fraction,
            reset_time: g.reset_time.clone(),
        }
    }

    pub fn format_percent(&self) -> String {
        format_percent(&self.remaining_fraction)
    }

    pub fn format_reset_time(&self) -> String {
        format_reset_time(&self.reset_time)
    }

    pub fn bar_class(&self) -> &'static str {
        bar_class(&self.remaining_fraction)
    }

    pub fn bar_width_style(&self) -> String {
        bar_width_style(&self.remaining_fraction)
    }
}

/// 视图用的账号（预计算状态）
#[derive(Debug, Clone)]
pub struct ViewAccount {
    pub session_id: String,
    pub display_name: String,
    pub enable: bool,
    pub is_expired: bool,
    pub status: AccountStatus,
}

/// 账号状态
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AccountStatus {
    Active,
    Expired,
    Disabled,
}

impl ViewAccount {
    pub fn from_account(acc: &Account) -> Self {
        let now = chrono::Utc::now().timestamp_millis();
        let is_expired = acc.is_expired(now);

        let status = if !acc.enable {
            AccountStatus::Disabled
        } else if is_expired {
            AccountStatus::Expired
        } else {
            AccountStatus::Active
        };

        let display_name = if !acc.email.is_empty() {
            acc.email.clone()
        } else if !acc.project_id.is_empty() {
            acc.project_id.clone()
        } else {
            "未命名账号".to_string()
        };

        Self {
            session_id: acc.session_id.clone(),
            display_name,
            enable: acc.enable,
            is_expired,
            status,
        }
    }

    pub fn is_active(&self) -> bool {
        self.status == AccountStatus::Active
    }

    pub fn is_disabled(&self) -> bool {
        self.status == AccountStatus::Disabled
    }
}

/// 将账号列表转换为视图账号列表
pub fn to_view_accounts(accounts: &[Account]) -> Vec<ViewAccount> {
    accounts.iter().map(ViewAccount::from_account).collect()
}

// ============================================================================
// 模板结构体（使用 askama）
// ============================================================================

/// 登录页面模板
#[derive(Template)]
#[template(path = "login.html")]
pub struct LoginTemplate {
    pub error_msg: String,
}

/// Dashboard 页面模板
#[derive(Template)]
#[template(path = "dashboard.html")]
pub struct DashboardTemplate {
    pub accounts: Vec<ViewAccount>,
    pub stats: Stats,
}

/// 统计卡片片段
#[derive(Template)]
#[template(path = "fragments/stats_cards.html")]
pub struct StatsCardsTemplate {
    pub stats: Stats,
}

/// 账号列表片段
#[derive(Template)]
#[template(path = "fragments/token_list.html")]
pub struct TokenListTemplate {
    pub accounts: Vec<ViewAccount>,
}

/// 单个账号卡片片段
#[derive(Template)]
#[template(path = "fragments/token_card.html")]
pub struct TokenCardTemplate {
    pub account: ViewAccount,
    pub quota_open: bool,
}

/// 配额内容片段
#[derive(Template)]
#[template(path = "fragments/quota_content.html")]
pub struct QuotaContentTemplate {
    pub session_id: String,
    pub groups: Vec<ViewQuotaGroup>,
    pub error_msg: String,
}

/// 配额 OOB 交换片段
#[derive(Template)]
#[template(path = "fragments/quota_swap_oob.html")]
pub struct QuotaSwapOOBTemplate {
    pub session_id: String,
    pub groups: Vec<ViewQuotaGroup>,
    pub error_msg: String,
}

/// 配额骨架屏
#[derive(Template)]
#[template(path = "fragments/quota_skeleton.html")]
pub struct QuotaSkeletonTemplate;

/// 设置页面片段
#[derive(Template)]
#[template(path = "fragments/settings.html")]
pub struct SettingsTemplate {
    pub settings: WebUISettings,
}

/// 模型设置页面片段（聊天测试 UI）
#[derive(Template)]
#[template(path = "fragments/model_settings.html")]
pub struct ModelSettingsTemplate {
    pub accounts: Vec<ViewAccount>,
}
