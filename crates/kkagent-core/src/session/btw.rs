//! BTW side-question service — kimi-code `SessionBtwService` parity.
//!
//! Tracks a text-only side channel (prior Q&A + optional child agent id)
//! and streams answers against main-session history without tools.

use crate::session::agent_lifecycle::{AgentLifecycleService, CreateAgentOptions, MAIN_AGENT_ID};
use crate::session::metadata::AgentKind;
use anyhow::{anyhow, Result};
use kkagent_config::AppConfig;
use kkagent_llm::create_provider;
use kkagent_llm::types::{ChatContent, ChatMessage, LlmRequest, StreamEvent};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, RwLock};
use tokio::sync::mpsc;

pub const TOOL_CALL_DISABLED_MESSAGE: &str =
    "Tool calls are disabled for side questions. Answer with text only.";

pub const SIDE_QUESTION_SYSTEM_REMINDER: &str = r#"<system-reminder>
You are answering a side question about the current conversation.
This is a temporary side conversation - your answer will NOT be added to the main conversation history.
Answer based on the conversation context provided. Be helpful and concise.
Match the language of the user's most recent question in your reply; do not default to English or any other fixed language.
Do not use tools. Respond with text only.
</system-reminder>"#;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BtwTurn {
    pub question: String,
    pub answer: String,
}

#[derive(Default)]
pub struct SessionBtwService {
    active_agent_id: RwLock<Option<String>>,
    context_snapshot: RwLock<Option<Vec<ChatMessage>>>,
    notes: RwLock<Vec<String>>,
    turns: RwLock<Vec<BtwTurn>>,
    /// When true, the in-flight side stream should stop emitting.
    cancel: RwLock<Arc<AtomicBool>>,
    busy: AtomicBool,
}

impl SessionBtwService {
    pub fn new() -> Self {
        Self::default()
    }

    /// Start (or reuse) the btw child agent; returns agent id.
    pub fn start(&self, agents: &AgentLifecycleService) -> String {
        if let Some(id) = self
            .active_agent_id
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            if agents.get(&id).is_some() {
                return id;
            }
        }
        let mut labels = HashMap::new();
        labels.insert("role".into(), "btw".into());
        let handle = agents
            .fork(
                MAIN_AGENT_ID,
                CreateAgentOptions {
                    kind: Some(AgentKind::Sub),
                    labels,
                    ..Default::default()
                },
            )
            .unwrap_or_else(|_| {
                agents.create(CreateAgentOptions {
                    agent_id: Some(format!("btw-{}", uuid::Uuid::new_v4())),
                    kind: Some(AgentKind::Sub),
                    ..Default::default()
                })
            });
        *self
            .active_agent_id
            .write()
            .unwrap_or_else(|e| e.into_inner()) = Some(handle.id.clone());
        handle.id
    }

    pub fn active_agent_id(&self) -> Option<String> {
        self.active_agent_id
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    /// Capture the parent conversation once, when this BTW conversation starts.
    /// Follow-up questions keep using that stable fork point, just like a real
    /// child agent, even if the main session continues in the meantime.
    pub fn context_snapshot(&self, history: &[ChatMessage]) -> Vec<ChatMessage> {
        let mut snapshot = self
            .context_snapshot
            .write()
            .unwrap_or_else(|e| e.into_inner());
        snapshot.get_or_insert_with(|| history.to_vec()).clone()
    }

    /// Delete this session's side conversation and dispose its virtual child.
    /// An in-flight request is asked to stop; `busy` remains set until that
    /// request observes cancellation. A fresh cancellation token allows a new
    /// `/btw` prompt to start immediately after replacement.
    pub fn clear(&self, agents: &AgentLifecycleService) {
        let old_cancel = {
            let mut cancel = self.cancel.write().unwrap_or_else(|e| e.into_inner());
            std::mem::replace(&mut *cancel, Arc::new(AtomicBool::new(false)))
        };
        old_cancel.store(true, Ordering::SeqCst);
        self.busy.store(false, Ordering::SeqCst);
        if let Some(agent_id) = self
            .active_agent_id
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .take()
        {
            agents.remove(&agent_id);
        }
        *self
            .context_snapshot
            .write()
            .unwrap_or_else(|e| e.into_inner()) = None;
        self.notes
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
        self.turns
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    pub fn add_note(&self, note: impl Into<String>) {
        self.notes
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .push(note.into());
    }

    pub fn notes(&self) -> Vec<String> {
        self.notes.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn turns(&self) -> Vec<BtwTurn> {
        self.turns.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn push_turn(&self, turn: BtwTurn) {
        self.turns
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .push(turn);
    }

    pub fn system_reminder(&self) -> &'static str {
        SIDE_QUESTION_SYSTEM_REMINDER.trim()
    }

    pub fn is_busy(&self) -> bool {
        self.busy.load(Ordering::SeqCst)
    }

    pub fn try_begin(&self) -> bool {
        self.busy
            .compare_exchange(false, true, Ordering::SeqCst, Ordering::SeqCst)
            .is_ok()
    }

    pub fn is_current(&self, cancel: &Arc<AtomicBool>) -> bool {
        Arc::ptr_eq(
            &self.cancel.read().unwrap_or_else(|e| e.into_inner()),
            cancel,
        )
    }

    pub fn end(&self, cancel: &Arc<AtomicBool>) {
        if self.is_current(cancel) {
            self.busy.store(false, Ordering::SeqCst);
        }
    }

    pub fn request_cancel(&self) {
        self.cancel
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .store(true, Ordering::SeqCst);
    }

    pub fn cancel_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.cancel.read().unwrap_or_else(|e| e.into_inner()))
    }

    /// Build messages for a side question (history + prior BTW turns + new question).
    pub fn build_messages(
        history: &[ChatMessage],
        prior_turns: &[BtwTurn],
        question: &str,
    ) -> Vec<ChatMessage> {
        let mut messages = Vec::new();

        for msg in history {
            let text = chat_message_text(msg);
            if text.trim().is_empty() {
                continue;
            }
            match msg.role.as_str() {
                "user" | "assistant" => {
                    messages.push(ChatMessage {
                        role: msg.role.clone(),
                        content: vec![ChatContent::Text { text }],
                        tools: None,
                    });
                }
                _ => {}
            }
        }

        for turn in prior_turns {
            messages.push(ChatMessage {
                role: "user".into(),
                content: vec![ChatContent::Text {
                    text: turn.question.clone(),
                }],
                tools: None,
            });
            messages.push(ChatMessage {
                role: "assistant".into(),
                content: vec![ChatContent::Text {
                    text: turn.answer.clone(),
                }],
                tools: None,
            });
        }

        let q = if question.trim().is_empty() {
            "Continue".to_string()
        } else {
            question.to_string()
        };
        messages.push(ChatMessage {
            role: "user".into(),
            content: vec![ChatContent::Text { text: q }],
            tools: None,
        });

        messages
    }

    /// Stream a side-question answer. Emits TextDelta / ThinkingDelta / MessageEnd / Error
    /// on `event_tx`. Does not use tools. Honors `cancel`.
    pub async fn stream_side_question(
        config: &AppConfig,
        model_alias: &str,
        history: &[ChatMessage],
        prior_turns: &[BtwTurn],
        question: &str,
        event_tx: mpsc::Sender<StreamEvent>,
        cancel: Arc<AtomicBool>,
    ) -> Result<()> {
        let model_alias = if model_alias.is_empty() {
            config
                .default_model_alias()
                .unwrap_or("default")
                .to_string()
        } else {
            model_alias.to_string()
        };
        // A session running on an ACP agent has no LLM provider of its own,
        // and the agent cannot be asked a side question while it works: answer
        // with the configured default model and say so.
        let model_alias = if kkagent_config::has_acp_alias_prefix(&model_alias) {
            let Some(default_alias) = config.default_model_alias() else {
                anyhow::bail!(
                    "/btw needs a regular model, but this session runs on the ACP agent \
                     '{model_alias}' and no default_model is configured."
                );
            };
            let _ = event_tx
                .send(StreamEvent::TextDelta(format!(
                    "(当前会话由 ACP 后端运行,/btw 改用默认模型 {default_alias} 回答)\n\n"
                )))
                .await;
            default_alias.to_string()
        } else {
            model_alias
        };
        let (model_config, provider_config) = config
            .resolve_model(&model_alias)
            .ok_or_else(|| anyhow!("Model '{}' not found", model_alias))?;

        let messages = Self::build_messages(history, prior_turns, question);
        let request = LlmRequest {
            model: model_config.model.clone(),
            messages,
            tools: vec![],
            max_tokens: model_config.max_output_size.map(|v| v as u32),
            system: Some(SIDE_QUESTION_SYSTEM_REMINDER.to_string()),
            thinking: None,
            prompt_cache_key: None,
            first_token_timeout: kkagent_config::resolve_first_token_timeout(
                model_config,
                provider_config,
            ),
        };

        let provider = create_provider(provider_config, model_config)?;
        let (stream_tx, mut stream_rx) = mpsc::channel::<StreamEvent>(256);
        let err_tx = stream_tx.clone();
        let handle = tokio::spawn(async move {
            if let Err(e) = provider.stream_chat(request, stream_tx).await {
                let _ = err_tx.send(kkagent_llm::stream_error_event(&e)).await;
            }
        });

        while let Some(evt) = stream_rx.recv().await {
            if cancel.load(Ordering::SeqCst) {
                handle.abort();
                let _ = event_tx.send(StreamEvent::Error("cancelled".into())).await;
                break;
            }
            if event_tx.send(evt).await.is_err() {
                handle.abort();
                break;
            }
        }
        Ok(())
    }
}

fn chat_message_text(msg: &ChatMessage) -> String {
    let mut out = String::new();
    for block in &msg.content {
        match block {
            ChatContent::Text { text } => {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(text);
            }
            ChatContent::Thinking { thinking } => {
                if !out.is_empty() {
                    out.push('\n');
                }
                out.push_str(thinking);
            }
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn build_messages_includes_history_and_btw_turns() {
        let history = vec![
            ChatMessage {
                role: "user".into(),
                content: vec![ChatContent::Text {
                    text: "main q".into(),
                }],
                tools: None,
            },
            ChatMessage {
                role: "assistant".into(),
                content: vec![ChatContent::Text {
                    text: "main a".into(),
                }],
                tools: None,
            },
        ];

        let prior = vec![BtwTurn {
            question: "side q1".into(),
            answer: "side a1".into(),
        }];
        let msgs = SessionBtwService::build_messages(&history, &prior, "side q2");
        assert_eq!(msgs.len(), 5);
        assert_eq!(msgs[0].role, "user");
        assert_eq!(msgs[4].role, "user");
    }

    #[test]
    fn context_snapshot_is_stable_until_cleared() {
        let svc = SessionBtwService::new();
        let agents = AgentLifecycleService::new();
        let first = vec![ChatMessage {
            role: "user".into(),
            content: vec![ChatContent::Text {
                text: "first".into(),
            }],
            tools: None,
        }];
        let later = vec![ChatMessage {
            role: "user".into(),
            content: vec![ChatContent::Text {
                text: "later".into(),
            }],
            tools: None,
        }];

        assert_eq!(chat_message_text(&svc.context_snapshot(&first)[0]), "first");
        assert_eq!(chat_message_text(&svc.context_snapshot(&later)[0]), "first");

        svc.clear(&agents);
        assert_eq!(chat_message_text(&svc.context_snapshot(&later)[0]), "later");
    }

    fn side_question_config(default_model: Option<&str>) -> AppConfig {
        use kkagent_config::{ModelConfig, ProviderConfig};
        let mut config = AppConfig {
            default_model: default_model.map(str::to_string),
            ..AppConfig::default()
        };
        config.providers.insert(
            "test".into(),
            ProviderConfig {
                provider_type: "openai-chat".into(),
                api_key: Some("token".into()),
                api_key_env: None,
                // Nothing listens here: the request fails right away.
                base_url: Some("http://127.0.0.1:1".into()),
                custom_headers: Default::default(),
                oauth: None,
                first_token_timeout_ms: None,
                request_timeout_ms: None,
                read_timeout_ms: None,
                extra_fields: Default::default(),
            },
        );
        config.models.insert(
            "test/model".into(),
            ModelConfig {
                provider: "test".into(),
                model: "test-model".into(),
                max_context_size: Some(16_000),
                max_output_size: Some(1_000),
                capabilities: Vec::new(),
                display_name: None,
                support_efforts: Vec::new(),
                default_effort: None,
                pricing: None,
                experimental_adaptive_thinking: false,
                experimental_vision_proxy: false,
                experimental_visible_empty_retries: 0,
                experimental_bad_toolcall_auto_retries: 0,
                experimental_retry_partial_stream_errors: None,
                first_token_timeout_ms: None,
            },
        );
        config
    }

    #[tokio::test]
    async fn side_question_on_an_acp_session_uses_the_default_model_and_says_so() {
        let config = side_question_config(Some("test/model"));
        let (tx, mut rx) = mpsc::channel(16);
        let result = SessionBtwService::stream_side_question(
            &config,
            "acp:stub-plugin.stub",
            &[],
            &[],
            "what is a monad?",
            tx,
            Arc::new(AtomicBool::new(false)),
        )
        .await;
        assert!(result.is_ok(), "{result:?}");
        match rx.recv().await {
            Some(StreamEvent::TextDelta(notice)) => {
                assert!(
                    notice.contains("ACP") && notice.contains("test/model"),
                    "{notice}"
                );
            }
            other => panic!("expected the fallback notice first, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn side_question_on_an_acp_session_needs_a_default_model() {
        let config = side_question_config(None);
        let (tx, _rx) = mpsc::channel(16);
        let error = SessionBtwService::stream_side_question(
            &config,
            "acp:stub-plugin.stub",
            &[],
            &[],
            "q",
            tx,
            Arc::new(AtomicBool::new(false)),
        )
        .await
        .unwrap_err();
        assert!(
            error.to_string().contains("needs a regular model"),
            "{error}"
        );
    }

    #[test]
    fn try_begin_is_exclusive() {
        let svc = SessionBtwService::new();
        assert!(svc.try_begin());
        assert!(!svc.try_begin());
        let cancel = svc.cancel_flag();
        svc.end(&cancel);
        assert!(svc.try_begin());
    }
}
