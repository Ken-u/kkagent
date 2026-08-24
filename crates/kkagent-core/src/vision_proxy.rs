//! Vision proxy for non-vision primary models.
//!
//! A model configured with `experimental_vision_proxy = true` acts as a shared
//! multimodal interface: when the active primary model declares no image input
//! capability, image blocks in the outgoing request are swapped for text
//! descriptions produced by the proxy model right before the request goes out.
//! Session history keeps the original image blocks; substitution only mutates
//! the projected per-turn copies, so switching back to a vision model restores
//! native reading without any history rewrite.
//!
//! Descriptions are cached by the SHA-256 of the base64 payload, so repeated
//! rounds (same image re-sent every step) cost one proxy call in total. The
//! existing `fold_old_media` compaction still bounds how many images stay in
//! history.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use kkagent_config::AppConfig;
use kkagent_llm::{
    create_provider, ChatContent, ChatMessage, LlmProvider, LlmRequest, StreamEvent, ToolDef,
};
use sha2::{Digest, Sha256};
use tokio::sync::Mutex;

/// Description cache keyed by SHA-256 of the base64 image payload.
pub type VisionCache = HashMap<String, String>;

const DESCRIBE_SYSTEM: &str = "You describe images for a coding agent whose primary model cannot see images. \
Produce a precise, self-contained description: transcribe visible text verbatim (UI labels, code, error messages, \
file paths), describe layout and UI elements, report tables and charts with their actual numbers, and explain \
diagram structure node by node. Be factual; only mention what is visible. Compact markdown, no preamble.";

/// Overall wall-clock budget for one proxy description call.
const DESCRIBE_TIMEOUT: Duration = Duration::from_secs(120);
/// Output cap for one description.
const DESCRIBE_MAX_TOKENS: u32 = 2048;

/// Whether the vision proxy should engage this turn: the primary model lacks
/// image input support and a proxy model is configured.
pub fn engaged(config: &AppConfig, capability_vision: bool) -> bool {
    !capability_vision && config.vision_proxy().is_some()
}

pub fn count_image_blocks(messages: &[ChatMessage]) -> usize {
    messages
        .iter()
        .flat_map(|message| message.content.iter())
        .filter(|part| matches!(part, ChatContent::Image { .. }))
        .count()
}

fn image_key(data: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Replacement text for one image block once its description is known.
fn replacement_text(media_type: &str, alias: &str, description: &str) -> String {
    format!(
        "[image {media_type} described by vision proxy '{alias}']\n\
         {description}\n\
         [To see the original image, ask ReadMediaFile again with its path.]"
    )
}

/// Replace every image block with a proxy-generated description.
///
/// Two slices are modified in lock-step:
/// - `session_messages`: the persistent history. Image blocks are permanently
///   replaced by text descriptions, dropping the base64 payload to save memory
///   and context budget. The original file path remains in the surrounding
///   message text (user `<image-attached>` markers or ReadMediaFile tool
///   results), so a vision model can re-read the source later.
/// - `turn_messages`: the per-turn working copy sent to the primary model.
///
/// Returns the number of blocks replaced. Each unique image is described at
/// most once thanks to the shared cache.
pub async fn substitute_image_blocks(
    config: &AppConfig,
    cache: &Mutex<VisionCache>,
    session_messages: &mut [ChatMessage],
    turn_messages: &mut [ChatMessage],
) -> anyhow::Result<usize> {
    if count_image_blocks(session_messages) == 0 && count_image_blocks(turn_messages) == 0 {
        return Ok(0);
    }
    let Some((alias, model_cfg, provider_cfg)) = config.vision_proxy() else {
        return Ok(0);
    };
    let provider: Arc<dyn LlmProvider> = Arc::from(
        create_provider(&provider_cfg, &model_cfg)
            .map_err(|e| anyhow::anyhow!("vision proxy '{alias}' provider init failed: {e}"))?,
    );

    let mut replaced = 0_usize;
    for message in session_messages.iter_mut() {
        replaced += replace_image_blocks_in_message(
            &alias,
            &model_cfg,
            &provider_cfg,
            &provider,
            cache,
            message,
        )
        .await?;
    }
    // Session is now text-only; mirror the same descriptions into the turn copy
    // so the outgoing request matches.
    for message in turn_messages.iter_mut() {
        replaced += replace_image_blocks_in_message(
            &alias,
            &model_cfg,
            &provider_cfg,
            &provider,
            cache,
            message,
        )
        .await?;
    }
    Ok(replaced)
}

/// Replace `Image` blocks inside a single message with cached/fresh descriptions.
async fn replace_image_blocks_in_message(
    alias: &str,
    model_cfg: &kkagent_config::ModelConfig,
    provider_cfg: &kkagent_config::ProviderConfig,
    provider: &Arc<dyn LlmProvider>,
    cache: &Mutex<VisionCache>,
    message: &mut ChatMessage,
) -> anyhow::Result<usize> {
    if !message
        .content
        .iter()
        .any(|c| matches!(c, ChatContent::Image { .. }))
    {
        return Ok(0);
    }
    let mut replaced = 0_usize;
    for part in message.content.iter_mut() {
        let ChatContent::Image { media_type, data } = part else {
            continue;
        };
        let key = image_key(data);
        let description = {
            let cached = cache.lock().await.get(&key).cloned();
            match cached {
                Some(description) => description,
                None => {
                    let description =
                        describe_one(provider.clone(), model_cfg, provider_cfg, media_type, data)
                            .await
                            .map_err(|error| {
                                anyhow::anyhow!(
                                    "vision proxy '{alias}' failed to describe image: {error}"
                                )
                            })?;
                    cache.lock().await.insert(key, description.clone());
                    description
                }
            }
        };
        *part = ChatContent::Text {
            text: replacement_text(media_type, alias, &description),
        };
        replaced += 1;
    }
    Ok(replaced)
}

/// One-shot describe call against the proxy model.
async fn describe_one(
    provider: Arc<dyn LlmProvider>,
    model_cfg: &kkagent_config::ModelConfig,
    provider_cfg: &kkagent_config::ProviderConfig,
    media_type: &str,
    data: &str,
) -> Result<String, String> {
    let prompt = format!(
        "Describe this {media_type} image for the agent. Capture all text verbatim, \
layout, and any data it shows."
    );
    let request = LlmRequest {
        model: model_cfg.model.clone(),
        messages: vec![ChatMessage {
            role: "user".into(),
            content: vec![
                ChatContent::Text { text: prompt },
                ChatContent::Image {
                    media_type: media_type.to_string(),
                    data: data.to_string(),
                },
            ],
            tools: None,
        }],
        tools: Vec::<ToolDef>::new(),
        max_tokens: Some(DESCRIBE_MAX_TOKENS),
        system: Some(DESCRIBE_SYSTEM.into()),
        thinking: None,
        prompt_cache_key: None,
        first_token_timeout: kkagent_config::resolve_first_token_timeout(model_cfg, provider_cfg),
    };

    let (tx, mut rx) = tokio::sync::mpsc::channel::<StreamEvent>(64);
    let handle = tokio::spawn(async move {
        if let Err(error) = provider.stream_chat(request, tx.clone()).await {
            let _ = tx.send(kkagent_llm::stream_error_event(&error)).await;
        }
    });
    let mut out = String::new();
    let collected = tokio::time::timeout(DESCRIBE_TIMEOUT, async {
        while let Some(event) = rx.recv().await {
            match event {
                StreamEvent::TextDelta(delta) => out.push_str(&delta),
                StreamEvent::MessageEnd { .. } => break,
                StreamEvent::Error(error) => return Err(error),
                StreamEvent::RateLimited { message, .. } => return Err(message),
                _ => {}
            }
        }
        Ok(())
    })
    .await;
    let _ = handle.await;
    match collected {
        Ok(Ok(())) if !out.trim().is_empty() => Ok(out),
        Ok(Ok(())) => Err("empty response".into()),
        Ok(Err(error)) => Err(error),
        Err(_) => Err(format!("timed out after {}s", DESCRIBE_TIMEOUT.as_secs())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use kkagent_llm::ChatMessage;

    fn image_msg(data: &str) -> ChatMessage {
        ChatMessage {
            role: "user".into(),
            content: vec![
                ChatContent::Text {
                    text: "look".into(),
                },
                ChatContent::Image {
                    media_type: "image/jpeg".into(),
                    data: data.into(),
                },
            ],
            tools: None,
        }
    }

    #[test]
    fn counts_image_blocks_across_messages() {
        let messages = vec![
            ChatMessage {
                role: "user".into(),
                content: vec![ChatContent::Text { text: "hi".into() }],
                tools: None,
            },
            image_msg("aaa"),
            image_msg("bbb"),
        ];
        assert_eq!(count_image_blocks(&messages), 2);
        assert_eq!(count_image_blocks(&messages[..1]), 0);
    }

    #[test]
    fn replacement_text_mentions_media_type_and_alias() {
        let text = replacement_text("image/png", "proxy-a", "a red circle");
        assert!(text.contains("image/png"));
        assert!(text.contains("proxy-a"));
        assert!(text.contains("a red circle"));
    }

    #[tokio::test]
    async fn substitution_without_images_is_a_noop() {
        let config = AppConfig::default();
        let cache = Mutex::new(VisionCache::default());
        let mut session = vec![ChatMessage {
            role: "user".into(),
            content: vec![ChatContent::Text { text: "hi".into() }],
            tools: None,
        }];
        let mut turn = session.clone();
        // Even without a configured proxy, zero images short-circuits.
        let replaced = substitute_image_blocks(&config, &cache, &mut session, &mut turn)
            .await
            .unwrap();
        assert_eq!(replaced, 0);
    }

    #[test]
    fn replacement_text_mentions_readmedia_hint() {
        let text = replacement_text("image/png", "proxy-a", "a red circle");
        assert!(text.contains("ReadMediaFile"));
    }
}
