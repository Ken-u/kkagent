//! Multi-level folding of the transcript.
//!
//! Adjacent foldable items — finished thinking blocks, tool calls and
//! tool-history overviews with no visible assistant text between them — fold
//! together into one *group* (level 1). Expanding a group reveals its members
//! as one-line summaries (level 2); each member then expands on its own
//! (level 3: full reasoning / the tools of an overview, level 4: a tool's full
//! output).
//!
//! Grouping is a pure function of the messages, recomputed at render time, so
//! it follows streaming and turn-end rewrites (`collapse_tools_in_turn`)
//! without migrating data. A group may span consecutive assistant messages.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use crate::app::{DisplayMessage, DisplayPart, DisplayToolCall, MessageRole, ToolHistorySummary};

/// A run needs at least this many adjacent items to be worth a group header.
const MIN_GROUP_MEMBERS: usize = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum MemberSlot {
    /// The finished thinking block of an assistant message.
    Thinking,
    /// A foldable part (`Tool` / `ToolHistory`) of an assistant message.
    Part(usize),
}

impl MemberSlot {
    /// Stable per-message key used to store a click override.
    pub(crate) fn code(self) -> usize {
        match self {
            Self::Thinking => 0,
            Self::Part(index) => index + 1,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) struct MemberRef {
    pub message: usize,
    pub slot: MemberSlot,
}

#[derive(Debug, Clone)]
pub(crate) struct FoldRun {
    pub leader: MemberRef,
    pub members: Vec<MemberRef>,
    pub thinking_count: usize,
    pub tool_count: usize,
    /// Turn duration when an overview recorded it, else the summed thinking time.
    pub duration_ms: u64,
    /// Tool names in first-use order with their call counts.
    pub tool_digest: Vec<(String, usize)>,
    /// Headline of the first thinking block (used when no tool ran).
    pub headline: String,
    /// Still growing: not yet cut off by assistant text while the turn runs.
    pub open: bool,
    /// Effective state: the user's click, else the global Ctrl+O mode, else
    /// "open runs show their members live".
    pub expanded: bool,
}

impl FoldRun {
    pub(crate) fn is_leader(&self, message: usize, slot: MemberSlot) -> bool {
        self.leader == MemberRef { message, slot }
    }

    /// Everything the rendered header depends on, for layout-cache keys.
    pub(crate) fn hash_into(&self, hasher: &mut impl Hasher) {
        self.leader.hash(hasher);
        self.members.len().hash(hasher);
        self.thinking_count.hash(hasher);
        self.tool_count.hash(hasher);
        self.duration_ms.hash(hasher);
        self.tool_digest.hash(hasher);
        self.headline.hash(hasher);
        self.open.hash(hasher);
        self.expanded.hash(hasher);
    }
}

#[derive(Debug, Default)]
pub(crate) struct FoldPlan {
    runs: Vec<FoldRun>,
    index: HashMap<MemberRef, usize>,
}

impl FoldPlan {
    pub(crate) fn run_of(&self, message: usize, slot: MemberSlot) -> Option<&FoldRun> {
        self.index
            .get(&MemberRef { message, slot })
            .map(|&run| &self.runs[run])
    }

    pub(crate) fn runs(&self) -> &[FoldRun] {
        &self.runs
    }
}

enum Info<'a> {
    Thinking { text: &'a str, ms: Option<u64> },
    Tool(&'a DisplayToolCall),
    History(&'a ToolHistorySummary),
}

/// Group adjacent foldable items of `messages`.
///
/// * `live_thinking` — the message whose thinking is still streaming in the
///   live tail; it is not a finished block yet.
/// * `global_expanded` — the Ctrl+O mode.
/// * `turn_active` — an agent turn is running, so a run that reaches the end
///   of the transcript is still growing.
pub(crate) fn plan_folds(
    messages: &[DisplayMessage],
    live_thinking: Option<usize>,
    global_expanded: bool,
    turn_active: bool,
) -> FoldPlan {
    let mut plan = FoldPlan::default();
    let mut pending: Vec<(MemberRef, Info)> = Vec::new();
    let flush = |plan: &mut FoldPlan, pending: &mut Vec<(MemberRef, Info)>, closed: bool| {
        if pending.len() >= MIN_GROUP_MEMBERS {
            let run = build_run(pending, messages, closed, turn_active, global_expanded);
            let run_index = plan.runs.len();
            for (member, _) in pending.iter() {
                plan.index.insert(*member, run_index);
            }
            plan.runs.push(run);
        }
        pending.clear();
    };

    for (message_index, message) in messages.iter().enumerate() {
        if message.role != MessageRole::Assistant {
            flush(&mut plan, &mut pending, true);
            continue;
        }
        if let Some(text) = message.thinking.as_deref() {
            if live_thinking != Some(message_index) && !text.trim().is_empty() {
                pending.push((
                    MemberRef {
                        message: message_index,
                        slot: MemberSlot::Thinking,
                    },
                    Info::Thinking {
                        text,
                        ms: message.thinking_ms,
                    },
                ));
            }
        }
        if message.parts.is_empty() {
            // Legacy layout (tools + content outside `parts`) is never grouped
            // but still separates runs.
            if !message.tool_calls.is_empty() || !message.content.is_empty() {
                flush(&mut plan, &mut pending, true);
            }
            continue;
        }
        for (part_index, part) in message.parts.iter().enumerate() {
            let member = MemberRef {
                message: message_index,
                slot: MemberSlot::Part(part_index),
            };
            match part {
                DisplayPart::Tool(tool) => pending.push((member, Info::Tool(tool))),
                DisplayPart::ToolHistory(history) => pending.push((member, Info::History(history))),
                DisplayPart::Text(text) if text.is_empty() => {}
                DisplayPart::Text(_)
                | DisplayPart::SkillActivation { .. }
                | DisplayPart::CompactionHistory(_) => flush(&mut plan, &mut pending, true),
            }
        }
    }
    flush(&mut plan, &mut pending, false);
    plan
}

fn build_run(
    members: &[(MemberRef, Info)],
    messages: &[DisplayMessage],
    closed: bool,
    turn_active: bool,
    global_expanded: bool,
) -> FoldRun {
    let mut thinking_count = 0;
    let mut tool_count = 0;
    let mut history_ms = 0u64;
    let mut thinking_ms = 0u64;
    let mut digest: Vec<(String, usize)> = Vec::new();
    let mut headline = String::new();
    let mut note_tool = |name: &str, calls: usize| match digest.iter_mut().find(|(n, _)| n == name)
    {
        Some(entry) => entry.1 += calls,
        None => digest.push((name.to_string(), calls)),
    };
    for (_, info) in members {
        match info {
            Info::Thinking { text, ms } => {
                thinking_count += 1;
                thinking_ms = thinking_ms.saturating_add(ms.unwrap_or(0));
                if headline.is_empty() {
                    headline = crate::components::thinking_headline(text);
                }
            }
            Info::Tool(tool) => {
                tool_count += 1;
                note_tool(&tool.name, 1);
            }
            Info::History(history) => {
                tool_count += history.tool_count as usize;
                history_ms = history_ms.saturating_add(history.duration_ms);
                for tool in &history.tools {
                    note_tool(&tool.name, 1);
                }
            }
        }
    }
    let leader = members[0].0;
    let open = !closed && turn_active;
    let user_choice = messages
        .get(leader.message)
        .and_then(|message| message.group_override(leader.slot.code()));
    FoldRun {
        leader,
        members: members.iter().map(|(member, _)| *member).collect(),
        thinking_count,
        tool_count,
        duration_ms: if history_ms > 0 {
            history_ms
        } else {
            thinking_ms
        },
        tool_digest: digest,
        headline,
        open,
        expanded: user_choice.unwrap_or(global_expanded || open),
    }
}

/// `Read×3 Edit×2 Bash` — what the folded work actually did.
pub(crate) fn tool_digest_label(digest: &[(String, usize)]) -> String {
    digest
        .iter()
        .map(|(name, calls)| {
            if *calls > 1 {
                format!("{name}×{calls}")
            } else {
                name.clone()
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// Level-1 header line of a group. Returns the line index (the click target).
pub(crate) fn push_group_header(
    lines: &mut Vec<ratatui::text::Line<'static>>,
    run: &FoldRun,
    width: u16,
    theme: &crate::theme::Theme,
) -> usize {
    let mut parts: Vec<String> = Vec::new();
    if run.thinking_count > 0 {
        parts.push(format!(
            "{} thought{}",
            run.thinking_count,
            if run.thinking_count == 1 { "" } else { "s" }
        ));
    }
    if run.tool_count > 0 {
        parts.push(format!(
            "{} tool{}",
            run.tool_count,
            if run.tool_count == 1 { "" } else { "s" }
        ));
    }
    if run.duration_ms >= 1_000 {
        parts.push(crate::i18n::duration_label(
            crate::i18n::Locale::En,
            run.duration_ms,
        ));
    }
    let core = format!(
        "{} {}",
        if run.expanded { "▾" } else { "▸" },
        parts.join(" · ")
    );
    // The detail only helps while the group is folded; expanded, the members
    // below say it better.
    let detail = if run.expanded {
        String::new()
    } else if !run.tool_digest.is_empty() {
        tool_digest_label(&run.tool_digest)
    } else {
        run.headline.clone()
    };
    let hint = if run.expanded {
        "ctrl+o / click to collapse"
    } else {
        "ctrl+o / click to expand"
    };
    let line = lines.len();
    lines.push(ratatui::text::Line::from(
        crate::components::fold_summary_spans(width, theme, core, &detail, hint),
    ));
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::DisplayToolCall;
    use crate::prompt_queue::DeliveryState;

    fn message(role: MessageRole) -> DisplayMessage {
        DisplayMessage {
            role,
            content: String::new(),
            thinking: None,
            thinking_ms: None,
            thinking_expanded: None,
            group_overrides: Vec::new(),
            parts: Vec::new(),
            tool_calls: Vec::new(),
            delivery: DeliveryState::Sent,
            idempotency_key: None,
        }
    }

    fn tool(name: &str, done: bool) -> DisplayPart {
        DisplayPart::Tool(DisplayToolCall {
            id: format!("id-{name}"),
            name: name.into(),
            input_summary: String::new(),
            output: done.then(|| "ok".to_string()),
            is_error: false,
            collapsed: true,
            user_overridden: false,
            started_at: None,
            stopping: false,
            queued_behind: None,
        })
    }

    fn assistant(thinking: Option<&str>, parts: Vec<DisplayPart>) -> DisplayMessage {
        let mut message = message(MessageRole::Assistant);
        message.thinking = thinking.map(str::to_string);
        message.parts = parts;
        message
    }

    #[test]
    fn adjacent_thinking_and_tools_form_one_group_across_messages() {
        let messages = vec![
            message(MessageRole::User),
            assistant(Some("**Look**\nbody"), vec![tool("Read", true)]),
            assistant(Some("more"), vec![tool("Read", true), tool("Edit", true)]),
            assistant(None, vec![DisplayPart::Text("done".into())]),
        ];
        let plan = plan_folds(&messages, None, false, false);
        assert_eq!(plan.runs().len(), 1);
        let run = &plan.runs()[0];
        assert_eq!(run.members.len(), 5);
        assert_eq!(
            run.leader,
            MemberRef {
                message: 1,
                slot: MemberSlot::Thinking
            }
        );
        assert_eq!((run.thinking_count, run.tool_count), (2, 3));
        assert_eq!(tool_digest_label(&run.tool_digest), "Read×2 Edit");
        assert_eq!(run.headline, "Look");
        assert!(!run.open && !run.expanded, "closed runs start folded");
        assert!(plan.run_of(2, MemberSlot::Part(1)).is_some());
        assert!(plan.run_of(3, MemberSlot::Part(0)).is_none());
    }

    #[test]
    fn visible_text_and_other_roles_split_runs() {
        let messages = vec![
            assistant(Some("a"), vec![tool("Read", true)]),
            assistant(
                None,
                vec![
                    DisplayPart::Text("hello".into()),
                    tool("Grep", true),
                    tool("Edit", true),
                ],
            ),
            message(MessageRole::System),
            assistant(Some("b"), vec![tool("Bash", true)]),
        ];
        let plan = plan_folds(&messages, None, false, false);
        let leaders: Vec<_> = plan.runs().iter().map(|run| run.leader.message).collect();
        assert_eq!(leaders, vec![0, 1, 3]);
        assert_eq!(plan.runs()[1].members.len(), 2);
    }

    #[test]
    fn single_items_and_empty_text_do_not_group() {
        let lone_tool = vec![assistant(None, vec![tool("Read", true)])];
        assert!(plan_folds(&lone_tool, None, false, false).runs().is_empty());

        let lone_thinking = vec![assistant(
            Some("thought"),
            vec![DisplayPart::Text("answer".into())],
        )];
        assert!(plan_folds(&lone_thinking, None, false, false)
            .runs()
            .is_empty());

        // An empty text part is not a barrier.
        let transparent = vec![assistant(
            Some("t"),
            vec![DisplayPart::Text(String::new()), tool("Read", true)],
        )];
        assert_eq!(plan_folds(&transparent, None, false, false).runs().len(), 1);
    }

    #[test]
    fn the_run_still_growing_is_open_and_shows_its_members() {
        let messages = vec![
            message(MessageRole::User),
            assistant(Some("t"), vec![tool("Read", false)]),
        ];
        let live = plan_folds(&messages, None, false, true);
        assert!(live.runs()[0].open);
        assert!(live.runs()[0].expanded, "open runs are expanded by default");

        // The same transcript once the turn is over folds up.
        let done = plan_folds(&messages, None, false, false);
        assert!(!done.runs()[0].open && !done.runs()[0].expanded);

        // Text after the run closes it even mid-turn.
        let mut with_answer = messages.clone();
        with_answer.push(assistant(None, vec![DisplayPart::Text("answer".into())]));
        let closed = plan_folds(&with_answer, None, false, true);
        assert!(!closed.runs()[0].open && !closed.runs()[0].expanded);
    }

    #[test]
    fn global_mode_and_click_override_decide_expansion() {
        let mut messages = vec![assistant(Some("t"), vec![tool("Read", true)])];
        assert!(plan_folds(&messages, None, true, false).runs()[0].expanded);

        // A click on the leader (slot 0 = thinking) beats the global mode.
        messages[0].set_group_override(0, false);
        assert!(!plan_folds(&messages, None, true, false).runs()[0].expanded);
        messages[0].set_group_override(0, true);
        assert!(plan_folds(&messages, None, false, false).runs()[0].expanded);
    }

    #[test]
    fn live_thinking_is_not_a_member_yet() {
        let messages = vec![assistant(Some("still streaming"), vec![tool("Read", true)])];
        let plan = plan_folds(&messages, Some(0), false, true);
        assert!(plan.runs().is_empty(), "one finished item is not a group");
    }

    #[test]
    fn history_overviews_count_their_tools_and_duration() {
        let history = DisplayPart::ToolHistory(ToolHistorySummary {
            tool_count: 3,
            duration_ms: 42_000,
            tokens: 0,
            expanded: false,
            user_overridden: false,
            tools: vec![
                match tool("Read", true) {
                    DisplayPart::Tool(tool) => tool,
                    _ => unreachable!(),
                },
                match tool("Bash", true) {
                    DisplayPart::Tool(tool) => tool,
                    _ => unreachable!(),
                },
            ],
        });
        let messages = vec![
            assistant(Some("one"), Vec::new()),
            assistant(Some("two"), vec![history, DisplayPart::Text("done".into())]),
        ];
        let plan = plan_folds(&messages, None, false, false);
        let run = &plan.runs()[0];
        assert_eq!((run.thinking_count, run.tool_count), (2, 3));
        assert_eq!(run.duration_ms, 42_000);
        assert_eq!(tool_digest_label(&run.tool_digest), "Read Bash");
    }
}
