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
        Self {
            client: build_client(),
            base_url,
            api_key,
        }
    }
}

#[async_trait]
impl LlmProvider for AnthropicProvider {
    async fn stream_chat(
        &self,
        request: LlmRequest,
        event_tx: mpsc::Sender<StreamEvent>,
    ) -> anyhow::Result<()> {
        with_retries(3, || {
            let client = self.client.clone();
            let base = self.base_url.clone();
            let key = self.api_key.clone();
            let req = request.clone();
            let tx = event_tx.clone();
            async move {
                crate::stream::anthropic_stream(&client, &base, &key, req, tx).await
            }
        })
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
    pub fn new(config: &ProviderConfig) -> Self {
        Self::with_responses(config, false)
    }

    pub fn responses(config: &ProviderConfig) -> Self {
        Self::with_responses(config, true)
    }

    fn with_responses(config: &ProviderConfig, responses: bool) -> Self {
        let base_url = config
            .base_url
            .as_deref()
            .unwrap_or("https://api.openai.com")
            .trim_end_matches('/')
            .to_string();
        Self {
            client: build_client(),
            base_url,
            api_key: config.api_key.clone().unwrap_or_default(),
            responses,
        }
    }
}

#[async_trait]
impl LlmProvider for OpenAiProvider {
    async fn stream_chat(
        &self,
        request: LlmRequest,
        event_tx: mpsc::Sender<StreamEvent>,
    ) -> anyhow::Result<()> {
        let use_responses =
            self.responses || crate::catalog::prefers_responses_api(&request.model);
        with_retries(3, || {
            let client = self.client.clone();
            let base = self.base_url.clone();
            let key = self.api_key.clone();
            let req = request.clone();
            let tx = event_tx.clone();
            let responses = use_responses;
            async move {
                if responses {
                    crate::openai_responses::openai_responses_stream(&client, &base, &key, req, tx)
                        .await
                } else {
                    crate::stream::openai_stream(&client, &base, &key, req, tx).await
                }
            }
        })
        .await
    }
}

pub struct GoogleProvider {
    client: reqwest::Client,
    base_url: String,
    api_key: String,
}

impl GoogleProvider {
    pub fn new(config: &ProviderConfig) -> Self {
        let base_url = config
            .base_url
            .as_deref()
            .unwrap_or("https://generativelanguage.googleapis.com")
            .trim_end_matches('/')
            .to_string();
        Self {
            client: build_client(),
            base_url,
            api_key: config.api_key.clone().unwrap_or_default(),
        }
    }
}

#[async_trait]
impl LlmProvider for GoogleProvider {
    async fn stream_chat(
        &self,
        request: LlmRequest,
        event_tx: mpsc::Sender<StreamEvent>,
    ) -> anyhow::Result<()> {
        with_retries(3, || {
            let client = self.client.clone();
            let base = self.base_url.clone();
            let key = self.api_key.clone();
            let req = request.clone();
            let tx = event_tx.clone();
            async move { crate::stream::google_stream(&client, &base, &key, req, tx).await }
        })
        .await
    }
}

fn build_client() -> reqwest::Client {
    reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(30))
        .timeout(std::time::Duration::from_secs(300))
        .pool_max_idle_per_host(0)
        .http1_only()
        .build()
        .unwrap_or_else(|_| reqwest::Client::new())
}

async fn with_retries<F, Fut>(max: u32, mut make: F) -> anyhow::Result<()>
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        match make().await {
            Ok(()) => return Ok(()),
            Err(e) => {
                let msg = e.to_string();
                let retryable = msg.contains("429")
                    || msg.contains("500")
                    || msg.contains("502")
                    || msg.contains("503")
                    || msg.contains("timeout")
                    || msg.contains("timed out")
                    || msg.contains("connection");
                if !retryable || attempt >= max {
                    return Err(e);
                }
                let backoff = std::time::Duration::from_millis(400 * attempt as u64);
                tracing::warn!(
                    "LLM request failed (attempt {}/{}): {}; retrying in {:?}",
                    attempt,
                    max,
                    msg,
                    backoff
                );
                tokio::time::sleep(backoff).await;
            }
        }
    }
}

pub fn create_provider(
    provider_config: &ProviderConfig,
    model_config: &ModelConfig,
) -> Box<dyn LlmProvider> {
    match provider_config.provider_type.as_str() {
        "openai-responses" | "responses" => {
            Box::new(OpenAiProvider::responses(provider_config))
        }
        "openai-legacy" | "openai-chat" => Box::new(OpenAiProvider::new(provider_config)),
        "openai" => {
            // Auto-select Responses for reasoning / GPT-4.1+ models.
            if crate::catalog::prefers_responses_api(&model_config.model) {
                Box::new(OpenAiProvider::responses(provider_config))
            } else {
                Box::new(OpenAiProvider::new(provider_config))
            }
        }
        "google" | "google-genai" | "gemini" => Box::new(GoogleProvider::new(provider_config)),
        // Kimi intentionally stays on Anthropic-compatible Messages (user request).
        "anthropic" | "kimi" | _ => Box::new(AnthropicProvider::new(provider_config)),
    }
}
