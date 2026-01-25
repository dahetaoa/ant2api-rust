use uuid::Uuid;

pub fn request_id() -> String {
    format!("agent-{}", Uuid::new_v4())
}

pub fn session_id() -> String {
    // Go 版本：[-, 9e18) 的随机整数（十进制字符串），并带前缀 "-".
    let n = random_u64() % 9_000_000_000_000_000_000u64;
    format!("-{n}")
}

pub fn project_id() -> String {
    const ADJECTIVES: [&str; 10] = [
        "useful", "bright", "swift", "calm", "bold", "happy", "clever", "gentle", "quick", "brave",
    ];
    const NOUNS: [&str; 10] = [
        "fuze", "wave", "spark", "flow", "core", "beam", "star", "wind", "leaf", "cloud",
    ];

    let adj = ADJECTIVES[(random_u64() as usize) % ADJECTIVES.len()];
    let noun = NOUNS[(random_u64() as usize) % NOUNS.len()];
    let suffix = random_alphanumeric(5);
    format!("{adj}-{noun}-{suffix}")
}

pub fn tool_call_id() -> String {
    format!("call_{}", Uuid::new_v4().simple())
}

pub fn chat_completion_id() -> String {
    let s = Uuid::new_v4().to_string();
    let prefix = s.split('-').next().unwrap_or(&s);
    let short = &prefix[..prefix.len().min(8)];
    format!("chatcmpl-{short}")
}

fn random_u64() -> u64 {
    // 复用 UUID v4 的随机源，避免额外引入 rand/getrandom 依赖。
    let b = *Uuid::new_v4().as_bytes();
    u64::from_le_bytes([b[0], b[1], b[2], b[3], b[4], b[5], b[6], b[7]])
}

fn random_alphanumeric(len: usize) -> String {
    const CHARSET: &[u8] = b"abcdefghijklmnopqrstuvwxyz0123456789";
    let mut out = String::with_capacity(len);
    for _ in 0..len {
        let idx = (random_u64() as usize) % CHARSET.len();
        out.push(CHARSET[idx] as char);
    }
    out
}
