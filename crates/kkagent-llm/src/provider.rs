use std::collections::HashMap;
use std::sync::{Arc, OnceLock, RwLock};

use async_trait::async_trait;
use tokio::sync::mpsc;

use crate::types::{LlmRequest, StreamEvent};

/// Everything a provider needs to talk to an LLM endpoint.
///
/// This is the whole contract between a caller and this crate: the crate does
/// not know where the values come from (a TOML config, an embedding host, a
/// test), which is what lets other programs use the providers as a library
/// without pulling in the CLI's configuration layer.
#[derive(Debug, Clone, Default)]
pub struct ProviderSettings {
    /// `openai`, `openai-responses`, `openai-legacy`, `anthropic`, `google`,
    /// `kimi`, or a type registered with [`register_provider_type`].
    pub provider_type: String,
    /// Endpoint root; each provider has its own default when unset.
    pub base_url: Option<String>,
    /// Already-resolved key (the caller reads environment variables or
    /// credential stores itself).
    pub api_key: Option<String>,
    /// Extra request headers; values are marked sensitive.
    pub custom_headers: HashMap<String, String>,
    /// Whole-request deadline in milliseconds; unset or `0` disables it.
    pub request_timeout_ms: Option<u64>,
    /// Per-read idle timeout in milliseconds; unset = 180 s, `0` disables it.
    pub read_timeout_ms: Option<u64>,
}

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
    pub fn new(settings: &ProviderSettings) -> anyhow::Result<Self> {
        let base_url = settings
            .base_url
            .as_deref()
            .unwrap_or("https://api.anthropic.com")
            .trim_end_matches('/')
            .to_string();
        let api_key = settings.api_key.clone().unwrap_or_default();
        Ok(Self {
            client: build_client(settings)?,
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
    /// `Some(true)` forces the Responses API (`/v1/responses`), `Some(false)` forces chat
    /// completions (what `openai-chat` / `openai-legacy` promise: a compatible gateway that
    /// only speaks chat completions must never be sent a model name that merely *looks* like
    /// a Responses-era one), `None` picks by model.
    responses: Option<bool>,
}

impl OpenAiProvider {
    /// Chat completions only.
    pub fn new(settings: &ProviderSettings) -> anyhow::Result<Self> {
        Self::with_responses(settings, Some(false))
    }

    /// Responses API only.
    pub fn responses(settings: &ProviderSettings) -> anyhow::Result<Self> {
        Self::with_responses(settings, Some(true))
    }

    /// Responses API for the models that prefer it, chat completions for the rest.
    pub fn auto(settings: &ProviderSettings) -> anyhow::Result<Self> {
        Self::with_responses(settings, None)
    }

    fn with_responses(
        settings: &ProviderSettings,
        responses: Option<bool>,
    ) -> anyhow::Result<Self> {
        let base_url = settings
            .base_url
            .as_deref()
            .unwrap_or("https://api.openai.com")
            .trim_end_matches('/')
            .to_string();
        Ok(Self {
            client: build_client(settings)?,
            base_url,
            api_key: settings.api_key.clone().unwrap_or_default(),
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
        let use_responses = self
            .responses
            .unwrap_or_else(|| crate::catalog::prefers_responses_api(&request.model));
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
    pub fn new(settings: &ProviderSettings) -> anyhow::Result<Self> {
        Ok(Self {
            client: build_client(settings)?,
            base_url: settings
                .base_url
                .as_deref()
                .unwrap_or("https://api.moonshot.ai/v1")
                .trim_end_matches('/')
                .to_string(),
            api_key: settings.api_key.clone().unwrap_or_default(),
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
    pub fn new(settings: &ProviderSettings) -> anyhow::Result<Self> {
        let base_url = settings
            .base_url
            .as_deref()
            .unwrap_or("https://generativelanguage.googleapis.com")
            .trim_end_matches('/')
            .to_string();
        Ok(Self {
            client: build_client(settings)?,
            base_url,
            api_key: settings.api_key.clone().unwrap_or_default(),
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

fn build_client(settings: &ProviderSettings) -> anyhow::Result<reqwest::Client> {
    let mut headers = reqwest::header::HeaderMap::new();
    for (name, value) in &settings.custom_headers {
        let name = reqwest::header::HeaderName::from_bytes(name.as_bytes())?;
        let mut value = reqwest::header::HeaderValue::from_str(value)?;
        value.set_sensitive(true);
        headers.insert(name, value);
    }
    // No default total timeout: streaming LLM responses can legitimately run
    // for minutes, and a whole-request deadline kills healthy long
    // generations mid-stream (surfacing as `kind=decode, kind=timeout`).
    // Stalls are bounded by the per-read idle timeout below plus the
    // first-token gate; a total deadline stays opt-in via
    // `request_timeout_ms`.
    //
    // The idle default is 180s: some OpenAI-compatible gateways do not stream
    // tool-call arguments incrementally but buffer the entire call (tens of
    // KB of JSON) and emit it in a single chunk — a 60s idle bound aborted
    // such healthy generations mid-stream.
    const DEFAULT_READ_TIMEOUT: u64 = 180_000;
    let read_timeout_ms = settings.read_timeout_ms.unwrap_or(DEFAULT_READ_TIMEOUT);
    let mut builder = reqwest::Client::builder()
        .default_headers(headers)
        .connect_timeout(std::time::Duration::from_secs(30))
        .pool_max_idle_per_host(0)
        .http1_only();
    // `0` explicitly disables the per-read idle timeout.
    if read_timeout_ms > 0 {
        builder = builder.read_timeout(std::time::Duration::from_millis(read_timeout_ms));
    }
    if let Some(ms) = settings.request_timeout_ms.filter(|ms| *ms > 0) {
        builder = builder.timeout(std::time::Duration::from_millis(ms));
    }
    Ok(builder.build()?)
}

/// Builds a provider of an externally defined type from its settings and the
/// model name.
pub type ProviderFactory =
    Arc<dyn Fn(&ProviderSettings, &str) -> anyhow::Result<Box<dyn LlmProvider>> + Send + Sync>;

fn extra_types() -> &'static RwLock<HashMap<String, ProviderFactory>> {
    static EXTRA: OnceLock<RwLock<HashMap<String, ProviderFactory>>> = OnceLock::new();
    EXTRA.get_or_init(|| RwLock::new(HashMap::new()))
}

/// Make `provider_type` known to [`create_provider`]. A program that embeds
/// this crate uses it to add a provider that is not an HTTP endpoint (an
/// embedding host that makes the model calls itself, a test double). Built-in
/// types cannot be replaced, and registering a name again replaces the earlier
/// factory.
pub fn register_provider_type(provider_type: &str, factory: ProviderFactory) -> anyhow::Result<()> {
    if is_builtin_type(provider_type) {
        anyhow::bail!("`{provider_type}` is a built-in provider type and cannot be replaced");
    }
    extra_types()
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .insert(provider_type.to_string(), factory);
    Ok(())
}

fn is_builtin_type(provider_type: &str) -> bool {
    matches!(
        provider_type,
        "openai-responses"
            | "openai_responses"
            | "responses"
            | "openai-legacy"
            | "openai-chat"
            | "openai"
            | "google"
            | "google-genai"
            | "gemini"
            | "anthropic"
            | "kimi"
    )
}

pub fn create_provider(
    settings: &ProviderSettings,
    model: &str,
) -> anyhow::Result<Box<dyn LlmProvider>> {
    let provider: Box<dyn LlmProvider> = match settings.provider_type.as_str() {
        "openai-responses" | "openai_responses" | "responses" => {
            Box::new(OpenAiProvider::responses(settings)?)
        }
        "openai-legacy" | "openai-chat" => Box::new(OpenAiProvider::new(settings)?),
        "openai" => {
            // Auto-select Responses for reasoning / GPT-4.1+ models.
            Box::new(OpenAiProvider::auto(settings)?)
        }
        "google" | "google-genai" | "gemini" => Box::new(GoogleProvider::new(settings)?),
        "anthropic" => Box::new(AnthropicProvider::new(settings)?),
        "kimi" => Box::new(KimiProvider::new(settings)?),
        other => {
            let factory = extra_types()
                .read()
                .unwrap_or_else(std::sync::PoisonError::into_inner)
                .get(other)
                .cloned();
            match factory {
                Some(factory) => factory(settings, model)?,
                None => anyhow::bail!("unsupported LLM provider type: {other}"),
            }
        }
    };
    Ok(provider)
}

/// Whether a provider's wire format natively supports message-level tool
/// declarations (`messages[].tools`).
///
/// Only such dialects can host progressive tool disclosure without rewriting
/// the top-level `tools[]` prefix — and thereby busting the provider prompt
/// cache — on every `SelectTools` load. The Kimi chat-completions dialect
/// supports it; OpenAI / Anthropic / Google wire formats do not and fold
/// loaded schemas into the top-level `tools[]` instead
/// (`merge_message_level_tools`).
pub fn supports_message_level_tools(provider_type: &str) -> bool {
    matches!(provider_type, "kimi")
}
