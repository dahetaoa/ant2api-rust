use crate::vertex::types::ThinkingConfig;

pub const CLAUDE_MAX_OUTPUT_TOKENS: i32 = 64_000;
pub const GEMINI_MAX_OUTPUT_TOKENS: i32 = 65_535;

pub const DEFAULT_CLAUDE_THINKING_BUDGET_TOKENS: i32 = 32_000;
pub const THINKING_BUDGET_HEADROOM_TOKENS: i32 = 1_024;
pub const THINKING_BUDGET_MIN_TOKENS: i32 = 1_024;
pub const THINKING_MAX_OUTPUT_TOKENS_OVERHEAD_TOKENS: i32 = 4_096;

pub const CLAUDE_THINKING_EFFORT_LOW_TOKENS: i32 = 1_024;
pub const CLAUDE_THINKING_EFFORT_MEDIUM_TOKENS: i32 = 4_096;
pub const CLAUDE_THINKING_EFFORT_HIGH_TOKENS: i32 = DEFAULT_CLAUDE_THINKING_BUDGET_TOKENS;

pub fn canonical_model_id(model: &str) -> String {
    let m = model.trim();
    let m = m.strip_prefix("models/").unwrap_or(m);
    m.trim().to_string()
}

fn canonical_lower(model: &str) -> String {
    canonical_model_id(model).to_lowercase()
}

pub fn backend_model_id(model: &str) -> String {
    if let Some((_, backend)) = gemini3_flash_thinking_config(model) {
        return backend;
    }
    if let Some((_, backend)) = claude_opus45_thinking_config(model) {
        return backend;
    }
    if let Some((_, backend)) = gemini_pro_image_size_config(model) {
        return backend;
    }
    canonical_model_id(model)
}

pub fn is_claude(model: &str) -> bool {
    canonical_lower(model).starts_with("claude-")
}

pub fn is_gemini(model: &str) -> bool {
    canonical_lower(model).starts_with("gemini-")
}

pub fn is_gemini3(model: &str) -> bool {
    let m = canonical_lower(model);
    m.starts_with("gemini-3-") || m.starts_with("gemini-3")
}

pub fn is_gemini25(model: &str) -> bool {
    let m = canonical_lower(model);
    m.starts_with("gemini-2.5-") || m.starts_with("gemini-2.5")
}

pub fn validate_media_resolution(value: &str) -> Option<String> {
    let v = value.trim().to_lowercase();
    match v.as_str() {
        "" => Some(String::new()),
        "low" | "media_resolution_low" => Some("low".to_string()),
        "medium" | "media_resolution_medium" => Some("medium".to_string()),
        "high" | "media_resolution_high" => Some("high".to_string()),
        _ => None,
    }
}

pub fn to_api_media_resolution(value: &str) -> Option<String> {
    let v = validate_media_resolution(value)?;
    match v.as_str() {
        "" => Some(String::new()),
        "low" => Some("MEDIA_RESOLUTION_LOW".to_string()),
        "medium" => Some("MEDIA_RESOLUTION_MEDIUM".to_string()),
        "high" => Some("MEDIA_RESOLUTION_HIGH".to_string()),
        _ => None,
    }
}

pub fn is_claude_thinking(model: &str) -> bool {
    let m = canonical_lower(model);
    if !m.starts_with("claude-") {
        return false;
    }
    m.ends_with("-thinking") || m.contains("-thinking-")
}

pub fn is_image_model(model: &str) -> bool {
    canonical_lower(model).contains("image")
}

pub fn is_gemini3_flash(model: &str) -> bool {
    gemini3_flash_thinking_config(model).is_some()
}

pub fn is_gemini_pro_image(model: &str) -> bool {
    canonical_lower(model).contains("gemini-3-pro-image")
}

pub fn gemini3_flash_thinking_config(model: &str) -> Option<(String, String)> {
    let mut m = model.trim();
    m = m.strip_prefix("models/").unwrap_or(m);
    let m = m.trim().to_lowercase();
    if m.is_empty() {
        return None;
    }

    const BASE: &str = "gemini-3-flash";
    const THINKING: &str = "gemini-3-flash-thinking";

    if let Some(suffix) = m.strip_prefix(THINKING) {
        return Some(("high".to_string(), format!("{BASE}{suffix}")));
    }
    if m.starts_with(BASE) {
        return Some((String::new(), m));
    }

    None
}

pub fn claude_opus45_thinking_config(model: &str) -> Option<(i32, String)> {
    let mut m = model.trim();
    m = m.strip_prefix("models/").unwrap_or(m);
    let m = m.trim().to_lowercase();
    if m.is_empty() {
        return None;
    }

    const BASE: &str = "claude-opus-4-5";
    const THINKING: &str = "claude-opus-4-5-thinking";

    if m.starts_with(THINKING) {
        return Some((DEFAULT_CLAUDE_THINKING_BUDGET_TOKENS, m));
    }
    if let Some(suffix) = m.strip_prefix(BASE) {
        return Some((0, format!("{THINKING}{suffix}")));
    }

    None
}

pub fn claude_sonnet45_thinking_budget(model: &str) -> Option<i32> {
    let mut m = model.trim();
    m = m.strip_prefix("models/").unwrap_or(m);
    let m = m.trim().to_lowercase();
    if m.is_empty() {
        return None;
    }

    const BASE: &str = "claude-sonnet-4-5";
    const THINKING: &str = "claude-sonnet-4-5-thinking";

    if m.starts_with(THINKING) {
        return Some(DEFAULT_CLAUDE_THINKING_BUDGET_TOKENS);
    }
    if m.starts_with(BASE) {
        return Some(0);
    }
    None
}

pub fn gemini_pro_image_size_config(model: &str) -> Option<(String, String)> {
    let m = canonical_lower(model);
    if m.is_empty() {
        return None;
    }

    const BASE: &str = "gemini-3-pro-image";
    match m.as_str() {
        "gemini-3-pro-image-1k" => Some(("1K".to_string(), BASE.to_string())),
        "gemini-3-pro-image-2k" => Some(("2K".to_string(), BASE.to_string())),
        "gemini-3-pro-image-4k" => Some(("4K".to_string(), BASE.to_string())),
        _ => None,
    }
}

pub fn forced_thinking_config(model: &str) -> Option<ThinkingConfig> {
    if let Some((level, _backend)) = gemini3_flash_thinking_config(model) {
        if level == "high" {
            return Some(ThinkingConfig {
                include_thoughts: true,
                thinking_level: "high".to_string(),
                thinking_budget: 0,
            });
        }
        // gemini-3-flash（非 "-thinking"）：强制 thinkingBudget=0。
        return Some(ThinkingConfig {
            include_thoughts: true,
            thinking_level: String::new(),
            thinking_budget: 0,
        });
    }

    if let Some(budget) = claude_sonnet45_thinking_budget(model) {
        return Some(ThinkingConfig {
            include_thoughts: true,
            thinking_level: String::new(),
            thinking_budget: budget,
        });
    }

    if let Some((budget, _backend)) = claude_opus45_thinking_config(model) {
        return Some(ThinkingConfig {
            include_thoughts: true,
            thinking_level: String::new(),
            thinking_budget: budget,
        });
    }

    None
}

pub fn thinking_config_from_openai(model: &str, reasoning_effort: &str) -> Option<ThinkingConfig> {
    if let Some(tc) = forced_thinking_config(model) {
        return Some(tc);
    }

    let effort = reasoning_effort.trim().to_lowercase();

    // 如果调用方显式选择 Claude “-thinking” 模型且未传 reasoning_effort，则默认开启 thinking。
    if effort.is_empty() && is_claude_thinking(model) {
        return Some(ThinkingConfig {
            include_thoughts: true,
            thinking_level: String::new(),
            thinking_budget: DEFAULT_CLAUDE_THINKING_BUDGET_TOKENS,
        });
    }

    // Gemini 3（非 Flash）在 OpenAI 兼容语义下默认开启 thinking_level=high。
    if is_gemini3(model) && !is_gemini3_flash(model) {
        return Some(ThinkingConfig {
            include_thoughts: true,
            thinking_level: "high".to_string(),
            thinking_budget: 0,
        });
    }

    if effort.is_empty() {
        return None;
    }

    if is_claude_thinking(model) || is_gemini25(model) {
        // 支持数字 effort 作为直接预算覆盖（budget-based 模型）。
        if let Ok(n) = effort.parse::<i32>()
            && n > 0
        {
            return Some(ThinkingConfig {
                include_thoughts: true,
                thinking_level: String::new(),
                thinking_budget: n,
            });
        }
        if is_claude_thinking(model) {
            return Some(ThinkingConfig {
                include_thoughts: true,
                thinking_level: String::new(),
                thinking_budget: map_effort_to_budget(&effort),
            });
        }
        return Some(ThinkingConfig {
            include_thoughts: true,
            thinking_level: String::new(),
            thinking_budget: map_gemini25_effort_to_budget(&effort),
        });
    }

    Some(ThinkingConfig {
        include_thoughts: true,
        thinking_level: effort,
        thinking_budget: 0,
    })
}

pub fn thinking_config_from_claude(
    model: &str,
    thinking_type: &str,
    budget: i32,
    budget_tokens: i32,
) -> Option<ThinkingConfig> {
    if let Some(tc) = forced_thinking_config(model) {
        return Some(tc);
    }
    if thinking_type.trim().to_lowercase() != "enabled" {
        return None;
    }

    let mut tc = ThinkingConfig {
        include_thoughts: true,
        thinking_level: String::new(),
        thinking_budget: 0,
    };

    if is_claude(model) {
        // Claude thinking 模型需要非零 thinkingBudget 才能输出 thoughts。
        let mut b = budget;
        if b <= 0 {
            b = budget_tokens;
        }
        if b <= 0 {
            b = DEFAULT_CLAUDE_THINKING_BUDGET_TOKENS;
        }
        tc.thinking_budget = b;
        return Some(tc);
    }

    if is_gemini3(model) {
        // Gemini 3（非 Flash）在请求 thinking 时强制使用 thinking_level=high。
        tc.thinking_level = "high".to_string();
        tc.thinking_budget = 0;
        return Some(tc);
    }

    // 其他模型：优先使用 budget/budget_tokens（若为 0 则不写出）。
    let mut b = budget;
    if b <= 0 {
        b = budget_tokens;
    }
    if b > 0 {
        tc.thinking_budget = b;
    }
    Some(tc)
}

pub fn thinking_config_from_gemini(
    model: &str,
    include_thoughts: bool,
    thinking_budget: i32,
    thinking_level: &str,
) -> Option<ThinkingConfig> {
    if let Some(tc) = forced_thinking_config(model) {
        return Some(tc);
    }
    if !include_thoughts {
        return None;
    }

    let mut tc = ThinkingConfig {
        include_thoughts: true,
        thinking_level: thinking_level.to_string(),
        thinking_budget,
    };

    // Gemini 3（非 Flash）在开启 thinking 时强制 thinking_level=high，且预算为 0。
    if is_gemini3(model) && !is_gemini3_flash(model) {
        tc.thinking_level = "high".to_string();
        tc.thinking_budget = 0;
    }

    // Claude：开启 thinking 时必须提供非零预算；thinkingLevel 需清空。
    if is_claude(model) {
        tc.thinking_level.clear();
        if tc.thinking_budget <= 0 {
            tc.thinking_budget = DEFAULT_CLAUDE_THINKING_BUDGET_TOKENS;
        }
    }

    Some(tc)
}

pub fn build_sorted_model_ids(
    models: &std::collections::HashMap<String, sonic_rs::Value>,
) -> Vec<String> {
    let mut ids: Vec<String> = Vec::with_capacity(models.len() + 5);
    let mut seen: std::collections::HashSet<String> =
        std::collections::HashSet::with_capacity(models.len() + 5);

    let mut has_gemini3_flash = false;
    let mut has_gemini3_pro_image = false;
    let mut has_claude_opus45 = false;
    let mut has_claude_opus45_thinking = false;

    for k in models.keys() {
        let idv = k.trim();
        if idv.is_empty() {
            continue;
        }
        if idv.eq_ignore_ascii_case("gemini-3-flash") {
            has_gemini3_flash = true;
        }
        if idv.eq_ignore_ascii_case("gemini-3-pro-image") {
            has_gemini3_pro_image = true;
        }
        let lower = idv.to_lowercase();
        if lower.starts_with("claude-opus-4-5-thinking") {
            has_claude_opus45_thinking = true;
        } else if lower.starts_with("claude-opus-4-5") {
            has_claude_opus45 = true;
        }

        if seen.insert(idv.to_string()) {
            ids.push(idv.to_string());
        }
    }

    // Virtual model injection: only add gemini-3-flash-thinking when gemini-3-flash exists.
    if has_gemini3_flash {
        const VIRTUAL: &str = "gemini-3-flash-thinking";
        if seen.insert(VIRTUAL.to_string()) {
            ids.push(VIRTUAL.to_string());
        }
    }
    // Virtual model injection: add gemini-3-pro-image-*k variants when gemini-3-pro-image exists.
    if has_gemini3_pro_image {
        for virtual_id in [
            "gemini-3-pro-image-1k",
            "gemini-3-pro-image-2k",
            "gemini-3-pro-image-4k",
        ] {
            if seen.insert(virtual_id.to_string()) {
                ids.push(virtual_id.to_string());
            }
        }
    }
    // Virtual model injection: add claude-opus-4-5 when only claude-opus-4-5-thinking* exists.
    if has_claude_opus45_thinking && !has_claude_opus45 {
        const VIRTUAL: &str = "claude-opus-4-5";
        if seen.insert(VIRTUAL.to_string()) {
            ids.push(VIRTUAL.to_string());
        }
    }

    ids.sort();
    ids
}

fn map_effort_to_budget(effort: &str) -> i32 {
    match effort.trim().to_lowercase().as_str() {
        "minimal" | "low" => CLAUDE_THINKING_EFFORT_LOW_TOKENS,
        "medium" => CLAUDE_THINKING_EFFORT_MEDIUM_TOKENS,
        "high" | "max" => CLAUDE_THINKING_EFFORT_HIGH_TOKENS,
        _ => CLAUDE_THINKING_EFFORT_HIGH_TOKENS,
    }
}

fn map_gemini25_effort_to_budget(_effort: &str) -> i32 {
    // Keep conservative by default: Gemini 2.5 examples commonly use small budgets (e.g. 1024).
    CLAUDE_THINKING_EFFORT_LOW_TOKENS
}
