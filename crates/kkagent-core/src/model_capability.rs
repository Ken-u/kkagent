//! Model capability registry derived from config `capabilities` lists.

use kkagent_config::ModelConfig;
use std::collections::HashSet;

#[derive(Debug, Clone, Default)]
pub struct ModelCapability {
    pub tools: bool,
    pub vision: bool,
    pub thinking: bool,
    pub audio: bool,
    pub video: bool,
    pub max_context: Option<u64>,
    pub max_output: Option<u64>,
    raw: HashSet<String>,
}

impl ModelCapability {
    pub fn from_model(model: &ModelConfig) -> Self {
        let raw: HashSet<String> = model
            .capabilities
            .iter()
            .map(|c| c.to_lowercase())
            .collect();
        let has = |keys: &[&str]| keys.iter().any(|k| raw.contains(*k));
        // Default: tools enabled unless explicitly disabled.
        let tools = if raw.is_empty() {
            true
        } else {
            has(&["tools", "tool_use", "function_calling"]) || !has(&["no_tools"])
        };
        Self {
            tools,
            vision: has(&["vision", "image", "image_in", "multimodal"]),
            thinking: has(&["thinking", "reasoning", "extended_thinking"]),
            audio: has(&["audio", "audio_in"]),
            video: has(&["video", "video_in"]),
            max_context: model.max_context_size,
            max_output: model.max_output_size,
            raw,
        }
    }

    pub fn supports(&self, name: &str) -> bool {
        self.raw.contains(&name.to_lowercase())
    }

    pub fn usable_context(&self, reserved: u64) -> Option<u64> {
        self.max_context.map(|m| m.saturating_sub(reserved))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_vision_and_thinking() {
        let m = ModelConfig {
            provider: "p".into(),
            model: "m".into(),
            max_context_size: Some(200_000),
            max_output_size: Some(8192),
            capabilities: vec!["vision".into(), "thinking".into(), "tools".into()],
            display_name: None,
            support_efforts: vec![],
            default_effort: None,
            pricing: None,
            experimental_adaptive_thinking: false,
            experimental_visible_empty_retries: 0,
        };
        let c = ModelCapability::from_model(&m);
        assert!(c.vision);
        assert!(c.thinking);
        assert!(c.tools);
        assert_eq!(c.max_context, Some(200_000));
    }

    #[test]
    fn parses_kimi_input_capability_names() {
        let m = ModelConfig {
            provider: "p".into(),
            model: "m".into(),
            max_context_size: None,
            max_output_size: None,
            capabilities: vec!["image_in".into(), "video_in".into(), "audio_in".into()],
            display_name: None,
            support_efforts: vec![],
            default_effort: None,
            pricing: None,
            experimental_adaptive_thinking: false,
            experimental_visible_empty_retries: 0,
        };
        let c = ModelCapability::from_model(&m);
        assert!(c.vision);
        assert!(c.video);
        assert!(c.audio);
    }
}
