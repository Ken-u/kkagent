//! Full compaction strategies beyond simple truncate.

use kkagent_llm::{ChatContent, ChatMessage};

use crate::context_projector::{build_compaction_digest, compact_messages};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompactionStrategy {
    /// Keep last N + local digest summary.
    KeepTail,
    /// Drop vacuous assistant/tool noise then keep tail.
    VacuousFold,
    /// Aggressive: keep only user texts + last tool outcomes.
    Handoff,
}

impl CompactionStrategy {
    pub fn parse(s: &str) -> Self {
        match s.trim().to_lowercase().as_str() {
            "vacuous" | "vacuous_fold" => Self::VacuousFold,
            "handoff" => Self::Handoff,
            _ => Self::KeepTail,
        }
    }
}

pub struct CompactionResult {
    pub dropped: usize,
    pub strategy: CompactionStrategy,
}

pub fn compact_full(
    messages: &mut Vec<ChatMessage>,
    keep_last: usize,
    strategy: CompactionStrategy,
) -> CompactionResult {
    match strategy {
        CompactionStrategy::KeepTail => {
            let cut = crate::context_projector::compact_cut_index(messages, keep_last);
            let digest = if cut == 0 {
                "No earlier turns.".into()
            } else {
                build_compaction_digest(&messages[..cut])
            };
            let dropped = compact_messages(messages, keep_last, &digest);
            CompactionResult { dropped, strategy }
        }
        CompactionStrategy::VacuousFold => {
            let before = messages.len();
            messages.retain(|m| !is_vacuous(m));
            let cut = crate::context_projector::compact_cut_index(messages, keep_last);
            let digest = if cut == 0 {
                "No earlier turns.".into()
            } else {
                build_compaction_digest(&messages[..cut])
            };
            let dropped_tail = compact_messages(messages, keep_last, &digest);
            CompactionResult {
                dropped: before.saturating_sub(messages.len()) + dropped_tail,
                strategy,
            }
        }
        CompactionStrategy::Handoff => {
            let mut kept = Vec::new();
            for m in messages.iter() {
                if m.role == "user" {
                    if let Some(text) = first_text(m) {
                        if !text.contains("<system-reminder>") && !text.contains("<cron-fire") {
                            kept.push(ChatMessage {
                                role: "user".into(),
                                content: vec![ChatContent::Text {
                                    text: text.chars().take(500).collect(),
                                }],
                            });
                        }
                    }
                }
            }
            // Keep last few messages for continuity.
            let tail: Vec<_> = messages
                .iter()
                .rev()
                .take(keep_last.min(4))
                .cloned()
                .collect::<Vec<_>>()
                .into_iter()
                .rev()
                .collect();
            let dropped = messages.len();
            messages.clear();
            messages.push(ChatMessage {
                role: "user".into(),
                content: vec![ChatContent::Text {
                    text: format!(
                        "<system-reminder>\nHandoff summary of prior work ({} user notes):\n{}\n</system-reminder>",
                        kept.len(),
                        kept.iter()
                            .filter_map(first_text)
                            .take(12)
                            .collect::<Vec<_>>()
                            .join("\n—\n")
                    ),
                }],
            });
            messages.extend(tail);
            CompactionResult { dropped, strategy }
        }
    }
}

fn is_vacuous(m: &ChatMessage) -> bool {
    match m.content.as_slice() {
        [ChatContent::Text { text }] => {
            let t = text.trim();
            t.is_empty() || t == "ok" || t == "OK" || t.starts_with("Skipped: duplicate tool call")
        }
        [ChatContent::ToolResult {
            content, is_error, ..
        }] => !*is_error && (content.is_empty() || content == "(no output)"),
        _ => false,
    }
}

fn first_text(m: &ChatMessage) -> Option<&str> {
    m.content.iter().find_map(|c| match c {
        ChatContent::Text { text } => Some(text.as_str()),
        _ => None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vacuous_fold_drops_noise() {
        let mut msgs = vec![
            ChatMessage {
                role: "assistant".into(),
                content: vec![ChatContent::Text { text: "ok".into() }],
            },
            ChatMessage {
                role: "user".into(),
                content: vec![ChatContent::Text {
                    text: "real work".into(),
                }],
            },
        ];
        let _ = compact_full(&mut msgs, 2, CompactionStrategy::VacuousFold);
        assert!(msgs.iter().any(|m| first_text(m) == Some("real work")));
    }
}
