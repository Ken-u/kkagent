//! JSON Schema argument validation (draft-07 / 2019-09 / 2020-12 via jsonschema).

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
    input.get(key).and_then(|v| v.as_bool()).unwrap_or(default)
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

/// Validate `input` against a JSON Schema (tool `parameters_schema`).
/// Returns Ok when schema is empty/invalid (tools keep failing at execute), or when input passes.
pub fn validate_against_schema(schema: &Value, input: &Value) -> Result<(), ValidationError> {
    if !schema.is_object() {
        return Ok(());
    }
    let validator = match jsonschema::validator_for(schema) {
        Ok(v) => v,
        Err(e) => {
            tracing::debug!("tool schema compile skipped: {e}");
            return Ok(());
        }
    };
    let errors: Vec<String> = validator
        .iter_errors(input)
        .take(8)
        .map(|e| {
            let path = e.instance_path.to_string();
            if path.is_empty() {
                e.to_string()
            } else {
                format!("{path}: {e}")
            }
        })
        .collect();
    if errors.is_empty() {
        Ok(())
    } else {
        Err(ValidationError {
            message: format!(
                "Invalid tool arguments:\n{}",
                errors
                    .into_iter()
                    .map(|e| format!("- {e}"))
                    .collect::<Vec<_>>()
                    .join("\n")
            ),
        })
    }
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

    #[test]
    fn schema_rejects_missing_required() {
        let schema = json!({
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"]
        });
        let err = validate_against_schema(&schema, &json!({})).unwrap_err();
        assert!(err.message.contains("path") || err.message.contains("required"));
    }

    #[test]
    fn schema_accepts_valid() {
        let schema = json!({
            "type": "object",
            "properties": {"path": {"type": "string"}},
            "required": ["path"]
        });
        assert!(validate_against_schema(&schema, &json!({"path": "a.rs"})).is_ok());
    }
}
