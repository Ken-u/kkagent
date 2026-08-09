//! Project conversation history into a budget-safe LLM payload.

use kkagent_llm::{ChatContent, ChatMessage};

const DEFAULT_KEEP_RECENT: usize = 12;
const TOOL_RESULT_PROJECT_MAX: usize = 2_000;
const TEXT_PROJECT_MAX: usize = 8_000;

#[derive(Debug, Clone)]
pub struct ProjectOptions {
    /// Keep the last N messages intact (after folding older ones).
    pub keep_recent: usize,
    pub tool_result_max_chars: usize,
    pub text_max_chars: usize,
    /// When true, strip thinking blocks from projected history.
    pub strip_thinking: bool,
}

impl Default for ProjectOptions {
    fn default() -> Self {
        Self {
            keep_recent: DEFAULT_KEEP_RECENT,
            tool_result_max_chars: TOOL_RESULT_PROJECT_MAX,
            text_max_chars: TEXT_PROJECT_MAX,
            strip_thinking: true,
        }
    }
}

/// Wire-safe projection: fold old tool results / long text while preserving recent turns.
pub fn project(messages: &[ChatMessage], opts: &ProjectOptions) -> Vec<ChatMessage> {
    if messages.is_empty() {
        return Vec::new();
    }
    let keep = opts.keep_recent.max(2);
    let split = messages.len().saturating_sub(keep);
    let mut out = Vec::with_capacity(messages.len());
    for (i, msg) in messages.iter().enumerate() {
        if i < split {
            out.push(fold_message(msg, opts, true));
        } else {
            out.push(fold_message(msg, opts, false));
        }
    }
    out
}

/// Aggressive projection used when still over budget after a soft project.
pub fn project_strict(messages: &[ChatMessage], opts: &ProjectOptions) -> Vec<ChatMessage> {
    let mut strict = opts.clone();
    strict.keep_recent = opts.keep_recent.min(6).max(2);
    strict.tool_result_max_chars = 400;
    strict.text_max_chars = 1_500;
    project(messages, &strict)
}

fn fold_message(msg: &ChatMessage, opts: &ProjectOptions, fold_hard: bool) -> ChatMessage {
    let tool_max = if fold_hard {
        opts.tool_result_max_chars.min(600)
    } else {
        opts.tool_result_max_chars
    };
    let text_max = if fold_hard {
        opts.text_max_chars.min(2_000)
    } else {
        opts.text_max_chars
    };

    let content: Vec<ChatContent> = msg
        .content
        .iter()
        .filter_map(|part| match part {
            ChatContent::Thinking { .. } if opts.strip_thinking && fold_hard => None,
            ChatContent::Thinking { thinking } => Some(ChatContent::Thinking {
                thinking: truncate_chars(thinking, text_max / 2),
            }),
            ChatContent::Text { text } => Some(ChatContent::Text {
                text: truncate_chars(text, text_max),
            }),
            ChatContent::ToolUse { id, name, input } => {
                let input = if fold_hard {
                    shrink_json(input, 400)
                } else {
                    input.clone()
                };
                Some(ChatContent::ToolUse {
                    id: id.clone(),
                    name: name.clone(),
                    input,
                })
            }
            ChatContent::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => Some(ChatContent::ToolResult {
                tool_use_id: tool_use_id.clone(),
                content: truncate_chars(content, tool_max),
                is_error: *is_error,
            }),
        })
        .collect();

    ChatMessage {
        role: msg.role.clone(),
        content,
    }
}

fn truncate_chars(s: &str, max: usize) -> String {
    let count = s.chars().count();
    if count <= max {
        return s.to_string();
    }
    let keep = max.saturating_sub(40);
    let head: String = s.chars().take(keep).collect();
    format!("{head}\n…[{} chars truncated]", count - keep)
}

fn shrink_json(v: &serde_json::Value, max: usize) -> serde_json::Value {
    let s = v.to_string();
    if s.len() <= max {
        return v.clone();
    }
    serde_json::Value::String(truncate_chars(&s, max))
}

/// In-place compact: replace everything before `keep_last` with a single summary user message.
pub fn compact_messages(messages: &mut Vec<ChatMessage>, keep_last: usize, summary: &str) -> usize {
    if messages.len() <= keep_last {
        return 0;
    }
    let drop_n = messages.len() - keep_last;
    let kept: Vec<ChatMessage> = messages.split_off(drop_n);
    messages.clear();
    messages.push(ChatMessage {
        role: "user".into(),
        content: vec![ChatContent::Text {
            text: format!(
                "<system-reminder>\nConversation compacted. Summary of earlier turns:\n{summary}\n</system-reminder>"
            ),
        }],
    });
    messages.extend(kept);
    drop_n
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool_result(id: &str, content: &str) -> ChatMessage {
        ChatMessage {
            role: "user".into(),
            content: vec![ChatContent::ToolResult {
                tool_use_id: id.into(),
                content: content.into(),
                is_error: false,
            }],
        }
    }

    #[test]
    fn project_truncates_old_tool_results() {
        let big = "x".repeat(5_000);
        let mut msgs = Vec::new();
        for i in 0..20 {
            msgs.push(tool_result(&format!("t{i}"), &big));
        }
        let opts = ProjectOptions::default();
        let projected = project(&msgs, &opts);
        assert_eq!(projected.len(), 20);
        // Older results should be shorter than recent ones.
        let old_len = match &projected[0].content[0] {
            ChatContent::ToolResult { content, .. } => content.len(),
            _ => panic!("expected tool result"),
        };
        let recent_len = match &projected[19].content[0] {
            ChatContent::ToolResult { content, .. } => content.len(),
            _ => panic!("expected tool result"),
        };
        assert!(old_len < recent_len);
        assert!(old_len < 800);
    }

    #[test]
    fn compact_keeps_tail() {
        let mut msgs: Vec<ChatMessage> = (0..10)
            .map(|i| ChatMessage {
                role: "user".into(),
                content: vec![ChatContent::Text {
                    text: format!("m{i}"),
                }],
            })
            .collect();
        let dropped = compact_messages(&mut msgs, 3, "summary here");
        assert_eq!(dropped, 7);
        assert_eq!(msgs.len(), 4); // summary + 3
        assert!(
            matches!(&msgs[0].content[0], ChatContent::Text { text } if text.contains("summary here"))
        );
    }
}
