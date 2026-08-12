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
    notes: RwLock<Vec<String>>,
    turns: RwLock<Vec<BtwTurn>>,
    /// When true, the in-flight side stream should stop emitting.
    cancel: Arc<AtomicBool>,
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

    pub fn end(&self) {
        self.busy.store(false, Ordering::SeqCst);
        self.cancel.store(false, Ordering::SeqCst);
    }

    pub fn request_cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    pub fn cancel_flag(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.cancel)
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
            });
            messages.push(ChatMessage {
                role: "assistant".into(),
                content: vec![ChatContent::Text {
                    text: turn.answer.clone(),
                }],
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
        };

        let provider = create_provider(provider_config, model_config)?;
        let (stream_tx, mut stream_rx) = mpsc::channel::<StreamEvent>(256);
        let err_tx = stream_tx.clone();
        let handle = tokio::spawn(async move {
            if let Err(e) = provider.stream_chat(request, stream_tx).await {
                let _ = err_tx.send(StreamEvent::Error(e.to_string())).await;
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
            },
            ChatMessage {
                role: "assistant".into(),
                content: vec![ChatContent::Text {
                    text: "main a".into(),
                }],
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
    fn try_begin_is_exclusive() {
        let svc = SessionBtwService::new();
        assert!(svc.try_begin());
        assert!(!svc.try_begin());
        svc.end();
        assert!(svc.try_begin());
    }
}
