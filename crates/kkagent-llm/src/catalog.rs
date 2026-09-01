//! Model capability catalog (kosong/catalog subset, non-Kimi).

#[derive(Debug, Clone)]
pub struct ModelCapabilityEntry {
    pub id: String,
    pub provider: String,
    pub context_window: u64,
    pub max_output: u64,
    pub tools: bool,
    pub vision: bool,
    pub thinking: bool,
    pub responses_api: bool,
}

pub fn builtin_catalog() -> Vec<ModelCapabilityEntry> {
    vec![
        ModelCapabilityEntry {
            id: "gpt-4.1".into(),
            provider: "openai".into(),
            context_window: 1_047_576,
            max_output: 32_768,
            tools: true,
            vision: true,
            thinking: false,
            responses_api: true,
        },
        ModelCapabilityEntry {
            id: "gpt-4.1-mini".into(),
            provider: "openai".into(),
            context_window: 1_047_576,
            max_output: 16_384,
            tools: true,
            vision: true,
            thinking: false,
            responses_api: true,
        },
        ModelCapabilityEntry {
            id: "gpt-5.6".into(),
            provider: "openai".into(),
            context_window: 1_050_000,
            max_output: 128_000,
            tools: true,
            vision: true,
            thinking: true,
            responses_api: true,
        },
        ModelCapabilityEntry {
            id: "o4-mini".into(),
            provider: "openai".into(),
            context_window: 200_000,
            max_output: 100_000,
            tools: true,
            vision: true,
            thinking: true,
            responses_api: true,
        },
        ModelCapabilityEntry {
            id: "o3".into(),
            provider: "openai".into(),
            context_window: 200_000,
            max_output: 100_000,
            tools: true,
            vision: true,
            thinking: true,
            responses_api: true,
        },
        ModelCapabilityEntry {
            id: "claude-sonnet-4-20250514".into(),
            provider: "anthropic".into(),
            context_window: 200_000,
            max_output: 64_000,
            tools: true,
            vision: true,
            thinking: true,
            responses_api: false,
        },
        ModelCapabilityEntry {
            id: "claude-opus-4-20250514".into(),
            provider: "anthropic".into(),
            context_window: 200_000,
            max_output: 32_000,
            tools: true,
            vision: true,
            thinking: true,
            responses_api: false,
        },
        ModelCapabilityEntry {
            id: "gemini-2.5-pro".into(),
            provider: "google".into(),
            context_window: 1_048_576,
            max_output: 65_536,
            tools: true,
            vision: true,
            thinking: true,
            responses_api: false,
        },
        ModelCapabilityEntry {
            id: "gemini-2.5-flash".into(),
            provider: "google".into(),
            context_window: 1_048_576,
            max_output: 65_536,
            tools: true,
            vision: true,
            thinking: true,
            responses_api: false,
        },
    ]
}

pub fn lookup(model: &str) -> Option<ModelCapabilityEntry> {
    let m = model.to_lowercase();
    builtin_catalog()
        .into_iter()
        .find(|e| e.id == model || e.id.to_lowercase() == m || m.contains(&e.id.to_lowercase()))
}

pub fn prefers_responses_api(model: &str) -> bool {
    lookup(model).map(|e| e.responses_api).unwrap_or_else(|| {
        let m = model.to_lowercase();
        m.starts_with('o') || m.contains("gpt-4.1") || m.contains("gpt-5")
    })
}

/// Ceiling for request-level max output tokens: the catalog's per-model
/// `max_output` when the model is known, otherwise `unknown_model_cap`
/// (callers pick a sensible default or pass the configured value through).
pub fn max_output_cap(model: &str, unknown_model_cap: u64) -> u64 {
    lookup(model)
        .map(|e| e.max_output)
        .unwrap_or(unknown_model_cap)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_o4() {
        assert!(prefers_responses_api("o4-mini"));
        assert!(lookup("gpt-4.1").unwrap().tools);
    }

    #[test]
    fn max_output_cap_uses_catalog_then_fallback() {
        assert_eq!(max_output_cap("gpt-5.6", 100_000), 128_000);
        assert_eq!(max_output_cap("gpt-4.1", 16_384), 32_768);
        assert_eq!(max_output_cap("totally-unknown-model", 16_384), 16_384);
    }
}
