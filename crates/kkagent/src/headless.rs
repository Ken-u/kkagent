//! Headless (`-p`) driver: structured output formats, stream-json input,
//! structured exit codes and session resume for CI pipelines.
//!
//! - `--output-format text` keeps the historical behavior (assistant deltas
//!   on stdout, retries on stderr) plus a final stderr resume hint.
//! - `--output-format json` prints a single result object at the end.
//! - `--output-format stream-json` emits NDJSON events on stdout:
//!   `system`, `session`, `message`, `tool_call`, `tool_result`, `usage`,
//!   `turn_start`/`turn_end`, `approval_*`, `question_*`, `llm_retry`,
//!   `error` and a final `result` line.
//!
//! - `--input-format stream-json` reads NDJSON messages from stdin
//!   (`user`, `steer`, `interrupt`, `quit`, `approval`, `question`,
//!   `question_cancel`) for programmatic multi-turn conversations.
//!   Approvals/questions are answered from stdin; with the default
//!   `text` input format they are auto-denied / auto-cancelled so the
//!   run always terminates instead of waiting for an approval timeout.

use anyhow::{anyhow, Result};
use clap::ValueEnum;
use kkagent_client::KkagentClient;
use kkagent_protocol::{
    AgentEvent, ApprovalDecision, ApprovalResponse, ApprovalScope, Frame, PermissionMode,
    QuestionResponse, TokenUsage,
};
use serde::Deserialize;
use std::time::Instant;
use tokio::io::AsyncBufReadExt;
use tokio::sync::mpsc;

/// Structured exit codes for headless runs (kimi-aligned subset, extended).
pub mod exit_codes {
    /// Turn completed without errors or denied approvals.
    pub const SUCCESS: i32 = 0;
    /// Provider / agent error, or the event stream closed early.
    pub const ERROR: i32 = 1;
    /// Malformed `--input-format stream-json` input line.
    pub const USAGE: i32 = 2;
    /// The `--max-turns` round limit was reached.
    pub const MAX_TURNS: i32 = 3;
    /// At least one tool approval was denied.
    pub const PERMISSION_DENIED: i32 = 4;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum OutputFormat {
    #[default]
    Text,
    Json,
    StreamJson,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, ValueEnum)]
#[value(rename_all = "kebab-case")]
pub enum InputFormat {
    #[default]
    Text,
    StreamJson,
}

/// Which session a headless run should talk to.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum Resume {
    /// Create a fresh session.
    #[default]
    New,
    /// `--continue`: attach to the most recent session.
    Latest,
    /// `--resume <id>`: attach by exact id or unique prefix (server resolves).
    Id(String),
}

#[derive(Debug, Clone)]
pub struct HeadlessOptions {
    pub prompt: String,
    pub permission_mode: PermissionMode,
    pub output_format: OutputFormat,
    pub input_format: InputFormat,
    pub max_turns: Option<u32>,
    pub resume: Resume,
}

/// CLI-facing wrapper: raw `Option` flags resolved into [`HeadlessOptions`].
#[derive(Debug, Clone, Default)]
pub struct PrintModeOptions {
    pub prompt: String,
    pub permission_mode: Option<PermissionMode>,
    pub output_format: Option<OutputFormat>,
    pub input_format: Option<InputFormat>,
    pub max_turns: Option<u32>,
    pub resume: Option<Resume>,
}

impl PrintModeOptions {
    pub fn resolve(self) -> HeadlessOptions {
        let permission_mode = self.permission_mode.unwrap_or(PermissionMode::Manual);
        let resume = self.resume.unwrap_or_default();
        HeadlessOptions {
            prompt: self.prompt,
            permission_mode,
            output_format: self.output_format.unwrap_or_default(),
            input_format: self.input_format.unwrap_or_default(),
            max_turns: self.max_turns,
            resume,
        }
    }
}

/// Messages accepted on stdin with `--input-format stream-json`.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum InputMessage {
    /// Start a new user turn (or steer when a turn is running).
    User { text: String },
    /// Alias of [`InputMessage::User`]; steers a running turn.
    Steer { text: String },
    /// Interrupt the running turn.
    Interrupt,
    /// Finish the run after the current turn completes (or immediately).
    Quit,
    /// Answer a pending [`AgentEvent::ApprovalRequested`].
    Approval {
        decision: ApprovalInputDecision,
        #[serde(default)]
        scope: Option<ApprovalInputScope>,
        #[serde(default)]
        feedback: Option<String>,
    },
    /// Answer a pending [`AgentEvent::QuestionAsked`].
    Question {
        #[serde(default)]
        selected_option_ids: Vec<String>,
        #[serde(default)]
        free_text: Option<String>,
    },
    /// Dismiss a pending question.
    QuestionCancel,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalInputDecision {
    Approve,
    Reject,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApprovalInputScope {
    Once,
    Turn,
    Session,
}

impl From<ApprovalInputDecision> for ApprovalDecision {
    fn from(value: ApprovalInputDecision) -> Self {
        match value {
            ApprovalInputDecision::Approve => ApprovalDecision::Approved,
            ApprovalInputDecision::Reject => ApprovalDecision::Rejected,
        }
    }
}

impl From<ApprovalInputScope> for ApprovalScope {
    fn from(value: ApprovalInputScope) -> Self {
        match value {
            ApprovalInputScope::Once => ApprovalScope::Once,
            ApprovalInputScope::Turn => ApprovalScope::Turn,
            ApprovalInputScope::Session => ApprovalScope::Session,
        }
    }
}

/// Run one headless conversation and return the process exit code.
pub async fn run(client: &mut KkagentClient, opts: HeadlessOptions) -> i32 {
    match Driver::new(client, opts).drive().await {
        Ok(code) => code,
        Err(err) => {
            eprintln!("Error: {err:#}");
            exit_codes::ERROR
        }
    }
}

/// How the driver stopped.
#[derive(Debug)]
enum Outcome {
    /// Turn(s) finished normally; classify via [`Driver::finish`].
    Completed,
    /// `AgentEvent::Error` observed.
    AgentError(String),
    /// Event stream closed before the turn finished.
    StreamClosed,
    /// Malformed stdin input line.
    Usage(String),
}

enum EventOutcome {
    Continue,
    TurnEnded,
    /// Agent-reported error; the run stops.
    Failed(String),
}

enum AfterTurn {
    NextTurn,
    Finish,
}

enum InputFlow {
    Continue,
    Finish,
}

impl PartialEq for InputFlow {
    fn eq(&self, other: &Self) -> bool {
        matches!(
            (self, other),
            (InputFlow::Continue, InputFlow::Continue) | (InputFlow::Finish, InputFlow::Finish)
        )
    }
}

/// An approval/question answer received before its request event arrived.
#[derive(Debug, Clone)]
enum QueuedAnswer {
    Approval {
        decision: ApprovalDecision,
        scope: Option<ApprovalScope>,
        feedback: Option<String>,
    },
    Question {
        selected_option_ids: Vec<String>,
        free_text: Option<String>,
        cancelled: bool,
    },
}

/// Which select branch fired; handlers run outside the select to keep
/// borrows of `self` disjoint from the polled futures.
enum Sel {
    Frame(Option<Frame>),
    Input(Option<std::result::Result<InputMessage, String>>),
}

struct ToolCallRecord {
    tool_call_id: String,
    tool_name: String,
    input: serde_json::Value,
    output: Option<String>,
    is_error: bool,
}

struct Driver<'a> {
    client: &'a mut KkagentClient,
    requester: kkagent_client::KkagentRequester,
    opts: HeadlessOptions,
    session_id: String,
    resumed: bool,
    started: Instant,
    /// Buffered assistant text of the current round, flushed as one
    /// `message` event before tool calls / turn end (stream-json only).
    pending_message: String,
    /// Last flushed assistant message (json `result.message`).
    last_message: String,
    rounds: u32,
    turn_ends: u32,
    turn_active: bool,
    max_turns_exceeded: bool,
    denied_approvals: u32,
    pending_approval: Option<kkagent_protocol::ApprovalRequest>,
    pending_question: Option<kkagent_protocol::QuestionPayload>,
    /// Answers that arrived before their approval/question request event.
    answer_queue: std::collections::VecDeque<QueuedAnswer>,
    tool_calls: Vec<ToolCallRecord>,
    usage_total: TokenUsage,
    /// stdin hit EOF or `quit`; stop after the current turn.
    stdin_done: bool,
}

impl<'a> Driver<'a> {
    fn new(client: &'a mut KkagentClient, opts: HeadlessOptions) -> Self {
        let requester = client.requester();
        Self {
            client,
            requester,
            opts,
            session_id: String::new(),
            resumed: false,
            started: Instant::now(),
            pending_message: String::new(),
            last_message: String::new(),
            rounds: 0,
            turn_ends: 0,
            turn_active: false,
            max_turns_exceeded: false,
            denied_approvals: 0,
            pending_approval: None,
            pending_question: None,
            answer_queue: std::collections::VecDeque::new(),
            tool_calls: Vec::new(),
            usage_total: TokenUsage {
                input_tokens: 0,
                output_tokens: 0,
                cache_creation_input_tokens: 0,
                cache_read_input_tokens: 0,
            },
            stdin_done: false,
        }
    }

    fn stream(&self) -> bool {
        self.opts.output_format == OutputFormat::StreamJson
    }

    fn emit(&self, value: serde_json::Value) {
        if self.stream() {
            println!("{value}");
        }
    }

    async fn drive(&mut self) -> Result<i32> {
        if self.stream() {
            self.emit(serde_json::json!({
                "type": "system",
                "subtype": "version",
                "version": env!("CARGO_PKG_VERSION"),
            }));
        }

        if let Err(err) = self.open_session().await {
            if self.stream() {
                self.emit(serde_json::json!({
                    "type": "error",
                    "message": format!("{err:#}"),
                }));
            }
            eprintln!("Error: {err:#}");
            return Ok(exit_codes::ERROR);
        }

        match self.opts.input_format {
            InputFormat::Text => {
                self.send_user_prompt(&self.opts.prompt.clone()).await?;
                self.run_single_turn().await
            }
            InputFormat::StreamJson => {
                if !self.opts.prompt.is_empty() {
                    self.send_user_prompt(&self.opts.prompt.clone()).await?;
                }
                self.run_stream_input().await
            }
        }
        .map(|outcome| self.finish(outcome))
    }

    async fn open_session(&mut self) -> Result<()> {
        match self.opts.resume.clone() {
            Resume::New => {
                let cwd = std::env::current_dir()?.to_string_lossy().to_string();
                let session_id = self
                    .client
                    .create_session(Some(&cwd), Some(self.opts.permission_mode))
                    .await?;
                self.session_id = session_id;
            }
            Resume::Latest => {
                let sessions = self.client.list_sessions(1).await?;
                let latest = sessions
                    .first()
                    .and_then(|s| s.get("session_id"))
                    .and_then(|v| v.as_str())
                    .ok_or_else(|| anyhow!("no previous session to --continue"))?
                    .to_string();
                self.attach_session(&latest).await?;
            }
            Resume::Id(query) => {
                // The server resolves exact ids and unique prefixes itself.
                self.attach_session(&query).await?;
            }
        }
        self.resumed = !matches!(self.opts.resume, Resume::New);
        self.emit(serde_json::json!({
            "type": "session",
            "subtype": if self.resumed { "resumed" } else { "start" },
            "session_id": self.session_id,
        }));
        Ok(())
    }

    /// Load an existing session server-side so prompts continue its transcript.
    /// Returns the server-resolved session id (prefixes become full ids).
    async fn attach_session(&mut self, session_id: &str) -> Result<()> {
        let cwd = std::env::current_dir()?.to_string_lossy().to_string();
        let params = serde_json::json!({
            "session_id": session_id,
            "workspace": cwd,
            "display_limit": 1,
        });
        let result = self
            .requester
            .rpc_call("session.resume", Some(params))
            .await?;
        self.session_id = result
            .get("session_id")
            .and_then(|v| v.as_str())
            .unwrap_or(session_id)
            .to_string();
        Ok(())
    }

    async fn send_user_prompt(&mut self, text: &str) -> Result<()> {
        if text.trim().is_empty() {
            return Err(anyhow!("empty user message"));
        }
        self.flush_message();
        self.rounds = 0;
        self.max_turns_exceeded = false;
        self.turn_active = true;
        self.client
            .send_prompt(&self.session_id, text)
            .await
            .map_err(|err| anyhow!("failed to send prompt: {err:#}"))
    }

    async fn steer(&mut self, text: &str) -> Result<()> {
        if text.trim().is_empty() {
            return Ok(());
        }
        let images: [(String, String); 0] = [];
        self.client
            .steer(&self.session_id, text, &images)
            .await
            .map_err(|err| anyhow!("failed to steer: {err:#}"))
    }

    async fn interrupt(&mut self) {
        let params = serde_json::json!({"session_id": self.session_id});
        let _ = self
            .requester
            .rpc_call("session.interrupt", Some(params))
            .await;
    }

    /// `--input-format text`: a single turn, no stdin interaction.
    async fn run_single_turn(&mut self) -> Result<Outcome> {
        loop {
            let Some(frame) = self.client.event_rx.recv().await else {
                return Ok(Outcome::StreamClosed);
            };
            let Frame::Event { data, .. } = frame else {
                continue;
            };
            let Ok(evt) = serde_json::from_value::<AgentEvent>(data) else {
                continue;
            };
            match self.on_event(evt).await? {
                EventOutcome::Continue => {}
                EventOutcome::TurnEnded => return Ok(Outcome::Completed),
                EventOutcome::Failed(message) => return Ok(Outcome::AgentError(message)),
            }
        }
    }

    /// `--input-format stream-json`: select over agent events and stdin lines.
    async fn run_stream_input(&mut self) -> Result<Outcome> {
        let (tx, mut rx) = mpsc::channel::<std::result::Result<InputMessage, String>>(64);
        tokio::spawn(read_stdin_lines(tx));

        loop {
            let sel = tokio::select! {
                frame = self.client.event_rx.recv() => Sel::Frame(frame),
                msg = rx.recv(), if !self.stdin_done => Sel::Input(msg),
            };
            match sel {
                Sel::Frame(None) => return Ok(Outcome::StreamClosed),
                Sel::Frame(Some(Frame::Event { data, .. })) => {
                    let Ok(evt) = serde_json::from_value::<AgentEvent>(data) else {
                        continue;
                    };
                    match self.on_event(evt).await? {
                        EventOutcome::Continue => {}
                        EventOutcome::TurnEnded => {
                            if self.stdin_done {
                                return Ok(Outcome::Completed);
                            }
                            match self.after_turn(&mut rx).await? {
                                AfterTurn::NextTurn => continue,
                                AfterTurn::Finish => return Ok(Outcome::Completed),
                            }
                        }
                        EventOutcome::Failed(message) => return Ok(Outcome::AgentError(message)),
                    }
                }
                Sel::Frame(Some(_)) => {}
                Sel::Input(Some(Ok(message))) => {
                    if self.on_input(message).await? == InputFlow::Finish {
                        return Ok(Outcome::Completed);
                    }
                }
                Sel::Input(Some(Err(line))) => {
                    self.interrupt().await;
                    return Ok(Outcome::Usage(line));
                }
                Sel::Input(None) => {
                    self.stdin_done = true;
                    if !self.turn_active {
                        return Ok(Outcome::Completed);
                    }
                    // Let the running turn finish, then stop.
                }
            }
        }
    }

    /// After a turn ends, wait for the next stdin-driven prompt.
    async fn after_turn(
        &mut self,
        rx: &mut mpsc::Receiver<std::result::Result<InputMessage, String>>,
    ) -> Result<AfterTurn> {
        loop {
            match rx.recv().await {
                Some(Ok(msg)) => match msg {
                    InputMessage::User { text } | InputMessage::Steer { text } => {
                        self.send_user_prompt(&text).await?;
                        return Ok(AfterTurn::NextTurn);
                    }
                    InputMessage::Quit => return Ok(AfterTurn::Finish),
                    InputMessage::Interrupt => {
                        eprintln!("kkagent: interrupt ignored between turns");
                    }
                    InputMessage::Approval {
                        decision,
                        scope,
                        feedback,
                    } => {
                        // Keep for the next turn's approval (input may race the event).
                        self.answer_queue.push_back(QueuedAnswer::Approval {
                            decision: decision.into(),
                            scope: scope.map(Into::into),
                            feedback,
                        });
                    }
                    InputMessage::Question {
                        selected_option_ids,
                        free_text,
                    } => {
                        self.answer_queue.push_back(QueuedAnswer::Question {
                            selected_option_ids,
                            free_text,
                            cancelled: false,
                        });
                    }
                    InputMessage::QuestionCancel => {
                        self.answer_queue.push_back(QueuedAnswer::Question {
                            selected_option_ids: Vec::new(),
                            free_text: None,
                            cancelled: true,
                        });
                    }
                },
                Some(Err(line)) => {
                    eprintln!("Error: invalid stream-json input: {line}");
                    return Ok(AfterTurn::Finish);
                }
                None => return Ok(AfterTurn::Finish),
            }
        }
    }

    async fn on_input(&mut self, message: InputMessage) -> Result<InputFlow> {
        match message {
            InputMessage::User { text } | InputMessage::Steer { text } => {
                if self.turn_active {
                    self.steer(&text).await?;
                } else {
                    self.send_user_prompt(&text).await?;
                }
            }
            InputMessage::Interrupt => {
                if self.turn_active {
                    self.interrupt().await;
                } else {
                    eprintln!("kkagent: interrupt ignored between turns");
                }
            }
            InputMessage::Quit => {
                self.stdin_done = true;
                if !self.turn_active {
                    return Ok(InputFlow::Finish);
                }
            }
            InputMessage::Approval {
                decision,
                scope,
                feedback,
            } => {
                self.answer_queue.push_back(QueuedAnswer::Approval {
                    decision: decision.into(),
                    scope: scope.map(Into::into),
                    feedback,
                });
                self.try_answer().await?;
            }
            InputMessage::Question {
                selected_option_ids,
                free_text,
            } => {
                self.answer_queue.push_back(QueuedAnswer::Question {
                    selected_option_ids,
                    free_text,
                    cancelled: false,
                });
                self.try_answer().await?;
            }
            InputMessage::QuestionCancel => {
                self.answer_queue.push_back(QueuedAnswer::Question {
                    selected_option_ids: Vec::new(),
                    free_text: None,
                    cancelled: true,
                });
                self.try_answer().await?;
            }
        }
        Ok(InputFlow::Continue)
    }

    /// Consume queued answers against pending approval/question requests.
    /// Order matters: a queued answer may target the request that just
    /// arrived, so callers invoke this both after input and after events.
    async fn try_answer(&mut self) -> Result<()> {
        while !self.answer_queue.is_empty() {
            let head = self.answer_queue.front().cloned().unwrap();
            match head {
                QueuedAnswer::Approval {
                    decision,
                    scope,
                    feedback,
                } => {
                    if self.pending_approval.is_none() {
                        return Ok(());
                    }
                    self.answer_queue.pop_front();
                    self.answer_pending_approval(decision, scope, feedback)
                        .await?;
                }
                QueuedAnswer::Question {
                    selected_option_ids,
                    free_text,
                    cancelled,
                } => {
                    if self.pending_question.is_none() {
                        return Ok(());
                    }
                    self.answer_queue.pop_front();
                    self.answer_pending_question(selected_option_ids, free_text, cancelled)
                        .await?;
                }
            }
        }
        Ok(())
    }

    async fn on_event(&mut self, evt: AgentEvent) -> Result<EventOutcome> {
        if evt.session_id() != self.session_id {
            return Ok(EventOutcome::Continue);
        }
        match evt {
            AgentEvent::TurnStart { .. } => {
                self.rounds += 1;
                self.turn_active = true;
                self.emit(serde_json::json!({
                    "type": "turn_start",
                    "session_id": self.session_id,
                    "round": self.rounds,
                }));
                if let Some(max) = self.opts.max_turns {
                    if self.rounds > max {
                        self.max_turns_exceeded = true;
                        self.interrupt().await;
                    }
                }
            }
            AgentEvent::MessageDelta { text, .. } => {
                if self.opts.output_format == OutputFormat::Text {
                    print!("{text}");
                } else {
                    self.pending_message.push_str(&text);
                }
            }
            AgentEvent::ToolCall {
                tool_call_id,
                tool_name,
                input,
                ..
            } => {
                self.flush_message();
                self.tool_calls.push(ToolCallRecord {
                    tool_call_id: tool_call_id.clone(),
                    tool_name: tool_name.clone(),
                    input: input.clone(),
                    output: None,
                    is_error: false,
                });
                self.emit(serde_json::json!({
                    "type": "tool_call",
                    "session_id": self.session_id,
                    "tool_call_id": tool_call_id,
                    "tool_name": tool_name,
                    "input": input,
                }));
            }
            AgentEvent::ToolResult {
                tool_call_id,
                tool_name,
                output,
                is_error,
                ..
            } => {
                if let Some(record) = self
                    .tool_calls
                    .iter_mut()
                    .rev()
                    .find(|r| r.tool_call_id == tool_call_id)
                {
                    record.output = Some(output.clone());
                    record.is_error = is_error;
                } else {
                    self.tool_calls.push(ToolCallRecord {
                        tool_call_id: tool_call_id.clone(),
                        tool_name: tool_name.clone(),
                        input: serde_json::Value::Null,
                        output: Some(output.clone()),
                        is_error,
                    });
                }
                self.emit(serde_json::json!({
                    "type": "tool_result",
                    "session_id": self.session_id,
                    "tool_call_id": tool_call_id,
                    "tool_name": tool_name,
                    "output": output,
                    "is_error": is_error,
                }));
            }
            AgentEvent::UsageUpdate { usage, .. } => {
                self.usage_total.input_tokens += usage.input_tokens;
                self.usage_total.output_tokens += usage.output_tokens;
                self.usage_total.cache_creation_input_tokens += usage.cache_creation_input_tokens;
                self.usage_total.cache_read_input_tokens += usage.cache_read_input_tokens;
                self.emit(serde_json::json!({
                    "type": "usage",
                    "session_id": self.session_id,
                    "usage": usage,
                }));
            }
            AgentEvent::ApprovalRequested { request, .. } => {
                if self.opts.input_format == InputFormat::StreamJson {
                    self.pending_approval = Some(request.clone());
                    self.emit(serde_json::json!({
                        "type": "approval_requested",
                        "session_id": self.session_id,
                        "approval_id": request.approval_id,
                        "tool_call_id": request.tool_call_id,
                        "tool_name": request.tool_name,
                        "action": request.action,
                    }));
                    self.try_answer().await?;
                } else {
                    self.deny_approval(&request.approval_id).await?;
                }
            }
            AgentEvent::QuestionAsked { question, .. } => {
                if self.opts.input_format == InputFormat::StreamJson {
                    self.pending_question = Some(question.clone());
                    self.emit(serde_json::json!({
                        "type": "question_asked",
                        "session_id": self.session_id,
                        "question_id": question.question_id,
                        "text": question.text,
                        "options": question.options,
                        "allow_free_text": question.allow_free_text,
                        "allow_multiple": question.allow_multiple,
                    }));
                    self.try_answer().await?;
                } else {
                    self.cancel_question(&question.question_id).await?;
                }
            }
            AgentEvent::Error { message, .. } => {
                if self.max_turns_exceeded {
                    // Expected consequence of our own interrupt; wait for TurnEnd.
                    return Ok(EventOutcome::Continue);
                }
                if self.opts.output_format == OutputFormat::Text {
                    eprintln!("Error: Agent turn failed: {message}");
                } else {
                    self.emit(serde_json::json!({
                        "type": "error",
                        "session_id": self.session_id,
                        "message": message,
                    }));
                }
                return Ok(EventOutcome::Failed(message));
            }
            AgentEvent::LlmRetry {
                retry_number,
                reason,
                remaining_seconds,
                ..
            } => match self.opts.output_format {
                OutputFormat::Text => {
                    let when = if remaining_seconds == 0 {
                        "now".to_string()
                    } else {
                        format!("in {remaining_seconds}s")
                    };
                    eprint!("\rLLM retry #{retry_number} {when}: {reason}");
                    let _ = std::io::Write::flush(&mut std::io::stderr());
                    if remaining_seconds == 0 {
                        eprintln!();
                    }
                }
                OutputFormat::StreamJson => {
                    self.emit(serde_json::json!({
                        "type": "llm_retry",
                        "session_id": self.session_id,
                        "retry_number": retry_number,
                        "wait_seconds": remaining_seconds,
                        "reason": reason,
                    }));
                }
                OutputFormat::Json => {}
            },
            AgentEvent::TurnEnd { .. } => {
                self.turn_active = false;
                self.turn_ends += 1;
                if self.opts.output_format == OutputFormat::Text {
                    println!();
                } else {
                    self.flush_message();
                    self.emit(serde_json::json!({
                        "type": "turn_end",
                        "session_id": self.session_id,
                    }));
                }
                return Ok(EventOutcome::TurnEnded);
            }
            _ => {}
        }
        Ok(EventOutcome::Continue)
    }

    /// Flush buffered assistant text as one `message` event (stream-json).
    fn flush_message(&mut self) {
        if self.pending_message.is_empty() {
            return;
        }
        let text = std::mem::take(&mut self.pending_message);
        self.last_message = text.clone();
        self.emit(serde_json::json!({
            "type": "message",
            "session_id": self.session_id,
            "role": "assistant",
            "content": text,
        }));
    }

    async fn answer_pending_approval(
        &mut self,
        decision: ApprovalDecision,
        scope: Option<ApprovalScope>,
        feedback: Option<String>,
    ) -> Result<()> {
        let Some(request) = self.pending_approval.take() else {
            eprintln!("kkagent: approval ignored (no pending approval)");
            return Ok(());
        };
        if matches!(decision, ApprovalDecision::Rejected) {
            self.denied_approvals += 1;
        }
        self.respond_approval(
            &request.approval_id,
            decision,
            scope.unwrap_or(ApprovalScope::Once),
            feedback,
        )
        .await
    }

    async fn deny_approval(&mut self, approval_id: &str) -> Result<()> {
        self.denied_approvals += 1;
        self.respond_approval(
            approval_id,
            ApprovalDecision::Rejected,
            ApprovalScope::Once,
            None,
        )
        .await
    }

    async fn respond_approval(
        &mut self,
        approval_id: &str,
        decision: ApprovalDecision,
        scope: ApprovalScope,
        feedback: Option<String>,
    ) -> Result<()> {
        let response = ApprovalResponse {
            approval_id: approval_id.to_string(),
            decision,
            scope: Some(scope),
            feedback,
            selected_label: None,
        };
        self.client
            .respond_approval(&self.session_id, response)
            .await?;
        self.emit(serde_json::json!({
            "type": "approval_result",
            "session_id": self.session_id,
            "approval_id": approval_id,
            "decision": decision,
        }));
        Ok(())
    }

    async fn answer_pending_question(
        &mut self,
        selected_option_ids: Vec<String>,
        free_text: Option<String>,
        cancelled: bool,
    ) -> Result<()> {
        let Some(question) = self.pending_question.take() else {
            eprintln!("kkagent: question ignored (no pending question)");
            return Ok(());
        };
        self.respond_question(
            &question.question_id,
            selected_option_ids,
            free_text,
            cancelled,
        )
        .await
    }

    async fn cancel_question(&mut self, question_id: &str) -> Result<()> {
        self.respond_question(question_id, Vec::new(), None, true)
            .await
    }

    async fn respond_question(
        &mut self,
        question_id: &str,
        selected_option_ids: Vec<String>,
        free_text: Option<String>,
        cancelled: bool,
    ) -> Result<()> {
        let response = QuestionResponse {
            question_id: question_id.to_string(),
            selected_option_ids,
            free_text,
            cancelled,
        };
        self.client
            .respond_question(&self.session_id, response)
            .await?;
        self.emit(serde_json::json!({
            "type": "question_result",
            "session_id": self.session_id,
            "question_id": question_id,
            "cancelled": cancelled,
        }));
        Ok(())
    }

    fn finish(&mut self, outcome: Outcome) -> i32 {
        match outcome {
            Outcome::Completed => {
                let subtype = if self.max_turns_exceeded {
                    "max_turns"
                } else if self.denied_approvals > 0 {
                    "permission_denied"
                } else {
                    "success"
                };
                let code = if self.max_turns_exceeded {
                    exit_codes::MAX_TURNS
                } else if self.denied_approvals > 0 {
                    exit_codes::PERMISSION_DENIED
                } else {
                    exit_codes::SUCCESS
                };
                self.print_result(subtype, code, None);
                code
            }
            Outcome::AgentError(message) => {
                let code = exit_codes::ERROR;
                self.print_result("error", code, Some(message));
                code
            }
            Outcome::StreamClosed => {
                let code = exit_codes::ERROR;
                self.print_result(
                    "error",
                    code,
                    Some("Agent event stream closed before the turn completed".into()),
                );
                code
            }
            Outcome::Usage(line) => {
                let code = exit_codes::USAGE;
                eprintln!("Error: invalid stream-json input: {line}");
                self.print_result("usage_error", code, Some(line));
                code
            }
        }
    }

    fn print_result(&self, subtype: &str, code: i32, error: Option<String>) {
        let usage = serde_json::json!({
            "input_tokens": self.usage_total.input_tokens,
            "output_tokens": self.usage_total.output_tokens,
            "cache_creation_input_tokens": self.usage_total.cache_creation_input_tokens,
            "cache_read_input_tokens": self.usage_total.cache_read_input_tokens,
        });
        match self.opts.output_format {
            OutputFormat::Text => {
                if !self.session_id.is_empty() {
                    eprintln!(
                        "kkagent: session {} (resume with --resume {})",
                        self.session_id, self.session_id
                    );
                }
            }
            OutputFormat::Json => {
                let tool_calls: Vec<serde_json::Value> = self
                    .tool_calls
                    .iter()
                    .map(|r| {
                        serde_json::json!({
                            "tool_call_id": r.tool_call_id,
                            "tool_name": r.tool_name,
                            "input": r.input,
                            "output": r.output,
                            "is_error": r.is_error,
                        })
                    })
                    .collect();
                let value = serde_json::json!({
                    "type": "result",
                    "subtype": subtype,
                    "exit_code": code,
                    "session_id": self.session_id,
                    "resumed": self.resumed,
                    "duration_ms": self.started.elapsed().as_millis() as u64,
                    "rounds": self.rounds,
                    "turns": self.turn_ends,
                    "usage": usage,
                    "message": self.last_message,
                    "denied_approvals": self.denied_approvals,
                    "tool_calls": tool_calls,
                    "error": error,
                });
                println!(
                    "{}",
                    serde_json::to_string_pretty(&value).unwrap_or_default()
                );
            }
            OutputFormat::StreamJson => {
                let value = serde_json::json!({
                    "type": "result",
                    "subtype": subtype,
                    "exit_code": code,
                    "session_id": self.session_id,
                    "resumed": self.resumed,
                    "duration_ms": self.started.elapsed().as_millis() as u64,
                    "rounds": self.rounds,
                    "turns": self.turn_ends,
                    "usage": usage,
                    "message": self.last_message,
                    "denied_approvals": self.denied_approvals,
                    "error": error,
                });
                println!("{value}");
            }
        }
    }
}

/// Read NDJSON `InputMessage` lines from stdin, forwarding parse results.
/// Blank lines are skipped. EOF closes the channel.
async fn read_stdin_lines(tx: mpsc::Sender<std::result::Result<InputMessage, String>>) {
    let stdin = tokio::io::stdin();
    let mut reader = tokio::io::BufReader::new(stdin);
    let mut line = String::new();
    loop {
        line.clear();
        match reader.read_line(&mut line).await {
            Ok(0) => break,
            Ok(_) => {
                let trimmed = line.trim();
                if trimmed.is_empty() {
                    continue;
                }
                let parsed = serde_json::from_str::<InputMessage>(trimmed).map_err(|err| {
                    format!("{}: {err}", trimmed.chars().take(200).collect::<String>())
                });
                if tx.send(parsed).await.is_err() {
                    break;
                }
            }
            Err(err) => {
                let _ = tx.send(Err(format!("stdin read error: {err}"))).await;
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use clap::ValueEnum;

    #[test]
    fn parses_user_and_steer_lines() {
        let user: InputMessage = serde_json::from_str(r#"{"type":"user","text":"hi"}"#).unwrap();
        assert_eq!(user, InputMessage::User { text: "hi".into() });
        let steer: InputMessage = serde_json::from_str(r#"{"type":"steer","text":"go"}"#).unwrap();
        assert_eq!(steer, InputMessage::Steer { text: "go".into() });
    }

    #[test]
    fn parses_approval_and_question_lines() {
        let approval: InputMessage =
            serde_json::from_str(r#"{"type":"approval","decision":"approve","scope":"session"}"#)
                .unwrap();
        match approval {
            InputMessage::Approval {
                decision,
                scope,
                feedback,
            } => {
                assert_eq!(decision, ApprovalInputDecision::Approve);
                assert_eq!(scope, Some(ApprovalInputScope::Session));
                assert!(feedback.is_none());
            }
            other => panic!("unexpected {other:?}"),
        }
        let question: InputMessage = serde_json::from_str(
            r#"{"type":"question","selected_option_ids":["a","b"],"free_text":"x"}"#,
        )
        .unwrap();
        match question {
            InputMessage::Question {
                selected_option_ids,
                free_text,
            } => {
                assert_eq!(selected_option_ids, vec!["a".to_string(), "b".to_string()]);
                assert_eq!(free_text.as_deref(), Some("x"));
            }
            other => panic!("unexpected {other:?}"),
        }
        let cancel: InputMessage = serde_json::from_str(r#"{"type":"question_cancel"}"#).unwrap();
        assert_eq!(cancel, InputMessage::QuestionCancel);
    }

    #[test]
    fn rejects_unknown_types_and_missing_text() {
        assert!(serde_json::from_str::<InputMessage>(r#"{"type":"bogus"}"#).is_err());
        assert!(serde_json::from_str::<InputMessage>(r#"{"type":"user"}"#).is_err());
        assert!(serde_json::from_str::<InputMessage>("not json").is_err());
    }

    #[test]
    fn parses_output_and_input_format_values() {
        let cases = [
            (OutputFormat::Text, "text"),
            (OutputFormat::Json, "json"),
            (OutputFormat::StreamJson, "stream-json"),
        ];
        for (value, name) in cases {
            assert_eq!(
                value.to_possible_value().map(|v| v.get_name().to_string()),
                Some(name.to_string())
            );
        }
        let cases = [
            (InputFormat::Text, "text"),
            (InputFormat::StreamJson, "stream-json"),
        ];
        for (value, name) in cases {
            assert_eq!(
                value.to_possible_value().map(|v| v.get_name().to_string()),
                Some(name.to_string())
            );
        }
    }

    #[test]
    fn input_decision_and_scope_map_to_protocol() {
        assert_eq!(
            ApprovalDecision::from(ApprovalInputDecision::Approve),
            ApprovalDecision::Approved
        );
        assert_eq!(
            ApprovalDecision::from(ApprovalInputDecision::Reject),
            ApprovalDecision::Rejected
        );
        assert_eq!(
            ApprovalScope::from(ApprovalInputScope::Once),
            ApprovalScope::Once
        );
        assert_eq!(
            ApprovalScope::from(ApprovalInputScope::Turn),
            ApprovalScope::Turn
        );
        assert_eq!(
            ApprovalScope::from(ApprovalInputScope::Session),
            ApprovalScope::Session
        );
    }

    #[test]
    fn defaults_are_text() {
        assert_eq!(OutputFormat::default(), OutputFormat::Text);
        assert_eq!(InputFormat::default(), InputFormat::Text);
        assert_eq!(Resume::default(), Resume::New);
    }
}
