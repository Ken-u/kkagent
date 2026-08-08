use serde_json::{Map, Value};

/// Drop non-primitive properties and redact obvious PII-looking strings.
pub fn clean_telemetry_properties(props: Map<String, Value>) -> Map<String, Value> {
    let mut out = Map::new();
    for (k, v) in props {
        match v {
            Value::Null | Value::Bool(_) | Value::Number(_) => {
                out.insert(k, v);
            }
            Value::String(s) => {
                out.insert(k, Value::String(redact_string(&s)));
            }
            _ => {
                // drop objects/arrays
            }
        }
    }
    out
}

fn redact_string(s: &str) -> String {
    let lower = s.to_ascii_lowercase();
    if lower.contains("api_key")
        || lower.contains("apikey")
        || lower.contains("authorization")
        || lower.contains("bearer ")
        || looks_like_email(s)
        || looks_like_token(s)
    {
        return "[redacted]".into();
    }
    s.to_string()
}

fn looks_like_email(s: &str) -> bool {
    s.contains('@') && s.contains('.') && s.len() < 128 && !s.contains(' ')
}

fn looks_like_token(s: &str) -> bool {
    s.len() >= 32
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.')
}
