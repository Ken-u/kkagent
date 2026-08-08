use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::types::{LlmRequest, StreamEvent};
use kkagent_config::{ModelConfig, ProviderConfig};

#[async_trait]
pub trait LlmProvider: Send + Sync {
    async fn stream_chat(
        &self,
        request: LlmRequest,
        event_tx: mpsc::Sender<StreamEvent>,
    ) -> anyhow::Result<()>;
}

pub struct AnthropicProvider {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl AnthropicProvider {
    pub fn new(config: &ProviderConfig) -> Self {
        let base_url = config
            .base_url
            .as_deref()
            .unwrap_or("https://api.anthropic.com")
            .trim_end_matches('/')
            .to_string();
        let api_key = config.api_key.clone().unwrap_or_default();

        let client = reqwest::Client::builder()
            .connect_timeout(std::time::Duration::from_secs(30))
            .timeout(std::time::Duration::from_secs(300))
            .pool_max_idle_per_host(0)
            .http1_only()
            .build()
            .unwrap_or_else(|_| reqwest::Client::new());

        Self { client, base_url, api_key }
    }
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    async fn stream_chat(
        &self,
        request: LlmRequest,
        event_tx: mpsc::Sender<StreamEvent>,
    ) -> anyhow::Result<()> {
        crate::stream::anthropic_stream(&self.client, &self.base_url, &self.api_key, request, event_tx).await
    }
}

pub fn create_provider(provider_config: &ProviderConfig, _model_config: &ModelConfig) -> Box<dyn LlmProvider> {
    match provider_config.provider_type.as_str() {
        "anthropic" | "openai" | "kimi" => Box::new(AnthropicProvider::new(provider_config)),
        _ => Box::new(AnthropicProvider::new(provider_config)),
    }
}
