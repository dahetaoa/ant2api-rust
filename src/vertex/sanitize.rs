use crate::vertex::types::{Content, Part, SystemInstruction};
use sonic_rs::prelude::*;
use std::collections::HashMap;

pub const AGENT_SYSTEM_PROMPT: &str = r#"You are Antigravity, a powerful agentic AI coding assistant designed by the Google Deepmind team working on Advanced Agentic Coding.
You are pair programming with a USER to solve their coding task. The task may require creating a new codebase, modifying or debugging an existing codebase, or simply answering a question.
- **Proactiveness**"#;

pub fn inject_agent_system_prompt(sys_instr: Option<SystemInstruction>) -> SystemInstruction {
    match sys_instr {
        None => SystemInstruction {
            role: "user".to_string(),
            parts: vec![Part {
                text: AGENT_SYSTEM_PROMPT.to_string(),
                ..Part::default()
            }],
        },
        Some(mut si) => {
            let existing_text = si.parts.first().map(|p| p.text.as_str()).unwrap_or("");
            let combined = if existing_text.is_empty() {
                AGENT_SYSTEM_PROMPT.to_string()
            } else {
                format!("{AGENT_SYSTEM_PROMPT}\n\n{existing_text}")
            };

            let mut parts = Vec::with_capacity(1 + si.parts.len().saturating_sub(1));
            if !si.parts.is_empty() {
                let mut first = si.parts.remove(0);
                first.text = combined;
                parts.push(first);
                parts.extend(si.parts);
            } else {
                parts.push(Part {
                    text: combined,
                    ..Part::default()
                });
            }

            SystemInstruction {
                role: "user".to_string(),
                parts,
            }
        }
    }
}

/// 丢弃无效/空的 contents/parts，避免 Vertex 400。
pub fn sanitize_contents(contents: Vec<Content>) -> Vec<Content> {
    if contents.is_empty() {
        return contents;
    }

    let mut out = Vec::with_capacity(contents.len());
    for mut c in contents {
        if c.parts.is_empty() {
            continue;
        }
        let mut parts = Vec::with_capacity(c.parts.len());
        for p in c.parts {
            if p.function_call.is_some() || p.function_response.is_some() || p.inline_data.is_some()
            {
                parts.push(p);
                continue;
            }
            if p.text.is_empty() {
                // Drop thought-only / signature-only / empty parts.
                continue;
            }
            parts.push(p);
        }
        if parts.is_empty() {
            continue;
        }
        c.parts = parts;
        out.push(c);
    }
    out
}

/// 将（Claude/OpenAI 风格）JSON Schema 近似结构转换为 Vertex functionDeclarations.parameters 可接受的子集。
pub fn sanitize_function_parameters_schema(
    schema: &HashMap<String, sonic_rs::Value>,
) -> HashMap<String, sonic_rs::Value> {
    let mut root = match sonic_rs::to_value(schema) {
        Ok(v) => v,
        Err(_) => return HashMap::new(),
    };
    let Some(obj) = root.as_object_mut() else {
        return HashMap::new();
    };
    sanitize_vertex_schema_in_place(obj);

    // 注意：`sonic_rs::from_value::<HashMap<String, Value>>` 在这里会失败（Value 反序列化会触发 newtype struct 错误），
    // 因此改为手动从 Object 拷贝一份 map，保证 tools schema 不会被意外清空。
    let Some(obj) = root.as_object() else {
        return HashMap::new();
    };
    let mut out: HashMap<String, sonic_rs::Value> = HashMap::with_capacity(obj.len());
    for (k, v) in obj.iter() {
        out.insert(k.to_string(), v.to_owned());
    }
    out
}

fn sanitize_vertex_schema_in_place(schema: &mut sonic_rs::Object) {
    // Remove unsupported/metadata keys early.
    schema.remove(&"$schema");
    schema.remove(&"$id");
    schema.remove(&"$anchor");
    schema.remove(&"$comment");

    // Vertex Schema uses "ref"/"defs" (no $ prefix).
    if let Some(v) = schema.remove(&"$ref")
        && schema.get(&"ref").is_none()
    {
        schema.insert(&"ref", v);
    }
    if let Some(v) = schema.remove(&"$defs")
        && schema.get(&"defs").is_none()
    {
        schema.insert(&"defs", v);
    }
    if let Some(v) = schema.remove(&"definitions")
        && schema.get(&"defs").is_none()
    {
        schema.insert(&"defs", v);
    }

    // Convert oneOf -> anyOf if needed; Vertex supports anyOf.
    if let Some(one_of) = schema.remove(&"oneOf") {
        if schema.get(&"anyOf").is_none() {
            schema.insert(&"anyOf", one_of);
        } else if let Some(dst) = schema.get_mut(&"anyOf").and_then(|v| v.as_array_mut())
            && let Some(src) = one_of.as_array()
        {
            for it in src.iter() {
                dst.push(it.to_owned());
            }
        }
    }

    // allOf is not supported; best-effort fallback to the first entry.
    if let Some(all_of) = schema.remove(&"allOf")
        && let Some(arr) = all_of.as_array()
        && let Some(first) = arr.first().and_then(|v| v.as_object())
    {
        for (k, vv) in first.iter() {
            if schema.get(&k).is_none() {
                schema.insert(k, vv.to_owned());
            }
        }
    }

    // exclusiveMinimum/exclusiveMaximum are not supported by Vertex Schema.
    convert_exclusive_bounds(schema);

    // Normalize "type" (Vertex expects enum names like "OBJECT", "STRING"...).
    normalize_type_field(schema);
    // 兜底：部分 schema 只提供了 properties/items 而缺少 type（在不同客户端里很常见）。
    // Vertex 对 tool schema 更严格，缺少 type 可能导致函数调用产出异常（如 MALFORMED_FUNCTION_CALL）。
    if schema.get(&"type").and_then(|v| v.as_str()).is_none() {
        if schema.get(&"properties").is_some() {
            schema.insert(&"type", "OBJECT");
        } else if schema.get(&"items").is_some() {
            schema.insert(&"type", "ARRAY");
        }
    }

    // Normalize "enum" to []string (Vertex Schema uses string enums).
    if let Some(v) = schema.get(&"enum").and_then(normalize_enum) {
        schema.insert(&"enum", v);
    } else {
        schema.remove(&"enum");
    }

    // Normalize "required" to []string.
    if let Some(v) = schema.get(&"required").and_then(normalize_string_array) {
        schema.insert(&"required", v);
    } else {
        schema.remove(&"required");
    }

    // Normalize numeric bounds to numbers.
    if let Some(f) = schema.get(&"minimum").and_then(to_f64) {
        if let Some(v) = value_from_f64(f) {
            schema.insert(&"minimum", v);
        } else {
            schema.remove(&"minimum");
        }
    } else {
        schema.remove(&"minimum");
    }
    if let Some(f) = schema.get(&"maximum").and_then(to_f64) {
        if let Some(v) = value_from_f64(f) {
            schema.insert(&"maximum", v);
        } else {
            schema.remove(&"maximum");
        }
    } else {
        schema.remove(&"maximum");
    }

    // Remove JSON Schema keywords not supported by Vertex Schema.
    for k in [
        "not",
        "if",
        "then",
        "else",
        "dependentSchemas",
        "dependentRequired",
        "dependencies",
        "patternProperties",
        "propertyNames",
        "unevaluatedProperties",
        "unevaluatedItems",
        "prefixItems",
        "contains",
        "minContains",
        "maxContains",
        "multipleOf",
        "pattern",
        "format",
        "minItems",
        "maxItems",
        "uniqueItems",
        "minLength",
        "maxLength",
        "minProperties",
        "maxProperties",
        "additionalProperties",
        "contentMediaType",
        "contentEncoding",
        "const",
        "examples",
        "readOnly",
        "writeOnly",
        "deprecated",
        "title",
        "default",
    ] {
        schema.remove(&k);
    }

    // Recurse into defs (if present).
    match schema.get_mut(&"defs") {
        Some(v) if v.is_object() => {
            if let Some(defs) = v.as_object_mut() {
                let keys: Vec<String> = defs.iter().map(|(k, _)| k.to_string()).collect();
                for k in keys {
                    let Some(child) = defs.get_mut(&k) else {
                        continue;
                    };
                    if let Some(obj) = child.as_object_mut() {
                        sanitize_vertex_schema_in_place(obj);
                    } else {
                        defs.remove(&k);
                    }
                }
            }
        }
        Some(_) => {
            schema.remove(&"defs");
        }
        None => {}
    }

    // Recurse into properties.
    match schema.get_mut(&"properties") {
        Some(v) if v.is_object() => {
            if let Some(props) = v.as_object_mut() {
                let keys: Vec<String> = props.iter().map(|(k, _)| k.to_string()).collect();
                for k in keys {
                    let Some(child) = props.get_mut(&k) else {
                        continue;
                    };
                    if let Some(obj) = child.as_object_mut() {
                        sanitize_vertex_schema_in_place(obj);
                    } else {
                        props.remove(&k);
                    }
                }
            }
        }
        Some(_) => {
            schema.remove(&"properties");
        }
        None => {}
    }

    // Recurse into items.
    if let Some(items) = schema.get_mut(&"items") {
        if let Some(obj) = items.as_object_mut() {
            sanitize_vertex_schema_in_place(obj);
        } else if let Some(arr) = items.as_array() {
            // JSON Schema 允许 array 形式；Vertex 期望单个 Schema。
            let mut picked: Option<sonic_rs::Value> = None;
            for it in arr.iter() {
                if it.is_object() {
                    picked = Some(it.to_owned());
                    break;
                }
            }
            if let Some(mut v) = picked {
                if let Some(obj) = v.as_object_mut() {
                    sanitize_vertex_schema_in_place(obj);
                }
                schema.insert(&"items", v);
            } else {
                schema.remove(&"items");
            }
        } else {
            schema.remove(&"items");
        }
    }

    // Recurse into anyOf.
    if let Some(any_of) = schema.get_mut(&"anyOf") {
        if let Some(arr) = any_of.as_array_mut() {
            let mut dst: Vec<sonic_rs::Value> = Vec::with_capacity(arr.len());
            for it in arr.iter() {
                if !it.is_object() {
                    continue;
                }
                let mut v = it.to_owned();
                if let Some(obj) = v.as_object_mut() {
                    sanitize_vertex_schema_in_place(obj);
                }
                dst.push(v);
            }
            if dst.is_empty() {
                schema.remove(&"anyOf");
            } else {
                let mut new_arr = sonic_rs::Array::with_capacity(dst.len());
                for v in dst {
                    new_arr.push(v);
                }
                schema.insert(&"anyOf", new_arr.into_value());
            }
        } else {
            schema.remove(&"anyOf");
        }
    }

    enforce_vertex_schema_allowlist(schema);
}

fn normalize_type_field(schema: &mut sonic_rs::Object) {
    let Some(raw) = schema.get(&"type").map(|v| v.to_owned()) else {
        return;
    };
    if let Some(s) = raw.as_str() {
        if let Some(norm) = normalize_vertex_type(s) {
            schema.insert(&"type", &norm);
        }
        return;
    }

    if let Some(arr) = raw.as_array() {
        // JSON Schema union types like ["string","null"].
        let mut has_null = false;
        let mut first_non_null: Option<&str> = None;
        for it in arr.iter() {
            let Some(s) = it.as_str() else {
                continue;
            };
            if s.eq_ignore_ascii_case("null") {
                has_null = true;
                continue;
            }
            if first_non_null.is_none() {
                first_non_null = Some(s);
            }
        }
        if has_null && schema.get(&"nullable").is_none() {
            schema.insert(&"nullable", true);
        }
        if let Some(t) = first_non_null {
            if let Some(norm) = normalize_vertex_type(t) {
                schema.insert(&"type", &norm);
            } else {
                let up = t.trim().to_uppercase();
                schema.insert(&"type", &up);
            }
        } else {
            schema.remove(&"type");
        }
        return;
    }

    // unexpected type
    schema.remove(&"type");
}

fn normalize_vertex_type(t: &str) -> Option<String> {
    match t.trim().to_lowercase().as_str() {
        "object" => Some("OBJECT".to_string()),
        "array" => Some("ARRAY".to_string()),
        "string" => Some("STRING".to_string()),
        "integer" | "int" => Some("INTEGER".to_string()),
        "number" => Some("NUMBER".to_string()),
        "boolean" | "bool" => Some("BOOLEAN".to_string()),
        "null" => Some("NULL".to_string()),
        _ => {
            let up = t.trim().to_uppercase();
            match up.as_str() {
                "TYPE_UNSPECIFIED" | "STRING" | "NUMBER" | "INTEGER" | "BOOLEAN" | "ARRAY"
                | "OBJECT" | "NULL" => Some(up),
                _ => None,
            }
        }
    }
}

fn normalize_enum(v: &sonic_rs::Value) -> Option<sonic_rs::Value> {
    let arr = v.as_array()?;
    let mut out: Vec<String> = Vec::with_capacity(arr.len());
    for it in arr.iter() {
        if let Some(s) = it.as_str() {
            out.push(s.to_string());
            continue;
        }
        if let Some(b) = it.as_bool() {
            out.push(b.to_string());
            continue;
        }
        if let Some(i) = it.as_i64() {
            out.push(i.to_string());
            continue;
        }
        if let Some(u) = it.as_u64() {
            out.push(u.to_string());
            continue;
        }
        if let Some(f) = it.as_f64() {
            out.push(trim_trailing_dot_zero(format!("{f}")));
            continue;
        }
        out.push(it.to_string());
    }
    let mut new_arr = sonic_rs::Array::with_capacity(out.len());
    for s in out {
        new_arr.push(&s);
    }
    Some(new_arr.into_value())
}

fn normalize_string_array(v: &sonic_rs::Value) -> Option<sonic_rs::Value> {
    let arr = v.as_array()?;
    let mut out: Vec<String> = Vec::with_capacity(arr.len());
    for it in arr.iter() {
        let Some(s) = it.as_str() else {
            continue;
        };
        let t = s.trim();
        if t.is_empty() {
            continue;
        }
        out.push(t.to_string());
    }
    if out.is_empty() {
        return None;
    }
    let mut new_arr = sonic_rs::Array::with_capacity(out.len());
    for s in out {
        new_arr.push(&s);
    }
    Some(new_arr.into_value())
}

fn trim_trailing_dot_zero(s: String) -> String {
    s.strip_suffix(".0").unwrap_or(&s).to_string()
}

fn enforce_vertex_schema_allowlist(schema: &mut sonic_rs::Object) {
    // Vertex 工具 schema 解析严格：未知字段会导致 400。
    let allowed = [
        "type",
        "properties",
        "required",
        "description",
        "enum",
        "items",
        "nullable",
        "minimum",
        "maximum",
        "anyOf",
        "ref",
        "defs",
    ];

    let keys: Vec<String> = schema.iter().map(|(k, _)| k.to_string()).collect();
    for k in keys {
        if k.starts_with('$') {
            schema.remove(&k);
            continue;
        }
        if !allowed.iter().any(|a| *a == k) {
            schema.remove(&k);
        }
    }

    // Final type checks.
    if let Some(v) = schema.get(&"ref")
        && v.as_str().is_none()
    {
        schema.remove(&"ref");
    }
    if let Some(v) = schema.get(&"type")
        && v.as_str().is_none()
    {
        schema.remove(&"type");
    }
    if let Some(v) = schema.get(&"description")
        && v.as_str().is_none()
    {
        schema.remove(&"description");
    }
    if let Some(v) = schema.get(&"nullable")
        && v.as_bool().is_none()
    {
        schema.remove(&"nullable");
    }
}

fn convert_exclusive_bounds(schema: &mut sonic_rs::Object) {
    if let Some(ex_min) = schema.remove(&"exclusiveMinimum") {
        if schema.get(&"minimum").is_none() {
            if let Some(f) = to_f64(&ex_min) {
                let v = adjust_exclusive(f, schema, true);
                if let Some(val) = value_from_f64(v) {
                    schema.insert(&"minimum", val);
                }
            }
        } else if ex_min.as_bool() == Some(true)
            && let Some(f) = schema.get(&"minimum").and_then(to_f64)
        {
            let v = adjust_exclusive(f, schema, true);
            if let Some(val) = value_from_f64(v) {
                schema.insert(&"minimum", val);
            }
        }
    }

    if let Some(ex_max) = schema.remove(&"exclusiveMaximum") {
        if schema.get(&"maximum").is_none() {
            if let Some(f) = to_f64(&ex_max) {
                let v = adjust_exclusive(f, schema, false);
                if let Some(val) = value_from_f64(v) {
                    schema.insert(&"maximum", val);
                }
            }
        } else if ex_max.as_bool() == Some(true)
            && let Some(f) = schema.get(&"maximum").and_then(to_f64)
        {
            let v = adjust_exclusive(f, schema, false);
            if let Some(val) = value_from_f64(v) {
                schema.insert(&"maximum", val);
            }
        }
    }
}

fn to_f64(v: &sonic_rs::Value) -> Option<f64> {
    if let Some(f) = v.as_f64() {
        return Some(f);
    }
    if let Some(i) = v.as_i64() {
        return Some(i as f64);
    }
    if let Some(u) = v.as_u64() {
        return Some(u as f64);
    }
    if let Some(s) = v.as_str() {
        let f: f64 = s.trim().parse().ok()?;
        return Some(f);
    }
    None
}

fn value_from_f64(f: f64) -> Option<sonic_rs::Value> {
    let n = sonic_rs::Number::from_f64(f)?;
    Some(sonic_rs::Value::from(n))
}

fn adjust_exclusive(bound: f64, schema: &sonic_rs::Object, is_min: bool) -> f64 {
    let t = schema.get(&"type").and_then(|v| v.as_str()).unwrap_or("");
    if t.eq_ignore_ascii_case("INTEGER") && is_whole_number(bound) {
        if is_min {
            return bound + 1.0;
        }
        return bound - 1.0;
    }
    bound
}

fn is_whole_number(f: f64) -> bool {
    f == (f as i64) as f64
}

#[cfg(test)]
mod tests {
    use super::sanitize_function_parameters_schema;
    use sonic_rs::prelude::*;
    use std::collections::HashMap;

    #[test]
    fn schema_defaults_to_object_when_properties_present() {
        let mut schema: HashMap<String, sonic_rs::Value> = HashMap::new();

        let mut prop_schema = sonic_rs::Object::new();
        prop_schema.insert(&"type", "string");

        let mut props = sonic_rs::Object::new();
        props.insert(&"query", prop_schema.into_value());

        schema.insert("properties".to_string(), props.into_value());

        let out = sanitize_function_parameters_schema(&schema);
        assert_eq!(out.get("type").and_then(|v| v.as_str()), Some("OBJECT"));
    }

    #[test]
    fn schema_defaults_to_array_when_items_present() {
        let mut schema: HashMap<String, sonic_rs::Value> = HashMap::new();

        let mut item_schema = sonic_rs::Object::new();
        item_schema.insert(&"type", "string");
        schema.insert("items".to_string(), item_schema.into_value());

        let out = sanitize_function_parameters_schema(&schema);
        assert_eq!(out.get("type").and_then(|v| v.as_str()), Some("ARRAY"));
    }
}
