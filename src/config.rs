use figment::Figment;
use figment::providers::Env;
use serde::Deserialize;
use std::path::{Path, PathBuf};

const DEFAULT_HOST: &str = "0.0.0.0";
const DEFAULT_PORT: u16 = 8045;
const DEFAULT_TIMEOUT_MS: u64 = 180_000;
const DEFAULT_USER_AGENT: &str = "antigravity/1.11.3 windows/amd64";

pub const DEFAULT_GOOGLE_CLIENT_ID: &str =
    "1071006060591-tmhssin2h21lcre235vtolojh4g403ep.apps.googleusercontent.com";
pub const DEFAULT_GOOGLE_CLIENT_SECRET: &str = "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";

#[derive(Debug, Clone)]
pub struct Config {
    pub host: String,
    pub port: u16,

    pub api_user_agent: String,
    pub timeout_ms: u64,
    pub proxy: String,

    pub api_key: String,

    pub retry_status_codes: Vec<u16>,
    pub retry_max_attempts: usize,

    pub debug: String,

    pub endpoint_mode: String,

    pub google_client_id: String,
    pub google_client_secret: String,

    pub data_dir: String,
    pub webui_password: String,
    pub gemini3_media_resolution: String,
}

#[derive(Debug, Default, Deserialize)]
struct RawEnv {
    #[serde(alias = "HOST")]
    host: Option<String>,
    #[serde(alias = "PORT")]
    port: Option<u16>,

    #[serde(alias = "API_USER_AGENT")]
    api_user_agent: Option<String>,
    #[serde(alias = "TIMEOUT")]
    timeout: Option<u64>,
    #[serde(alias = "PROXY")]
    proxy: Option<String>,

    #[serde(alias = "API_KEY")]
    api_key: Option<String>,

    #[serde(alias = "RETRY_STATUS_CODES")]
    retry_status_codes: Option<String>,
    #[serde(alias = "RETRY_MAX_ATTEMPTS")]
    retry_max_attempts: Option<usize>,

    #[serde(alias = "DEBUG")]
    debug: Option<String>,
    #[serde(alias = "ENDPOINT_MODE")]
    endpoint_mode: Option<String>,

    #[serde(alias = "GOOGLE_CLIENT_ID")]
    google_client_id: Option<String>,
    #[serde(alias = "GOOGLE_CLIENT_SECRET")]
    google_client_secret: Option<String>,

    #[serde(alias = "DATA_DIR")]
    data_dir: Option<String>,
    #[serde(alias = "WEBUI_PASSWORD")]
    webui_password: Option<String>,
    #[serde(alias = "GEMINI3_MEDIA_RESOLUTION")]
    gemini3_media_resolution: Option<String>,
}

impl Config {
    pub fn load() -> Self {
        load_dotenv();

        let raw = Figment::from(Env::raw())
            .extract::<RawEnv>()
            .unwrap_or_default();

        let mut cfg = Self {
            host: raw.host.unwrap_or_else(|| DEFAULT_HOST.to_string()),
            port: raw.port.unwrap_or(DEFAULT_PORT),
            api_user_agent: raw
                .api_user_agent
                .unwrap_or_else(|| DEFAULT_USER_AGENT.to_string()),
            timeout_ms: raw.timeout.unwrap_or(DEFAULT_TIMEOUT_MS),
            proxy: raw.proxy.unwrap_or_default(),
            api_key: raw.api_key.unwrap_or_default(),
            retry_status_codes: parse_status_codes(raw.retry_status_codes.as_deref())
                .unwrap_or_else(|| vec![429, 500]),
            retry_max_attempts: raw.retry_max_attempts.unwrap_or(3),
            debug: raw.debug.unwrap_or_else(|| "off".to_string()),
            endpoint_mode: raw.endpoint_mode.unwrap_or_else(|| "daily".to_string()),
            google_client_id: raw.google_client_id.unwrap_or_default(),
            google_client_secret: raw.google_client_secret.unwrap_or_default(),
            data_dir: raw.data_dir.unwrap_or_else(|| "./data".to_string()),
            webui_password: raw.webui_password.unwrap_or_default(),
            gemini3_media_resolution: raw.gemini3_media_resolution.unwrap_or_default(),
        };

        // 兼容 Go 版本的命令行覆盖：-debug <level>
        let mut args = std::env::args().skip(1);
        while let Some(arg) = args.next() {
            if arg == "-debug"
                && let Some(v) = args.next()
            {
                cfg.debug = v;
            }
        }

        cfg
    }

    pub fn effective_google_client_id(&self) -> &str {
        let v = self.google_client_id.trim();
        if v.is_empty() {
            DEFAULT_GOOGLE_CLIENT_ID
        } else {
            v
        }
    }

    pub fn effective_google_client_secret(&self) -> &str {
        let v = self.google_client_secret.trim();
        if v.is_empty() {
            DEFAULT_GOOGLE_CLIENT_SECRET
        } else {
            v
        }
    }

    pub fn log_level(&self) -> crate::logging::LogLevel {
        crate::logging::LogLevel::parse(&self.debug)
    }

    pub fn client_log_enabled(&self) -> bool {
        self.log_level().client_enabled()
    }

    pub fn backend_log_enabled(&self) -> bool {
        self.log_level().backend_enabled()
    }
}

fn parse_status_codes(value: Option<&str>) -> Option<Vec<u16>> {
    let value = value?;
    let mut out = Vec::new();
    for part in value.split(',') {
        let p = part.trim();
        if p.is_empty() {
            continue;
        }
        if let Ok(n) = p.parse::<u16>() {
            out.push(n);
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

fn load_dotenv() {
    let Some(dotenv_path) = find_dotenv_path() else {
        return;
    };

    let Ok(file) = std::fs::File::open(&dotenv_path) else {
        return;
    };
    let mut api_key_defined = false;

    let reader = std::io::BufReader::new(file);
    for line in std::io::BufRead::lines(reader).map_while(Result::ok) {
        let Some((key, value)) = parse_dotenv_line(&line) else {
            continue;
        };
        if key == "API_KEY" {
            api_key_defined = true;
        }
        // Rust 2024：修改进程环境变量在并发场景下可能触发 UB，因此 API 为 unsafe。
        // 这里在启动阶段加载 .env，且未并发访问环境变量，符合使用前提。
        unsafe {
            std::env::set_var(key, value);
        }
    }

    if !api_key_defined {
        unsafe {
            std::env::remove_var("API_KEY");
        }
    }
}

fn find_dotenv_path() -> Option<PathBuf> {
    let cwd = std::env::current_dir().ok()?;
    let mut dir: &Path = cwd.as_path();

    loop {
        let candidate = dir.join(".env");
        if candidate.is_file() {
            return Some(candidate);
        }

        // 避免跨越仓库根目录：发现 Cargo.toml 或 .git 即停止向上寻找。
        if dir.join("Cargo.toml").is_file() || dir.join(".git").is_dir() {
            return None;
        }

        let Some(parent) = dir.parent() else {
            break;
        };
        if parent == dir {
            break;
        }
        dir = parent;
    }

    None
}

fn parse_dotenv_line(line: &str) -> Option<(String, String)> {
    let mut line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }

    if let Some(rest) = line.strip_prefix("export ") {
        line = rest.trim_start();
    }

    let eq_idx = line.find('=')?;
    if eq_idx == 0 {
        return None;
    }

    let key = line[..eq_idx].trim();
    if key.is_empty() {
        return None;
    }

    let mut raw = line[eq_idx + 1..].trim();
    if raw.is_empty() {
        return Some((key.to_string(), String::new()));
    }

    let bytes = raw.as_bytes();
    if bytes.len() >= 2 {
        let first = bytes[0];
        let last = bytes[bytes.len() - 1];
        if (first == b'"' && last == b'"') || (first == b'\'' && last == b'\'') {
            raw = &raw[1..raw.len() - 1];
            return Some((key.to_string(), raw.to_string()));
        }
    }

    raw = strip_inline_comment(raw);
    Some((key.to_string(), raw.trim().to_string()))
}

fn strip_inline_comment(value: &str) -> &str {
    let bytes = value.as_bytes();
    for i in 0..bytes.len() {
        if bytes[i] != b'#' {
            continue;
        }
        if i == 0 || bytes[i - 1] == b' ' || bytes[i - 1] == b'\t' {
            return value[..i].trim_end();
        }
    }
    value
}
