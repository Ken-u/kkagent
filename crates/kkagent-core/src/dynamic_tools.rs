//! Progressive tool disclosure: name-only incremental announcements and
//! message-level schema injection (aligned with `ref/kimi-code`).

use std::collections::HashSet;

use kkagent_llm::{ChatContent, ChatMessage, ToolDef};
use kkagent_protocol::tools::{ToolDefinition, ToolDisclosure};

use crate::session::Session;
use crate::system_reminder;

const TOOLS_ADDED_OPEN: &str = "<tools_added>";
const TOOLS_ADDED_CLOSE: &str = "</tools_added>";
const TOOLS_REMOVED_OPEN: &str = "<tools_removed>";
const TOOLS_REMOVED_CLOSE: &str = "</tools_removed>";

/// True for a `<tools_added>/<tools_removed>` announcement reminder.
pub fn is_loadable_tools_announcement(message: &ChatMessage) -> bool {
    if message.role != "user" {
        return false;
    }
    let text = message_text(message);
    let trimmed = text.trim();
    if !trimmed.contains("<system-reminder>") {
        return false;
    }
    trimmed.contains(TOOLS_ADDED_OPEN) || trimmed.contains(TOOLS_REMOVED_OPEN)
}

pub fn is_dynamic_tool_schema_message(message: &ChatMessage) -> bool {
    message.is_dynamic_tool_schema()
}

/// Drop loadable-tools announcements and strip `message.tools` (dropping the
/// message entirely when nothing else remains). Used for the compaction
/// summarizer so protocol context is not folded into the summary.
pub fn strip_dynamic_tool_context(history: &[ChatMessage]) -> Vec<ChatMessage> {
    if !history
        .iter()
        .any(|m| is_dynamic_tool_schema_message(m) || is_loadable_tools_announcement(m))
    {
        return history.to_vec();
    }
    let mut out = Vec::with_capacity(history.len());
    for message in history {
        if is_loadable_tools_announcement(message) {
            continue;
        }
        if is_dynamic_tool_schema_message(message) {
            if message.content.is_empty() {
                continue;
            }
            let mut rest = message.clone();
            rest.tools = None;
            out.push(rest);
            continue;
        }
        out.push(message.clone());
    }
    out
}

/// Union of tool names loaded by dynamic tool schema messages in `history`.
pub fn collect_loaded_dynamic_tool_names(history: &[ChatMessage]) -> HashSet<String> {
    let mut names = HashSet::new();
    for message in history {
        let Some(tools) = &message.tools else {
            continue;
        };
        for tool in tools {
            names.insert(tool.name.clone());
        }
    }
    names
}

/// Fold every loadable-tools announcement in `history`, in order, into the
/// currently-announced name set (`tools_removed` deletes, then `tools_added`
/// adds — last wins). The announcements are the context's own record of what
/// the model has been told is loadable.
pub fn fold_announced_tool_names(history: &[ChatMessage]) -> HashSet<String> {
    let mut announced = HashSet::new();
    for message in history {
        if !is_loadable_tools_announcement(message) {
            continue;
        }
        let text = message_text(message);
        for name in match_tool_name_block(&text, TOOLS_REMOVED_OPEN, TOOLS_REMOVED_CLOSE) {
            announced.remove(&name);
        }
        for name in match_tool_name_block(&text, TOOLS_ADDED_OPEN, TOOLS_ADDED_CLOSE) {
            announced.insert(name);
        }
    }
    announced
}

fn match_tool_name_block(text: &str, open: &str, close: &str) -> Vec<String> {
    let mut names = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find(open) {
        let after_open = start + open.len();
        let Some(end) = rest[after_open..].find(close) else {
            break;
        };
        let body = &rest[after_open..after_open + end];
        for line in body.lines() {
            let name = line.trim();
            if !name.is_empty() {
                names.push(name.to_string());
            }
        }
        rest = &rest[after_open + end + close.len()..];
    }
    names
}

/// Render one diff announcement. Only the blocks with content are emitted; the
/// guidance sentence never contains a literal block tag.
pub fn render_loadable_tools_announcement(added: &[String], removed: &[String]) -> String {
    let mut sections = Vec::new();
    if !added.is_empty() {
        sections.push(format!(
            "{TOOLS_ADDED_OPEN}\n{}\n{TOOLS_ADDED_CLOSE}",
            added.join("\n")
        ));
    }
    if !removed.is_empty() {
        sections.push(format!(
            "{TOOLS_REMOVED_OPEN}\n{}\n{TOOLS_REMOVED_CLOSE}",
            removed.join("\n")
        ));
    }
    sections.push(
        "Use the SelectTools tool with exact names to load full tool definitions before calling them. \
Names listed as removed are no longer loadable — do not select them. \
Fold all announcements in this conversation in order to get the current list."
            .into(),
    );
    sections.join("\n\n")
}

pub fn deferred_names(all_defs: &[ToolDefinition]) -> Vec<String> {
    all_defs
        .iter()
        .filter(|td| td.disclosure == ToolDisclosure::Deferred)
        .map(|td| td.name.clone())
        .collect()
}

pub fn unloaded_deferred_names(
    all_defs: &[ToolDefinition],
    loaded: &HashSet<String>,
) -> Vec<String> {
    all_defs
        .iter()
        .filter(|td| td.disclosure == ToolDisclosure::Deferred && !loaded.contains(&td.name))
        .map(|td| td.name.clone())
        .collect()
}

pub fn to_llm_tool_def(td: &ToolDefinition) -> ToolDef {
    ToolDef {
        name: td.name.clone(),
        description: td.description.clone(),
        input_schema: td.parameters.clone(),
    }
}

/// Rebuild announcement / loaded ledgers from history so compaction, undo,
/// and resume self-heal without a manual `clear()`.
pub fn sync_deferred_tool_ledgers(session: &mut Session) {
    session.announced_deferred_tools = fold_announced_tool_names(&session.messages);
    session.loaded_deferred_tools = collect_loaded_dynamic_tool_names(&session.messages);
}

/// At a turn boundary, append one name-only diff reminder iff the loadable
/// set changed. Returns true when a message was injected.
pub fn inject_deferred_tools_diff(session: &mut Session, all_defs: &[ToolDefinition]) -> bool {
    sync_deferred_tool_ledgers(session);
    let current = unloaded_deferred_names(all_defs, &session.loaded_deferred_tools);
    let current_set: HashSet<String> = current.iter().cloned().collect();
    let added: Vec<String> = current
        .iter()
        .filter(|name| !session.announced_deferred_tools.contains(*name))
        .cloned()
        .collect();
    let mut removed: Vec<String> = session
        .announced_deferred_tools
        .iter()
        .filter(|name| !current_set.contains(*name))
        .cloned()
        .collect();
    removed.sort();
    if added.is_empty() && removed.is_empty() {
        session.announced_deferred_tools = current_set;
        return false;
    }
    let body = render_loadable_tools_announcement(&added, &removed);
    session.add_user_message(system_reminder::wrap(&body));
    session.announced_deferred_tools = current_set;
    true
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectToolsApply {
    pub content: String,
    pub is_error: bool,
    pub loaded: Vec<String>,
}

/// Classify `SelectTools` names, inject a schema-only system message for newly
/// loaded tools, and return the model-facing result text.
pub fn apply_select_tools(
    session: &mut Session,
    all_defs: &[ToolDefinition],
    requested: &[String],
) -> SelectToolsApply {
    let loadable: HashSet<String> = deferred_names(all_defs).into_iter().collect();
    let mut to_load = Vec::new();
    let mut already = Vec::new();
    let mut unknown = Vec::new();
    let mut seen = HashSet::new();
    for name in requested {
        if name.is_empty() || !seen.insert(name.clone()) {
            continue;
        }
        if session.loaded_deferred_tools.contains(name) {
            already.push(name.clone());
        } else if loadable.contains(name) {
            to_load.push(name.clone());
        } else {
            unknown.push(name.clone());
        }
    }
    to_load.sort();
    if !to_load.is_empty() {
        for name in &to_load {
            session.loaded_deferred_tools.insert(name.clone());
        }
    }

    let mut lines = Vec::new();
    if !to_load.is_empty() {
        lines.push(format!("Loaded: {}", to_load.join(", ")));
    }
    if !already.is_empty() {
        lines.push(format!("Already available: {}", already.join(", ")));
    }
    let all_loadable_names: Vec<String> = loadable.iter().cloned().collect();
    for name in &unknown {
        let suggestions = suggest_similar_tools(name, &all_loadable_names, 3);
        if suggestions.is_empty() {
            lines.push(format!(
                "Unknown tool: {name}. Pick from the latest announced tools list."
            ));
        } else {
            lines.push(format!(
                "Unknown tool: {name}. Did you mean: {}?",
                suggestions.join(", ")
            ));
        }
    }
    let is_error = to_load.is_empty() && already.is_empty();
    SelectToolsApply {
        content: if lines.is_empty() {
            "No tools requested.".into()
        } else {
            lines.join("\n")
        },
        is_error,
        loaded: to_load,
    }
}

/// Append a schema-only system message for the just-loaded deferred tools.
pub fn inject_loaded_tool_schemas(
    session: &mut Session,
    all_defs: &[ToolDefinition],
    loaded: &[String],
) {
    if loaded.is_empty() {
        return;
    }
    let tools: Vec<ToolDef> = loaded
        .iter()
        .filter_map(|name| {
            all_defs
                .iter()
                .find(|td| td.name == *name)
                .map(to_llm_tool_def)
        })
        .collect();
    if !tools.is_empty() {
        session.messages.push(ChatMessage::schema(tools));
    }
}

// ---------------------------------------------------------------------------
// BM25-based fuzzy tool-name matching
// ---------------------------------------------------------------------------

/// Tokenize a tool name into searchable terms: split on `__` and `_`, then
/// generate character bigrams and trigrams for each segment so that minor
/// typos still produce overlapping tokens.
fn tokenize_tool_name(name: &str) -> Vec<String> {
    let mut tokens = Vec::new();
    for segment in name.split("__").flat_map(|s| s.split('_')) {
        let seg = segment.to_lowercase();
        if seg.is_empty() {
            continue;
        }
        tokens.push(seg.clone());
        let chars: Vec<char> = seg.chars().collect();
        for w in 2..=3 {
            for window in chars.windows(w) {
                tokens.push(window.iter().collect());
            }
        }
    }
    tokens
}

/// Compute BM25 score of `query` against a single `document` (both are tool
/// names). The "corpus" is `all_names` — used only for IDF estimation.
fn bm25_score(query: &str, document: &str, all_names: &[&str]) -> f64 {
    let k1: f64 = 1.2;
    let b: f64 = 0.75;

    let query_tokens = tokenize_tool_name(query);
    let doc_tokens = tokenize_tool_name(document);
    let n = all_names.len() as f64;

    let avg_dl: f64 = if all_names.is_empty() {
        1.0
    } else {
        all_names
            .iter()
            .map(|name| tokenize_tool_name(name).len() as f64)
            .sum::<f64>()
            / n
    };
    let dl = doc_tokens.len() as f64;

    // Term frequency in the document.
    let mut tf_map = std::collections::HashMap::<&str, usize>::new();
    for t in &doc_tokens {
        *tf_map.entry(t.as_str()).or_insert(0) += 1;
    }

    // Document frequency across the corpus (for IDF).
    let corpus_tokens: Vec<Vec<String>> = all_names.iter().map(|n| tokenize_tool_name(n)).collect();

    let mut score = 0.0_f64;
    let mut seen_query = std::collections::HashSet::new();
    for qt in &query_tokens {
        if !seen_query.insert(qt.as_str()) {
            continue;
        }
        let df = corpus_tokens
            .iter()
            .filter(|tokens| tokens.iter().any(|t| t == qt))
            .count() as f64;
        if df == 0.0 {
            continue;
        }
        let idf = ((n - df + 0.5) / (df + 0.5) + 1.0).ln();
        let tf = *tf_map.get(qt.as_str()).unwrap_or(&0) as f64;
        score += idf * (tf * (k1 + 1.0)) / (tf + k1 * (1.0 - b + b * dl / avg_dl));
    }
    score
}

/// Return the top-N most similar tool names from `candidates` for `query`,
/// filtered to a minimum score threshold.
fn suggest_similar_tools(query: &str, candidates: &[String], top_n: usize) -> Vec<String> {
    let all_refs: Vec<&str> = candidates.iter().map(|s| s.as_str()).collect();
    let mut scored: Vec<(f64, &str)> = candidates
        .iter()
        .map(|c| (bm25_score(query, c, &all_refs), c.as_str()))
        .filter(|(s, _)| *s > 0.5)
        .collect();
    scored.sort_by(|a, b| b.0.partial_cmp(&a.0).unwrap_or(std::cmp::Ordering::Equal));
    scored
        .into_iter()
        .take(top_n)
        .map(|(_, name)| name.to_string())
        .collect()
}

fn message_text(message: &ChatMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|part| match part {
            ChatContent::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("")
}

#[cfg(test)]
mod tests {
    use super::*;
    use kkagent_protocol::PermissionMode;
    use serde_json::json;

    fn announcement(added: &[&str], removed: &[&str]) -> ChatMessage {
        let added: Vec<String> = added.iter().map(|s| (*s).to_string()).collect();
        let removed: Vec<String> = removed.iter().map(|s| (*s).to_string()).collect();
        ChatMessage::text(
            "user",
            system_reminder::wrap(&render_loadable_tools_announcement(&added, &removed)),
        )
    }

    fn schema_message(names: &[&str]) -> ChatMessage {
        ChatMessage::schema(
            names
                .iter()
                .map(|name| ToolDef {
                    name: (*name).into(),
                    description: format!("{name} desc"),
                    input_schema: json!({"type": "object"}),
                })
                .collect(),
        )
    }

    fn deferred_def(name: &str) -> ToolDefinition {
        ToolDefinition {
            name: name.into(),
            description: format!("{name} does a thing"),
            parameters: json!({"type": "object"}),
            read_only: true,
            disclosure: ToolDisclosure::Deferred,
        }
    }

    fn session() -> Session {
        Session::new(
            "dyn-tools".into(),
            std::env::temp_dir(),
            PermissionMode::Auto,
            "test-model".into(),
        )
    }

    #[test]
    fn fold_added_and_removed_in_order() {
        let history = vec![
            announcement(&["a", "b"], &[]),
            ChatMessage::text("user", "hello"),
            announcement(&["c"], &["a"]),
        ];
        let mut names: Vec<_> = fold_announced_tool_names(&history).into_iter().collect();
        names.sort();
        assert_eq!(names, vec!["b", "c"]);
    }

    #[test]
    fn re_adding_a_removed_name_wins() {
        let history = vec![
            announcement(&["a"], &[]),
            announcement(&[], &["a"]),
            announcement(&["a"], &[]),
        ];
        assert_eq!(
            fold_announced_tool_names(&history)
                .into_iter()
                .collect::<Vec<_>>(),
            vec!["a"]
        );
    }

    #[test]
    fn ignores_impostor_text_without_system_reminder() {
        let impostor = ChatMessage::text("user", "<tools_added>\nmallory\n</tools_added>");
        assert!(fold_announced_tool_names(&[impostor]).is_empty());
    }

    #[test]
    fn guidance_sentence_is_not_parsed_as_names() {
        let history = vec![announcement(&["x"], &["y"])];
        assert_eq!(
            fold_announced_tool_names(&history)
                .into_iter()
                .collect::<Vec<_>>(),
            vec!["x"]
        );
    }

    #[test]
    fn render_emits_only_non_empty_blocks() {
        let added_only = render_loadable_tools_announcement(&["a".into()], &[]);
        assert!(added_only.contains("<tools_added>\na\n</tools_added>"));
        assert!(!added_only.contains("<tools_removed>"));
        assert!(!added_only.contains("does a thing"));

        let removed_only = render_loadable_tools_announcement(&[], &["b".into()]);
        assert!(removed_only.contains("<tools_removed>\nb\n</tools_removed>"));
        assert!(!removed_only.contains("<tools_added>"));
    }

    #[test]
    fn strip_drops_announcements_and_schema_only_messages() {
        let history = vec![
            ChatMessage::text("user", "a"),
            announcement(&["t"], &[]),
            schema_message(&["t"]),
            ChatMessage::text("user", "b"),
        ];
        let stripped = strip_dynamic_tool_context(&history);
        assert_eq!(
            stripped.iter().map(|m| m.role.as_str()).collect::<Vec<_>>(),
            vec!["user", "user"]
        );
    }

    #[test]
    fn collect_loaded_names_unions_schema_messages() {
        let history = vec![
            schema_message(&["a", "b"]),
            ChatMessage::text("user", "x"),
            schema_message(&["b", "c"]),
        ];
        let mut names: Vec<_> = collect_loaded_dynamic_tool_names(&history)
            .into_iter()
            .collect();
        names.sort();
        assert_eq!(names, vec!["a", "b", "c"]);
    }

    #[test]
    fn turn_boundary_injects_added_then_is_silent_when_unchanged() {
        let mut session = session();
        session.add_user_message("hello".into());
        let defs = vec![deferred_def("mcp__a"), deferred_def("mcp__b")];
        assert!(inject_deferred_tools_diff(&mut session, &defs));
        assert!(is_loadable_tools_announcement(
            session.messages.last().unwrap()
        ));
        let last = message_text(session.messages.last().unwrap());
        assert!(last.contains("<tools_added>"));
        assert!(last.contains("mcp__a"));
        assert!(!last.contains("does a thing"));
        assert!(!inject_deferred_tools_diff(&mut session, &defs));
        assert_eq!(
            session
                .messages
                .iter()
                .filter(|m| is_loadable_tools_announcement(m))
                .count(),
            1
        );
    }

    #[test]
    fn loading_a_tool_emits_removed_on_the_next_boundary() {
        let mut session = session();
        let defs = vec![deferred_def("mcp__a"), deferred_def("mcp__b")];
        assert!(inject_deferred_tools_diff(&mut session, &defs));
        let applied = apply_select_tools(&mut session, &defs, &["mcp__a".into()]);
        assert!(!applied.is_error);
        assert_eq!(applied.loaded, vec!["mcp__a"]);
        inject_loaded_tool_schemas(&mut session, &defs, &applied.loaded);
        assert!(session.messages.iter().any(is_dynamic_tool_schema_message));
        assert!(inject_deferred_tools_diff(&mut session, &defs));
        let last = message_text(session.messages.last().unwrap());
        assert!(last.contains("<tools_removed>\nmcp__a\n</tools_removed>"));
        assert!(!last.contains("<tools_added>"));
    }

    #[test]
    fn compaction_self_heals_ledgers_from_history() {
        let mut session = session();
        let defs = vec![deferred_def("mcp__a")];
        assert!(inject_deferred_tools_diff(&mut session, &defs));
        let applied = apply_select_tools(&mut session, &defs, &["mcp__a".into()]);
        inject_loaded_tool_schemas(&mut session, &defs, &applied.loaded);
        assert!(session.loaded_deferred_tools.contains("mcp__a"));
        session.messages.clear();
        sync_deferred_tool_ledgers(&mut session);
        assert!(session.announced_deferred_tools.is_empty());
        assert!(session.loaded_deferred_tools.is_empty());
        assert!(inject_deferred_tools_diff(&mut session, &defs));
        assert!(message_text(session.messages.last().unwrap()).contains("mcp__a"));
    }

    #[test]
    fn select_tools_classifies_mixed_input() {
        let mut session = session();
        let defs = vec![deferred_def("mcp__a")];
        apply_select_tools(&mut session, &defs, &["mcp__a".into()]);
        let applied = apply_select_tools(
            &mut session,
            &defs,
            &["mcp__a".into(), "nope".into(), "mcp__a".into()],
        );
        assert!(!applied.is_error);
        assert!(applied.content.contains("Already available: mcp__a"));
        assert!(applied.content.contains("Unknown tool: nope"));
        assert!(applied.loaded.is_empty());
    }

    #[test]
    fn announcement_has_no_description() {
        let body = render_loadable_tools_announcement(&["mcp__server__tool".into()], &[]);
        assert!(body.contains("<tools_added>\nmcp__server__tool\n</tools_added>"));
        assert!(!body.contains("does a thing"));
        assert!(!body.contains("description"));
    }

    // ---- BM25 fuzzy matching tests ----

    #[test]
    fn tokenize_splits_on_underscores_and_generates_ngrams() {
        let tokens = tokenize_tool_name("mcp__server__read_file");
        assert!(tokens.contains(&"mcp".to_string()));
        assert!(tokens.contains(&"server".to_string()));
        assert!(tokens.contains(&"read".to_string()));
        assert!(tokens.contains(&"file".to_string()));
        // bigrams
        assert!(tokens.contains(&"re".to_string()));
        assert!(tokens.contains(&"fi".to_string()));
    }

    #[test]
    fn bm25_similar_names_score_higher() {
        let corpus = &[
            "mcp__server__read_file",
            "mcp__server__write_file",
            "mcp__other__delete",
        ];
        let score_read = bm25_score("mcp__server__read", "mcp__server__read_file", corpus);
        let score_delete = bm25_score("mcp__server__read", "mcp__other__delete", corpus);
        assert!(
            score_read > score_delete,
            "read_file ({score_read}) should score higher than delete ({score_delete})"
        );
    }

    #[test]
    fn suggest_returns_top_candidates() {
        let candidates = vec![
            "mcp__github__read_file".into(),
            "mcp__github__read_dir".into(),
            "mcp__github__delete_repo".into(),
            "mcp__slack__send_message".into(),
        ];
        let suggestions = suggest_similar_tools("mcp__github__read_flie", &candidates, 3);
        assert!(
            !suggestions.is_empty(),
            "should suggest at least one candidate"
        );
        assert!(
            suggestions.contains(&"mcp__github__read_file".to_string()),
            "should suggest read_file for the typo read_flie, got: {suggestions:?}"
        );
    }

    #[test]
    fn suggest_returns_empty_for_totally_unrelated() {
        let candidates = vec!["aaa__bbb__ccc".into()];
        let suggestions = suggest_similar_tools("zzz_yyy_xxx", &candidates, 3);
        assert!(
            suggestions.is_empty(),
            "unrelated names should not be suggested: {suggestions:?}"
        );
    }

    #[test]
    fn select_tools_unknown_with_bm25_suggestion() {
        let mut session = session();
        let defs = vec![
            deferred_def("mcp__server__read_file"),
            deferred_def("mcp__server__write_file"),
        ];
        let applied = apply_select_tools(&mut session, &defs, &["mcp__server__read_flie".into()]);
        assert!(applied.is_error);
        assert!(
            applied.content.contains("Did you mean"),
            "should contain BM25 suggestion: {}",
            applied.content
        );
        assert!(
            applied.content.contains("mcp__server__read_file"),
            "should suggest the correct tool name: {}",
            applied.content
        );
    }
}
