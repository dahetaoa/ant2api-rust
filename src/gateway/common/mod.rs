pub mod extract;
pub mod retry;

/// 一次转发到后端所需的账号上下文（providers 共享）。
#[derive(Debug, Clone, Default)]
pub struct AccountContext {
    pub project_id: String,
    pub session_id: String,
    pub access_token: String,
    pub email: String,
}

pub fn find_function_name(
    contents: &[crate::vertex::types::Content],
    tool_call_id: &str,
) -> String {
    let tool_call_id = tool_call_id.trim();
    if tool_call_id.is_empty() {
        return String::new();
    }
    for c in contents.iter().rev() {
        for p in c.parts.iter().rev() {
            let Some(fc) = &p.function_call else {
                continue;
            };
            if fc.id == tool_call_id {
                return fc.name.clone();
            }
        }
    }
    String::new()
}
