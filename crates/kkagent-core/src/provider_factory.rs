//! Turn the configuration's provider into what the model client needs.
//!
//! `kkagent-llm` knows nothing of the configuration layer (so a host can use the
//! providers as a library); this is the one place where a configured provider
//! and model become a [`LlmProvider`]. Every call site goes through
//! [`create_provider`], which is also how a provider of type `host` (see
//! [`crate::host_llm`]) is found.

use kkagent_config::{ModelConfig, ProviderConfig};
use kkagent_llm::{LlmProvider, ProviderSettings};

/// The model client's view of a configured provider.
pub fn llm_settings(provider: &ProviderConfig) -> ProviderSettings {
    ProviderSettings {
        provider_type: provider.provider_type.clone(),
        base_url: provider.base_url.clone(),
        api_key: provider.api_key.clone(),
        custom_headers: provider.custom_headers.clone(),
        request_timeout_ms: provider.request_timeout_ms,
        read_timeout_ms: provider.read_timeout_ms,
    }
}

/// The provider for a configured provider / model pair.
pub fn create_provider(
    provider: &ProviderConfig,
    model: &ModelConfig,
) -> anyhow::Result<Box<dyn LlmProvider>> {
    kkagent_llm::create_provider(&llm_settings(provider), &model.model)
}
