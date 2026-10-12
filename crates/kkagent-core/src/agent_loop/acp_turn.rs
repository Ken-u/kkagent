//! Turn runner for sessions whose model alias is `acp:<plugin>.<agent>`.
//!
//! [`AgentLoop::run_turn_step`] branches here before it resolves a model: the
//! whole step runs on an external ACP agent (see [`crate::acp_backend`]) and
//! kkagent only renders it. The runner
//!
//! 1. completes the per-turn bookkeeping the early branch skipped,
//! 2. checks the (session, agent) backend out of the hub, aligns its mode with
//!    kkagent's plan mode and builds the prompt (unanswered user messages plus
//!    a bounded preface for whatever the agent has not seen),
//! 3. drives the prompt while translating the agent's progress into the usual
//!    [`AgentEvent`]s, forwarding permission requests, `cursor/ask_question`
//!    and `cursor/create_plan` to the user through the regular approval /
//!    question / plan-review machinery, and polling for interrupts,
//! 4. leaves the transcript in the shape every other model understands:
//!    `[user, assistant text, harness-only activity summary]` — never a
//!    `ToolUse` block — and finishes the turn like any other step.

use super::{truncate_chars_for_event, AgentLoop, TurnStep};
use crate::acp_backend::{
    acp_tool_name, format_activity_summary, AcpInteraction, AcpLease, ToolActivity,
};
use crate::plan_review::{resolve_exit_plan_approval, PlanReviewDisplay};
use crate::session::Session;
use kkagent_acp::{
    AcpAskQuestionOutcome, AcpAskQuestionRequest, AcpCreatePlanOutcome, AcpCreatePlanRequest,
    AcpPermissionChoice, AcpPermissionOutcome, AcpPermissionRequest, AcpQuestionAnswer,
    ExternalProgress, PromptOutcome,
};
use kkagent_llm::{ChatContent, ChatMessage};
use kkagent_protocol::{
    AgentEvent, ApprovalDecision, ApprovalRequest, ApprovalResponse, ApprovalScope, PermissionMode,
    QuestionOption, QuestionPayload, SessionStatus, TodoItemEvent,
};
use serde_json::{json, Map, Value};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::time::MissedTickBehavior;

/// Buffered progress events between the client's reader and the runner.
const PROGRESS_BUFFER: usize = 1024;
/// Reverse requests waiting for the runner (they are answered one by one).
const INTERACTION_BUFFER: usize = 8;
/// How often a running prompt checks for an interrupt.
const INTERRUPT_POLL: Duration = Duration::from_millis(100);
/// Time the agent gets to wind down after `session/cancel` before its process
/// is killed.
const CANCEL_GRACE: Duration = if cfg!(test) {
    Duration::from_millis(400)
} else {
    Duration::from_secs(8)
};
/// Longest tool output forwarded in one `ToolResult` event (chars).
const TOOL_OUTPUT_EVENT_CHARS: usize = 8_000;
/// Longest detail shown in an approval action line (chars).
const ACTION_DETAIL_CHARS: usize = 200;
/// Assistant text stored when the agent ran tools but wrote no answer, so the
/// transcript keeps its user/assistant alternation.
const NO_TEXT_REPLY: &str = "[The external agent finished without a text reply.]";
/// Same, when the user interrupted the turn before any text arrived.
const NO_TEXT_INTERRUPTED: &str =
    "[The turn was interrupted before the external agent wrote a reply.]";
/// Same, when the agent failed (crash, error, killed) before any text arrived.
const NO_TEXT_FAILED: &str = "[The external agent stopped with an error before writing a reply.]";

// ---------------------------------------------------------------------------
// Per-turn state
// ---------------------------------------------------------------------------

/// Latest known state of one tool call of the external agent.
#[derive(Debug, Default, Clone)]
struct ToolTrack {
    id: String,
    kind: Option<String>,
    title: String,
    status: Option<String>,
    raw_input: Option<Value>,
    locations: Vec<String>,
    output: Option<String>,
    /// Name used in the `ToolCall` event (and so in its `ToolResult`).
    event_name: String,
    /// Why the call was refused (by the user or by policy). The agent itself
    /// reports a refused call as plain `completed`, which would otherwise
    /// show up as a success on the card and in the transcript summary.
    rejected: Option<String>,
    started: bool,
    finished: bool,
}

/// Fields of one progress update merged into a [`ToolTrack`].
#[derive(Default)]
struct ToolUpdate<'a> {
    id: &'a str,
    title: Option<&'a str>,
    kind: Option<&'a str>,
    status: Option<&'a str>,
    raw_input: Option<&'a Value>,
    output: Option<&'a str>,
    locations: &'a [String],
}

struct RunState {
    session_id: String,
    agent_name: String,
    /// The agent's answer so far (what ends up in the transcript).
    text: String,
    /// A tool card was shown since the last text: separate the next text
    /// chunk from the previous one in the stored answer.
    break_before_text: bool,
    tools: Vec<ToolTrack>,
    index: HashMap<String, usize>,
    /// Started-but-unfinished tool calls (drives the status line).
    running: usize,
    /// The user cancelled from an approval card: stop the turn.
    stop_requested: bool,
    /// Activity that has no tool call of its own (plans, answered questions).
    notes: Vec<ToolActivity>,
}

impl RunState {
    fn new(session_id: &str, agent_name: &str) -> Self {
        Self {
            session_id: session_id.to_string(),
            agent_name: agent_name.to_string(),
            text: String::new(),
            break_before_text: false,
            tools: Vec::new(),
            index: HashMap::new(),
            running: 0,
            stop_requested: false,
            notes: Vec::new(),
        }
    }

    fn upsert_tool(&mut self, update: ToolUpdate<'_>) -> usize {
        let index = match self.index.get(update.id) {
            Some(index) => *index,
            None => {
                self.tools.push(ToolTrack {
                    id: update.id.to_string(),
                    ..ToolTrack::default()
                });
                self.index
                    .insert(update.id.to_string(), self.tools.len() - 1);
                self.tools.len() - 1
            }
        };
        let tool = &mut self.tools[index];
        if let Some(title) = update.title.map(str::trim).filter(|t| !t.is_empty()) {
            tool.title = title.to_string();
        }
        if let Some(kind) = update.kind.map(str::trim).filter(|k| !k.is_empty()) {
            tool.kind = Some(kind.to_string());
        }
        if let Some(status) = update.status.map(str::trim).filter(|s| !s.is_empty()) {
            tool.status = Some(status.to_string());
        }
        if let Some(raw) = update.raw_input {
            // Agents announce `{}` first and fill the input in later; never
            // let the placeholder wipe out what we already know.
            if is_meaningful(raw) || tool.raw_input.is_none() {
                tool.raw_input = Some(raw.clone());
            }
        }
        if let Some(output) = update.output.filter(|o| !o.trim().is_empty()) {
            tool.output = Some(output.to_string());
        }
        if !update.locations.is_empty() {
            tool.locations = update.locations.to_vec();
        }
        index
    }

    /// Everything the turn did, for the transcript summary.
    fn activity(&self) -> Vec<ToolActivity> {
        self.tools
            .iter()
            .map(|tool| ToolActivity {
                kind: tool.kind.clone().unwrap_or_default(),
                title: tool.title.clone(),
                status: if tool.rejected.is_some() {
                    "rejected".to_string()
                } else {
                    tool.status.clone().unwrap_or_default()
                },
                locations: tool.locations.clone(),
            })
            .chain(self.notes.iter().cloned())
            .collect()
    }

    /// Remember that the call `tool_call_id` was refused (see
    /// [`ToolTrack::rejected`]).
    fn mark_rejected(&mut self, tool_call_id: &str, note: String) {
        if let Some(index) = self.index.get(tool_call_id).copied() {
            self.tools[index].rejected = Some(note);
        }
    }

    /// Record what happened to a call that has no card of its own (or refine
    /// the title of the one it belongs to).
    fn annotate(
        &mut self,
        tool_call_id: &str,
        kind: &str,
        title: String,
        status: &str,
        locations: &[String],
    ) {
        if let Some(index) = self.index.get(tool_call_id).copied() {
            let tool = &mut self.tools[index];
            tool.title = title;
            tool.status = Some(status.to_string());
            if tool.kind.is_none() {
                tool.kind = Some(kind.to_string());
            }
            if tool.locations.is_empty() {
                tool.locations = locations.to_vec();
            }
            return;
        }
        self.notes.push(ToolActivity {
            kind: kind.to_string(),
            title,
            status: status.to_string(),
            locations: locations.to_vec(),
        });
    }
}

fn is_meaningful(value: &Value) -> bool {
    match value {
        Value::Null => false,
        Value::Object(map) => !map.is_empty(),
        Value::Array(items) => !items.is_empty(),
        Value::String(text) => !text.trim().is_empty(),
        _ => true,
    }
}

fn is_terminal_status(status: Option<&str>) -> bool {
    matches!(
        status.map(|s| s.trim().to_ascii_lowercase()).as_deref(),
        Some("completed" | "failed" | "error" | "cancelled" | "canceled")
    )
}

fn is_failure_status(status: Option<&str>) -> bool {
    matches!(
        status.map(|s| s.trim().to_ascii_lowercase()).as_deref(),
        Some("failed" | "error" | "cancelled" | "canceled")
    )
}

/// A call is worth a card once it says what it does: an input, the files it
/// touches, or progress beyond the `pending` announcement.
fn ready_to_start(tool: &ToolTrack) -> bool {
    tool.raw_input.as_ref().is_some_and(is_meaningful)
        || !tool.locations.is_empty()
        || matches!(
            tool.status
                .as_deref()
                .map(|s| s.trim().to_ascii_lowercase())
                .as_deref(),
            Some("in_progress" | "completed" | "failed" | "error")
        )
}

fn string_field(value: Option<&Value>, key: &str) -> Option<String> {
    value
        .and_then(|v| v.get(key))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(str::to_string)
}

/// `input` of the `ToolCall` event: the keys the TUI summarizes (`command`,
/// `path`, `description`) plus the raw material.
fn tool_event_input(tool: &ToolTrack) -> Value {
    let mut input = Map::new();
    if let Some(command) = string_field(tool.raw_input.as_ref(), "command") {
        input.insert("command".into(), json!(command));
    }
    let path =
        string_field(tool.raw_input.as_ref(), "path").or_else(|| tool.locations.first().cloned());
    if let Some(path) = path {
        input.insert("path".into(), json!(path));
    }
    if !tool.title.is_empty() {
        input.insert("description".into(), json!(tool.title));
    }
    if let Some(kind) = &tool.kind {
        input.insert("kind".into(), json!(kind));
    }
    if tool.locations.len() > 1 {
        input.insert("locations".into(), json!(tool.locations));
    }
    if let Some(raw) = tool.raw_input.as_ref().filter(|raw| is_meaningful(raw)) {
        input.insert("raw".into(), raw.clone());
    }
    Value::Object(input)
}

/// What the permission chain evaluates and the approval card shows: `command`
/// and `path` like built-in tool inputs (the chain's secret/git gates read
/// those), plus the request's own details.
fn permission_input(request: &AcpPermissionRequest, tool: &ToolTrack) -> Value {
    let raw = request.raw_input.as_ref().or(tool.raw_input.as_ref());
    let kind = request.tool_kind.as_deref().or(tool.kind.as_deref());
    let title = if request.title.trim().is_empty() {
        tool.title.as_str()
    } else {
        request.title.trim()
    };
    let locations: &[String] = if request.locations.is_empty() {
        &tool.locations
    } else {
        &request.locations
    };
    let mut input = Map::new();
    let command = string_field(raw, "command")
        .or_else(|| (kind == Some("execute") && !title.is_empty()).then(|| title.to_string()));
    if let Some(command) = command {
        input.insert("command".into(), json!(command));
    }
    // A sensitive location wins so the gate sees it even when the call
    // touches several files.
    let path = locations
        .iter()
        .find(|location| kkagent_tools::path_policy::is_sensitive_path(Path::new(location)))
        .or_else(|| locations.first())
        .cloned()
        .or_else(|| string_field(raw, "path"));
    if let Some(path) = path {
        input.insert("path".into(), json!(path));
    }
    if !title.is_empty() {
        input.insert("title".into(), json!(title));
    }
    if let Some(kind) = kind {
        input.insert("kind".into(), json!(kind));
    }
    if locations.len() > 1 {
        input.insert("locations".into(), json!(locations));
    }
    if let Some(raw) = raw.filter(|raw| is_meaningful(raw)) {
        input.insert("raw_input".into(), raw.clone());
    }
    if let Some(preview) = request.preview.as_deref().filter(|p| !p.trim().is_empty()) {
        input.insert(
            "preview".into(),
            json!(truncate_chars_for_event(preview, 2_000)),
        );
    }
    Value::Object(input)
}

fn permission_action(tool_name: &str, input: &Value) -> String {
    let detail = string_field(Some(input), "command")
        .or_else(|| string_field(Some(input), "path"))
        .or_else(|| string_field(Some(input), "title"))
        .unwrap_or_default();
    let short: String = detail.chars().take(ACTION_DETAIL_CHARS).collect();
    format!("{tool_name}  {short}")
}

/// Markdown document for a `cursor/create_plan` request.
fn plan_markdown(request: &AcpCreatePlanRequest, name: &str) -> String {
    let body = request.plan.trim();
    let mut out = String::new();
    if !body.lines().any(|line| line.starts_with("# ")) {
        out.push_str(&format!("# {name}\n\n"));
        if let Some(overview) = request
            .overview
            .as_deref()
            .map(str::trim)
            .filter(|o| !o.is_empty())
        {
            out.push_str(overview);
            out.push_str("\n\n");
        }
    }
    out.push_str(body);
    if !request.todos.is_empty() {
        out.push_str("\n\n## Todos\n\n");
        for todo in &request.todos {
            let mark = if todo.status.eq_ignore_ascii_case("completed") {
                "x"
            } else {
                " "
            };
            out.push_str(&format!("- [{mark}] {}\n", todo.content));
        }
    }
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

fn file_uri(path: &Path) -> Option<String> {
    reqwest::Url::from_file_path(path)
        .ok()
        .map(|url| url.to_string())
}

fn normalize_todo_status(status: &str) -> String {
    match status.trim().to_ascii_lowercase().as_str() {
        "in_progress" | "in-progress" | "inprogress" | "running" => "in_progress",
        "completed" | "complete" | "done" => "completed",
        "cancelled" | "canceled" => "cancelled",
        _ => "pending",
    }
    .to_string()
}

// ---------------------------------------------------------------------------
// The step
// ---------------------------------------------------------------------------

impl AgentLoop {
    /// One step of a session whose model is the ACP agent `agent_id`
    /// (`<plugin>.<name>`). `effort` is the thinking level the alias asks
    /// of the agent (`acp:<id>#max`); `None` leaves the agent on its own.
    pub(super) async fn run_acp_turn_step(
        &self,
        session: &mut Session,
        agent_id: &str,
        effort: Option<&str>,
    ) -> anyhow::Result<TurnStep> {
        let session_id = session.id.clone();

        // Per-turn bookkeeping `run_turn_step` performs after resolving its
        // model (the early branch bypasses it). Idempotent across steps.
        session.usage.begin_turn();
        session.services.mark_turn_started();
        session.begin_turn();

        if let Some(hooks) = &self.hooks {
            let _ = hooks
                .fire(
                    kkagent_mcp::hooks::HookEvent::TurnStart,
                    &json!({
                        "session_id": session_id,
                        "workspace": session.working_dir,
                    }),
                )
                .await;
        }

        // Steers that arrived before this step are ordinary user messages now.
        let steers = session.drain_steers_into_messages()?;
        if steers > 0 {
            tracing::info!("Queued {steers} steer message(s) join this ACP prompt");
        }

        let Some(hub) = self.acp_backends.clone() else {
            return self
                .acp_fail(
                    session,
                    "ACP backends are not available in this context; pick another model with /model."
                        .to_string(),
                )
                .await;
        };

        let cwd = session.working_dir.clone();
        let mut lease = match hub.checkout(&session_id, agent_id, &cwd).await {
            Ok(lease) => lease,
            Err(error) => return self.acp_fail(session, format!("{error:#}")).await,
        };
        let agent_name = lease.agent_name();
        tracing::info!(session = %session_id, agent = %agent_id, "Running turn on ACP agent");

        // kkagent's plan mode drives the agent's own plan mode.
        let base_mode = lease
            .spec()
            .transport_config
            .mode
            .clone()
            .filter(|mode| !mode.trim().is_empty() && !mode.eq_ignore_ascii_case("plan"))
            .unwrap_or_else(|| "agent".to_string());
        let desired_mode = if session.plan_mode {
            "plan".to_string()
        } else {
            base_mode
        };

        let prepared = match lease.prepare(&session.messages, &cwd).await {
            Ok(prepared) => prepared,
            Err(error) => {
                lease.invalidate().await;
                return self.acp_fail(session, format!("{error:#}")).await;
            }
        };
        if prepared.text.trim().is_empty() {
            return self
                .acp_fail(
                    session,
                    format!("Nothing to send to the {agent_name} agent: no pending user message."),
                )
                .await;
        }

        // The agent's session settings, applied after `prepare`: a rebuilt
        // ACP session starts from the agent's defaults again.
        lease.ensure_mode(&desired_mode).await;
        let note = match effort {
            Some(level) => lease
                .ensure_thought_level(level)
                .await
                .note(&agent_name, level),
            None => None,
        };
        let snapshot = lease.agent_snapshot();
        let _ = self
            .event_tx
            .send(AgentEvent::AcpAgentState {
                session_id: session_id.clone(),
                agent: agent_name.clone(),
                model: snapshot.model,
                thought_level: snapshot.thought_level,
                thought_levels: snapshot.thought_levels,
                note,
            })
            .await;
        if prepared.rebuilt || prepared.has_preface {
            tracing::info!(
                session = %session_id,
                rebuilt = prepared.rebuilt,
                preface = prepared.has_preface,
                "ACP prompt carries transcript context"
            );
        }

        let (progress_tx, mut progress_rx) =
            mpsc::channel::<(String, ExternalProgress)>(PROGRESS_BUFFER);
        let (interaction_tx, mut interaction_rx) =
            mpsc::channel::<AcpInteraction>(INTERACTION_BUFFER);
        let attachment = lease.bridge().attach(interaction_tx);

        let client = lease.client();
        let acp_session_id = lease.acp_session_id();
        let idle_timeout = lease.idle_timeout();
        let prompt_text = prepared.text;
        let mut prompt = {
            let client = client.clone();
            let acp_session_id = acp_session_id.clone();
            Box::pin(async move {
                client
                    .prompt_interactive(&acp_session_id, &prompt_text, progress_tx, idle_timeout)
                    .await
            })
        };

        let mut run = RunState::new(&session_id, &agent_name);
        let mut ticker = tokio::time::interval(INTERRUPT_POLL);
        ticker.set_missed_tick_behavior(MissedTickBehavior::Delay);

        let mut interrupted = false;
        let mut outcome: Option<anyhow::Result<PromptOutcome>> = None;
        loop {
            tokio::select! {
                biased;
                _ = ticker.tick() => {
                    if run.stop_requested || session.is_interrupted() {
                        interrupted = true;
                        break;
                    }
                }
                Some(interaction) = interaction_rx.recv() => {
                    // Show everything the agent said before it asked.
                    while let Ok((_, progress)) = progress_rx.try_recv() {
                        self.acp_progress(session, &mut lease, &mut run, progress).await;
                    }
                    self.acp_interaction(session, &mut run, interaction).await;
                }
                Some((_, progress)) = progress_rx.recv() => {
                    self.acp_progress(session, &mut lease, &mut run, progress).await;
                }
                result = &mut prompt => {
                    outcome = Some(result);
                    break;
                }
            }
        }

        // Interrupt: ask the agent to stop, give it a moment, then kill it.
        let mut killed = false;
        if interrupted {
            client.cancel(&acp_session_id).await;
            let settle = async {
                loop {
                    tokio::select! {
                        result = &mut prompt => break result,
                        Some(interaction) = interaction_rx.recv() => cancel_interaction(interaction),
                        Some((_, progress)) = progress_rx.recv() => {
                            self.acp_progress(session, &mut lease, &mut run, progress).await;
                        }
                    }
                }
            };
            match tokio::time::timeout(CANCEL_GRACE, settle).await {
                Ok(result) => outcome = Some(result),
                Err(_) => {
                    tracing::warn!(session = %session_id, "ACP agent ignored session/cancel; killing it");
                    killed = true;
                }
            }
        }
        drop(attachment);
        while let Ok((_, progress)) = progress_rx.try_recv() {
            self.acp_progress(session, &mut lease, &mut run, progress)
                .await;
        }

        // Settle what the agent reported.
        let mut notices: Vec<String> = Vec::new();
        let mut failed = killed;
        match outcome {
            Some(Ok(done)) => {
                if run.text.is_empty() && !done.text.is_empty() {
                    // The agent only delivered its answer with the result.
                    run.text = done.text.clone();
                    let _ = self
                        .event_tx
                        .send(AgentEvent::MessageDelta {
                            session_id: session_id.clone(),
                            text: done.text.clone(),
                        })
                        .await;
                }
                if let Some(notice) =
                    stop_reason_notice(&done.stop_reason, &agent_name, interrupted)
                {
                    notices.push(notice);
                }
            }
            Some(Err(error)) => {
                failed = true;
                if !interrupted {
                    notices.push(format!("{agent_name}: {error:#}"));
                }
            }
            None => {}
        }

        // Cards still spinning get closed so the TUI never shows stale tools.
        self.acp_close_tools(&mut run, interrupted || failed).await;

        // Transcript: answer text, then the activity summary.
        let answer = run.text.trim_end().to_string();
        let activity = run.activity();
        let summary = format_activity_summary(agent_id, &agent_name, &activity);
        if !answer.is_empty() || summary.is_some() {
            let text = if !answer.is_empty() {
                answer
            } else if interrupted {
                NO_TEXT_INTERRUPTED.to_string()
            } else if failed {
                NO_TEXT_FAILED.to_string()
            } else {
                NO_TEXT_REPLY.to_string()
            };
            session.messages.push(ChatMessage {
                role: "assistant".into(),
                content: vec![ChatContent::Text { text }],
                tools: None,
            });
            self.persist_step(session);
        }
        if let Some(summary) = summary {
            session.messages.push(ChatMessage {
                role: "user".into(),
                content: vec![ChatContent::Text { text: summary }],
                tools: None,
            });
            self.persist_step(session);
        }

        // The ACP session is a cache of the transcript: keep it only when it
        // is healthy and has seen everything now in the transcript.
        if failed {
            lease.invalidate().await;
        } else {
            lease.mark_synced(&session.messages);
            drop(lease);
        }

        for notice in notices {
            let _ = self
                .event_tx
                .send(AgentEvent::Error {
                    session_id: session_id.clone(),
                    message: notice,
                })
                .await;
        }

        if interrupted {
            self.finish_interrupted(session).await?;
            return Ok(TurnStep::Done);
        }
        if session.finish_or_apply_steers()? {
            tracing::info!("Continuing ACP turn for newly buffered steer input");
            return Ok(TurnStep::Continue);
        }
        self.finish_turn(session, false).await?;
        Ok(TurnStep::Done)
    }

    /// Report a failure that happened before or instead of a prompt and end
    /// the step cleanly (idle status, steers preserved).
    async fn acp_fail(&self, session: &mut Session, message: String) -> anyhow::Result<TurnStep> {
        let _ = self
            .event_tx
            .send(AgentEvent::Error {
                session_id: session.id.clone(),
                message,
            })
            .await;
        if session.finish_or_apply_steers()? {
            return Ok(TurnStep::Continue);
        }
        self.finish_turn(session, false).await?;
        Ok(TurnStep::Done)
    }

    async fn acp_status(&self, session_id: &str, status: SessionStatus) {
        let _ = self
            .event_tx
            .send(AgentEvent::StatusUpdate {
                session_id: session_id.to_string(),
                status,
            })
            .await;
    }

    /// Status to show once an interaction is answered.
    fn acp_resting_status(run: &RunState) -> SessionStatus {
        if run.running > 0 {
            SessionStatus::ToolExecuting
        } else {
            SessionStatus::Thinking
        }
    }

    // -- progress -----------------------------------------------------------

    async fn acp_progress(
        &self,
        session: &mut Session,
        lease: &mut AcpLease,
        run: &mut RunState,
        progress: ExternalProgress,
    ) {
        match progress {
            ExternalProgress::Text { delta } => {
                if delta.is_empty() {
                    return;
                }
                if run.break_before_text {
                    run.break_before_text = false;
                    if !run.text.is_empty() && !run.text.ends_with('\n') {
                        run.text.push_str("\n\n");
                    }
                }
                run.text.push_str(&delta);
                let _ = self
                    .event_tx
                    .send(AgentEvent::MessageDelta {
                        session_id: run.session_id.clone(),
                        text: delta,
                    })
                    .await;
            }
            ExternalProgress::Thought { delta } => {
                if delta.is_empty() {
                    return;
                }
                let _ = self
                    .event_tx
                    .send(AgentEvent::ThinkingDelta {
                        session_id: run.session_id.clone(),
                        text: delta,
                    })
                    .await;
            }
            ExternalProgress::ToolCall {
                tool_call_id,
                title,
                tool_kind,
                status,
                raw_input,
                output,
                locations,
            } => {
                if tool_call_id.is_empty() {
                    return;
                }
                let index = run.upsert_tool(ToolUpdate {
                    id: &tool_call_id,
                    title: Some(&title),
                    kind: tool_kind.as_deref(),
                    status: status.as_deref(),
                    raw_input: raw_input.as_ref(),
                    output: output.as_deref(),
                    locations: &locations,
                });
                if is_terminal_status(run.tools[index].status.as_deref()) {
                    self.acp_emit_finish(run, index, None).await;
                } else if ready_to_start(&run.tools[index]) {
                    self.acp_emit_start(run, index).await;
                }
            }
            ExternalProgress::Todos { todos } => {
                let items = todos
                    .into_iter()
                    .map(|todo| TodoItemEvent {
                        id: todo.id,
                        content: todo.content,
                        status: normalize_todo_status(&todo.status),
                    })
                    .collect();
                self.acp_set_todos(session, items).await;
            }
            ExternalProgress::Plan { entries } => {
                let items: Vec<TodoItemEvent> = entries
                    .iter()
                    .enumerate()
                    .filter_map(|(position, entry)| {
                        let content = entry.get("content")?.as_str()?.trim();
                        if content.is_empty() {
                            return None;
                        }
                        Some(TodoItemEvent {
                            id: format!("plan-{}", position + 1),
                            content: content.to_string(),
                            status: normalize_todo_status(
                                entry
                                    .get("status")
                                    .and_then(Value::as_str)
                                    .unwrap_or("pending"),
                            ),
                        })
                    })
                    .collect();
                if !items.is_empty() {
                    self.acp_set_todos(session, items).await;
                }
            }
            ExternalProgress::ModeChanged { mode_id } => {
                lease.note_mode(&mode_id);
            }
            ExternalProgress::Unknown { update } => {
                tracing::debug!(session = %run.session_id, "ignored ACP update: {update}");
            }
        }
    }

    async fn acp_set_todos(&self, session: &mut Session, items: Vec<TodoItemEvent>) {
        if let Err(error) = session.set_todos_persisted(items) {
            tracing::warn!(session = %session.id, "cannot persist ACP todos: {error:#}");
        }
        let _ = self
            .event_tx
            .send(AgentEvent::TodoUpdated {
                session_id: session.id.clone(),
                items: session.todo_items(),
            })
            .await;
    }

    /// Show the tool card (once).
    async fn acp_emit_start(&self, run: &mut RunState, index: usize) {
        if run.tools[index].started {
            return;
        }
        let name = acp_tool_name(&run.agent_name, run.tools[index].kind.as_deref());
        let input = tool_event_input(&run.tools[index]);
        let tool_call_id = run.tools[index].id.clone();
        {
            let tool = &mut run.tools[index];
            tool.started = true;
            tool.event_name = name.clone();
        }
        run.break_before_text = true;
        if run.running == 0 {
            self.acp_status(&run.session_id, SessionStatus::ToolExecuting)
                .await;
        }
        run.running += 1;
        let _ = self
            .event_tx
            .send(AgentEvent::ToolCall {
                session_id: run.session_id.clone(),
                tool_call_id,
                tool_name: name,
                input,
            })
            .await;
    }

    /// Complete the tool card (once). `forced` closes a call the agent never
    /// finished with `(note, is_error)`.
    async fn acp_emit_finish(
        &self,
        run: &mut RunState,
        index: usize,
        forced: Option<(&str, bool)>,
    ) {
        if run.tools[index].finished {
            return;
        }
        self.acp_emit_start(run, index).await;
        let (tool_call_id, name, output, is_error) = {
            let tool = &mut run.tools[index];
            tool.finished = true;
            let failure = is_failure_status(tool.status.as_deref());
            let refused = tool.rejected.as_deref();
            let output = match (
                refused.or(forced.map(|(note, _)| note)),
                tool.output.as_deref(),
            ) {
                (Some(note), _) => note.to_string(),
                (None, Some(output)) => truncate_chars_for_event(output, TOOL_OUTPUT_EVENT_CHARS),
                (None, None) if failure => "[failed]".to_string(),
                (None, None) => "[ok]".to_string(),
            };
            (
                tool.id.clone(),
                tool.event_name.clone(),
                output,
                failure || refused.is_some() || forced.is_some_and(|(_, is_error)| is_error),
            )
        };
        run.running = run.running.saturating_sub(1);
        let _ = self
            .event_tx
            .send(AgentEvent::ToolResult {
                session_id: run.session_id.clone(),
                tool_call_id,
                tool_name: name,
                output,
                is_error,
            })
            .await;
        if run.running == 0 {
            self.acp_status(&run.session_id, SessionStatus::Thinking)
                .await;
        }
    }

    /// Close every started card that never got a result.
    async fn acp_close_tools(&self, run: &mut RunState, abnormal: bool) {
        for index in 0..run.tools.len() {
            let tool = &run.tools[index];
            if tool.started && !tool.finished {
                let note = if abnormal {
                    "[interrupted]"
                } else {
                    "[no result reported]"
                };
                self.acp_emit_finish(run, index, Some((note, abnormal)))
                    .await;
            }
        }
    }

    // -- reverse requests ---------------------------------------------------

    async fn acp_interaction(
        &self,
        session: &mut Session,
        run: &mut RunState,
        interaction: AcpInteraction,
    ) {
        match interaction {
            AcpInteraction::Permission { request, reply } => {
                let outcome = self.acp_permission(session, run, request).await;
                let _ = reply.send(outcome);
            }
            AcpInteraction::Question { request, reply } => {
                let outcome = self.acp_question(session, run, request).await;
                let _ = reply.send(outcome);
            }
            AcpInteraction::Plan { request, reply } => {
                let outcome = self.acp_plan(session, run, request).await;
                let _ = reply.send(outcome);
            }
        }
    }

    /// `session/request_permission`: the permission chain decides; "ask"
    /// becomes an approval card.
    async fn acp_permission(
        &self,
        session: &mut Session,
        run: &mut RunState,
        request: AcpPermissionRequest,
    ) -> AcpPermissionOutcome {
        let tool_call_id = if request.tool_call_id.is_empty() {
            format!("acp-{}", uuid::Uuid::new_v4())
        } else {
            request.tool_call_id.clone()
        };
        // The request is the first moment the host learns what the tool will
        // do: make sure its card exists before the approval card refers to it.
        let index = run.upsert_tool(ToolUpdate {
            id: &tool_call_id,
            title: Some(&request.title),
            kind: request.tool_kind.as_deref(),
            status: None,
            raw_input: request.raw_input.as_ref(),
            output: None,
            locations: &request.locations,
        });
        self.acp_emit_start(run, index).await;
        let tool = run.tools[index].clone();
        let kind = tool.kind.clone();
        let tool_name = acp_tool_name(&run.agent_name, kind.as_deref());
        let audit_tool = format!(
            "acp:{}:{}",
            run.agent_name,
            kind.as_deref().unwrap_or("tool")
        );
        let input = permission_input(&request, &tool);

        let (decision, source) = {
            let perm = self.permission.lock().await;
            let (decision, source) = perm.evaluate_external(
                &tool_name,
                kind.as_deref(),
                &input,
                &session.working_dir,
                session.plan_mode,
                Some(&session.plan_file_path),
            );
            let verdict = match &decision {
                crate::permission::PermissionDecision::Approve => "approve",
                crate::permission::PermissionDecision::Ask => "ask",
                crate::permission::PermissionDecision::Deny(_) => "deny",
            };
            crate::audit::record(&crate::audit::AuditEvent::PermissionVerdict {
                at: &crate::audit::now_rfc3339(),
                session_id: &session.id,
                tool: &audit_tool,
                verdict,
                source,
                detail: "",
                permission_mode: &format!("{:?}", perm.current_mode()),
            });
            (decision, source)
        };
        tracing::info!("ACP permission for {tool_name}: {decision:?} ({source})");

        let choice = match decision {
            crate::permission::PermissionDecision::Approve => Some(AcpPermissionChoice::AllowOnce),
            crate::permission::PermissionDecision::Deny(reason) => {
                tracing::info!("ACP request {tool_name} denied: {reason}");
                run.mark_rejected(&tool_call_id, format!("[denied: {reason}]"));
                Some(AcpPermissionChoice::RejectOnce)
            }
            crate::permission::PermissionDecision::Ask => {
                self.acp_ask_permission(
                    session,
                    run,
                    &tool_name,
                    &audit_tool,
                    &tool_call_id,
                    &input,
                )
                .await
            }
        };

        match choice {
            None => AcpPermissionOutcome::Cancelled,
            Some(choice) => select_permission_option(&request, choice),
        }
    }

    /// Ask the user about one tool request. `None` means the user cancelled
    /// the whole turn.
    async fn acp_ask_permission(
        &self,
        session: &mut Session,
        run: &mut RunState,
        tool_name: &str,
        audit_tool: &str,
        tool_call_id: &str,
        input: &Value,
    ) -> Option<AcpPermissionChoice> {
        let approval_id = uuid::Uuid::new_v4().to_string();
        let _ = self
            .event_tx
            .send(AgentEvent::ApprovalRequested {
                session_id: run.session_id.clone(),
                request: ApprovalRequest {
                    approval_id: approval_id.clone(),
                    session_id: run.session_id.clone(),
                    tool_call_id: tool_call_id.to_string(),
                    tool_name: tool_name.to_string(),
                    action: permission_action(tool_name, input),
                    tool_input_display: Some(input.clone()),
                    created_at: chrono::Utc::now(),
                },
            })
            .await;
        self.acp_status(&run.session_id, SessionStatus::WaitingApproval)
            .await;

        let response = self.acp_wait_approval(session, &approval_id).await;
        self.acp_status(&run.session_id, Self::acp_resting_status(run))
            .await;

        let record = |response: &str, scope: &str| {
            crate::audit::record(&crate::audit::AuditEvent::ApprovalResponse {
                at: &crate::audit::now_rfc3339(),
                session_id: &run.session_id,
                tool: audit_tool,
                response,
                scope,
            });
        };
        match response.decision {
            ApprovalDecision::Approved => {
                let scope = response.scope.unwrap_or(ApprovalScope::Once);
                record("approved", &format!("{scope:?}"));
                match scope {
                    ApprovalScope::Session => {
                        // kkagent keeps the memory: the agent's own "always"
                        // would outlive this session in its own config.
                        let mut perm = self.permission.lock().await;
                        perm.record_session_approval(tool_name, input);
                        Some(AcpPermissionChoice::AllowOnce)
                    }
                    ApprovalScope::Turn => {
                        let mut perm = self.permission.lock().await;
                        perm.record_turn_approval(tool_name, input);
                        Some(AcpPermissionChoice::AllowOnce)
                    }
                    ApprovalScope::Always => {
                        let mut perm = self.permission.lock().await;
                        perm.record_always_approval(tool_name, input);
                        Some(AcpPermissionChoice::AllowAlways)
                    }
                    ApprovalScope::Once => Some(AcpPermissionChoice::AllowOnce),
                }
            }
            ApprovalDecision::Rejected => {
                record("rejected", "once");
                let note = match response
                    .feedback
                    .as_deref()
                    .map(str::trim)
                    .filter(|feedback| !feedback.is_empty())
                {
                    Some(feedback) => format!("[rejected: {feedback}]"),
                    None => "[rejected by user]".to_string(),
                };
                run.mark_rejected(tool_call_id, note);
                Some(AcpPermissionChoice::RejectOnce)
            }
            ApprovalDecision::Cancelled => {
                record("cancelled", "once");
                run.stop_requested = true;
                None
            }
        }
    }

    /// Wait for the user's answer to an approval card, bounded by the
    /// configured approval timeout (expiry rejects).
    async fn acp_wait_approval(
        &self,
        session: &mut Session,
        approval_id: &str,
    ) -> ApprovalResponse {
        let timeout_secs = self
            .config
            .background
            .as_ref()
            .and_then(|background| background.approval_timeout_s)
            .unwrap_or(900)
            .clamp(1, 86_400);
        match tokio::time::timeout(
            Duration::from_secs(timeout_secs),
            session.wait_approval(approval_id),
        )
        .await
        {
            Ok(response) => response,
            Err(_) => ApprovalResponse {
                approval_id: approval_id.to_string(),
                decision: ApprovalDecision::Rejected,
                scope: None,
                feedback: Some(format!("approval timed out after {timeout_secs} seconds")),
                selected_label: Some(crate::plan_review::LABEL_REVISE.into()),
            },
        }
    }

    /// `cursor/ask_question`: always goes to the user, one question card at a
    /// time; the answers are merged into one reply.
    async fn acp_question(
        &self,
        session: &mut Session,
        run: &mut RunState,
        request: AcpAskQuestionRequest,
    ) -> AcpAskQuestionOutcome {
        if request.questions.is_empty() {
            return AcpAskQuestionOutcome::Skipped(Some("No questions were asked.".into()));
        }
        let total = request.questions.len();
        let title = request
            .title
            .as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty());
        let mut answers: Vec<AcpQuestionAnswer> = Vec::new();
        let mut summary: Vec<String> = Vec::new();

        for (position, question) in request.questions.iter().enumerate() {
            let question_id = uuid::Uuid::new_v4().to_string();
            let mut text = question.prompt.trim().to_string();
            if position == 0 {
                if let Some(title) = title.filter(|title| !text.contains(title)) {
                    text = format!("{title}\n\n{text}");
                }
            }
            if total > 1 {
                text = format!("[{}/{}] {text}", position + 1, total);
            }
            let options: Vec<QuestionOption> = question
                .options
                .iter()
                .map(|option| QuestionOption {
                    id: option.id.clone(),
                    label: option.label.clone(),
                })
                .collect();
            let _ = self
                .event_tx
                .send(AgentEvent::QuestionAsked {
                    session_id: run.session_id.clone(),
                    question: QuestionPayload {
                        question_id: question_id.clone(),
                        text,
                        options,
                        // Cursor can only take option ids; a question
                        // without options is answered in free text.
                        allow_free_text: question.options.is_empty(),
                        allow_multiple: question.allow_multiple,
                    },
                })
                .await;
            self.acp_status(&run.session_id, SessionStatus::WaitingQuestion)
                .await;

            let response = session.wait_question(&question_id).await;
            self.acp_status(&run.session_id, Self::acp_resting_status(run))
                .await;
            if session.is_interrupted() {
                return AcpAskQuestionOutcome::Cancelled;
            }
            let free_text = response
                .free_text
                .as_deref()
                .map(str::trim)
                .filter(|t| !t.is_empty())
                .map(str::to_string);
            if response.cancelled {
                run.annotate(
                    &request.tool_call_id,
                    "ask_question",
                    format!("Question: {} → dismissed", question.prompt.trim()),
                    "skipped",
                    &[],
                );
                return AcpAskQuestionOutcome::Skipped(Some(
                    "The user dismissed the question without answering.".into(),
                ));
            }
            let selected: Vec<String> = response
                .selected_option_ids
                .iter()
                .filter(|id| question.options.iter().any(|option| &option.id == *id))
                .cloned()
                .collect();
            if selected.is_empty() {
                // The protocol cannot carry free text: hand it over as the
                // reason for skipping so the agent still gets the answer.
                let reason = match free_text {
                    Some(text) => format!("The user answered in free text: {text}"),
                    None => "The user did not select an option.".to_string(),
                };
                run.annotate(
                    &request.tool_call_id,
                    "ask_question",
                    format!("Question: {} → {reason}", question.prompt.trim()),
                    "skipped",
                    &[],
                );
                return AcpAskQuestionOutcome::Skipped(Some(reason));
            }
            let selected = if question.allow_multiple {
                selected
            } else {
                selected.into_iter().take(1).collect()
            };
            let labels: Vec<String> = selected
                .iter()
                .map(|id| {
                    question
                        .options
                        .iter()
                        .find(|option| &option.id == id)
                        .map(|option| option.label.clone())
                        .unwrap_or_else(|| id.clone())
                })
                .collect();
            summary.push(format!(
                "{} → {}",
                question.prompt.trim(),
                labels.join(", ")
            ));
            answers.push(AcpQuestionAnswer {
                question_id: question.id.clone(),
                selected_option_ids: selected,
            });
        }

        run.annotate(
            &request.tool_call_id,
            "ask_question",
            format!("Question: {}", summary.join("; ")),
            "answered",
            &[],
        );
        AcpAskQuestionOutcome::Answered(answers)
    }

    /// `cursor/create_plan`: saved as the session's plan document and
    /// reviewed through the `ExitPlanMode` card (auto mode accepts).
    async fn acp_plan(
        &self,
        session: &mut Session,
        run: &mut RunState,
        request: AcpCreatePlanRequest,
    ) -> AcpCreatePlanOutcome {
        let name = request
            .name
            .as_deref()
            .map(str::trim)
            .filter(|n| !n.is_empty())
            .unwrap_or("Plan")
            .to_string();
        let markdown = plan_markdown(&request, &name);
        let path = self.acp_save_plan(session, &markdown).await;
        let path_text = path
            .as_ref()
            .map(|path| path.display().to_string())
            .unwrap_or_default();
        let plan_uri = path.as_deref().and_then(file_uri);
        if path.is_some() {
            let _ = self
                .event_tx
                .send(AgentEvent::PlanFileUpdated {
                    session_id: run.session_id.clone(),
                    path: path_text.clone(),
                    content: markdown.clone(),
                })
                .await;
        }
        let locations: Vec<String> = if path_text.is_empty() {
            Vec::new()
        } else {
            vec![path_text.clone()]
        };
        let note = |status: &str, run: &mut RunState| {
            run.annotate(
                &request.tool_call_id,
                "create_plan",
                format!("Plan: {name}"),
                status,
                &locations,
            );
        };

        if session.get_permission_mode() == PermissionMode::Auto {
            self.acp_leave_plan_mode(session).await;
            note("accepted", run);
            return AcpCreatePlanOutcome::Accepted { plan_uri };
        }

        let display = PlanReviewDisplay {
            plan: markdown,
            path: path_text,
            options: Vec::new(),
        };
        let approval_id = uuid::Uuid::new_v4().to_string();
        let approval = ApprovalRequest {
            approval_id: approval_id.clone(),
            session_id: run.session_id.clone(),
            tool_call_id: request.tool_call_id.clone(),
            tool_name: "ExitPlanMode".into(),
            action: "Ready to build with this plan?".into(),
            tool_input_display: Some(display.to_display_json()),
            created_at: chrono::Utc::now(),
        };
        if let Err(error) = session.set_pending_plan_review(Some(approval.clone())) {
            tracing::warn!("cannot persist the pending plan review: {error:#}");
        }
        let _ = self
            .event_tx
            .send(AgentEvent::ApprovalRequested {
                session_id: run.session_id.clone(),
                request: approval,
            })
            .await;
        self.acp_status(&run.session_id, SessionStatus::WaitingApproval)
            .await;
        let response = self.acp_wait_approval(session, &approval_id).await;
        if let Err(error) = session.set_pending_plan_review(None) {
            tracing::warn!("cannot clear the pending plan review: {error:#}");
        }
        self.acp_status(&run.session_id, Self::acp_resting_status(run))
            .await;

        let (output, exit) = resolve_exit_plan_approval(&response, &display);
        if exit {
            self.acp_leave_plan_mode(session).await;
        }
        match response.decision {
            ApprovalDecision::Approved => {
                note("accepted", run);
                AcpCreatePlanOutcome::Accepted { plan_uri }
            }
            ApprovalDecision::Rejected => {
                note("rejected", run);
                AcpCreatePlanOutcome::Rejected {
                    reason: Some(output.content),
                }
            }
            ApprovalDecision::Cancelled => {
                if session.is_interrupted() {
                    AcpCreatePlanOutcome::Cancelled
                } else {
                    note("dismissed", run);
                    AcpCreatePlanOutcome::Rejected {
                        reason: Some("The user dismissed the plan review.".into()),
                    }
                }
            }
        }
    }

    /// Leave kkagent's plan mode once a plan was accepted (or the review
    /// ended the plan) and tell the clients so their footer follows.
    async fn acp_leave_plan_mode(&self, session: &mut Session) {
        if !session.plan_mode {
            return;
        }
        if let Err(error) = session.set_plan_mode_persisted(false) {
            tracing::warn!("cannot leave plan mode after the plan review: {error:#}");
            return;
        }
        let _ = self
            .event_tx
            .send(AgentEvent::PlanModeChanged {
                session_id: session.id.clone(),
                enabled: false,
            })
            .await;
    }

    /// Write the plan into the session's plan file (best effort) so resume,
    /// the plan viewer and later models can find it.
    async fn acp_save_plan(&self, session: &mut Session, markdown: &str) -> Option<PathBuf> {
        let path = session.plan_file_path.clone();
        if let Some(parent) = path.parent() {
            if let Err(error) = tokio::fs::create_dir_all(parent).await {
                tracing::warn!("cannot create the plan directory: {error}");
                return None;
            }
        }
        if let Err(error) = tokio::fs::write(&path, markdown).await {
            tracing::warn!("cannot write the plan file {}: {error}", path.display());
            return None;
        }
        if let Err(error) = session.finalize_plan_filename(markdown) {
            tracing::debug!("plan file keeps its generated name: {error:#}");
        }
        Some(session.plan_file_path.clone())
    }
}

/// Answer a reverse request that arrives while the turn is being cancelled.
fn cancel_interaction(interaction: AcpInteraction) {
    match interaction {
        AcpInteraction::Permission { reply, .. } => {
            let _ = reply.send(AcpPermissionOutcome::Cancelled);
        }
        AcpInteraction::Question { reply, .. } => {
            let _ = reply.send(AcpAskQuestionOutcome::Cancelled);
        }
        AcpInteraction::Plan { reply, .. } => {
            let _ = reply.send(AcpCreatePlanOutcome::Cancelled);
        }
    }
}

/// Pick the option of `request` that expresses `choice`.
fn select_permission_option(
    request: &AcpPermissionRequest,
    choice: AcpPermissionChoice,
) -> AcpPermissionOutcome {
    if let Some(option) = request.option_for(choice) {
        return AcpPermissionOutcome::Selected(option.option_id.clone());
    }
    // The agent offered no option of that kind: any option of the same
    // polarity expresses the decision.
    let allow = matches!(
        choice,
        AcpPermissionChoice::AllowOnce | AcpPermissionChoice::AllowAlways
    );
    let wanted = if allow { "allow" } else { "reject" };
    request
        .options
        .iter()
        .find(|option| option.kind.to_ascii_lowercase().starts_with(wanted))
        .map(|option| AcpPermissionOutcome::Selected(option.option_id.clone()))
        .unwrap_or(AcpPermissionOutcome::Cancelled)
}

/// User-visible note for a `stopReason` other than a plain end of turn.
fn stop_reason_notice(stop_reason: &str, agent: &str, interrupted: bool) -> Option<String> {
    match stop_reason {
        "max_tokens" => Some(format!("{agent} stopped: it reached its output limit.")),
        "max_turn_requests" => Some(format!(
            "{agent} stopped: it reached the maximum number of model requests for one turn."
        )),
        "refusal" => Some(format!("{agent} refused to continue.")),
        "cancelled" if !interrupted => Some(format!("{agent} cancelled the turn.")),
        _ => None,
    }
}

#[cfg(test)]
mod tests;
