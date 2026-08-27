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

/// Wire-safe projection for normal model requests.
///
/// Historical tool results are deliberately kept byte-stable. Rewriting an
/// already-sent `function_call_output` breaks provider prompt-cache prefixes.
/// Context pressure is handled by explicit compaction instead.
pub fn project(messages: &[ChatMessage], opts: &ProjectOptions) -> Vec<ChatMessage> {
    project_owned(messages.to_vec(), opts)
}

/// Project owned messages, moving the recent intact tail instead of cloning it.
pub fn project_owned(messages: Vec<ChatMessage>, opts: &ProjectOptions) -> Vec<ChatMessage> {
    project_owned_impl(messages, opts, false)
}

/// Projection for the separate compaction-summary request. This request does
/// not extend the main conversation cache prefix, so folding tool results here
/// is safe and keeps the summarizer payload bounded.
pub(crate) fn project_for_compaction(
    messages: &[ChatMessage],
    opts: &ProjectOptions,
) -> Vec<ChatMessage> {
    project_owned_impl(messages.to_vec(), opts, true)
}

fn project_owned_impl(
    messages: Vec<ChatMessage>,
    opts: &ProjectOptions,
    fold_tool_results: bool,
) -> Vec<ChatMessage> {
    if messages.is_empty() {
        return Vec::new();
    }
    let keep = opts.keep_recent.max(2);
    let split = messages.len().saturating_sub(keep);
    let mut out = Vec::with_capacity(messages.len());
    for (i, msg) in messages.into_iter().enumerate() {
        if i < split {
            out.push(fold_message(msg, opts, true, fold_tool_results));
        } else {
            out.push(msg);
        }
    }
    out
}

/// Aggressive projection used when still over budget after a soft project.
/// Tool results remain stable here as well; if the request is still too large,
/// the caller must compact the conversation rather than rewrite old outputs.
pub fn project_strict(messages: &[ChatMessage], opts: &ProjectOptions) -> Vec<ChatMessage> {
    let mut strict = opts.clone();
    strict.keep_recent = opts.keep_recent.clamp(2, 6);
    strict.text_max_chars = 1_500;
    project_owned_impl(messages.to_vec(), &strict, false)
}

/// Replace older media with compact text markers while retaining the newest messages.
/// Used proactively for oversized histories and reactively after HTTP 413 responses.
pub fn fold_old_media(messages: &mut [ChatMessage], keep_recent_messages: usize) -> usize {
    let split = messages.len().saturating_sub(keep_recent_messages.max(1));
    let mut folded = 0;
    for message in &mut messages[..split] {
        for part in &mut message.content {
            let marker = match part {
                ChatContent::Image { media_type, data } => Some(format!(
                    "[older image omitted from request: {media_type}, approximately {} bytes]",
                    data.len().saturating_mul(3) / 4
                )),
                ChatContent::Video {
                    media_type,
                    filename,
                    ..
                } => Some(format!(
                    "[older video omitted from request: {filename}, {media_type}]"
                )),
                _ => None,
            };
            if let Some(text) = marker {
                *part = ChatContent::Text { text };
                folded += 1;
            }
        }
    }
    folded
}

fn fold_message(
    msg: ChatMessage,
    opts: &ProjectOptions,
    fold_hard: bool,
    fold_tool_results: bool,
) -> ChatMessage {
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
        .into_iter()
        .filter_map(|part| match part {
            ChatContent::Thinking { .. } if opts.strip_thinking && fold_hard => None,
            ChatContent::Thinking { thinking } => Some(ChatContent::Thinking {
                thinking: truncate_chars(&thinking, text_max / 2),
            }),
            ChatContent::Text { text } => Some(ChatContent::Text {
                text: truncate_chars(&text, text_max),
            }),
            ChatContent::Image { media_type, data } if fold_hard => Some(ChatContent::Text {
                text: format!(
                    "[older image omitted: {media_type}, approximately {} bytes]",
                    data.len().saturating_mul(3) / 4
                ),
            }),
            ChatContent::Image { media_type, data } => {
                Some(ChatContent::Image { media_type, data })
            }
            ChatContent::Video {
                media_type,
                filename,
                ..
            } if fold_hard => Some(ChatContent::Text {
                text: format!("[older video omitted: {filename}, {media_type}]"),
            }),
            ChatContent::Video {
                media_type,
                path,
                filename,
            } => Some(ChatContent::Video {
                media_type,
                path,
                filename,
            }),
            ChatContent::ToolUse { id, name, input } => {
                Some(ChatContent::ToolUse {
                    id,
                    name,
                    // Provider protocols require tool input to remain a JSON object.
                    // Truncating its serialized form into a string corrupts historical
                    // tool calls and makes the next request fail validation.
                    input,
                })
            }
            ChatContent::ToolResult {
                tool_use_id,
                content,
                is_error,
            } if fold_tool_results => Some(ChatContent::ToolResult {
                tool_use_id,
                content: truncate_chars(&content, tool_max),
                is_error,
            }),
            ChatContent::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => Some(ChatContent::ToolResult {
                tool_use_id,
                content,
                is_error,
            }),
        })
        .collect();

    ChatMessage {
        role: msg.role,
        content,
        tools: msg.tools,
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

const SYNTHETIC_TOOL_RESULT: &str = "Tool result is not available in the current context. Do not assume the tool completed successfully.";

fn tool_use_ids(msg: &ChatMessage) -> Vec<String> {
    msg.content
        .iter()
        .filter_map(|c| match c {
            ChatContent::ToolUse { id, .. } => Some(id.clone()),
            _ => None,
        })
        .collect()
}

fn tool_result_ids(msg: &ChatMessage) -> Vec<String> {
    msg.content
        .iter()
        .filter_map(|c| match c {
            ChatContent::ToolResult { tool_use_id, .. } => Some(tool_use_id.clone()),
            _ => None,
        })
        .collect()
}

fn is_tool_result_only(msg: &ChatMessage) -> bool {
    msg.role == "user"
        && !msg.content.is_empty()
        && msg
            .content
            .iter()
            .all(|c| matches!(c, ChatContent::ToolResult { .. }))
}

fn slice_has_tool_use(msgs: &[ChatMessage], id: &str) -> bool {
    msgs.iter().any(|m| {
        m.content
            .iter()
            .any(|c| matches!(c, ChatContent::ToolUse { id: tid, .. } if tid == id))
    })
}

fn slice_has_tool_result(msgs: &[ChatMessage], id: &str) -> bool {
    msgs.iter().any(|m| {
        m.content
            .iter()
            .any(|c| matches!(c, ChatContent::ToolResult { tool_use_id, .. } if tool_use_id == id))
    })
}

/// Choose a cut that keeps ~`keep_last` messages without splitting a tool exchange.
///
/// Mirrors kimi-code compaction boundary rules adapted to Anthropic-style history
/// (tool results live on user messages):
/// - Prefer pulling the assistant `tool_use` into the kept tail when its results
///   would otherwise be orphaned there.
/// - Push leading orphan tool-result-only messages into the compacted prefix so
///   they are covered by the digest instead of surviving as wire-invalid orphans.
pub fn compact_cut_index(messages: &[ChatMessage], keep_last: usize) -> usize {
    if messages.len() <= keep_last {
        return 0;
    }
    let mut cut = messages.len() - keep_last.max(1);

    // If the message just before the cut is an assistant tool_use whose results
    // sit in the kept tail, include that assistant so the exchange stays intact.
    while cut > 0 {
        let ids = tool_use_ids(&messages[cut - 1]);
        if ids.is_empty() {
            break;
        }
        let tail = &messages[cut..];
        if ids.iter().any(|id| slice_has_tool_result(tail, id)) {
            cut -= 1;
            continue;
        }
        break;
    }

    // Leading kept messages that are only tool_results for calls outside the
    // kept region are orphans — fold them into the compacted prefix instead.
    while cut < messages.len() {
        let msg = &messages[cut];
        if !is_tool_result_only(msg) {
            break;
        }
        let ids = tool_result_ids(msg);
        let kept = &messages[cut..];
        if ids.iter().all(|id| slice_has_tool_use(kept, id)) {
            break;
        }
        cut += 1;
    }

    cut
}

/// Local (no-LLM) digest of a history prefix. Includes tool calls/results so
/// auto-compact does not silently lose what the model did.
pub fn build_compaction_digest(messages: &[ChatMessage]) -> String {
    let mut out = String::from("Earlier conversation digest:\n");
    for m in messages.iter().take(48) {
        let mut parts: Vec<String> = Vec::new();
        for c in &m.content {
            match c {
                ChatContent::Text { text } => {
                    let t = text.trim();
                    if !t.is_empty() {
                        parts.push(t.chars().take(240).collect());
                    }
                }
                ChatContent::ToolUse { name, input, .. } => {
                    let args = serde_json::to_string(input).unwrap_or_default();
                    let short: String = args.chars().take(160).collect();
                    parts.push(format!("called {name}({short})"));
                }
                ChatContent::ToolResult {
                    tool_use_id,
                    content,
                    is_error,
                } => {
                    let flag = if *is_error { " error" } else { "" };
                    let body: String = content.chars().take(200).collect();
                    parts.push(format!("tool_result{flag} {tool_use_id}: {body}"));
                }
                ChatContent::Thinking { .. }
                | ChatContent::Image { .. }
                | ChatContent::Video { .. } => {}
            }
        }
        if parts.is_empty() {
            continue;
        }
        out.push_str(&format!("[{}] {}\n", m.role, parts.join(" · ")));
    }
    if out.chars().count() > 4_500 {
        out.chars().take(4_500).collect()
    } else {
        out
    }
}

/// Repair tool_use / tool_result pairing (kimi-code projector compatible).
///
/// - Drop tool_results whose tool_use is missing anywhere in `messages`.
/// - When `synthesize_missing` is true, close unpaired tool_uses with a
///   synthetic tool_result (needed after compaction slices a delayed result
///   into the dropped prefix / retained tail).
pub fn repair_tool_exchanges(messages: &mut Vec<ChatMessage>, synthesize_missing: bool) {
    let use_ids: std::collections::HashSet<String> =
        messages.iter().flat_map(tool_use_ids).collect();

    // Drop orphan tool_result parts; drop messages left empty.
    messages.retain_mut(|msg| {
        msg.content.retain(|c| match c {
            ChatContent::ToolResult { tool_use_id, .. } => use_ids.contains(tool_use_id),
            _ => true,
        });
        !msg.content.is_empty()
    });

    if !synthesize_missing {
        return;
    }

    let result_ids: std::collections::HashSet<String> =
        messages.iter().flat_map(tool_result_ids).collect();

    let mut inserts: Vec<(usize, ChatMessage)> = Vec::new();
    for (i, msg) in messages.iter().enumerate() {
        let missing: Vec<String> = tool_use_ids(msg)
            .into_iter()
            .filter(|id| !result_ids.contains(id))
            .collect();
        if missing.is_empty() {
            continue;
        }
        let content: Vec<ChatContent> = missing
            .into_iter()
            .map(|tool_use_id| ChatContent::ToolResult {
                tool_use_id,
                content: SYNTHETIC_TOOL_RESULT.into(),
                is_error: true,
            })
            .collect();
        inserts.push((
            i + 1,
            ChatMessage {
                role: "user".into(),
                content,
                tools: None,
            },
        ));
    }
    // Insert from the back so indices stay valid.
    for (idx, msg) in inserts.into_iter().rev() {
        messages.insert(idx, msg);
    }
}

/// In-place compact: replace everything before a tool-safe cut with a summary.
pub fn compact_messages(messages: &mut Vec<ChatMessage>, keep_last: usize, summary: &str) -> usize {
    if messages.len() <= keep_last {
        return 0;
    }
    let cut = compact_cut_index(messages, keep_last);
    if cut == 0 {
        return 0;
    }
    let drop_n = cut;
    let mut kept: Vec<ChatMessage> = messages.split_off(cut);
    // Final safety: drop any remaining leading orphan tool-result-only msgs.
    while kept.first().is_some_and(|m| {
        is_tool_result_only(m) && {
            let ids = tool_result_ids(m);
            !ids.iter().all(|id| slice_has_tool_use(&kept, id))
        }
    }) {
        kept.remove(0);
    }
    repair_tool_exchanges(&mut kept, true);
    messages.clear();
    messages.push(ChatMessage {
        role: "user".into(),
        content: vec![ChatContent::Text {
            text: format!(
                "<system-reminder>\nConversation compacted. Summary of earlier turns:\n{summary}\n</system-reminder>"
            ),
        }],
        tools: None,
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
            tools: None,
        }
    }

    #[test]
    fn project_keeps_old_tool_results_byte_stable() {
        let big = "x".repeat(5_000);
        let mut messages = vec![tool_result("stable", &big)];
        messages.extend((0..20).map(|index| ChatMessage {
            role: "user".into(),
            content: vec![ChatContent::Text {
                text: format!("later message {index}"),
            }],
            tools: None,
        }));

        let projected = project(&messages, &ProjectOptions::default());
        let ChatContent::ToolResult { content, .. } = &projected[0].content[0] else {
            panic!("expected tool result");
        };
        assert_eq!(content, &big);
    }

    #[test]
    fn appending_messages_does_not_rewrite_previous_tool_output() {
        let original = "tool output\n".repeat(500);
        let mut messages = vec![tool_result("stable", &original)];
        messages.extend((0..11).map(|index| ChatMessage {
            role: "user".into(),
            content: vec![ChatContent::Text {
                text: format!("initial message {index}"),
            }],
            tools: None,
        }));

        let before = project(&messages, &ProjectOptions::default());
        messages.push(ChatMessage {
            role: "user".into(),
            content: vec![ChatContent::Text {
                text: "moves the tool result out of the recent window".into(),
            }],
            tools: None,
        });
        let after = project(&messages, &ProjectOptions::default());

        let before_content = match &before[0].content[0] {
            ChatContent::ToolResult { content, .. } => content,
            _ => panic!("expected tool result"),
        };
        let after_content = match &after[0].content[0] {
            ChatContent::ToolResult { content, .. } => content,
            _ => panic!("expected tool result"),
        };
        assert_eq!(before_content, &original);
        assert_eq!(after_content, before_content);
    }

    #[test]
    fn strict_projection_keeps_tool_results_byte_stable() {
        let original = "z".repeat(5_000);
        let mut messages = vec![tool_result("stable", &original)];
        messages.extend((0..20).map(|index| ChatMessage {
            role: "user".into(),
            content: vec![ChatContent::Text {
                text: format!("later message {index}"),
            }],
            tools: None,
        }));

        let projected = project_strict(&messages, &ProjectOptions::default());
        let ChatContent::ToolResult { content, .. } = &projected[0].content[0] else {
            panic!("expected tool result");
        };
        assert_eq!(content, &original);
    }

    #[test]
    fn compaction_projection_can_truncate_tool_results() {
        let big = "x".repeat(5_000);
        let mut messages = vec![tool_result("summary-only", &big)];
        messages.extend((0..20).map(|index| ChatMessage {
            role: "user".into(),
            content: vec![ChatContent::Text {
                text: format!("later message {index}"),
            }],
            tools: None,
        }));

        let projected = project_for_compaction(&messages, &ProjectOptions::default());
        let ChatContent::ToolResult { content, .. } = &projected[0].content[0] else {
            panic!("expected tool result");
        };
        assert!(content.len() < big.len());
        assert!(content.contains("truncated"));
    }

    #[test]
    fn project_preserves_large_tool_input_as_an_object() {
        let large_input = serde_json::json!({
            "path": "example.txt",
            "old_string": "x".repeat(1_000),
            "new_string": "y".repeat(1_000),
        });
        let mut messages = vec![ChatMessage {
            role: "assistant".into(),
            content: vec![ChatContent::ToolUse {
                id: "functions.Edit:0".into(),
                name: "Edit".into(),
                input: large_input.clone(),
            }],
            tools: None,
        }];
        messages.extend((0..20).map(|index| ChatMessage {
            role: "user".into(),
            content: vec![ChatContent::Text {
                text: format!("later message {index}"),
            }],
            tools: None,
        }));

        let projected = project(&messages, &ProjectOptions::default());
        let ChatContent::ToolUse { input, .. } = &projected[0].content[0] else {
            panic!("expected tool use");
        };
        assert!(input.is_object());
        assert_eq!(input, &large_input);
    }

    #[test]
    fn compact_keeps_tail() {
        let mut msgs: Vec<ChatMessage> = (0..10)
            .map(|i| ChatMessage {
                role: "user".into(),
                content: vec![ChatContent::Text {
                    text: format!("m{i}"),
                }],
                tools: None,
            })
            .collect();
        let dropped = compact_messages(&mut msgs, 3, "summary here");
        assert_eq!(dropped, 7);
        assert_eq!(msgs.len(), 4); // summary + 3
        assert!(
            matches!(&msgs[0].content[0], ChatContent::Text { text } if text.contains("summary here"))
        );
    }

    #[test]
    fn compact_does_not_orphan_tool_results_at_cut() {
        let mut msgs = vec![
            ChatMessage {
                role: "user".into(),
                content: vec![ChatContent::Text { text: "old".into() }],
                tools: None,
            },
            ChatMessage {
                role: "assistant".into(),
                content: vec![ChatContent::ToolUse {
                    id: "call-1".into(),
                    name: "Read".into(),
                    input: serde_json::json!({"path": "a.rs"}),
                }],
                tools: None,
            },
            ChatMessage {
                role: "user".into(),
                content: vec![ChatContent::ToolResult {
                    tool_use_id: "call-1".into(),
                    content: "fn main() {}".into(),
                    is_error: false,
                }],
                tools: None,
            },
            ChatMessage {
                role: "user".into(),
                content: vec![ChatContent::Text {
                    text: "thanks".into(),
                }],
                tools: None,
            },
        ];
        // Naive keep_last=2 would start on the tool_result. The cut should pull
        // the matching tool_use into the kept side so the exchange stays intact.
        let cut = compact_cut_index(&msgs, 2);
        assert!(
            cut <= 1,
            "cut={cut} should include the tool_use with its result"
        );
        let _ = compact_messages(&mut msgs, 2, "summary");
        assert!(
            msgs.iter()
                .any(|m| tool_use_ids(m).iter().any(|id| id == "call-1")),
            "kept history should retain tool_use call-1"
        );
        assert!(
            msgs.iter()
                .any(|m| tool_result_ids(m).iter().any(|id| id == "call-1")),
            "kept history should retain tool_result call-1"
        );
        // No orphan: every kept tool_result has a tool_use in the kept history.
        for m in &msgs {
            for id in tool_result_ids(m) {
                assert!(
                    slice_has_tool_use(&msgs, &id),
                    "orphan tool_result {id} after compact"
                );
            }
        }
    }

    #[test]
    fn repair_drops_orphan_results_and_synthesizes_missing() {
        let mut msgs = vec![
            ChatMessage {
                role: "assistant".into(),
                content: vec![ChatContent::ToolUse {
                    id: "keep".into(),
                    name: "Bash".into(),
                    input: serde_json::json!({"command": "ls"}),
                }],
                tools: None,
            },
            ChatMessage {
                role: "user".into(),
                content: vec![
                    ChatContent::ToolResult {
                        tool_use_id: "keep".into(),
                        content: "ok".into(),
                        is_error: false,
                    },
                    ChatContent::ToolResult {
                        tool_use_id: "ghost".into(),
                        content: "orphan".into(),
                        is_error: false,
                    },
                ],
                tools: None,
            },
            ChatMessage {
                role: "assistant".into(),
                content: vec![ChatContent::ToolUse {
                    id: "open".into(),
                    name: "Read".into(),
                    input: serde_json::json!({"path": "x"}),
                }],
                tools: None,
            },
        ];
        repair_tool_exchanges(&mut msgs, true);
        let all = format!("{:?}", msgs);
        assert!(!all.contains("ghost"));
        assert!(all.contains("open"));
        assert!(all.contains(SYNTHETIC_TOOL_RESULT));
    }

    #[test]
    fn folds_only_old_media_into_markers() {
        let image = || ChatMessage {
            role: "user".into(),
            content: vec![ChatContent::Image {
                media_type: "image/png".into(),
                data: "AAAA".into(),
            }],
            tools: None,
        };
        let mut messages = vec![image(), image(), image()];
        assert_eq!(fold_old_media(&mut messages, 1), 2);
        assert!(matches!(messages[0].content[0], ChatContent::Text { .. }));
        assert!(matches!(messages[1].content[0], ChatContent::Text { .. }));
        assert!(matches!(messages[2].content[0], ChatContent::Image { .. }));
    }
}
