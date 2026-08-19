//! Rebuild session message history from transcript DB / wire-like records.

use kkagent_llm::{ChatContent, ChatMessage};
use serde_json::Value;

use crate::transcript::MessageRecord;

pub struct ReplayBuilder;

impl ReplayBuilder {
    pub fn from_transcript_records(records: &[MessageRecord]) -> Vec<ChatMessage> {
        records
            .iter()
            .filter_map(|r| {
                crate::transcript::message_from_transcript(r.role.clone(), &r.content_json)
            })
            .collect()
    }

    /// Accept wire-like JSONL objects with `{role, content}` or agent event frames.
    pub fn from_wire_values(values: &[Value]) -> Vec<ChatMessage> {
        let mut out = Vec::new();
        for v in values {
            if let Some(role) = v.get("role").and_then(|r| r.as_str()) {
                if let Some(content) = v.get("content") {
                    if let Ok(parts) = serde_json::from_value::<Vec<ChatContent>>(content.clone()) {
                        out.push(ChatMessage {
                            role: role.into(),
                            content: parts,
                            tools: v
                                .get("tools")
                                .and_then(|t| serde_json::from_value(t.clone()).ok()),
                        });
                        continue;
                    }
                    if let Some(text) = content.as_str() {
                        out.push(ChatMessage {
                            role: role.into(),
                            content: vec![ChatContent::Text { text: text.into() }],
                            tools: None,
                        });
                    }
                }
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn wire_text() {
        let vals = vec![json!({"role":"user","content":"hi"})];
        let msgs = ReplayBuilder::from_wire_values(&vals);
        assert_eq!(msgs.len(), 1);
        assert_eq!(msgs[0].role, "user");
    }
}
