//! Lightweight JSON-schema-ish argument validation for tools.

use serde_json::Value;

#[derive(Debug, Clone)]
pub struct ValidationError {
    pub message: String,
}

pub fn require_string(input: &Value, key: &str) -> Result<String, ValidationError> {
    match input.get(key).and_then(|v| v.as_str()) {
        Some(s) if !s.is_empty() => Ok(s.to_string()),
        Some(_) => Err(ValidationError {
            message: format!("`{key}` must be a non-empty string"),
        }),
        None => Err(ValidationError {
            message: format!("Missing required field `{key}`"),
        }),
    }
}

pub fn optional_u64(input: &Value, key: &str) -> Result<Option<u64>, ValidationError> {
    match input.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(v) => v.as_u64().map(Some).ok_or_else(|| ValidationError {
            message: format!("`{key}` must be an integer"),
        }),
    }
}

pub fn optional_bool(input: &Value, key: &str, default: bool) -> bool {
    input
        .get(key)
        .and_then(|v| v.as_bool())
        .unwrap_or(default)
}

pub fn validate_required_keys(input: &Value, keys: &[&str]) -> Result<(), ValidationError> {
    for key in keys {
        if input.get(*key).is_none() {
            return Err(ValidationError {
                message: format!("Missing required field `{key}`"),
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn require_string_ok() {
        let v = json!({"path": "a.rs"});
        assert_eq!(require_string(&v, "path").unwrap(), "a.rs");
    }
}
