//! 运行时可动态修改的配置。
//!
//! 用于支持 WebUI 设置页面"立即生效"的需求。
//! 使用 ArcSwap 实现无锁读取，写入时创建新的配置快照。

use arc_swap::ArcSwap;
use std::path::Path;
use std::sync::Arc;

use crate::config::Config;
use crate::logging::LogLevel;

pub const DEFAULT_BACKEND_HOST: &str = "cloudcode-pa.googleapis.com";
pub const DAILY_BACKEND_HOST: &str = "daily-cloudcode-pa.sandbox.googleapis.com";
pub const ENDPOINT_MODE_PRODUCTION: &str = "production";
pub const ENDPOINT_MODE_DAILY: &str = "daily";

/// 运行时配置快照。
#[derive(Debug, Clone)]
pub struct RuntimeSettings {
    /// WebUI 登录密码
    pub webui_password: String,
    /// API User-Agent（用于 Vertex/OAuth 请求）
    pub api_user_agent: String,
    /// Gemini 3 媒体分辨率
    pub gemini3_media_resolution: String,
    /// 调试日志级别
    pub debug: String,
    /// API Key（仅存储，当前不用于鉴权）
    pub api_key: String,
    /// 后端请求地址模式
    pub endpoint_mode: String,
    /// 服务端口（只读，用于 OAuth redirect_uri）
    pub port: u16,
}

impl RuntimeSettings {
    /// 从初始 Config 创建运行时配置。
    pub fn from_config(cfg: &Config) -> Self {
        Self {
            webui_password: cfg.webui_password.clone(),
            api_user_agent: cfg.api_user_agent.clone(),
            gemini3_media_resolution: normalize_media_resolution(&cfg.gemini3_media_resolution),
            debug: cfg.debug.clone(),
            api_key: cfg.api_key.clone(),
            endpoint_mode: normalize_endpoint_mode(&cfg.endpoint_mode),
            port: cfg.port,
        }
    }

    /// 获取日志级别。
    pub fn log_level(&self) -> LogLevel {
        LogLevel::parse(&self.debug)
    }
}

/// 全局运行时配置存储。
static RUNTIME_SETTINGS: std::sync::OnceLock<ArcSwap<RuntimeSettings>> = std::sync::OnceLock::new();

/// 初始化运行时配置（在 main 中调用一次）。
pub fn init(cfg: &Config) {
    let settings = RuntimeSettings::from_config(cfg);
    let _ = RUNTIME_SETTINGS.set(ArcSwap::from_pointee(settings));
}

/// 获取当前运行时配置快照。
pub fn get() -> Arc<RuntimeSettings> {
    RUNTIME_SETTINGS
        .get()
        .map(|s| s.load_full())
        .unwrap_or_else(|| {
            Arc::new(RuntimeSettings {
                webui_password: String::new(),
                api_user_agent: String::from("ant2api/1.0"),
                gemini3_media_resolution: String::new(),
                debug: String::from("off"),
                api_key: String::new(),
                endpoint_mode: ENDPOINT_MODE_PRODUCTION.to_string(),
                port: 8045,
            })
        })
}

/// 更新运行时配置（从 WebUI 设置页面调用）。
pub fn update(new_settings: RuntimeSettings) {
    if let Some(store) = RUNTIME_SETTINGS.get() {
        store.store(Arc::new(new_settings));
    }
}

/// WebUI 可编辑的设置（用于 JSON 序列化）。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WebUISettings {
    pub api_key: String,
    pub webui_password: String,
    pub debug: String,
    pub user_agent: String,
    pub gemini3_media_resolution: String,
    #[serde(default)]
    pub endpoint_mode: String,
}

impl WebUISettings {
    /// 从运行时配置创建。
    pub fn from_runtime(rt: &RuntimeSettings) -> Self {
        Self {
            api_key: rt.api_key.clone(),
            webui_password: rt.webui_password.clone(),
            debug: rt.debug.clone(),
            user_agent: rt.api_user_agent.clone(),
            gemini3_media_resolution: rt.gemini3_media_resolution.clone(),
            endpoint_mode: rt.endpoint_mode.clone(),
        }
    }

    /// 验证设置。
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.webui_password.trim().is_empty() {
            return Err("WebUI 登录密码不能为空");
        }
        let debug = self.debug.trim().to_lowercase();
        if !debug.is_empty()
            && debug != "off"
            && debug != "low"
            && debug != "medium"
            && debug != "high"
        {
            return Err("日志级别必须是 off、low、medium 或 high");
        }
        let endpoint_mode = self.normalized_endpoint_mode();
        if endpoint_mode != ENDPOINT_MODE_PRODUCTION && endpoint_mode != ENDPOINT_MODE_DAILY {
            return Err("后端请求地址无效");
        }
        Ok(())
    }

    /// 标准化 debug 值。
    pub fn normalized_debug(&self) -> String {
        let d = self.debug.trim().to_lowercase();
        if d.is_empty() || d == "off" {
            "off".to_string()
        } else if d == "low" {
            "low".to_string()
        } else if d == "medium" {
            "medium".to_string()
        } else if d == "high" {
            "high".to_string()
        } else {
            "off".to_string()
        }
    }

    /// 标准化 endpoint mode 值。
    pub fn normalized_endpoint_mode(&self) -> String {
        normalize_endpoint_mode(&self.endpoint_mode)
    }

    /// 应用到运行时配置。
    pub fn apply_to_runtime(&self, current: &RuntimeSettings) -> RuntimeSettings {
        RuntimeSettings {
            webui_password: self.webui_password.trim().to_string(),
            api_user_agent: self.user_agent.trim().to_string(),
            gemini3_media_resolution: normalize_media_resolution(&self.gemini3_media_resolution),
            debug: self.normalized_debug(),
            api_key: self.api_key.trim().to_string(),
            endpoint_mode: self.normalized_endpoint_mode(),
            port: current.port,
        }
    }
}

/// 标准化媒体分辨率值。
fn normalize_media_resolution(value: &str) -> String {
    let v = value.trim().to_lowercase();
    match v.as_str() {
        "low" | "medium" | "high" => v,
        _ => String::new(),
    }
}

pub fn normalize_endpoint_mode(value: &str) -> String {
    let v = value.trim();
    if v.eq_ignore_ascii_case(ENDPOINT_MODE_DAILY) {
        ENDPOINT_MODE_DAILY.to_string()
    } else {
        ENDPOINT_MODE_PRODUCTION.to_string()
    }
}

pub fn endpoint_host_for_mode(value: &str) -> &'static str {
    let v = value.trim();
    if v.eq_ignore_ascii_case(ENDPOINT_MODE_DAILY) {
        DAILY_BACKEND_HOST
    } else {
        DEFAULT_BACKEND_HOST
    }
}

pub fn current_endpoint_host() -> String {
    let settings = get();
    endpoint_host_for_mode(&settings.endpoint_mode).to_string()
}

pub fn current_endpoint() -> crate::vertex::client::Endpoint {
    let settings = get();
    let mode = normalize_endpoint_mode(&settings.endpoint_mode);
    crate::vertex::client::Endpoint {
        key: mode,
        host: endpoint_host_for_mode(&settings.endpoint_mode).to_string(),
    }
}

/// 持久化设置到 .env 文件。
pub fn persist_to_dotenv(settings: &WebUISettings) -> Result<(), String> {
    let updates = [
        ("API_KEY", settings.api_key.trim()),
        ("WEBUI_PASSWORD", settings.webui_password.trim()),
        ("DEBUG", &settings.normalized_debug()),
        ("API_USER_AGENT", settings.user_agent.trim()),
        (
            "GEMINI3_MEDIA_RESOLUTION",
            &normalize_media_resolution(&settings.gemini3_media_resolution),
        ),
        ("ENDPOINT_MODE", &settings.normalized_endpoint_mode()),
    ];

    let dotenv_path =
        find_or_create_dotenv_path().map_err(|e| format!("无法获取 .env 路径: {e}"))?;

    update_dotenv_file(&dotenv_path, &updates).map_err(|e| format!("无法更新 .env 文件: {e}"))?;

    Ok(())
}

/// 查找或创建 .env 文件路径。
fn find_or_create_dotenv_path() -> Result<std::path::PathBuf, std::io::Error> {
    let cwd = std::env::current_dir()?;
    let mut dir: &Path = cwd.as_path();

    loop {
        let candidate = dir.join(".env");
        if candidate.is_file() {
            return Ok(candidate);
        }

        // 遇到项目边界则停止
        if dir.join("Cargo.toml").is_file() || dir.join(".git").is_dir() {
            // 在当前目录创建
            return Ok(cwd.join(".env"));
        }

        let Some(parent) = dir.parent() else {
            break;
        };
        if parent == dir {
            break;
        }
        dir = parent;
    }

    // 默认在当前目录创建
    Ok(cwd.join(".env"))
}

/// 更新 .env 文件中的键值对。
fn update_dotenv_file(path: &Path, updates: &[(&str, &str)]) -> Result<(), std::io::Error> {
    use std::io::{BufRead, Write};

    // 读取现有内容
    let mut lines: Vec<String> = if path.exists() {
        let file = std::fs::File::open(path)?;
        let reader = std::io::BufReader::new(file);
        reader.lines().collect::<Result<Vec<_>, _>>()?
    } else {
        Vec::new()
    };

    let mut updated_keys = std::collections::HashSet::new();

    // 更新现有行
    for line in lines.iter_mut() {
        if let Some((key, _)) = parse_dotenv_line(line) {
            for (uk, uv) in updates {
                if key == *uk {
                    *line = format_env_line(uk, uv);
                    updated_keys.insert(*uk);
                    break;
                }
            }
        }
    }

    // 追加新键
    for (key, value) in updates {
        if !updated_keys.contains(*key) {
            lines.push(format_env_line(key, value));
        }
    }

    // 写回文件
    let mut file = std::fs::File::create(path)?;
    for line in lines {
        writeln!(file, "{}", line)?;
    }

    Ok(())
}

/// 解析 .env 行，返回 (key, value)。
fn parse_dotenv_line(line: &str) -> Option<(String, String)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }

    let line = line.strip_prefix("export ").unwrap_or(line).trim();
    let eq_idx = line.find('=')?;
    if eq_idx == 0 {
        return None;
    }

    let key = line[..eq_idx].trim().to_string();
    let value = line[eq_idx + 1..].trim().to_string();
    Some((key, value))
}

/// 格式化 .env 行，必要时添加引号。
fn format_env_line(key: &str, value: &str) -> String {
    if value.is_empty()
        || value.contains(' ')
        || value.contains('\t')
        || value.contains('"')
        || value.contains('\'')
    {
        format!("{}=\"{}\"", key, value)
    } else {
        format!("{}={}", key, value)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_debug_level_validation_and_normalization() {
        let base = WebUISettings {
            api_key: String::new(),
            webui_password: "pw".to_string(),
            debug: "off".to_string(),
            user_agent: String::new(),
            gemini3_media_resolution: String::new(),
            endpoint_mode: ENDPOINT_MODE_PRODUCTION.to_string(),
        };

        for level in ["off", "low", "medium", "high", "  HIGH  ", ""] {
            let mut s = base.clone();
            s.debug = level.to_string();
            assert!(s.validate().is_ok());
        }

        let mut s = base.clone();
        s.debug = "invalid".to_string();
        assert!(s.validate().is_err());

        let mut s = base.clone();
        s.debug = "HIGH".to_string();
        assert_eq!(s.normalized_debug(), "high");

        let mut s = base;
        s.debug = "medium".to_string();
        assert_eq!(s.normalized_debug(), "medium");
    }

    #[test]
    fn test_normalize_media_resolution() {
        assert_eq!(normalize_media_resolution("low"), "low");
        assert_eq!(normalize_media_resolution("MEDIUM"), "medium");
        assert_eq!(normalize_media_resolution("  HIGH  "), "high");
        assert_eq!(normalize_media_resolution("invalid"), "");
        assert_eq!(normalize_media_resolution(""), "");
    }

    #[test]
    fn test_normalize_endpoint_mode() {
        assert_eq!(normalize_endpoint_mode("daily"), ENDPOINT_MODE_DAILY);
        assert_eq!(normalize_endpoint_mode("DAILY"), ENDPOINT_MODE_DAILY);
        assert_eq!(
            normalize_endpoint_mode("production"),
            ENDPOINT_MODE_PRODUCTION
        );
        assert_eq!(normalize_endpoint_mode(""), ENDPOINT_MODE_PRODUCTION);
        assert_eq!(normalize_endpoint_mode("invalid"), ENDPOINT_MODE_PRODUCTION);
    }

    #[test]
    fn test_format_env_line() {
        assert_eq!(format_env_line("KEY", "value"), "KEY=value");
        assert_eq!(format_env_line("KEY", "with space"), "KEY=\"with space\"");
        assert_eq!(format_env_line("KEY", ""), "KEY=\"\"");
    }
}
