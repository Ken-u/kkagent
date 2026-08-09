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
    pub fn new(config: &ProviderConfig) -> anyhow::Result<Self> {
        let base_url = config
            .base_url
            .as_deref()
            .unwrap_or("https://api.anthropic.com")
            .trim_end_matches('/')
            .to_string();
        let api_key = config.api_key.clone().unwrap_or_default();
        Ok(Self {
            client: build_client(config)?,
            base_url,
            api_key,
        })
    }
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    async fn stream_chat(
        &self,
        request: LlmRequest,
        event_tx: mpsc::Sender<StreamEvent>,
    ) -> anyhow::Result<()> {
        crate::stream::anthropic_stream(
            &self.client,
            &self.base_url,
            &self.api_key,
            request,
            event_tx,
        )
        .await
    }
}

pub struct OpenAiProvider {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
    /// Force Responses API (`/v1/responses`) instead of chat completions.
    responses: bool,
}

impl OpenAiProvider {
    pub fn new(config: &ProviderConfig) -> anyhow::Result<Self> {
        Self::with_responses(config, false)
    }

    pub fn responses(config: &ProviderConfig) -> anyhow::Result<Self> {
        Self::with_responses(config, true)
    }

    fn with_responses(config: &ProviderConfig, responses: bool) -> anyhow::Result<Self> {
        let base_url = config
            .base_url
            .as_deref()
            .unwrap_or("https://api.openai.com")
            .trim_end_matches('/')
            .to_string();
        Ok(Self {
            client: build_client(config)?,
            base_url,
            api_key: config.api_key.clone().unwrap_or_default(),
            responses,
        })
    }
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    async fn stream_chat(
        &self,
        request: LlmRequest,
        event_tx: mpsc::Sender<StreamEvent>,
    ) -> anyhow::Result<()> {
        let use_responses = self.responses || crate::catalog::prefers_responses_api(&request.model);
        if use_responses {
            crate::openai_responses::openai_responses_stream(
                &self.client,
                &self.base_url,
                &self.api_key,
                request,
                event_tx,
            )
            .await
        } else {
            crate::stream::openai_stream(
                &self.client,
                &self.base_url,
                &self.api_key,
                request,
                event_tx,
            )
            .await
        }
    }
}

pub struct GoogleProvider {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
}

pub struct KimiProvider {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl KimiProvider {
    pub fn new(config: &ProviderConfig) -> anyhow::Result<Self> {
        Ok(Self {
            client: build_client(config)?,
            base_url: config
                .base_url
                .as_deref()
                .unwrap_or("https://api.moonshot.ai/v1")
                .trim_end_matches('/')
                .to_string(),
            api_key: config.api_key.clone().unwrap_or_default(),
        })
    }
}

#[async_trait]
impl LlmProvider for KimiProvider {
    async fn stream_chat(
        &self,
        request: LlmRequest,
        event_tx: mpsc::Sender<StreamEvent>,
    ) -> anyhow::Result<()> {
        crate::stream::kimi_stream(
            &self.client,
            &self.base_url,
            &self.api_key,
            request,
            event_tx,
        )
        .await
    }
}

impl GoogleProvider {
    pub fn new(config: &ProviderConfig) -> anyhow::Result<Self> {
        let base_url = config
            .base_url
            .as_deref()
            .unwrap_or("https://generativelanguage.googleapis.com")
            .trim_end_matches('/')
            .to_string();
        Ok(Self {
            client: build_client(config)?,
            base_url,
            api_key: config.api_key.clone().unwrap_or_default(),
        })
    }
}

#[async_trait]
impl LlmProvider for GoogleProvider {
    async fn stream_chat(
        &self,
        request: LlmRequest,
        event_tx: mpsc::Sender<StreamEvent>,
    ) -> anyhow::Result<()> {
        crate::stream::google_stream(
            &self.client,
            &self.base_url,
            &self.api_key,
            request,
            event_tx,
        )
        .await
    }
}

fn build_client(config: &ProviderConfig) -> anyhow::Result<reqwest::Client> {
    let mut headers = reqwest::header::HeaderMap::new();
    for (name, value) in &config.custom_headers {
        let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())?;
        let mut value = reqwest::header::HeaderValue::from_str(value)?;
        value.set_sensitive(true);
        headers.insert(name, value);
    }
    Ok(reqwest::Client::builder()
        .default_headers(headers)
        .connect_timeout(std::time::Duration::from_secs(30))
        .timeout(std::time::Duration::from_secs(300))
        .pool_max_idle_per_host(0)
        .http1_only()
        .build()?)
}

pub fn create_provider(
    provider_config: &ProviderConfig,
    model_config: &ModelConfig,
) -> anyhow::Result<Box<dyn LlmProvider>> {
    let provider: Box<dyn LlmProvider> = match provider_config.provider_type.as_str() {
        "openai-responses" | "openai_responses" | "responses" => {
            Box::new(OpenAiProvider::responses(provider_config)?)
        }
        "openai-legacy" | "openai-chat" => Box::new(OpenAiProvider::new(provider_config)?),
        "openai" => {
            // Auto-select Responses for reasoning / GPT-4.1+ models.
            if crate::catalog::prefers_responses_api(&model_config.model) {
                Box::new(OpenAiProvider::responses(provider_config)?)
            } else {
                Box::new(OpenAiProvider::new(provider_config)?)
            }
        }
        "google" | "google-genai" | "gemini" => Box::new(GoogleProvider::new(provider_config)?),
        "anthropic" => Box::new(AnthropicProvider::new(provider_config)?),
        "kimi" => Box::new(KimiProvider::new(provider_config)?),
        other => anyhow::bail!("unsupported LLM provider type: {other}"),
    };
    Ok(provider)
}
