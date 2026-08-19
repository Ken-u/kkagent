//! User-editable conversation turn discovery shared by RPC/session features.

use kkagent_llm::{ChatContent, ChatMessage};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EditableTurn {
    /// Zero-based index among real user prompts.
    pub turn_index: usize,
    /// Index in `Session::messages`; forking before this message replays the
    /// conversation up to, but not including, the selected prompt.
    pub message_index: usize,
    /// User-facing prompt text suitable for putting back into the composer.
    pub text: String,
}

pub fn editable_turns(messages: &[ChatMessage]) -> Vec<EditableTurn> {
    let mut turns = Vec::new();
    for (message_index, message) in messages.iter().enumerate() {
        let Some(text) = editable_user_text(message) else {
            continue;
        };
        turns.push(EditableTurn {
            turn_index: turns.len(),
            message_index,
            text,
        });
    }
    turns
}

fn editable_user_text(message: &ChatMessage) -> Option<String> {
    if message.role != "user" {
        return None;
    }
    let text = message
        .content
        .iter()
        .filter_map(|content| match content {
            ChatContent::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n");
    if text.trim().is_empty() || kkagent_protocol::is_harness_only_user_text(&text) {
        return None;
    }
    let visible = kkagent_protocol::visible_user_text(&text);
    let editable = visible
        .lines()
        .filter(|line| !is_generated_media_marker(line.trim()))
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string();
    (!editable.is_empty()).then_some(editable)
}

fn is_generated_media_marker(line: &str) -> bool {
    ["image-attached", "video-attached"]
        .iter()
        .any(|tag| line.starts_with(&format!("<{tag} ")) && line.ends_with("/>"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn message(role: &str, content: Vec<ChatContent>) -> ChatMessage {
        ChatMessage {
            role: role.into(),
            content,
            tools: None,
        }
    }

    #[test]
    fn enumerates_only_real_user_prompts_with_exact_message_indices() {
        let messages = vec![
            message(
                "user",
                vec![ChatContent::Text {
                    text: "first prompt".into(),
                }],
            ),
            message(
                "assistant",
                vec![ChatContent::ToolUse {
                    id: "tool-1".into(),
                    name: "Read".into(),
                    input: serde_json::json!({}),
                }],
            ),
            message(
                "user",
                vec![ChatContent::ToolResult {
                    tool_use_id: "tool-1".into(),
                    content: "result".into(),
                    is_error: false,
                }],
            ),
            message(
                "user",
                vec![ChatContent::Text {
                    text: "<system-reminder>hidden</system-reminder>\nsecond prompt\n<image-attached name=\"x.png\" bytes=\"12\"/>".into(),
                }],
            ),
        ];

        assert_eq!(
            editable_turns(&messages),
            vec![
                EditableTurn {
                    turn_index: 0,
                    message_index: 0,
                    text: "first prompt".into(),
                },
                EditableTurn {
                    turn_index: 1,
                    message_index: 3,
                    text: "second prompt".into(),
                },
            ]
        );
    }
}
