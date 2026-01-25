use chrono::{DateTime, Datelike, FixedOffset, TimeZone, Timelike, Utc};
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Account {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_in: i32,
    pub timestamp: i64,
    #[serde(
        rename = "projectId",
        skip_serializing_if = "String::is_empty",
        default
    )]
    pub project_id: String,
    #[serde(skip_serializing_if = "String::is_empty", default)]
    pub email: String,
    pub enable: bool,
    #[serde(default = "default_created_at")]
    pub created_at: DateTime<Utc>,
    #[serde(skip)]
    pub session_id: String,
}

impl Account {
    pub fn is_expired(&self, now_ms: i64) -> bool {
        if self.timestamp == 0 || self.expires_in == 0 {
            return true;
        }
        let expires_at = self.timestamp + (self.expires_in as i64) * 1000;
        // Go 版本：提前 5 分钟视为过期，避免请求中途失效。
        now_ms >= expires_at - 300_000
    }

    pub fn format_expires_at(&self) -> String {
        if self.timestamp == 0 || self.expires_in == 0 {
            return "-".to_string();
        }
        let ms = self.timestamp + (self.expires_in as i64) * 1000;
        let Some(dt) = DateTime::<Utc>::from_timestamp_millis(ms) else {
            return "-".to_string();
        };
        dt.with_timezone(&china_tz())
            .format("%Y-%m-%d %H:%M:%S")
            .to_string()
    }

    pub fn format_created_at(&self) -> String {
        if is_zero_time(&self.created_at) {
            return "-".to_string();
        }
        self.created_at
            .with_timezone(&china_tz())
            .format("%Y-%m-%d %H:%M:%S")
            .to_string()
    }
}

fn china_tz() -> FixedOffset {
    // 中国时区 (UTC+8)
    FixedOffset::east_opt(8 * 3600).unwrap_or_else(|| FixedOffset::east_opt(0).unwrap())
}

fn default_created_at() -> DateTime<Utc> {
    // 对齐 Go 的 time.Time 零值："0001-01-01T00:00:00Z"
    Utc.from_utc_datetime(
        &chrono::NaiveDate::from_ymd_opt(1, 1, 1)
            .unwrap()
            .and_hms_opt(0, 0, 0)
            .unwrap(),
    )
}

fn is_zero_time(dt: &DateTime<Utc>) -> bool {
    dt.year() == 1
        && dt.month() == 1
        && dt.day() == 1
        && dt.hour() == 0
        && dt.minute() == 0
        && dt.second() == 0
        && dt.timestamp_subsec_nanos() == 0
}
