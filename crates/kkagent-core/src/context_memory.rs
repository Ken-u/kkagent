//! contextMemory — vacuous fold, handoff, loop-event fold (agent-core-v2 aligned).

use kkagent_llm::{ChatContent, ChatMessage};

/// True when a content part carries nothing the provider wire can represent.
pub fn is_vacuous_text(text: &str) -> bool {
    text.trim().is_empty()
}

pub fn is_vacuous_part(part: &ChatContent) -> bool {
    match part {
        ChatContent::Text { text } => is_vacuous_text(text),
        ChatContent::Thinking { thinking } => is_vacuous_text(thinking),
        ChatContent::ToolResult { content, .. } => is_vacuous_text(content),
        ChatContent::ToolUse { .. } | ChatContent::Image { .. } | ChatContent::Video { .. } => {
            false
        }
    }
}

pub fn is_vacuous_message(msg: &ChatMessage) -> bool {
    if msg.content.is_empty() {
        return true;
    }
    msg.content.iter().all(is_vacuous_part)
}

/// Drop vacuous assistant/user text messages; keep system / tool-use structure.
pub fn fold_vacuous(messages: &mut Vec<ChatMessage>) -> usize {
    let before = messages.len();
    messages.retain(|m| {
        if m.role == "system" {
            return true;
        }
        // Keep messages that still have a non-vacuous tool_use.
        if m.content
            .iter()
            .any(|c| matches!(c, ChatContent::ToolUse { .. }))
        {
            return true;
        }
        !is_vacuous_message(m)
    });
    before.saturating_sub(messages.len())
}

/// Handoff summary block inserted after full compaction (v1-compatible shape).
#[derive(Debug, Clone)]
pub struct CompactionHandoff {
    pub summary: String,
    pub kept_tail: usize,
}

impl CompactionHandoff {
    pub fn to_system_message(&self) -> ChatMessage {
        ChatMessage {
            role: "system".into(),
            content: vec![ChatContent::Text {
                text: format!(
                    "<compaction-handoff>\n{}\n</compaction-handoff>\n\
                     ({} recent messages retained after compaction.)",
                    self.summary.trim(),
                    self.kept_tail
                ),
            }],
        }
    }
}

/// Fold ephemeral loop events (status noise) out of the projected context.
pub fn fold_loop_events(messages: &[ChatMessage]) -> Vec<ChatMessage> {
    messages
        .iter()
        .filter(|m| {
            !m.content.iter().any(|c| {
                if let ChatContent::Text { text } = c {
                    let trim = text.trim();
                    trim.starts_with("<loop-event>") && trim.ends_with("</loop-event>")
                } else {
                    false
                }
            })
        })
        .cloned()
        .collect()
}

/// Owned variant used on the hot request path so retained messages are moved
/// instead of deep-cloned a second time.
pub fn fold_loop_events_owned(mut messages: Vec<ChatMessage>) -> Vec<ChatMessage> {
    messages.retain(|message| {
        !message.content.iter().any(|content| {
            if let ChatContent::Text { text } = content {
                let trim = text.trim();
                trim.starts_with("<loop-event>") && trim.ends_with("</loop-event>")
            } else {
                false
            }
        })
    });
    messages
}

/// Participants that must coordinate on conversation undo.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UndoParticipant {
    Messages,
    Files,
    Todos,
    Goals,
}

pub fn default_undo_participants() -> Vec<UndoParticipant> {
    vec![
        UndoParticipant::Messages,
        UndoParticipant::Files,
        UndoParticipant::Todos,
        UndoParticipant::Goals,
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vacuous_fold() {
        let mut msgs = vec![
            ChatMessage {
                role: "user".into(),
                content: vec![ChatContent::Text { text: "hi".into() }],
            },
            ChatMessage {
                role: "assistant".into(),
                content: vec![ChatContent::Text { text: "   ".into() }],
            },
        ];
        assert_eq!(fold_vacuous(&mut msgs), 1);
        assert_eq!(msgs.len(), 1);
    }
}
