use kkagent_config::AppConfig;
use kkagent_llm::{
    create_provider, ChatContent, ChatMessage, LlmRequest, StreamEvent, ThinkingParams, ToolDef,
};
use kkagent_protocol::goal::GoalManager;
use kkagent_protocol::{AgentEvent, SessionStatus};
use kkagent_tools::{infer_accesses, ToolContext, ToolOutput, ToolRegistry};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex};
use tokio::task::{AbortHandle, JoinHandle};

use crate::context_projector::{fold_old_media, project_owned, project_strict, ProjectOptions};
use crate::file_conflict::FileConflictTracker;
use crate::full_compaction::{
    compact_full, compact_full_async, observe_context_overflow, CompactionPolicy,
    CompactionStrategy, MAX_OVERFLOW_COMPACTION_ATTEMPTS,
};
use crate::model_capability::ModelCapability;
use crate::permission::{PermissionChain, PermissionDecision};
use crate::plan_review::{
    format_auto_approved_plan, resolve_exit_plan_approval, PlanReviewDisplay,
};
use crate::session::Session;
use crate::token_counting::TokenCountingStrategy;
use crate::tool_results::{TOOL_RESULT_MAX_CHARS, TOOL_RESULT_PREVIEW_CHARS};
use crate::tool_scheduler::{box_start, ToolCallTask, ToolScheduler};
use crate::transcript::TranscriptDb;
use std::path::PathBuf;
use std::sync::OnceLock;

fn global_file_tracker() -> &'static FileConflictTracker {
    static TRACKER: OnceLock<FileConflictTracker> = OnceLock::new();
    TRACKER.get_or_init(FileConflictTracker::new)
}

pub struct AgentLoop {
    config: Arc<AppConfig>,
    tools: Arc<ToolRegistry>,
    permission: Arc<Mutex<PermissionChain>>,
    event_tx: mpsc::Sender<AgentEvent>,
    abort_registry: Arc<Mutex<HashMap<String, AbortHandle>>>,
    /// Max LLM rounds per top-level run_turn (tool recursion counts); 0 is unlimited.
    max_rounds: u32,
    hooks: Option<Arc<kkagent_mcp::HookManager>>,
    goal_mgr: Option<Arc<GoalManager>>,
    /// Where oversized tool results are spilled. `None` keeps results inline
    /// (truncated with a notice) — used by subagent loops that share the
    /// parent's store instead of owning one.
    tool_result_store: Option<Arc<ToolResultStore>>,
    /// When attached, every complete message pushed during a turn is appended
    /// to the transcript DB at once (never per stream chunk). Cuts the crash
    /// window from a whole turn down to the currently streaming/executing step.
    transcript_db: Option<TranscriptDb>,
}

/// Spills oversized tool results to `<config_dir>/tool-results/` and, when a
/// transcript DB is attached, records the mapping for trash archival.
pub struct ToolResultStore {
    config_dir: PathBuf,
    db: Option<TranscriptDb>,
    /// When set (subagents), files are bucketed under this parent session id
    /// and no DB rows are written — the parent's trash sweep covers the files.
    bucket_override: Option<String>,
}

pub struct RecordedToolResult {
    pub file_path: PathBuf,
}

impl ToolResultStore {
    pub fn new(config_dir: PathBuf, db: Option<TranscriptDb>) -> Self {
        Self {
            config_dir,
            db,
            bucket_override: None,
        }
    }

    /// A store shared by a subagent run: oversized outputs spill into the
    /// parent session's bucket (no DB rows; `crate::trash` sweeps the whole
    /// directory when the parent session is deleted).
    pub fn for_subagent(config_dir: PathBuf, parent_session_id: String) -> Self {
        Self {
            config_dir,
            db: None,
            bucket_override: Some(parent_session_id),
        }
    }

    fn persist(
        &self,
        session: &Session,
        tool_name: &str,
        tool_call_id: &str,
        content: &str,
    ) -> Result<RecordedToolResult, String> {
        let bucket = self.bucket_override.as_deref().unwrap_or(&session.id);
        let persisted = crate::tool_results::persist(
            &self.config_dir,
            bucket,
            tool_name,
            tool_call_id,
            content,
        )?;
        if let (Some(db), None) = (&self.db, &self.bucket_override) {
            let record = crate::transcript::ToolResultRecord {
                id: uuid::Uuid::new_v4().to_string(),
                session_id: session.id.clone(),
                turn_id: None,
                tool_call_id: tool_call_id.to_string(),
                tool_name: tool_name.to_string(),
                file_path: persisted.path.display().to_string(),
                output_size_chars: persisted.output_size_chars,
                output_size_bytes: persisted.output_size_bytes,
                created_at: chrono::Utc::now().timestamp(),
            };
            if let Err(error) = db.record_tool_result(&record) {
                tracing::warn!("failed to record tool result in DB: {error}");
            }
        }
        Ok(RecordedToolResult {
            file_path: persisted.path,
        })
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TurnStep {
    Continue,
    Done,
}

const TURN_HEARTBEAT_INTERVAL: Duration = Duration::from_secs(15);

struct TurnHeartbeat {
    task: JoinHandle<()>,
}

impl TurnHeartbeat {
    fn spawn(event_tx: mpsc::Sender<AgentEvent>, session_id: String) -> Self {
        Self::spawn_with_interval(event_tx, session_id, TURN_HEARTBEAT_INTERVAL)
    }

    fn spawn_with_interval(
        event_tx: mpsc::Sender<AgentEvent>,
        session_id: String,
        interval: Duration,
    ) -> Self {
        let task = tokio::spawn(async move {
            loop {
                tokio::time::sleep(interval).await;
                if event_tx
                    .send(AgentEvent::Heartbeat {
                        session_id: session_id.clone(),
                    })
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
        Self { task }
    }
}

impl Drop for TurnHeartbeat {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl AgentLoop {
    pub fn new(
        config: Arc<AppConfig>,
        tools: Arc<ToolRegistry>,
        permission: Arc<Mutex<PermissionChain>>,
        event_tx: mpsc::Sender<AgentEvent>,
        abort_registry: Arc<Mutex<HashMap<String, AbortHandle>>>,
    ) -> Self {
        let max = config
            .loop_control
            .as_ref()
            .map(|l| l.max_steps_per_turn)
            .unwrap_or(0);
        Self::with_max_rounds(config, tools, permission, event_tx, abort_registry, max)
    }

    pub fn with_hooks(mut self, hooks: Arc<kkagent_mcp::HookManager>) -> Self {
        self.hooks = Some(hooks);
        self
    }

    pub fn with_goal_manager(mut self, goal_mgr: Arc<GoalManager>) -> Self {
        self.goal_mgr = Some(goal_mgr);
        self
    }

    async fn emit_goal_updated(&self, session: &Session, change: &str) {
        let Some(goal_mgr) = &self.goal_mgr else {
            return;
        };
        let (goal_json, budget_json) = match goal_mgr.snapshot_with_budget().await {
            Some((goal, budget)) => (
                Some(serde_json::to_value(&goal).unwrap_or(serde_json::Value::Null)),
                Some(serde_json::to_value(&budget).unwrap_or(serde_json::Value::Null)),
            ),
            None => (None, None),
        };
        let _ = self
            .event_tx
            .send(AgentEvent::GoalUpdated {
                session_id: session.id.clone(),
                goal: goal_json,
                budget: budget_json,
                change: change.to_string(),
            })
            .await;
    }

    /// Attach the oversized-tool-result store (and optional transcript DB).
    pub fn with_tool_result_store(mut self, store: Arc<ToolResultStore>) -> Self {
        self.tool_result_store = Some(store);
        self
    }

    /// Attach a transcript DB for per-message persistence within a turn.
    /// Without it, messages only reach disk when the turn ends (old behavior).
    pub fn with_transcript_db(mut self, db: TranscriptDb) -> Self {
        self.transcript_db = Some(db);
        self
    }

    /// Persist any not-yet-written messages at a complete-message boundary.
    /// Failures are logged but never abort the turn — same policy as the
    /// turn-end persistence path.
    fn persist_step(&self, session: &mut Session) {
        let Some(db) = &self.transcript_db else {
            return;
        };
        if let Err(error) = crate::transcript::persist_session_delta(db, session) {
            tracing::error!(
                session = %session.id,
                error = %error,
                "per-message transcript persist failed (non-fatal)"
            );
        }
    }

    pub fn with_max_rounds(
        config: Arc<AppConfig>,
        tools: Arc<ToolRegistry>,
        permission: Arc<Mutex<PermissionChain>>,
        event_tx: mpsc::Sender<AgentEvent>,
        abort_registry: Arc<Mutex<HashMap<String, AbortHandle>>>,
        max_rounds: u32,
    ) -> Self {
        Self {
            config,
            tools,
            permission,
            event_tx,
            abort_registry,
            max_rounds,
            hooks: None,
            goal_mgr: None,
            tool_result_store: None,
            transcript_db: None,
        }
    }

    pub fn run_turn<'a>(
        &'a self,
        session: &'a mut Session,
    ) -> futures::future::BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            session.steer_mailbox.start_turn();
            let _heartbeat = TurnHeartbeat::spawn(self.event_tx.clone(), session.id.clone());
            let mut completed_rounds = 0_u32;
            let mut goal_continuations = 0_u32;
            const MAX_GOAL_CONTINUATIONS: u32 = 64;
            loop {
                match self.run_turn_step(session).await? {
                    TurnStep::Done => {
                        self.sync_requested_plan_mode(session).await?;
                        if let Some(goal_mgr) = &self.goal_mgr {
                            if goal_mgr.should_continue().await
                                && goal_continuations < MAX_GOAL_CONTINUATIONS
                                && !session.is_interrupted()
                            {
                                goal_continuations = goal_continuations.saturating_add(1);
                                tracing::info!(
                                    "Goal continuation {} for session {}",
                                    goal_continuations,
                                    session.id
                                );
                                session.add_user_message(format!(
                                    "<system-reminder>\n{}\n</system-reminder>",
                                    kkagent_protocol::goal::GOAL_CONTINUATION_PROMPT
                                ));
                                completed_rounds = 0;
                                continue;
                            }
                        }
                        return Ok(());
                    }
                    TurnStep::Continue
                        if self.max_rounds > 0
                            && completed_rounds.saturating_add(1) >= self.max_rounds =>
                    {
                        tracing::warn!("Agent turn limit reached for session {}", session.id);
                        self.finish_turn(session, true).await?;
                        self.sync_requested_plan_mode(session).await?;
                        // Step-cap: still allow goal continuation with a fresh turn budget.
                        if let Some(goal_mgr) = &self.goal_mgr {
                            if goal_mgr.should_continue().await
                                && goal_continuations < MAX_GOAL_CONTINUATIONS
                                && !session.is_interrupted()
                            {
                                goal_continuations = goal_continuations.saturating_add(1);
                                session.add_user_message(format!(
                                    "<system-reminder>\nThe previous goal turn reached the per-turn step limit. \
{}\n</system-reminder>",
                                    kkagent_protocol::goal::GOAL_CONTINUATION_PROMPT
                                ));
                                completed_rounds = 0;
                                continue;
                            }
                        }
                        return Ok(());
                    }
                    TurnStep::Continue => {
                        completed_rounds = completed_rounds.saturating_add(1);
                        if self.max_rounds > 0 {
                            tracing::info!(
                                "Continuing turn ({} rounds left)",
                                self.max_rounds.saturating_sub(completed_rounds)
                            );
                        } else {
                            tracing::info!("Continuing turn (no step limit)");
                        }
                    }
                }
            }
        })
    }

    async fn run_turn_step(&self, session: &mut Session) -> anyhow::Result<TurnStep> {
        session.image_config = self.config.image.clone();
        let session_id = session.id.clone();
        self.sync_requested_plan_mode(session).await?;
        tracing::info!("Starting turn for session {}", session_id);

        if session.is_interrupted() {
            self.finish_interrupted(session).await?;
            return Ok(TurnStep::Done);
        }

        // A new step clears the overflow→compact loop guard once we get past it.
        session.consecutive_overflow_compacts = 0;

        let _ = self
            .event_tx
            .send(AgentEvent::TurnStart {
                session_id: session_id.clone(),
            })
            .await;

        let _ = self
            .event_tx
            .send(AgentEvent::StatusUpdate {
                session_id: session_id.clone(),
                status: SessionStatus::Thinking,
            })
            .await;

        // Keep permission chain in sync with session mode (/yolo /auto /permission)
        {
            let perm = self.permission.lock().await;
            perm.set_mode(session.get_permission_mode());
        }

        let model_alias = {
            let alias = session.get_model_alias();
            if alias.is_empty() {
                self.config
                    .default_model_alias()
                    .unwrap_or("default")
                    .to_string()
            } else {
                alias
            }
        };
        let (primary_model_config, _) = self
            .config
            .resolve_model(&model_alias)
            .ok_or_else(|| anyhow::anyhow!("Model '{}' not found", model_alias))?;
        tracing::info!(
            "Using model alias={} id={}",
            model_alias,
            primary_model_config.model
        );
        let capability = ModelCapability::from_model(primary_model_config);

        // Sync token counting strategy from config.
        if let Some(lc) = &self.config.loop_control {
            session.token_counter.strategy = TokenCountingStrategy::parse(&lc.token_counting);
        }

        // Goal budget gate — stop the turn if exhausted.
        if let Some(goal_mgr) = &self.goal_mgr {
            if let Some(goal) = goal_mgr.get_goal().await {
                if goal.status == kkagent_protocol::goal::GoalStatus::Active
                    && goal.is_budget_exhausted()
                {
                    goal_mgr
                        .block_goal("Blocked after goal budget reached")
                        .await;
                    session.add_user_message(format!(
                        "<system-reminder>\n{}\n</system-reminder>",
                        kkagent_protocol::goal::GOAL_BUDGET_STOP_REMINDER
                    ));
                    let _ = self
                        .event_tx
                        .send(AgentEvent::Error {
                            session_id: session_id.clone(),
                            message: "Goal budget exhausted".into(),
                        })
                        .await;
                    self.emit_goal_updated(session, "budget_blocked").await;
                    self.finish_turn(session, false).await?;
                    return Ok(TurnStep::Done);
                }
                if goal.status == kkagent_protocol::goal::GoalStatus::Active
                    && !session_has_goal_reminder(session)
                {
                    session.add_user_message(goal.active_reminder());
                }
            }
        }

        session.usage.begin_turn();
        session.services.mark_turn_started();
        session.begin_turn();

        let mut tool_defs: Vec<ToolDef> = self
            .tools
            .tool_definitions()
            .iter()
            .filter(|td| {
                tool_allowed(session, &td.name) && (td.name != "ReadMediaFile" || capability.vision)
            })
            .map(|td| ToolDef {
                name: td.name.clone(),
                description: td.description.clone(),
                input_schema: td.parameters.clone(),
            })
            .collect();
        if !capability.tools {
            tool_defs.clear();
        }
        tracing::debug!("Sending {} tools to LLM", tool_defs.len());

        // Todo reminder injection
        session.turns_since_todo = session.turns_since_todo.saturating_add(1);
        if session.turns_since_todo >= 8 {
            session.add_user_message(
                "<system-reminder>\nThe TodoList tool has not been updated recently. \
If you are working on multi-step tasks, consider updating TodoList. \
Do not mention this reminder to the user.\n</system-reminder>"
                    .into(),
            );
            session.turns_since_todo = 0;
        }

        if let Some(hooks) = &self.hooks {
            let _ = hooks
                .fire(
                    kkagent_mcp::hooks::HookEvent::TurnStart,
                    &serde_json::json!({
                        "session_id": session_id,
                        "workspace": session.working_dir,
                    }),
                )
                .await;
        }

        if !capability.vision {
            let latest_has_image = session.messages.last().is_some_and(|message| {
                message
                    .content
                    .iter()
                    .any(|content| matches!(content, ChatContent::Image { .. }))
            });
            if latest_has_image {
                anyhow::bail!(
                    "current model does not declare image input support; add `image_in` to its capabilities or select a vision model"
                );
            }
            if fold_old_media(&mut session.messages, 1) > 0 {
                session.transcript_rewrite_required = true;
            }
        }

        let system_prompt = session.effective_system_prompt();
        // Early-trigger / blocking auto-compact (LLM summary + user retention).
        self.ensure_context_budget(session, &tool_defs, &system_prompt, false)
            .await?;
        let steers = session.drain_steers_into_messages()?;
        if steers > 0 {
            tracing::info!("Injected {steers} steer message(s) before the next model step");
        }
        let mut messages = self.prepare_messages(session, &tool_defs, &system_prompt);
        tracing::debug!("Conversation has {} messages (projected)", messages.len());

        let max_attempts = self
            .config
            .loop_control
            .as_ref()
            .map(|l| l.max_attempts_per_step)
            .unwrap_or(3)
            .max(1);
        let rate_limit_retry_base = Duration::from_secs(
            self.config
                .loop_control
                .as_ref()
                .map(|control| control.rate_limit_retry_base_seconds)
                .unwrap_or(5),
        );

        let mut assistant_text = String::new();
        let mut thinking_text = String::new();
        let mut tool_calls: Vec<PendingToolCall> = Vec::new();
        let mut interrupted = false;
        let mut terminal_stream_error: Option<String> = None;
        let mut rejected_tool_call_recovery: Option<ToolCallRollback> = None;
        let follows_tool_result = request_follows_tool_result(&messages);
        let mut active_model_alias = model_alias.clone();
        let fallback_model_alias = session.resolve_fallback_model(&self.config);
        let mut using_fallback = false;
        let mut visible_empty_retry_limit = primary_model_config.experimental_visible_empty_retries;
        let mut visible_empty_retries = 0_u32;
        let mut bad_toolcall_retry_limit =
            primary_model_config.experimental_bad_toolcall_auto_retries;
        let mut bad_toolcall_retries = 0_u32;
        let mut failure_retries = 0_u32;
        let mut retry_notice_count = 0_u32;

        loop {
            assistant_text.clear();
            thinking_text.clear();
            tool_calls.clear();
            let mut stream_failed = false;
            let mut last_stream_error: Option<String> = None;
            let mut server_retry_after: Option<Duration> = None;
            let mut rate_limited = false;
            let mut stop_reason: Option<String> = None;

            let (model_config, provider_config) = self
                .config
                .resolve_model(&active_model_alias)
                .ok_or_else(|| anyhow::anyhow!("Model '{}' not found", active_model_alias))?;
            let active_capability = ModelCapability::from_model(model_config);
            if using_fallback
                && !active_capability.vision
                && messages.iter().any(|message| {
                    message
                        .content
                        .iter()
                        .any(|content| matches!(content, ChatContent::Image { .. }))
                })
            {
                terminal_stream_error = Some(format!(
                    "Fallback model '{active_model_alias}' does not declare image input support"
                ));
                break;
            }
            let thinking = self.config.thinking.as_ref().and_then(|thinking_config| {
                if thinking_config.enabled || active_capability.thinking {
                    Some(ThinkingParams {
                        budget_tokens: 10000,
                        adaptive: model_config.experimental_adaptive_thinking,
                        effort: model_config.experimental_adaptive_thinking.then(|| {
                            thinking_config
                                .effort
                                .clone()
                                .or_else(|| model_config.default_effort.clone())
                                .unwrap_or_else(|| "high".into())
                        }),
                    })
                } else {
                    None
                }
            });

            let request = LlmRequest {
                model: model_config.model.clone(),
                messages: messages.clone(),
                tools: if active_capability.tools {
                    tool_defs.clone()
                } else {
                    Vec::new()
                },
                max_tokens: model_config.max_output_size.map(|v| v as u32),
                system: Some(system_prompt.clone()),
                thinking: thinking.clone(),
                first_token_timeout: kkagent_config::resolve_first_token_timeout(
                    model_config,
                    provider_config,
                ),
            };

            let (stream_tx, mut stream_rx) = mpsc::channel::<StreamEvent>(256);
            let provider = create_provider(provider_config, model_config)?;
            let stream_error_tx = stream_tx.clone();
            let handle = tokio::spawn(async move {
                if let Err(e) = provider.stream_chat(request, stream_tx).await {
                    tracing::error!("LLM stream error: {}", e);
                    let _ = stream_error_tx
                        .send(kkagent_llm::stream_error_event(&e))
                        .await;
                }
            });
            self.abort_registry
                .lock()
                .await
                .insert(session_id.clone(), handle.abort_handle());

            let mut active_tools: HashMap<String, (PendingToolCall, String)> = HashMap::new();
            let mut got_message_end = false;

            loop {
                if session.is_interrupted() {
                    interrupted = true;
                    if let Some(h) = self.abort_registry.lock().await.remove(&session_id) {
                        h.abort();
                    }
                    break;
                }

                match tokio::time::timeout(Duration::from_millis(100), stream_rx.recv()).await {
                    Ok(Some(event)) => match event {
                        StreamEvent::TextDelta(text) => {
                            assistant_text.push_str(&text);
                            let _ = self
                                .event_tx
                                .send(AgentEvent::MessageDelta {
                                    session_id: session_id.clone(),
                                    text,
                                })
                                .await;
                        }
                        StreamEvent::ThinkingDelta(text) => {
                            thinking_text.push_str(&text);
                            let _ = self
                                .event_tx
                                .send(AgentEvent::ThinkingDelta {
                                    session_id: session_id.clone(),
                                    text,
                                })
                                .await;
                        }
                        StreamEvent::ToolUseStart { id, name } => {
                            tracing::info!("Tool use start: {} ({})", name, id);
                            active_tools.insert(
                                id.clone(),
                                (
                                    PendingToolCall {
                                        id,
                                        name,
                                        input: serde_json::Value::Null,
                                        input_error: None,
                                    },
                                    String::new(),
                                ),
                            );
                        }
                        StreamEvent::ToolUseInputDelta { id, delta } => {
                            if let Some((_, input)) = active_tools.get_mut(&id) {
                                input.push_str(&delta);
                            } else {
                                tracing::warn!("tool input delta for unknown call {id}");
                            }
                        }
                        StreamEvent::ToolUseEnd { id } => {
                            if let Some((mut tool, input)) = active_tools.remove(&id) {
                                (tool.input, tool.input_error) = parse_tool_arguments(&input);
                                tracing::info!(
                                    "Tool use collected: {} -> {}",
                                    tool.name,
                                    serde_json::to_string(&tool.input)
                                        .unwrap_or_default()
                                        .chars()
                                        .take(200)
                                        .collect::<String>()
                                );
                                tool_calls.push(tool);
                            } else {
                                tracing::warn!("tool end for unknown call {id}");
                            }
                        }
                        StreamEvent::MessageEnd {
                            usage,
                            stop_reason: reason,
                        } => {
                            for (_, (mut tool, input)) in active_tools.drain() {
                                (tool.input, tool.input_error) = parse_tool_arguments(&input);
                                tool_calls.push(tool);
                            }
                            got_message_end = true;
                            stop_reason = reason;
                            tracing::debug!(
                                "Message end: in={} out={} stop_reason={}",
                                usage.input_tokens,
                                usage.output_tokens,
                                stop_reason.as_deref().unwrap_or("unknown")
                            );
                            session
                                .token_counter
                                .record_measured(usage.input_tokens, usage.output_tokens);
                            // Successful generate — overflow compact loop can reset.
                            session.consecutive_overflow_compacts = 0;
                            let tu = kkagent_protocol::TokenUsage {
                                input_tokens: usage.input_tokens,
                                output_tokens: usage.output_tokens,
                                cache_creation_input_tokens: usage.cache_creation_input_tokens,
                                cache_read_input_tokens: usage.cache_read_input_tokens,
                            };
                            session.usage.record(&tu);
                            let _ = self
                                .event_tx
                                .send(AgentEvent::UsageUpdate {
                                    session_id: session_id.clone(),
                                    usage: tu,
                                })
                                .await;
                        }
                        StreamEvent::Error(msg) => {
                            if msg.contains(kkagent_llm::FIRST_TOKEN_TIMEOUT_MARKER) {
                                tracing::warn!(
                                    model = %model_config.model,
                                    provider = %provider_config.provider_type,
                                    "first_token_timeout"
                                );
                            }
                            tracing::error!("Stream error: {}", msg);
                            last_stream_error = Some(msg);
                            stream_failed = true;
                        }
                        StreamEvent::RateLimited {
                            message,
                            retry_after,
                        } => {
                            tracing::warn!(
                                retry_after_seconds = retry_after.map(|delay| delay.as_secs_f64()),
                                "LLM rate limited: {message}"
                            );
                            last_stream_error = Some(message);
                            server_retry_after = retry_after;
                            rate_limited = true;
                            stream_failed = true;
                        }
                    },
                    Ok(None) => break,
                    Err(_) => continue, // timeout — re-check interrupt
                }
            }

            self.abort_registry.lock().await.remove(&session_id);

            if interrupted {
                break;
            }

            let empty =
                assistant_text.is_empty() && thinking_text.is_empty() && tool_calls.is_empty();
            let failed = stream_failed || !got_message_end;
            let visible_empty = assistant_text.trim().is_empty() && tool_calls.is_empty();
            if !failed
                && follows_tool_result
                && visible_empty
                && visible_empty_retries < visible_empty_retry_limit
            {
                visible_empty_retries += 1;
                tracing::warn!(
                    model = %model_config.model,
                    stop_reason = stop_reason.as_deref().unwrap_or("unknown"),
                    thinking_len = thinking_text.len(),
                    retry = visible_empty_retries,
                    retry_limit = visible_empty_retry_limit,
                    "Experimental recovery retry for visible-empty response after tool result"
                );
                retry_notice_count += 1;
                send_llm_retry_notice(
                    &self.event_tx,
                    &session_id,
                    retry_notice_count,
                    "The model returned an empty response after a tool result".into(),
                    Duration::ZERO,
                    Duration::ZERO,
                    true,
                )
                .await;
                continue;
            }
            if failed && empty {
                if let Some(error) = last_stream_error
                    .as_deref()
                    .filter(|error| is_tool_call_arguments_object_error(error))
                {
                    if let Some(mut rollback) =
                        rollback_rejected_tool_call(&mut session.messages, &messages, error)
                    {
                        rollback.provider_error = error.to_string();
                        session.transcript_rewrite_required = true;
                        tracing::warn!(
                            tool_call_id = %rollback.tool_call_id,
                            removed_blocks = rollback.removed_blocks,
                            "Rolled back provider-rejected assistant tool call"
                        );

                        // Experimental: auto-retry after rollback instead of stopping.
                        if bad_toolcall_retries < bad_toolcall_retry_limit {
                            bad_toolcall_retries += 1;
                            retry_notice_count += 1;
                            messages = self.prepare_messages(session, &tool_defs, &system_prompt);
                            send_llm_retry_notice(
                                &self.event_tx,
                                &session_id,
                                retry_notice_count,
                                format!(
                                    "Model returned a malformed tool call (`{}`); rolled back that micro-step and retrying ({}/{})",
                                    rollback.tool_call_id,
                                    bad_toolcall_retries,
                                    bad_toolcall_retry_limit
                                ),
                                Duration::ZERO,
                                Duration::ZERO,
                                true,
                            )
                            .await;
                            continue;
                        }

                        // Retry budget exhausted (or feature disabled): stop and
                        // wait for the user to send `continue`, as before.
                        rejected_tool_call_recovery = Some(rollback);
                        break;
                    }
                }
            }
            if failed
                && empty
                && failure_retries + 1 < max_attempts
                && last_stream_error
                    .as_deref()
                    .is_some_and(is_request_too_large)
            {
                let folded = fold_old_media(&mut session.messages, 2);
                if folded > 0 {
                    session.transcript_rewrite_required = true;
                    messages = self.prepare_messages(session, &tool_defs, &system_prompt);
                    tracing::warn!(
                        "LLM request too large; folded {folded} older media block(s) before retry"
                    );
                    failure_retries += 1;
                    retry_notice_count += 1;
                    send_llm_retry_notice(
                        &self.event_tx,
                        &session_id,
                        retry_notice_count,
                        format!("Request was too large; folded {folded} older media block(s)"),
                        Duration::ZERO,
                        Duration::ZERO,
                        true,
                    )
                    .await;
                    continue;
                }
                // Overflow recovery: compact (user retention) then retry.
                session.consecutive_overflow_compacts =
                    session.consecutive_overflow_compacts.saturating_add(1);
                let max_overflow = self
                    .config
                    .loop_control
                    .as_ref()
                    .and_then(|l| l.compact_max_overflow_attempts)
                    .unwrap_or(MAX_OVERFLOW_COMPACTION_ATTEMPTS);
                if session.consecutive_overflow_compacts > max_overflow {
                    terminal_stream_error = Some(format!(
                        "Compaction failed to bring the context under the model window after {max_overflow} attempts."
                    ));
                    break;
                }
                let est = session
                    .token_counter
                    .request_size(&system_prompt, &tool_defs, &messages);
                let configured = self
                    .config
                    .resolve_model(&active_model_alias)
                    .and_then(|(m, _)| m.max_context_size)
                    .unwrap_or(200_000);
                session.observed_max_context = Some(observe_context_overflow(
                    est,
                    session.observed_max_context.unwrap_or(configured),
                ));
                if self
                    .ensure_context_budget(session, &tool_defs, &system_prompt, true)
                    .await
                    .is_ok()
                {
                    messages = self.prepare_messages(session, &tool_defs, &system_prompt);
                    tracing::warn!(
                        "LLM request too large; compacted history before retry (overflow attempt {})",
                        session.consecutive_overflow_compacts
                    );
                    failure_retries += 1;
                    retry_notice_count += 1;
                    send_llm_retry_notice(
                        &self.event_tx,
                        &session_id,
                        retry_notice_count,
                        "Request was too large; compacted conversation history".into(),
                        Duration::ZERO,
                        Duration::ZERO,
                        true,
                    )
                    .await;
                    continue;
                }
            }
            if failed && empty && failure_retries + 1 < max_attempts {
                failure_retries += 1;
                tracing::warn!(
                    "LLM step retry {}/{} ({})",
                    failure_retries,
                    max_attempts,
                    last_stream_error
                        .as_deref()
                        .unwrap_or("empty/incomplete stream")
                );
                let delay = retry_delay(
                    failure_retries,
                    server_retry_after,
                    rate_limited,
                    rate_limit_retry_base,
                );
                tracing::warn!(
                    retry_in_seconds = delay.as_secs_f64(),
                    server_directed = server_retry_after.is_some(),
                    "Waiting before LLM retry"
                );
                retry_notice_count += 1;
                let reason = last_stream_error
                    .clone()
                    .unwrap_or_else(|| "LLM returned an empty or incomplete stream".into());
                if !wait_for_llm_retry(
                    &self.event_tx,
                    session,
                    &session_id,
                    retry_notice_count,
                    reason,
                    delay,
                )
                .await
                {
                    interrupted = true;
                    break;
                }
                continue;
            }
            if failed && empty && !using_fallback {
                if let Some(fallback_alias) = fallback_model_alias.as_ref() {
                    let reason = last_stream_error
                        .clone()
                        .unwrap_or_else(|| "LLM returned an empty or incomplete stream".into());
                    tracing::warn!(
                        primary_model = %model_alias,
                        fallback_model = %fallback_alias,
                        attempts = max_attempts,
                        %reason,
                        "Primary model exhausted retries; switching to fallback model"
                    );
                    retry_notice_count += 1;
                    send_llm_retry_notice(
                        &self.event_tx,
                        &session_id,
                        retry_notice_count,
                        format!(
                            "Model {model_alias} failed after {max_attempts} attempt(s); switching to fallback {fallback_alias}: {reason}"
                        ),
                        Duration::ZERO,
                        Duration::ZERO,
                        true,
                    )
                    .await;
                    active_model_alias.clone_from(fallback_alias);
                    using_fallback = true;
                    failure_retries = 0;
                    visible_empty_retries = 0;
                    visible_empty_retry_limit = self
                        .config
                        .resolve_model(fallback_alias)
                        .map(|(model, _)| model.experimental_visible_empty_retries)
                        .unwrap_or(0);
                    bad_toolcall_retries = 0;
                    bad_toolcall_retry_limit = self
                        .config
                        .resolve_model(fallback_alias)
                        .map(|(model, _)| model.experimental_bad_toolcall_auto_retries)
                        .unwrap_or(0);
                    continue;
                }
            }
            if failed {
                terminal_stream_error = Some(last_stream_error.unwrap_or_else(|| {
                    if empty {
                        "LLM returned an empty or incomplete stream".into()
                    } else {
                        "LLM stream ended before completion".into()
                    }
                }));
            }
            break;
        }

        if interrupted {
            if !assistant_text.is_empty() || !thinking_text.is_empty() || !tool_calls.is_empty() {
                let mut content = Vec::new();
                if !thinking_text.is_empty() {
                    content.push(ChatContent::Thinking {
                        thinking: thinking_text,
                    });
                }
                if !assistant_text.is_empty() {
                    content.push(ChatContent::Text {
                        text: assistant_text,
                    });
                }
                for tc in &tool_calls {
                    content.push(ChatContent::ToolUse {
                        id: tc.id.clone(),
                        name: tc.name.clone(),
                        input: tc.input.clone(),
                    });
                }
                session.messages.push(ChatMessage {
                    role: "assistant".into(),
                    content,
                });
                self.persist_step(session);
            }
            self.finish_interrupted(session).await?;
            return Ok(TurnStep::Done);
        }

        if let Some(rollback) = rejected_tool_call_recovery {
            let message = format!(
                "Model rejected malformed tool call `{}`; discarded that micro-step and stopped{}. Send `continue` to resume. ({})",
                rollback.tool_call_id,
                if bad_toolcall_retry_limit > 0 {
                    format!(
                        " after {} auto-retr{}",
                        bad_toolcall_retry_limit,
                        if bad_toolcall_retry_limit == 1 { "y" } else { "ies" }
                    )
                } else {
                    String::new()
                },
                truncate_chars_for_event(&rollback.provider_error, 300)
            );
            self.finish_interrupted_with_message(session, message)
                .await?;
            return Ok(TurnStep::Done);
        }

        if let Some(error) = terminal_stream_error {
            anyhow::bail!(error);
        }

        tracing::info!(
            "Stream done: text_len={} tool_calls={}",
            assistant_text.len(),
            tool_calls.len()
        );

        // Record assistant message
        let mut content = Vec::new();
        if !thinking_text.is_empty() {
            content.push(ChatContent::Thinking {
                thinking: thinking_text,
            });
        }
        if !assistant_text.is_empty() {
            content.push(ChatContent::Text {
                text: assistant_text,
            });
        }
        for tc in &tool_calls {
            content.push(ChatContent::ToolUse {
                id: tc.id.clone(),
                name: tc.name.clone(),
                input: tc.input.clone(),
            });
        }
        if !content.is_empty() {
            session.messages.push(ChatMessage {
                role: "assistant".into(),
                content,
            });
            self.persist_step(session);
        }

        if !tool_calls.is_empty() {
            // AgentSwarm exclusivity (swarm domain).
            let names: Vec<String> = tool_calls.iter().map(|t| t.name.clone()).collect();
            if let Some(reason) = crate::swarm::SwarmService::veto_mixed_agent_swarm(&names) {
                for tc in &tool_calls {
                    session.messages.push(ChatMessage {
                        role: "user".into(),
                        content: vec![ChatContent::ToolResult {
                            tool_use_id: tc.id.clone(),
                            content: reason.clone(),
                            is_error: true,
                        }],
                    });
                    self.persist_step(session);
                    let _ = self
                        .event_tx
                        .send(AgentEvent::ToolResult {
                            session_id: session_id.clone(),
                            tool_call_id: tc.id.clone(),
                            tool_name: tc.name.clone(),
                            output: reason.clone(),
                            is_error: true,
                        })
                        .await;
                }
                return Ok(TurnStep::Continue);
            }

            // Same-step + cross-turn tool dedupe.
            let call_pairs: Vec<(String, serde_json::Value)> = tool_calls
                .iter()
                .map(|t| (t.name.clone(), t.input.clone()))
                .collect();
            let dedupe = session.tool_dedupe.observe_step(&call_pairs);
            if !dedupe.skip_indices.is_empty() {
                tracing::info!(
                    "Tool dedupe skipped {} duplicate call(s)",
                    dedupe.skip_indices.len()
                );
            }
            let dedupe_reminder = dedupe.reminder.clone();
            let dedupe_force_stop = dedupe.force_stop;

            // If AskUserQuestion is present in this step, defer any ExitPlanMode
            // — answer the user's question first, then let the model regenerate.
            // The model will call ExitPlanMode again in the next turn if it still
            // wants to exit plan mode.
            let has_ask_user = tool_calls.iter().any(|tc| tc.name == "AskUserQuestion");

            let _ = self
                .event_tx
                .send(AgentEvent::StatusUpdate {
                    session_id: session_id.clone(),
                    status: SessionStatus::ToolExecuting,
                })
                .await;

            // Phase 1: permissions + interactive tools (serial). Phase 2: conflict-aware parallel exec.
            enum Prepared {
                Done(ToolOutput),
                Ready {
                    name: String,
                    input: serde_json::Value,
                },
            }
            let mut prepared: Vec<(String, String, Prepared)> = Vec::new();

            for (idx, tc) in tool_calls.iter().enumerate() {
                self.sync_requested_plan_mode(session).await?;
                if session.is_interrupted() {
                    self.finish_interrupted(session).await?;
                    return Ok(TurnStep::Done);
                }

                let _ = self
                    .event_tx
                    .send(AgentEvent::ToolCall {
                        session_id: session_id.clone(),
                        tool_call_id: tc.id.clone(),
                        tool_name: tc.name.clone(),
                        input: tc.input.clone(),
                    })
                    .await;

                if let Some(error) = &tc.input_error {
                    prepared.push((
                        tc.id.clone(),
                        tc.name.clone(),
                        Prepared::Done(ToolOutput::error(format!(
                            "Invalid arguments for tool `{}`: {error}",
                            tc.name
                        ))),
                    ));
                    continue;
                }

                if dedupe.skip_indices.contains(&idx) {
                    prepared.push((
                        tc.id.clone(),
                        tc.name.clone(),
                        Prepared::Done(ToolOutput::success(
                            "Skipped: duplicate tool call in the same step (identical args).",
                        )),
                    ));
                    continue;
                }

                // Defer ExitPlanMode when AskUserQuestion is in the same step —
                // answer the user's question first; the model can call ExitPlanMode
                // again next turn after incorporating the answer.
                if has_ask_user && tc.name == "ExitPlanMode" {
                    tracing::info!(
                        "Deferring ExitPlanMode: AskUserQuestion is present in the same step"
                    );
                    prepared.push((
                        tc.id.clone(),
                        tc.name.clone(),
                        Prepared::Done(ToolOutput::success(
                            "Deferred: AskUserQuestion is present in this step. \
                             Answer the user's question first, then call ExitPlanMode \
                             again in the next turn if you still want to exit plan mode.",
                        )),
                    ));
                    continue;
                }

                let decision = {
                    let perm = self.permission.lock().await;
                    let (verdict, source) = perm.evaluate_sourced(
                        &tc.name,
                        &tc.input,
                        &session.working_dir,
                        session.plan_mode,
                        Some(&session.plan_file_path),
                    );
                    let verdict_str = match &verdict {
                        PermissionDecision::Approve => "approve",
                        PermissionDecision::Ask => "ask",
                        PermissionDecision::Deny(_) => "deny",
                    };
                    crate::audit::record(&crate::audit::AuditEvent::PermissionVerdict {
                        at: &crate::audit::now_rfc3339(),
                        session_id: &session.id,
                        tool: &tc.name,
                        verdict: verdict_str,
                        source,
                        detail: "",
                        permission_mode: &format!("{:?}", perm.current_mode()),
                    });
                    verdict
                };
                tracing::info!("Permission for {}: {:?}", tc.name, decision);

                let prep = match decision {
                    PermissionDecision::Approve => {
                        if tc.name == "AskUserQuestion" {
                            Prepared::Done(self.execute_tool(session, &tc.name, &tc.input).await)
                        } else if tc.name == "ExitPlanMode" {
                            Prepared::Done(self.auto_exit_plan_mode(session, &tc.input).await)
                        } else {
                            if tc.name == "WritePlan" {
                                session
                                    .record_pre_change(session.plan_file_path.clone())
                                    .await;
                            } else if tc.name == "Write" || tc.name == "Edit" {
                                if let Some(path_str) =
                                    tc.input.get("path").and_then(|v| v.as_str())
                                {
                                    let path = if std::path::Path::new(path_str).is_absolute() {
                                        std::path::PathBuf::from(path_str)
                                    } else {
                                        session.working_dir.join(path_str)
                                    };
                                    session.record_pre_change(path).await;
                                }
                            }
                            Prepared::Ready {
                                name: tc.name.clone(),
                                input: tc.input.clone(),
                            }
                        }
                    }
                    PermissionDecision::Ask => {
                        if tc.name == "ExitPlanMode" {
                            Prepared::Done(
                                self.review_exit_plan_mode(session, &session_id, tc).await?,
                            )
                        } else {
                            let approval_id = uuid::Uuid::new_v4().to_string();
                            let action = describe_tool_action(&tc.name, &tc.input);
                            let _ = self
                                .event_tx
                                .send(AgentEvent::ApprovalRequested {
                                    session_id: session_id.clone(),
                                    request: kkagent_protocol::ApprovalRequest {
                                        approval_id: approval_id.clone(),
                                        session_id: session_id.clone(),
                                        tool_call_id: tc.id.clone(),
                                        tool_name: tc.name.clone(),
                                        action,
                                        tool_input_display: Some(tc.input.clone()),
                                        created_at: chrono::Utc::now(),
                                    },
                                })
                                .await;

                            let _ = self
                                .event_tx
                                .send(AgentEvent::StatusUpdate {
                                    session_id: session_id.clone(),
                                    status: SessionStatus::WaitingApproval,
                                })
                                .await;

                            let approval_timeout = self
                                .config
                                .background
                                .as_ref()
                                .and_then(|background| background.approval_timeout_s)
                                .unwrap_or(900)
                                .clamp(1, 86_400);
                            let response = match tokio::time::timeout(
                                std::time::Duration::from_secs(approval_timeout),
                                session.wait_approval(&approval_id),
                            )
                            .await
                            {
                                Ok(response) => response,
                                Err(_) => kkagent_protocol::ApprovalResponse {
                                    approval_id: approval_id.clone(),
                                    decision: kkagent_protocol::ApprovalDecision::Rejected,
                                    scope: None,
                                    feedback: Some(format!(
                                        "approval timed out after {approval_timeout} seconds"
                                    )),
                                    selected_label: None,
                                },
                            };
                            match response.decision {
                                kkagent_protocol::ApprovalDecision::Approved => {
                                    crate::audit::record(
                                        &crate::audit::AuditEvent::ApprovalResponse {
                                            at: &crate::audit::now_rfc3339(),
                                            session_id: &session.id,
                                            tool: &tc.name,
                                            response: "approved",
                                            scope: &format!(
                                                "{:?}",
                                                response
                                                    .scope
                                                    .as_ref()
                                                    .map(|s| format!("{s:?}"))
                                                    .unwrap_or_else(|| "once".into())
                                            ),
                                        },
                                    );
                                    match response.scope {
                                        Some(kkagent_protocol::ApprovalScope::Session) => {
                                            let mut perm = self.permission.lock().await;
                                            perm.record_session_approval(&tc.name, &tc.input);
                                        }
                                        Some(kkagent_protocol::ApprovalScope::Turn) => {
                                            let mut perm = self.permission.lock().await;
                                            perm.record_turn_approval(&tc.name, &tc.input);
                                        }
                                        Some(kkagent_protocol::ApprovalScope::Always) => {
                                            let mut perm = self.permission.lock().await;
                                            perm.record_always_approval(&tc.name, &tc.input);
                                        }
                                        Some(kkagent_protocol::ApprovalScope::Once) | None => {}
                                    }
                                    if tc.name == "AskUserQuestion" {
                                        Prepared::Done(
                                            self.execute_tool(session, &tc.name, &tc.input).await,
                                        )
                                    } else {
                                        if tc.name == "Write" || tc.name == "Edit" {
                                            if let Some(path_str) =
                                                tc.input.get("path").and_then(|v| v.as_str())
                                            {
                                                let path = if std::path::Path::new(path_str)
                                                    .is_absolute()
                                                {
                                                    std::path::PathBuf::from(path_str)
                                                } else {
                                                    session.working_dir.join(path_str)
                                                };
                                                let conflicts = global_file_tracker()
                                                    .conflicts_for(
                                                        &session.id,
                                                        &path,
                                                        &session.working_dir,
                                                    );
                                                if !conflicts.is_empty() {
                                                    let others = conflicts.join(", ");
                                                    let _ = self
                                                        .event_tx
                                                        .send(AgentEvent::Error {
                                                            session_id: session.id.clone(),
                                                            message: format!(
                                                                "File conflict warning: {} also touched by session(s) [{others}]. Continue, switch session, or use a separate worktree.",
                                                                path.display()
                                                            ),
                                                        })
                                                        .await;
                                                }
                                                global_file_tracker().record_write(
                                                    &session.id,
                                                    &path,
                                                    &session.working_dir,
                                                );
                                                session.record_pre_change(path).await;
                                            }
                                        }
                                        Prepared::Ready {
                                            name: tc.name.clone(),
                                            input: tc.input.clone(),
                                        }
                                    }
                                }
                                kkagent_protocol::ApprovalDecision::Cancelled => {
                                    self.finish_interrupted(session).await?;
                                    return Ok(TurnStep::Done);
                                }
                                other => {
                                    crate::audit::record(
                                        &crate::audit::AuditEvent::ApprovalResponse {
                                            at: &crate::audit::now_rfc3339(),
                                            session_id: &session.id,
                                            tool: &tc.name,
                                            response: &format!("{other:?}").to_lowercase(),
                                            scope: "once",
                                        },
                                    );
                                    Prepared::Done(ToolOutput::error(
                                        "Tool call was rejected by user",
                                    ))
                                }
                            }
                        } // end else non-ExitPlanMode Ask
                    }
                    PermissionDecision::Deny(reason) => {
                        Prepared::Done(ToolOutput::error(format!("Denied: {}", reason)))
                    }
                };
                prepared.push((tc.id.clone(), tc.name.clone(), prep));
            }

            // Final live re-gate: `/permission` may have switched to manual after an
            // earlier Approve under auto/yolo (including while waiting on another Ask).
            for prep_slot in prepared.iter_mut() {
                let (tool_call_id, tool_name, prep) = prep_slot;
                let Prepared::Ready { name, input } = prep else {
                    continue;
                };
                let decision = {
                    let perm = self.permission.lock().await;
                    perm.evaluate(
                        name,
                        input,
                        &session.working_dir,
                        session.plan_mode,
                        Some(&session.plan_file_path),
                    )
                };
                match decision {
                    PermissionDecision::Approve => {}
                    PermissionDecision::Deny(reason) => {
                        *prep = Prepared::Done(ToolOutput::error(format!("Denied: {reason}")));
                    }
                    PermissionDecision::Ask => {
                        let approval_id = uuid::Uuid::new_v4().to_string();
                        let action = describe_tool_action(name, input);
                        let _ = self
                            .event_tx
                            .send(AgentEvent::ApprovalRequested {
                                session_id: session_id.clone(),
                                request: kkagent_protocol::ApprovalRequest {
                                    approval_id: approval_id.clone(),
                                    session_id: session_id.clone(),
                                    tool_call_id: tool_call_id.clone(),
                                    tool_name: tool_name.clone(),
                                    action,
                                    tool_input_display: Some(input.clone()),
                                    created_at: chrono::Utc::now(),
                                },
                            })
                            .await;
                        let _ = self
                            .event_tx
                            .send(AgentEvent::StatusUpdate {
                                session_id: session_id.clone(),
                                status: SessionStatus::WaitingApproval,
                            })
                            .await;
                        let approval_timeout = self
                            .config
                            .background
                            .as_ref()
                            .and_then(|background| background.approval_timeout_s)
                            .unwrap_or(900)
                            .clamp(1, 86_400);
                        let response = match tokio::time::timeout(
                            std::time::Duration::from_secs(approval_timeout),
                            session.wait_approval(&approval_id),
                        )
                        .await
                        {
                            Ok(response) => response,
                            Err(_) => kkagent_protocol::ApprovalResponse {
                                approval_id: approval_id.clone(),
                                decision: kkagent_protocol::ApprovalDecision::Rejected,
                                scope: None,
                                feedback: Some(format!(
                                    "approval timed out after {approval_timeout} seconds"
                                )),
                                selected_label: None,
                            },
                        };
                        match response.decision {
                            kkagent_protocol::ApprovalDecision::Approved => match response.scope {
                                Some(kkagent_protocol::ApprovalScope::Session) => {
                                    let mut perm = self.permission.lock().await;
                                    perm.record_session_approval(name, input);
                                }
                                Some(kkagent_protocol::ApprovalScope::Turn) => {
                                    let mut perm = self.permission.lock().await;
                                    perm.record_turn_approval(name, input);
                                }
                                Some(kkagent_protocol::ApprovalScope::Always) => {
                                    let mut perm = self.permission.lock().await;
                                    perm.record_always_approval(name, input);
                                }
                                Some(kkagent_protocol::ApprovalScope::Once) | None => {}
                            },
                            kkagent_protocol::ApprovalDecision::Cancelled => {
                                self.finish_interrupted(session).await?;
                                return Ok(TurnStep::Done);
                            }
                            _ => {
                                *prep = Prepared::Done(ToolOutput::error(
                                    "Tool call was rejected by user",
                                ));
                            }
                        }
                    }
                }
            }

            // Server-side stale-file hard gate before Edit/Write execute.
            for prep_slot in prepared.iter_mut() {
                let (_id, _name, prep) = prep_slot;
                let Prepared::Ready { name, input } = prep else {
                    continue;
                };
                if name != "Edit" && name != "Write" {
                    continue;
                }
                let Some(path_str) = input.get("path").and_then(|v| v.as_str()) else {
                    continue;
                };
                let path = session.resolve_tracked_path(path_str);
                if let Some(err) = session.check_stale_before_write(&path) {
                    *prep = Prepared::Done(ToolOutput::error(err));
                }
            }

            let _ = self
                .event_tx
                .send(AgentEvent::StatusUpdate {
                    session_id: session_id.clone(),
                    status: SessionStatus::ToolExecuting,
                })
                .await;

            // Build scheduler tasks for Ready items; keep Done as-is.
            let mut ready_indices = Vec::new();
            let mut tasks = Vec::new();
            for (idx, (tool_call_id, _name, prep)) in prepared.iter().enumerate() {
                if let Prepared::Ready { name, input } = prep {
                    ready_indices.push(idx);
                    let accesses = infer_accesses(name, input, &session.working_dir);
                    let tools = Arc::clone(&self.tools);
                    let hooks = self.hooks.clone();
                    let working_dir = session.working_dir.clone();
                    let sid = session.id.clone();
                    let enabled = session.enabled_tools.clone();
                    let name = name.clone();
                    let input = input.clone();
                    let tool_call_id = tool_call_id.clone();
                    let interrupted = session.interrupted.clone();
                    let plan_file_path = session.plan_file_path.clone();
                    let image = self.config.image.clone();
                    let tools_config = self.config.tools.clone();
                    let msg_count = session.messages.len();
                    tasks.push(ToolCallTask {
                        accesses,
                        start: box_start(move || {
                            let tools = tools;
                            let hooks = hooks;
                            let working_dir = working_dir;
                            let sid = sid;
                            let enabled = enabled;
                            let name = name;
                            let input = input;
                            let tool_call_id = tool_call_id;
                            let tools_config = tools_config;
                            let msg_count = msg_count;
                            async move {
                                execute_tool_parallel(ParallelToolRequest {
                                    tools,
                                    hooks,
                                    working_dir,
                                    session_id: sid.clone(),
                                    turn_id: format!("{}:{}", sid, msg_count),
                                    image,
                                    enabled_tools: enabled,
                                    name,
                                    input,
                                    tool_call_id: Some(tool_call_id),
                                    interrupted,
                                    plan_file_path,
                                    tools_config,
                                })
                                .await
                            }
                        }),
                    });
                }
            }

            let parallel_outputs = if tasks.is_empty() {
                Vec::new()
            } else {
                ToolScheduler::run_all(tasks).await
            };
            let mut parallel_iter = parallel_outputs.into_iter();
            let mut resolved: Vec<(String, String, Option<serde_json::Value>, ToolOutput)> =
                Vec::new();
            for (i, (id, name, prep)) in prepared.into_iter().enumerate() {
                let tc_input = tool_calls.get(i).map(|tc| tc.input.clone());
                let (input, output) = match prep {
                    Prepared::Done(o) => (tc_input, o),
                    Prepared::Ready { input, .. } => {
                        let _ = ready_indices.contains(&i);
                        let output = parallel_iter
                            .next()
                            .unwrap_or_else(|| Err("scheduler missing result".into()))
                            .unwrap_or_else(ToolOutput::error);
                        (Some(input), output)
                    }
                };
                resolved.push((id, name, input, output));
            }

            let mut tool_results = Vec::new();
            for (id, name, input, mut output) in resolved {
                // ExitPlanMode / EnterPlanMode flip plan_mode inside their helpers /
                // execute paths; mirror to the TUI here.
                if name == "ExitPlanMode" {
                    let _ = self
                        .event_tx
                        .send(AgentEvent::PlanModeChanged {
                            session_id: session_id.clone(),
                            enabled: session.plan_mode,
                        })
                        .await;
                }
                if name == "EnterPlanMode" && !output.is_error {
                    match session.set_plan_mode_persisted(true) {
                        Ok(()) => {
                            output.content =
                                kkagent_tools::builtin::plan::entered_plan_mode_message();
                            let _ = self
                                .event_tx
                                .send(AgentEvent::PlanModeChanged {
                                    session_id: session_id.clone(),
                                    enabled: true,
                                })
                                .await;
                        }
                        Err(error) => {
                            output =
                                ToolOutput::error(format!("Failed to persist plan mode: {error}"));
                        }
                    }
                }
                if name == "SelectTools" && !output.is_error {
                    if let Some(data) = &output.data {
                        if data.get("tools").map(|v| v.is_null()).unwrap_or(false) {
                            session.enabled_tools = None;
                            session.tool_policy.layers_mut().profile.tools = None;
                        } else if let Some(arr) = data.get("tools").and_then(|v| v.as_array()) {
                            let set: std::collections::HashSet<String> = arr
                                .iter()
                                .filter_map(|v| v.as_str().map(String::from))
                                .collect();
                            session.tool_policy.layers_mut().profile.tools =
                                Some(set.iter().cloned().collect());
                            session.enabled_tools = Some(set);
                        }
                    }
                }
                if (name == "AgentSwarm" || name == "Task") && !output.is_error {
                    if let Some(reminder) =
                        session.swarm.enter(crate::swarm::SwarmModeTrigger::Tool)
                    {
                        session.add_user_message(reminder.into());
                    }
                }

                if !output.is_error {
                    if name == "Read" {
                        if let Some(path_str) = input
                            .as_ref()
                            .and_then(|v| v.get("path"))
                            .and_then(|v| v.as_str())
                        {
                            if let Some(hash) = output
                                .data
                                .as_ref()
                                .and_then(|d| d.get("content_hash"))
                                .and_then(|v| v.as_str())
                            {
                                let path = session.resolve_tracked_path(path_str);
                                session.record_read_content_hash(&path, hash.to_string());
                            }
                        }
                    } else if name == "Edit" || name == "Write" {
                        if let Some(path_str) = input
                            .as_ref()
                            .and_then(|v| v.get("path"))
                            .and_then(|v| v.as_str())
                        {
                            let path = session.resolve_tracked_path(path_str);
                            session.refresh_tracked_file_hash(&path);
                        }
                    }
                }

                session.maybe_append_concurrent_write_reminder(&name, &mut output);

                let output = truncate_tool_output(
                    self.tool_result_store.as_deref(),
                    session,
                    &name,
                    &id,
                    output,
                );

                tracing::info!(
                    "Tool {} result: error={} len={}",
                    name,
                    output.is_error,
                    output.content.len()
                );

                // Commit the latest TodoList snapshot before any hook or UI
                // await so an app exit cannot strand a visibly completed update.
                let todo_items = if !output.is_error && name == "TodoList" {
                    session.turns_since_todo = 0;
                    let items = todo_items_from_output(&output);
                    if let Some(items) = items.as_ref() {
                        if let Err(error) = session.set_todos_persisted(items.clone()) {
                            tracing::warn!(%error, "failed to persist session todo list");
                        }
                    }
                    items
                } else {
                    None
                };

                if let Some(hooks) = &self.hooks {
                    let _ = hooks
                        .fire(
                            kkagent_mcp::hooks::HookEvent::PostToolCall,
                            &serde_json::json!({
                                "tool": name,
                                "tool_call_id": id,
                                "is_error": output.is_error,
                                "output_len": output.content.len(),
                                "workspace": session.working_dir,
                            }),
                        )
                        .await;
                }

                let _ = self
                    .event_tx
                    .send(AgentEvent::ToolResult {
                        session_id: session_id.clone(),
                        tool_call_id: id.clone(),
                        tool_name: name.clone(),
                        // UI sees content only — note stays model-side.
                        output: output.content.clone(),
                        is_error: output.is_error,
                    })
                    .await;

                if let Some(items) = todo_items {
                    let _ = self
                        .event_tx
                        .send(AgentEvent::TodoUpdated {
                            session_id: session_id.clone(),
                            items,
                        })
                        .await;
                }

                if !output.is_error && name == "Skill" {
                    if let Some(evt) = skill_activated_from_output(&session_id, &output) {
                        let _ = self.event_tx.send(evt).await;
                    }
                }

                if !output.is_error && (name == "Write" || name == "Edit") {
                    // Reconstruct input from tool call list
                    if let Some(tc) = tool_calls.iter().find(|t| t.id == id) {
                        if let Some(content) = read_plan_file_if_matched(session, &tc.input).await {
                            let _ = self
                                .event_tx
                                .send(AgentEvent::PlanFileUpdated {
                                    session_id: session_id.clone(),
                                    path: session.plan_file_path.display().to_string(),
                                    content,
                                })
                                .await;
                        }
                    }
                }
                if !output.is_error && name == "WritePlan" {
                    if let Some(content) = output
                        .data
                        .as_ref()
                        .filter(|data| {
                            data.get("kind").and_then(|value| value.as_str()) == Some("plan_write")
                        })
                        .and_then(|data| data.get("content"))
                        .and_then(|content| content.as_str())
                    {
                        let _ = self
                            .event_tx
                            .send(AgentEvent::PlanFileUpdated {
                                session_id: session_id.clone(),
                                path: session.plan_file_path.display().to_string(),
                                content: content.to_string(),
                            })
                            .await;
                    }
                }

                tool_results.push((id, output));
            }

            // Append dedupe reminder to the last tool result when streak is high.
            if let Some(reminder) = dedupe_reminder {
                if let Some((_, output)) = tool_results.last_mut() {
                    output.content.push_str(&reminder);
                }
            }

            let mut result_content = Vec::new();
            let mut deliveries = Vec::new();
            for (id, output) in &tool_results {
                result_content.push(ChatContent::ToolResult {
                    tool_use_id: id.clone(),
                    content: output.model_content(),
                    is_error: output.is_error,
                });
                result_content.extend(output.images.iter().map(|image| ChatContent::Image {
                    media_type: image.media_type.clone(),
                    data: image.data.clone(),
                }));
                if let Some(delivery) = &output.delivery {
                    deliveries.push(delivery.message.clone());
                }
            }

            session.messages.push(ChatMessage {
                role: "user".into(),
                content: result_content,
            });
            self.persist_step(session);

            // Steer / delivery messages land after tool results (next model turn).
            for msg in deliveries {
                session.add_user_message(msg);
            }
            if !session.transcript_rewrite_required {
                self.persist_step(session);
            }

            if session.is_interrupted() {
                self.finish_interrupted(session).await?;
                return Ok(TurnStep::Done);
            }

            if dedupe_force_stop {
                tracing::warn!("Tool dedupe force-stop after repeated identical calls");
                self.finish_turn(session, true).await?;
                return Ok(TurnStep::Done);
            }

            if tool_results.iter().any(|(_, o)| o.stop_turn) {
                self.finish_turn(session, true).await?;
                return Ok(TurnStep::Done);
            }

            return Ok(TurnStep::Continue);
        }

        if session.finish_or_apply_steers()? {
            tracing::info!("Continuing turn for newly buffered steer input");
            return Ok(TurnStep::Continue);
        }
        self.finish_turn(session, true).await?;
        Ok(TurnStep::Done)
    }

    async fn sync_requested_plan_mode(&self, session: &mut Session) -> anyhow::Result<()> {
        if session.sync_requested_plan_mode()? {
            let _ = self
                .event_tx
                .send(AgentEvent::PlanModeChanged {
                    session_id: session.id.clone(),
                    enabled: session.plan_mode,
                })
                .await;
        }
        Ok(())
    }

    async fn finish_interrupted(&self, session: &mut Session) -> anyhow::Result<()> {
        self.finish_interrupted_with_message(session, "Interrupted".into())
            .await
    }

    async fn finish_interrupted_with_message(
        &self,
        session: &mut Session,
        message: String,
    ) -> anyhow::Result<()> {
        let _ = session.close_and_apply_steers()?;
        let session_id = session.id.clone();
        tracing::info!("Turn interrupted for session {}", session_id);
        session.note_turn_cancelled();
        let _ = self
            .event_tx
            .send(AgentEvent::Error {
                session_id: session_id.clone(),
                message,
            })
            .await;
        let _ = self
            .event_tx
            .send(AgentEvent::StatusUpdate {
                session_id: session_id.clone(),
                status: SessionStatus::Idle,
            })
            .await;
        if let Some(hooks) = &self.hooks {
            let _ = hooks
                .fire(
                    kkagent_mcp::hooks::HookEvent::TurnEnd,
                    &serde_json::json!({
                        "session_id": session_id,
                        "workspace": session.working_dir,
                        "interrupted": true,
                    }),
                )
                .await;
        }
        let _ = self.event_tx.send(AgentEvent::TurnEnd { session_id }).await;
        Ok(())
    }

    async fn finish_turn(&self, session: &mut Session, record_goal: bool) -> anyhow::Result<()> {
        let _ = session.close_and_apply_steers()?;
        {
            let mut perm = self.permission.lock().await;
            perm.clear_turn_approvals();
        }
        if let Some(reminder) = session.swarm.on_turn_end() {
            session.add_user_message(reminder.into());
        }
        let session_id = session.id.clone();
        if record_goal {
            if let Some(goal_mgr) = &self.goal_mgr {
                let tokens = session.token_counter.session_usage().0
                    + session.token_counter.session_usage().1;
                // Prefer last measured step tokens for this turn accounting.
                let step_tokens = session.token_counter.latest_measured();
                goal_mgr
                    .record_turn(if step_tokens > 0 { step_tokens } else { tokens })
                    .await;
                if let Some(goal) = goal_mgr.get_goal().await {
                    if goal.is_budget_exhausted()
                        && goal.status == kkagent_protocol::goal::GoalStatus::Active
                    {
                        goal_mgr
                            .block_goal("Blocked after goal budget reached")
                            .await;
                        self.emit_goal_updated(session, "budget_blocked").await;
                    } else {
                        self.emit_goal_updated(session, "account_usage").await;
                    }
                }
            }
        }
        session.note_turn_completed();
        let _ = self
            .event_tx
            .send(AgentEvent::StatusUpdate {
                session_id: session_id.clone(),
                status: SessionStatus::Idle,
            })
            .await;
        if let Some(hooks) = &self.hooks {
            let _ = hooks
                .fire(
                    kkagent_mcp::hooks::HookEvent::TurnEnd,
                    &serde_json::json!({
                        "session_id": session_id,
                        "workspace": session.working_dir,
                    }),
                )
                .await;
        }
        let _ = self
            .event_tx
            .send(AgentEvent::TurnEnd {
                session_id: session_id.clone(),
            })
            .await;
        tracing::info!("Turn completed for session {}", session_id);
        Ok(())
    }

    /// Early-trigger / blocking compaction with LLM summary + user retention.
    /// `force` bypasses the "nothing new since last compact" guard (overflow path).
    async fn ensure_context_budget(
        &self,
        session: &mut Session,
        tools: &[ToolDef],
        system: &str,
        force: bool,
    ) -> anyhow::Result<()> {
        let auto_compact = self
            .config
            .loop_control
            .as_ref()
            .map(|l| l.auto_compact)
            .unwrap_or(true);
        if !auto_compact && !force {
            return Ok(());
        }

        let policy = self
            .config
            .loop_control
            .as_ref()
            .map(CompactionPolicy::from_loop_control)
            .unwrap_or_default();

        let configured_max = {
            let alias = session.get_model_alias();
            self.config
                .resolve_model(&alias)
                .and_then(|(m, _)| m.max_context_size)
                .unwrap_or(200_000)
        };
        let max_context = session
            .observed_max_context
            .map(|o| o.min(configured_max))
            .unwrap_or(configured_max);

        let projected = project_owned(session.build_messages(), &ProjectOptions::default());
        let used = session
            .token_counter
            .request_size(system, tools, &projected);

        // Nothing new since last compact — avoid nesting summaries.
        if !force {
            if let Some(floor) = session.last_compacted_tokens {
                if used <= floor {
                    return Ok(());
                }
            }
        }

        if !force && !policy.should_compact(max_context, used) {
            return Ok(());
        }
        // Blocking protection: when over block ratio we compact synchronously
        // before the LLM step (same as kimi block-on-compact).
        let _ = force || policy.should_block(max_context, used);

        let _ = self
            .event_tx
            .send(AgentEvent::StatusUpdate {
                session_id: session.id.clone(),
                status: SessionStatus::Compacting,
            })
            .await;

        let result = compact_full_async(self.config.clone(), &mut session.messages, None).await;
        session.transcript_rewrite_required = true;
        // Checkpoints survive compaction: file snapshots stay restorable,
        // transcript truncation for pre-compaction turns no longer applies.
        session.invalidate_undo_message_indices();
        let after = session
            .token_counter
            .request_size(system, tools, &session.build_messages());
        session.last_compacted_tokens = Some(after);
        tracing::info!(
            "Compacted session {}: kept_users={} summarizer_dropped={} est_tokens {}→{}",
            session.id,
            result.kept_user_message_count,
            result.summarizer_dropped_count,
            used,
            after
        );

        let _ = self
            .event_tx
            .send(AgentEvent::StatusUpdate {
                session_id: session.id.clone(),
                status: SessionStatus::Thinking,
            })
            .await;
        Ok(())
    }

    /// Project / auto-compact messages to fit model context budget.
    fn prepare_messages(
        &self,
        session: &mut Session,
        tools: &[ToolDef],
        system: &str,
    ) -> Vec<ChatMessage> {
        let policy = self
            .config
            .loop_control
            .as_ref()
            .map(CompactionPolicy::from_loop_control)
            .unwrap_or_default();
        let auto_compact = self
            .config
            .loop_control
            .as_ref()
            .map(|l| l.auto_compact)
            .unwrap_or(true);
        let keep_last = self
            .config
            .loop_control
            .as_ref()
            .map(|l| l.compact_keep_last as usize)
            .unwrap_or(8)
            .max(2);

        let configured_max = {
            let alias = session.get_model_alias();
            self.config
                .resolve_model(&alias)
                .and_then(|(m, _)| m.max_context_size)
                .unwrap_or(200_000)
        };
        let max_context = session
            .observed_max_context
            .map(|o| o.min(configured_max))
            .unwrap_or(configured_max);

        let opts = ProjectOptions::default();
        // contextMemory: drop vacuous noise + loop-event markers before project.
        let _ = crate::context_memory::fold_vacuous(&mut session.messages);
        let projected = crate::context_memory::fold_loop_events_owned(session.build_messages());
        let mut messages = project_owned(projected, &opts);
        let mut req = session.token_counter.request_size(system, tools, &messages);

        if policy.should_compact(max_context, req) {
            messages = project_strict(&session.build_messages(), &opts);
            req = session.token_counter.request_size(system, tools, &messages);
        }

        // Sync fallback if still over budget (e.g. LLM compact unavailable).
        if auto_compact
            && policy.should_compact(max_context, req)
            && session
                .last_compacted_tokens
                .map(|floor| req > floor)
                .unwrap_or(true)
        {
            let result = compact_full(
                &mut session.messages,
                keep_last,
                CompactionStrategy::KeepUsers,
            );
            session.transcript_rewrite_required = true;
            // See full compaction: keep file snapshots, drop stale indices.
            session.invalidate_undo_message_indices();
            let after =
                session
                    .token_counter
                    .request_size(system, tools, &session.build_messages());
            session.last_compacted_tokens = Some(after);
            tracing::info!(
                "Auto-compacted session {} (local digest): kept_users={} est_tokens={}->{}",
                session.id,
                result.kept_user_message_count,
                req,
                after
            );
            messages = project_owned(session.build_messages(), &opts);
        }

        messages
    }

    async fn auto_exit_plan_mode(
        &self,
        session: &mut Session,
        input: &serde_json::Value,
    ) -> ToolOutput {
        if !session.plan_mode {
            return ToolOutput::error(
                "ExitPlanMode can only be called while plan mode is active. Use EnterPlanMode (or /plan) first.",
            );
        }
        let plan = tokio::fs::read_to_string(&session.plan_file_path)
            .await
            .unwrap_or_default();
        if plan.trim().is_empty() {
            return ToolOutput::error(
                "No plan document found. Call WritePlan first, then call ExitPlanMode.",
            );
        }
        if let Err(error) = session.finalize_plan_filename(&plan) {
            return ToolOutput::error(error.to_string());
        }
        let path = session.plan_file_path.display().to_string();
        let _ = self
            .event_tx
            .send(AgentEvent::PlanFileUpdated {
                session_id: session.id.clone(),
                path: path.clone(),
                content: plan.clone(),
            })
            .await;
        let _ = input;
        if let Err(error) = session.set_plan_mode_persisted(false) {
            return ToolOutput::error(format!("Failed to persist plan mode: {error}"));
        }
        ToolOutput::success(format_auto_approved_plan(&plan, &path))
    }

    async fn review_exit_plan_mode(
        &self,
        session: &mut Session,
        session_id: &str,
        tc: &PendingToolCall,
    ) -> anyhow::Result<ToolOutput> {
        if !session.plan_mode {
            return Ok(ToolOutput::error(
                "ExitPlanMode can only be called while plan mode is active. Use EnterPlanMode (or /plan) first.",
            ));
        }
        let plan = tokio::fs::read_to_string(&session.plan_file_path)
            .await
            .unwrap_or_default();
        if plan.trim().is_empty() {
            return Ok(ToolOutput::error(
                "No plan document found. Call WritePlan first, then call ExitPlanMode.",
            ));
        }
        if let Err(error) = session.finalize_plan_filename(&plan) {
            return Ok(ToolOutput::error(error.to_string()));
        }
        let path = session.plan_file_path.display().to_string();

        let display = PlanReviewDisplay::from_tool_input(&tc.input, plan.clone(), path.clone());
        let _ = self
            .event_tx
            .send(AgentEvent::PlanFileUpdated {
                session_id: session_id.to_string(),
                path: path.clone(),
                content: plan,
            })
            .await;

        let approval_id = uuid::Uuid::new_v4().to_string();
        let request = kkagent_protocol::ApprovalRequest {
            approval_id: approval_id.clone(),
            session_id: session_id.to_string(),
            tool_call_id: tc.id.clone(),
            tool_name: "ExitPlanMode".into(),
            action: "Ready to build with this plan?".into(),
            tool_input_display: Some(display.to_display_json()),
            created_at: chrono::Utc::now(),
        };
        session.set_pending_plan_review(Some(request.clone()))?;
        let _ = self
            .event_tx
            .send(AgentEvent::ApprovalRequested {
                session_id: session_id.to_string(),
                request,
            })
            .await;
        let _ = self
            .event_tx
            .send(AgentEvent::StatusUpdate {
                session_id: session_id.to_string(),
                status: SessionStatus::WaitingApproval,
            })
            .await;

        let approval_timeout = self
            .config
            .background
            .as_ref()
            .and_then(|background| background.approval_timeout_s)
            .unwrap_or(900)
            .clamp(1, 86_400);
        let response = match tokio::time::timeout(
            std::time::Duration::from_secs(approval_timeout),
            session.wait_approval(&approval_id),
        )
        .await
        {
            Ok(response) => response,
            Err(_) => kkagent_protocol::ApprovalResponse {
                approval_id: approval_id.clone(),
                decision: kkagent_protocol::ApprovalDecision::Rejected,
                scope: None,
                feedback: Some(format!(
                    "approval timed out after {approval_timeout} seconds"
                )),
                selected_label: Some(crate::plan_review::LABEL_REVISE.into()),
            },
        };
        let _ = self
            .event_tx
            .send(AgentEvent::StatusUpdate {
                session_id: session_id.to_string(),
                status: SessionStatus::Thinking,
            })
            .await;

        session.set_pending_plan_review(None)?;

        if response.decision == kkagent_protocol::ApprovalDecision::Cancelled {
            return Ok(ToolOutput::success(
                "Plan approval dismissed. Plan mode remains active.",
            ));
        }

        let (output, exit) = resolve_exit_plan_approval(&response, &display);
        if exit {
            session.set_plan_mode_persisted(false)?;
        }
        Ok(output)
    }

    async fn execute_tool(
        &self,
        session: &mut Session,
        name: &str,
        input: &serde_json::Value,
    ) -> ToolOutput {
        if name == "AskUserQuestion" {
            return self.run_ask_user_question(session, input).await;
        }
        if name == "Write" || name == "Edit" {
            if let Some(path_str) = input.get("path").and_then(|v| v.as_str()) {
                let path = if std::path::Path::new(path_str).is_absolute() {
                    std::path::PathBuf::from(path_str)
                } else {
                    session.working_dir.join(path_str)
                };
                session.record_pre_change(path).await;
            }
        }
        execute_tool_parallel(ParallelToolRequest {
            tools: Arc::clone(&self.tools),
            hooks: self.hooks.clone(),
            working_dir: session.working_dir.clone(),
            session_id: session.id.clone(),
            turn_id: format!("{}:{}", session.id, session.messages.len()),
            image: self.config.image.clone(),
            enabled_tools: session.enabled_tools.clone(),
            name: name.to_string(),
            input: input.clone(),
            tool_call_id: None,
            interrupted: session.interrupted.clone(),
            plan_file_path: session.plan_file_path.clone(),
            tools_config: self.config.tools.clone(),
        })
        .await
    }

    async fn run_ask_user_question(
        &self,
        session: &mut Session,
        input: &serde_json::Value,
    ) -> ToolOutput {
        let question_text = input
            .get("question")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        if question_text.is_empty() {
            return ToolOutput::error("Missing 'question'");
        }

        let options: Vec<kkagent_protocol::QuestionOption> = input
            .get("options")
            .and_then(|v| v.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|o| {
                        Some(kkagent_protocol::QuestionOption {
                            id: o.get("id")?.as_str()?.to_string(),
                            label: o.get("label")?.as_str()?.to_string(),
                        })
                    })
                    .collect()
            })
            .unwrap_or_default();

        let allow_multiple = input
            .get("allow_multiple")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);
        let allow_free_text = input
            .get("allow_free_text")
            .and_then(|v| v.as_bool())
            .unwrap_or(options.is_empty());
        let question_id = uuid::Uuid::new_v4().to_string();
        let session_id = session.id.clone();

        let _ = self
            .event_tx
            .send(AgentEvent::QuestionAsked {
                session_id: session_id.clone(),
                question: kkagent_protocol::QuestionPayload {
                    question_id: question_id.clone(),
                    text: question_text.to_string(),
                    options: options.clone(),
                    allow_free_text,
                    allow_multiple,
                },
            })
            .await;
        let _ = self
            .event_tx
            .send(AgentEvent::StatusUpdate {
                session_id: session_id.clone(),
                status: SessionStatus::WaitingQuestion,
            })
            .await;

        let response = session.wait_question(&question_id).await;
        if response.cancelled || session.is_interrupted() {
            return ToolOutput::error("Question was cancelled");
        }

        let mut parts = Vec::new();
        if !response.selected_option_ids.is_empty() {
            let labels: Vec<String> = response
                .selected_option_ids
                .iter()
                .map(|id| {
                    options
                        .iter()
                        .find(|o| &o.id == id)
                        .map(|o| o.label.clone())
                        .unwrap_or_else(|| id.clone())
                })
                .collect();
            if allow_multiple {
                parts.push(format!("Selected: {}", labels.join(", ")));
            } else {
                parts.push(format!(
                    "Selected: {}",
                    labels.first().cloned().unwrap_or_default()
                ));
            }
        }
        if let Some(text) = response.free_text.filter(|t| !t.trim().is_empty()) {
            parts.push(format!("Answer: {}", text));
        }
        if parts.is_empty() {
            return ToolOutput::error("No answer provided");
        }
        ToolOutput::success(parts.join("\n"))
    }
}

fn is_request_too_large(error: &str) -> bool {
    let error = error.to_ascii_lowercase();
    error.contains("http 413")
        || error.contains("payload too large")
        || error.contains("request too large")
        || error.contains("request_too_large")
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ToolCallRollback {
    tool_call_id: String,
    removed_blocks: usize,
    provider_error: String,
}

fn request_follows_tool_result(messages: &[ChatMessage]) -> bool {
    messages
        .iter()
        .rev()
        .take_while(|message| message.role == "user")
        .flat_map(|message| &message.content)
        .any(|content| matches!(content, ChatContent::ToolResult { .. }))
}

fn is_tool_call_arguments_object_error(error: &str) -> bool {
    let lower = error.to_ascii_lowercase();
    lower.contains("assistant tool call")
        && lower.contains(".arguments must be a json object")
        && (lower.contains("status_code=400")
            || lower.contains("http 400")
            || lower.contains("400 bad request"))
}

fn rejected_tool_call_id(error: &str) -> Option<String> {
    const PREFIX: &str = "assistant tool call ";
    const SUFFIX: &str = ".arguments must be a json object";
    let lower = error.to_ascii_lowercase();
    let start = lower.find(PREFIX)? + PREFIX.len();
    let end = lower[start..].find(SUFFIX)? + start;
    let id = error[start..end]
        .trim()
        .trim_matches(|ch| matches!(ch, '\'' | '"' | '`'));
    (!id.is_empty()).then(|| id.to_string())
}

fn rollback_rejected_tool_call(
    messages: &mut Vec<ChatMessage>,
    projected_messages: &[ChatMessage],
    error: &str,
) -> Option<ToolCallRollback> {
    let requested_id = rejected_tool_call_id(error);
    let exact_exists = requested_id.as_deref().is_some_and(|id| {
        projected_messages.iter().any(|message| {
            message.content.iter().any(
                |part| matches!(part, ChatContent::ToolUse { id: candidate, .. } if candidate == id),
            )
        })
    });
    let tool_call_id = if exact_exists {
        requested_id?
    } else {
        projected_messages
            .iter()
            .rev()
            .find_map(|message| {
                message.content.iter().rev().find_map(|part| match part {
                    ChatContent::ToolUse { id, input, .. } if !input.is_object() => {
                        Some(id.clone())
                    }
                    _ => None,
                })
            })
            .or_else(|| latest_tool_call_id(projected_messages))?
    };

    let rollback_index = messages.iter().rposition(|message| {
        message
            .content
            .iter()
            .any(|part| matches!(part, ChatContent::ToolUse { id, .. } if id == &tool_call_id))
    })?;
    let removed_blocks = messages[rollback_index..]
        .iter()
        .map(|message| message.content.len())
        .sum();
    messages.truncate(rollback_index);
    (removed_blocks > 0).then(|| ToolCallRollback {
        tool_call_id,
        removed_blocks,
        provider_error: String::new(),
    })
}

fn latest_tool_call_id(messages: &[ChatMessage]) -> Option<String> {
    messages.iter().rev().find_map(|message| {
        message.content.iter().rev().find_map(|part| match part {
            ChatContent::ToolUse { id, .. } => Some(id.clone()),
            _ => None,
        })
    })
}

fn truncate_chars_for_event(value: &str, max_chars: usize) -> String {
    let mut chars = value.chars();
    let truncated: String = chars.by_ref().take(max_chars).collect();
    if chars.next().is_some() {
        format!("{truncated}…")
    } else {
        truncated
    }
}

#[derive(Debug, Clone)]
struct PendingToolCall {
    id: String,
    name: String,
    input: serde_json::Value,
    input_error: Option<String>,
}

fn parse_tool_arguments(input: &str) -> (serde_json::Value, Option<String>) {
    if input.trim().is_empty() {
        return (serde_json::json!({}), None);
    }
    match serde_json::from_str::<serde_json::Value>(input) {
        Ok(value) if value.is_object() => (value, None),
        Ok(_) => (
            serde_json::Value::Null,
            Some("expected a JSON object".to_string()),
        ),
        Err(error) => (serde_json::Value::Null, Some(error.to_string())),
    }
}

async fn read_plan_file_if_matched(session: &Session, input: &serde_json::Value) -> Option<String> {
    let path_str = input.get("path").and_then(|v| v.as_str())?;
    let candidate = if std::path::Path::new(path_str).is_absolute() {
        std::path::PathBuf::from(path_str)
    } else {
        session.working_dir.join(path_str)
    };
    let plan = &session.plan_file_path;
    let same = match (
        tokio::fs::canonicalize(&candidate).await,
        tokio::fs::canonicalize(plan).await,
    ) {
        (Ok(a), Ok(b)) => a == b,
        _ => normalize_path_lex(&candidate) == normalize_path_lex(plan),
    };
    if !same {
        return None;
    }
    if let Ok(content) = tokio::fs::read_to_string(plan).await {
        return Some(content);
    }
    // File may be briefly unavailable — fall back to Write payload.
    input
        .get("contents")
        .or_else(|| input.get("content"))
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn normalize_path_lex(p: &std::path::Path) -> std::path::PathBuf {
    let mut out = std::path::PathBuf::new();
    for c in p.components() {
        match c {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                out.pop();
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

fn todo_items_from_output(output: &ToolOutput) -> Option<Vec<kkagent_protocol::TodoItemEvent>> {
    let data = output.data.as_ref()?;
    let arr = data.get("items")?.as_array()?;
    let items = arr
        .iter()
        .filter_map(|v| {
            Some(kkagent_protocol::TodoItemEvent {
                id: v.get("id")?.as_str()?.to_string(),
                content: v.get("content")?.as_str()?.to_string(),
                status: v.get("status")?.as_str()?.to_string(),
            })
        })
        .collect();
    Some(items)
}

fn skill_activated_from_output(session_id: &str, output: &ToolOutput) -> Option<AgentEvent> {
    let data = output.data.as_ref()?;
    if data.get("kind").and_then(|v| v.as_str()) != Some("skill_activation") {
        return None;
    }
    let skill_name = data.get("skill_name")?.as_str()?.to_string();
    let skill_args = data
        .get("skill_args")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    let trigger = data
        .get("trigger")
        .and_then(|v| v.as_str())
        .unwrap_or("model-tool")
        .to_string();
    Some(AgentEvent::SkillActivated {
        session_id: session_id.to_string(),
        activation_id: uuid::Uuid::new_v4().to_string(),
        skill_name,
        skill_args,
        trigger,
    })
}

fn tool_allowed(session: &Session, name: &str) -> bool {
    if session
        .services
        .tool_policy_gate
        .session_policy
        .is_disabled(name)
    {
        return false;
    }
    if !session.tool_policy.is_active(name) {
        // Always keep disclosure / interaction primitives available.
        if !matches!(
            name,
            "SelectTools"
                | "AskUserQuestion"
                | "TodoList"
                | "ExitPlanMode"
                | "EnterPlanMode"
                | "WritePlan"
        ) {
            return false;
        }
    }
    tool_allowed_set(session.enabled_tools.as_ref(), name)
}

fn tool_allowed_set(enabled: Option<&std::collections::HashSet<String>>, name: &str) -> bool {
    match enabled {
        None => true,
        Some(set) => {
            set.contains(name)
                || name == "SelectTools"
                || name == "AskUserQuestion"
                || name == "TodoList"
                || name == "ExitPlanMode"
                || name == "EnterPlanMode"
                || name == "WritePlan"
        }
    }
}

struct ParallelToolRequest {
    tools: Arc<ToolRegistry>,
    hooks: Option<Arc<kkagent_mcp::HookManager>>,
    working_dir: std::path::PathBuf,
    session_id: String,
    turn_id: String,
    image: kkagent_config::ImageConfig,
    enabled_tools: Option<std::collections::HashSet<String>>,
    name: String,
    input: serde_json::Value,
    tool_call_id: Option<String>,
    interrupted: Arc<std::sync::atomic::AtomicBool>,
    plan_file_path: std::path::PathBuf,
    tools_config: kkagent_config::ToolsConfig,
}

async fn execute_tool_parallel(request: ParallelToolRequest) -> ToolOutput {
    let ParallelToolRequest {
        tools,
        hooks,
        working_dir,
        session_id,
        turn_id,
        image,
        enabled_tools,
        name,
        input,
        tool_call_id,
        interrupted,
        plan_file_path,
        tools_config,
    } = request;
    if !tool_allowed_set(enabled_tools.as_ref(), &name) {
        return ToolOutput::error(format!(
            "Tool `{name}` is not in the current SelectTools allowlist"
        ));
    }

    let mut input = input;
    if let Some(hooks) = &hooks {
        match hooks
            .fire_with_control(
                kkagent_mcp::hooks::HookEvent::PreToolCall,
                &serde_json::json!({
                    "tool": name,
                    "input": input,
                    "session_id": session_id,
                    "workspace": working_dir,
                }),
            )
            .await
        {
            Ok(outcome) if outcome.block => {
                return ToolOutput::error(
                    outcome
                        .reason
                        .unwrap_or_else(|| "Blocked by PreToolCall hook".into()),
                );
            }
            Ok(outcome) => {
                if let Some(rw) = outcome.rewrite {
                    if let Some(new_input) = rw.get("input") {
                        input = new_input.clone();
                    }
                }
            }
            Err(e) => tracing::warn!("PreToolCall hook error: {e}"),
        }
    }

    let tool = match tools.get(&name) {
        Some(t) => t,
        None => return ToolOutput::error(format!("Unknown tool: {}", name)),
    };

    if let Err(e) =
        kkagent_tools::args_validator::validate_against_schema(&tool.parameters_schema(), &input)
    {
        return ToolOutput::error(e.message);
    }

    let ctx = ToolContext {
        working_dir,
        session_id,
        turn_id,
        plan_file_path: Some(plan_file_path),
        image,
        tool_call_id,
        interrupted: Some(interrupted),
        tools_config,
    };

    match tool.execute(input, &ctx).await {
        Ok(output) => output,
        Err(e) => ToolOutput::error(format!("Tool execution error: {}", e)),
    }
}

fn truncate_tool_output(
    store: Option<&ToolResultStore>,
    session: &Session,
    tool_name: &str,
    tool_call_id: &str,
    mut output: ToolOutput,
) -> ToolOutput {
    if output.content.chars().count() <= TOOL_RESULT_MAX_CHARS {
        return output;
    }
    let full_chars = output.content.chars().count();
    let full_bytes = output.content.len();
    let preview: String = output
        .content
        .chars()
        .take(TOOL_RESULT_PREVIEW_CHARS)
        .collect();
    let store = match store {
        Some(store) => store,
        None => {
            output.content = format!(
                "{preview}\n\n… tool result truncated ({tool_name}, {full_chars} chars). The full output could not be saved: transcript store unavailable"
            );
            return output;
        }
    };
    output.content = match store.persist(session, tool_name, tool_call_id, &output.content) {
        Ok(record) => format!(
            "{preview}\n\n… tool result truncated. Full output saved to disk.\ntool_name: {tool_name}\ntool_call_id: {tool_call_id}\noutput_size_chars: {full_chars}\noutput_size_bytes: {full_bytes}\noutput_path: {path}\nnext_step: use the Read tool on output_path to inspect the full result if needed.",
            path = record.file_path.display()
        ),
        Err(error) => format!(
            "{preview}\n\n… tool result truncated ({tool_name}, {full_chars} chars). The full output could not be saved: {error}"
        ),
    };
    output
}

fn session_has_goal_reminder(session: &Session) -> bool {
    session.messages.iter().rev().take(6).any(|m| {
        m.content.iter().any(|c| match c {
            ChatContent::Text { text } => {
                text.contains("Active goal:") || text.contains("<untrusted_objective>")
            }
            _ => false,
        })
    })
}

fn retry_delay(
    attempt: u32,
    server_retry_after: Option<Duration>,
    rate_limited: bool,
    rate_limit_base: Duration,
) -> Duration {
    server_retry_after.unwrap_or_else(|| {
        let exponent = attempt.saturating_sub(1).min(5);
        if rate_limited {
            rate_limit_base.saturating_mul(1_u32 << exponent)
        } else {
            Duration::from_millis(200_u64.saturating_mul(1_u64 << exponent))
        }
    })
}

async fn wait_for_llm_retry(
    event_tx: &mpsc::Sender<AgentEvent>,
    session: &Session,
    session_id: &str,
    retry_number: u32,
    reason: String,
    delay: Duration,
) -> bool {
    let deadline = tokio::time::Instant::now() + delay;
    let mut displayed_seconds = duration_ceil_seconds(delay);
    send_llm_retry_notice(
        event_tx,
        session_id,
        retry_number,
        reason.clone(),
        delay,
        delay,
        true,
    )
    .await;

    while tokio::time::Instant::now() < deadline {
        if session.is_interrupted() {
            return false;
        }
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        tokio::time::sleep(remaining.min(Duration::from_millis(100))).await;
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        let remaining_seconds = duration_ceil_seconds(remaining);
        if remaining_seconds != displayed_seconds {
            displayed_seconds = remaining_seconds;
            send_llm_retry_notice(
                event_tx,
                session_id,
                retry_number,
                reason.clone(),
                delay,
                remaining,
                false,
            )
            .await;
        }
    }
    !session.is_interrupted()
}

async fn send_llm_retry_notice(
    event_tx: &mpsc::Sender<AgentEvent>,
    session_id: &str,
    retry_number: u32,
    reason: String,
    delay: Duration,
    remaining: Duration,
    initial: bool,
) {
    let _ = event_tx
        .send(AgentEvent::LlmRetry {
            session_id: session_id.to_string(),
            retry_number,
            reason,
            wait_seconds: duration_ceil_seconds(delay),
            remaining_seconds: duration_ceil_seconds(remaining),
            initial,
        })
        .await;
}

fn duration_ceil_seconds(duration: Duration) -> u64 {
    let milliseconds = duration.as_millis();
    u64::try_from(milliseconds.saturating_add(999) / 1_000).unwrap_or(u64::MAX)
}

fn describe_tool_action(name: &str, input: &serde_json::Value) -> String {
    match name {
        "Read" | "Write" | "Edit" => {
            let path = input.get("path").and_then(|v| v.as_str()).unwrap_or("?");
            format!("{}  {}", name, path)
        }
        "Bash" => {
            let cmd = input.get("command").and_then(|v| v.as_str()).unwrap_or("?");
            let short: String = cmd.chars().take(120).collect();
            format!("Bash  {}", short)
        }
        "Grep" => {
            let pat = input.get("pattern").and_then(|v| v.as_str()).unwrap_or("?");
            format!("Grep  {}", pat)
        }
        "Glob" => {
            let pat = input.get("pattern").and_then(|v| v.as_str()).unwrap_or("?");
            format!("Glob  {}", pat)
        }
        "Skill" => {
            let skill = input
                .get("name")
                .or_else(|| input.get("skill"))
                .and_then(|v| v.as_str())
                .unwrap_or("?");
            let args = input.get("args").and_then(|v| v.as_str()).unwrap_or("");
            if args.is_empty() {
                format!("invoke skill {skill}")
            } else {
                let short: String = args.chars().take(80).collect();
                format!("invoke skill {skill} ({short})")
            }
        }
        _ => {
            let pretty = serde_json::to_string_pretty(input).unwrap_or_default();
            let short: String = pretty.chars().take(200).collect();
            format!("{}  {}", name, short)
        }
    }
}

#[cfg(test)]
mod retry_tests {
    use super::*;
    use kkagent_config::{LoopControlConfig, ModelConfig, ProviderConfig};
    use kkagent_protocol::PermissionMode;
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::TcpListener,
    };

    #[test]
    fn server_retry_after_overrides_exponential_backoff() {
        let base = Duration::from_secs(5);
        assert_eq!(
            retry_delay(1, None, false, base),
            Duration::from_millis(200)
        );
        assert_eq!(
            retry_delay(3, None, false, base),
            Duration::from_millis(800)
        );
        assert_eq!(retry_delay(1, None, true, base), Duration::from_secs(5));
        assert_eq!(retry_delay(2, None, true, base), Duration::from_secs(10));
        assert_eq!(retry_delay(3, None, true, base), Duration::from_secs(20));
        assert_eq!(
            retry_delay(1, Some(Duration::from_secs(17)), true, base),
            Duration::from_secs(17)
        );
        assert_eq!(duration_ceil_seconds(Duration::from_millis(1)), 1);
        assert_eq!(duration_ceil_seconds(Duration::from_millis(1_001)), 2);
        assert_eq!(duration_ceil_seconds(Duration::ZERO), 0);
    }

    #[tokio::test]
    async fn turn_heartbeat_reports_liveness_until_dropped() {
        let (event_tx, mut event_rx) = mpsc::channel(4);
        let heartbeat = TurnHeartbeat::spawn_with_interval(
            event_tx.clone(),
            "heartbeat-session".into(),
            Duration::from_millis(10),
        );

        let event = tokio::time::timeout(Duration::from_millis(100), event_rx.recv())
            .await
            .expect("heartbeat should arrive")
            .expect("heartbeat channel should remain open");
        assert!(matches!(
            event,
            AgentEvent::Heartbeat { session_id } if session_id == "heartbeat-session"
        ));

        drop(heartbeat);
        while event_rx.try_recv().is_ok() {}
        assert!(
            tokio::time::timeout(Duration::from_millis(40), event_rx.recv())
                .await
                .is_err(),
            "dropping the heartbeat guard should stop future heartbeats"
        );
    }

    #[tokio::test]
    async fn retry_wait_emits_a_live_countdown() {
        let (event_tx, mut event_rx) = mpsc::channel(8);
        let session = Session::new(
            "countdown-session".into(),
            std::env::temp_dir(),
            PermissionMode::Auto,
            "test/model".into(),
        );

        assert!(
            wait_for_llm_retry(
                &event_tx,
                &session,
                "countdown-session",
                1,
                "rate limited".into(),
                Duration::from_millis(1_100),
            )
            .await
        );

        let mut remaining = Vec::new();
        while let Ok(event) = event_rx.try_recv() {
            if let AgentEvent::LlmRetry {
                remaining_seconds, ..
            } = event
            {
                remaining.push(remaining_seconds);
            }
        }
        assert_eq!(remaining.first(), Some(&2));
        assert!(remaining.contains(&1));
        assert_eq!(remaining.last(), Some(&0));
    }

    #[test]
    fn tool_arguments_must_be_json_objects() {
        assert_eq!(parse_tool_arguments("").0, serde_json::json!({}));
        assert_eq!(
            parse_tool_arguments("{\"path\":\"a.rs\"}").0,
            serde_json::json!({"path": "a.rs"})
        );
        assert!(parse_tool_arguments("[1,2]").1.is_some());
        assert!(parse_tool_arguments("{broken").1.is_some());
    }

    #[test]
    fn rolls_back_to_before_the_rejected_tool_call_micro_step() {
        let bad_id = "functions.BadTool:0";
        let mut messages = vec![
            ChatMessage {
                role: "user".into(),
                content: vec![ChatContent::Text {
                    text: "work".into(),
                }],
            },
            ChatMessage {
                role: "assistant".into(),
                content: vec![
                    ChatContent::Text {
                        text: "checking".into(),
                    },
                    ChatContent::ToolUse {
                        id: "functions.GoodTool:0".into(),
                        name: "GoodTool".into(),
                        input: serde_json::json!({"ok": true}),
                    },
                    ChatContent::ToolUse {
                        id: bad_id.into(),
                        name: "BadTool".into(),
                        input: serde_json::Value::Null,
                    },
                ],
            },
            ChatMessage {
                role: "user".into(),
                content: vec![
                    ChatContent::ToolResult {
                        tool_use_id: "functions.GoodTool:0".into(),
                        content: "ok".into(),
                        is_error: false,
                    },
                    ChatContent::ToolResult {
                        tool_use_id: bad_id.into(),
                        content: "invalid arguments".into(),
                        is_error: true,
                    },
                ],
            },
        ];
        let error = format!(
            "HTTP 400 Bad Request: status_code=400, Assistant tool call {bad_id}.arguments must be a JSON object."
        );
        assert!(is_tool_call_arguments_object_error(&error));
        let projected = messages.clone();
        let rollback = rollback_rejected_tool_call(&mut messages, &projected, &error).unwrap();
        assert_eq!(rollback.tool_call_id, bad_id);
        assert_eq!(rollback.removed_blocks, 5);
        assert_eq!(messages.len(), 1);
        assert!(matches!(
            &messages[0].content[0],
            ChatContent::Text { text } if text == "work"
        ));
    }

    #[test]
    fn masked_provider_id_falls_back_to_latest_non_object_tool_call() {
        let mut messages = vec![ChatMessage {
            role: "assistant".into(),
            content: vec![ChatContent::ToolUse {
                id: "actual-call".into(),
                name: "BadTool".into(),
                input: serde_json::json!([1, 2]),
            }],
        }];
        let error = "status_code=400, Assistant tool call ***.arguments must be a JSON object.";
        let projected = messages.clone();
        let rollback = rollback_rejected_tool_call(&mut messages, &projected, error).unwrap();
        assert_eq!(rollback.tool_call_id, "actual-call");
        assert!(messages.is_empty());
    }

    #[test]
    fn masked_provider_id_uses_corruption_from_projected_request() {
        let mut messages = vec![
            ChatMessage {
                role: "user".into(),
                content: vec![ChatContent::Text {
                    text: "work".into(),
                }],
            },
            ChatMessage {
                role: "assistant".into(),
                content: vec![ChatContent::ToolUse {
                    id: "functions.Edit:9".into(),
                    name: "Edit".into(),
                    input: serde_json::json!({"old_string": "x", "new_string": "y"}),
                }],
            },
            ChatMessage {
                role: "user".into(),
                content: vec![ChatContent::ToolResult {
                    tool_use_id: "functions.Edit:9".into(),
                    content: "edited".into(),
                    is_error: false,
                }],
            },
        ];
        let mut projected = messages.clone();
        let ChatContent::ToolUse { input, .. } = &mut projected[1].content[0] else {
            panic!("expected tool use");
        };
        *input = serde_json::Value::String("truncated arguments".into());

        let error = "status_code=400, Assistant tool call ***.arguments must be a JSON object.";
        let rollback = rollback_rejected_tool_call(&mut messages, &projected, error).unwrap();
        assert_eq!(rollback.tool_call_id, "functions.Edit:9");
        assert_eq!(messages.len(), 1);
    }

    #[test]
    fn truncates_oversized_output_with_metadata_notice() {
        let base = std::env::temp_dir().join(format!("kkagent-output-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&base).unwrap();
        let store = crate::agent_loop::ToolResultStore::new(base.clone(), None);
        let mut session = crate::session::Session::new(
            "sess-1".to_string(),
            base.clone(),
            kkagent_protocol::PermissionMode::Manual,
            "test-model".into(),
        );
        session.working_dir = base.clone();
        let big = "x".repeat(TOOL_RESULT_MAX_CHARS + 1);
        let output = truncate_tool_output(
            Some(&store),
            &session,
            "Bash",
            "call-1",
            kkagent_tools::ToolOutput::success(big),
        );
        assert!(output.content.contains("tool_name: Bash"));
        assert!(output.content.contains("tool_call_id: call-1"));
        assert!(output.content.contains("output_size_chars:"));
        assert!(output.content.contains("output_size_bytes:"));
        assert!(output.content.contains("output_path:"));
        assert!(output.content.contains("next_step:"));
        assert!(!output.is_error);
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn small_output_stays_inline() {
        let base =
            std::env::temp_dir().join(format!("kkagent-output-inline-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&base).unwrap();
        let store = crate::agent_loop::ToolResultStore::new(base.clone(), None);
        let mut session = crate::session::Session::new(
            "sess-1".to_string(),
            base.clone(),
            kkagent_protocol::PermissionMode::Manual,
            "test-model".into(),
        );
        session.working_dir = base.clone();
        // Multi-byte characters: char count below the threshold even though
        // the byte length exceeds it.
        let big = "汉".repeat(TOOL_RESULT_PREVIEW_CHARS + 1);
        assert!(big.len() > TOOL_RESULT_PREVIEW_CHARS * 3);
        let output = truncate_tool_output(
            Some(&store),
            &session,
            "Bash",
            "call-1",
            kkagent_tools::ToolOutput::success(big.clone()),
        );
        assert_eq!(output.content, big);
        assert!(!crate::tool_results::tool_results_root(&base)
            .join("sess-1")
            .exists());
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn subagent_store_buckets_into_parent_session() {
        let base =
            std::env::temp_dir().join(format!("kkagent-output-sub-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&base).unwrap();
        let store =
            crate::agent_loop::ToolResultStore::for_subagent(base.clone(), "parent-1".into());
        let mut session = crate::session::Session::new(
            "sub-1".to_string(),
            base.clone(),
            kkagent_protocol::PermissionMode::Manual,
            "test-model".into(),
        );
        session.working_dir = base.clone();
        let big = "y".repeat(TOOL_RESULT_MAX_CHARS + 1);
        let output = truncate_tool_output(
            Some(&store),
            &session,
            "Grep",
            "call-9",
            kkagent_tools::ToolOutput::success(big),
        );
        assert!(output.content.contains("output_path:"));
        // File lands in the parent session's bucket...
        assert!(crate::tool_results::tool_results_root(&base)
            .join("parent-1")
            .exists());
        // ...and no sub-1 bucket is created.
        assert!(!crate::tool_results::tool_results_root(&base)
            .join("sub-1")
            .exists());
        std::fs::remove_dir_all(&base).unwrap();
    }

    #[test]
    fn automatic_compaction_marks_transcript_for_rewrite() {
        let config = Arc::new(AppConfig {
            loop_control: Some(LoopControlConfig {
                max_attempts_per_step: 1,
                reserved_context_size: 200_000,
                max_steps_per_turn: 4,
                auto_compact: true,
                compact_keep_last: 2,
                token_counting: "estimated".into(),
                ..Default::default()
            }),
            ..AppConfig::default()
        });
        let (event_tx, _) = mpsc::channel(4);
        let loop_ = AgentLoop::new(
            config,
            Arc::new(ToolRegistry::new()),
            Arc::new(Mutex::new(PermissionChain::new(
                PermissionMode::Auto,
                Vec::new(),
            ))),
            event_tx,
            Arc::new(Mutex::new(HashMap::new())),
        );
        let workspace =
            std::env::temp_dir().join(format!("kkagent-compact-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&workspace).unwrap();
        let mut session = Session::new(
            "compact-test".into(),
            workspace.clone(),
            PermissionMode::Auto,
            "missing/model".into(),
        );
        for index in 0..6 {
            session.add_user_message(format!("message {index}"));
        }

        let _ = loop_.prepare_messages(&mut session, &[], "system");

        assert!(session.transcript_rewrite_required);
        // KeepUsers compaction retains real user prompts + summary (no tools).
        assert!(session.messages.len() >= 2);
        assert!(session.messages.iter().any(|m| {
            m.content.iter().any(|c| match c {
                ChatContent::Text { text } => {
                    text.contains("compacted to free up context")
                        || text.contains("Earlier conversation digest")
                }
                _ => false,
            })
        }));
        assert!(session.last_compacted_tokens.is_some());
        std::fs::remove_dir_all(workspace).unwrap();
    }

    #[tokio::test]
    async fn retries_transient_empty_failures_without_publishing_early_error() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        tokio::spawn(async move {
            for attempt in 1..=3 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0_u8; 8192];
                let _ = socket.read(&mut request).await.unwrap();
                let (status, content_type, body) = if attempt < 3 {
                    ("429 Too Many Requests", "application/json", "rate limited")
                } else {
                    (
                        "200 OK",
                        "text/event-stream",
                        "data: {\"choices\":[{\"delta\":{\"content\":\"recovered\"}}]}\n\
                         data: [DONE]\n",
                    )
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });

        let mut config = AppConfig {
            default_model: Some("test/model".into()),
            loop_control: Some(LoopControlConfig {
                max_attempts_per_step: 3,
                rate_limit_retry_base_seconds: 0,
                reserved_context_size: 1_000,
                max_steps_per_turn: 4,
                auto_compact: true,
                compact_keep_last: 4,
                token_counting: "estimated".into(),
                ..Default::default()
            }),
            ..AppConfig::default()
        };
        config.providers.insert(
            "test".into(),
            ProviderConfig {
                provider_type: "openai-chat".into(),
                api_key: Some("token".into()),
                base_url: Some(base_url),
                custom_headers: HashMap::new(),
                oauth: None,
                first_token_timeout_ms: None,
            },
        );
        config.models.insert(
            "test/model".into(),
            ModelConfig {
                provider: "test".into(),
                model: "test-model".into(),
                max_context_size: Some(16_000),
                max_output_size: Some(1_000),
                capabilities: Vec::new(),
                display_name: None,
                support_efforts: Vec::new(),
                default_effort: None,
                pricing: None,
                experimental_adaptive_thinking: false,
                experimental_visible_empty_retries: 0,
                experimental_bad_toolcall_auto_retries: 0,
                first_token_timeout_ms: None,
            },
        );
        let config = Arc::new(config);
        let (event_tx, mut event_rx) = mpsc::channel(64);
        let loop_ = AgentLoop::new(
            config,
            Arc::new(ToolRegistry::new()),
            Arc::new(Mutex::new(PermissionChain::new(
                PermissionMode::Auto,
                Vec::new(),
            ))),
            event_tx,
            Arc::new(Mutex::new(HashMap::new())),
        );
        let workspace =
            std::env::temp_dir().join(format!("kkagent-retry-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&workspace).unwrap();
        let mut session = Session::new(
            "retry-test".into(),
            workspace.clone(),
            PermissionMode::Auto,
            "test/model".into(),
        );
        session.add_user_message("hello".into());

        loop_.run_turn(&mut session).await.unwrap();
        let mut errors = Vec::new();
        let mut retries = Vec::new();
        while let Ok(event) = event_rx.try_recv() {
            match event {
                AgentEvent::Error { message, .. } => errors.push(message),
                AgentEvent::LlmRetry {
                    retry_number,
                    remaining_seconds,
                    initial,
                    ..
                } => retries.push((retry_number, remaining_seconds, initial)),
                _ => {}
            }
        }
        assert!(errors.is_empty());
        assert_eq!(retries, vec![(1, 0, true), (2, 0, true)]);
        assert!(session.messages.iter().any(|message| {
            message
                .content
                .iter()
                .any(|content| matches!(content, ChatContent::Text { text } if text == "recovered"))
        }));
        std::fs::remove_dir_all(workspace).unwrap();
    }

    #[tokio::test]
    async fn auto_retries_bad_toolcall_then_recovers_when_feature_enabled() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let bad_id = "functions.BadTool:0".to_string();
        let error_body = format!(
            "HTTP 400 Bad Request: status_code=400, Assistant tool call {bad_id}.arguments must be a JSON object."
        );
        tokio::spawn(async move {
            for attempt in 1..=2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0_u8; 8192];
                let _ = socket.read(&mut request).await.unwrap();
                let (status, content_type, body) = if attempt == 1 {
                    ("400 Bad Request", "application/json", error_body.clone())
                } else {
                    (
                        "200 OK",
                        "text/event-stream",
                        "data: {\"choices\":[{\"delta\":{\"content\":\"recovered\"}}]}\n\
                         data: [DONE]\n"
                            .to_string(),
                    )
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });

        let mut config = AppConfig {
            default_model: Some("test/model".into()),
            loop_control: Some(LoopControlConfig {
                max_attempts_per_step: 3,
                rate_limit_retry_base_seconds: 0,
                reserved_context_size: 1_000,
                max_steps_per_turn: 4,
                auto_compact: true,
                compact_keep_last: 4,
                token_counting: "estimated".into(),
                ..Default::default()
            }),
            ..AppConfig::default()
        };
        config.providers.insert(
            "test".into(),
            ProviderConfig {
                provider_type: "openai-chat".into(),
                api_key: Some("token".into()),
                base_url: Some(base_url),
                custom_headers: HashMap::new(),
                oauth: None,
                first_token_timeout_ms: None,
            },
        );
        config.models.insert(
            "test/model".into(),
            ModelConfig {
                provider: "test".into(),
                model: "test-model".into(),
                max_context_size: Some(16_000),
                max_output_size: Some(1_000),
                capabilities: Vec::new(),
                display_name: None,
                support_efforts: Vec::new(),
                default_effort: None,
                pricing: None,
                experimental_adaptive_thinking: false,
                experimental_visible_empty_retries: 0,
                experimental_bad_toolcall_auto_retries: 1,
                first_token_timeout_ms: None,
            },
        );
        let config = Arc::new(config);
        let (event_tx, mut event_rx) = mpsc::channel(64);
        let loop_ = AgentLoop::new(
            config,
            Arc::new(ToolRegistry::new()),
            Arc::new(Mutex::new(PermissionChain::new(
                PermissionMode::Auto,
                Vec::new(),
            ))),
            event_tx,
            Arc::new(Mutex::new(HashMap::new())),
        );
        let workspace =
            std::env::temp_dir().join(format!("kkagent-badtoolcall-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&workspace).unwrap();
        let mut session = Session::new(
            "badtoolcall-test".into(),
            workspace.clone(),
            PermissionMode::Auto,
            "test/model".into(),
        );
        // Pre-populate the transcript with a malformed tool call so the rollback
        // path has something to roll back when the provider rejects it.
        session.messages = vec![
            ChatMessage {
                role: "user".into(),
                content: vec![ChatContent::Text {
                    text: "work".into(),
                }],
            },
            ChatMessage {
                role: "assistant".into(),
                content: vec![
                    ChatContent::Text {
                        text: "checking".into(),
                    },
                    ChatContent::ToolUse {
                        id: bad_id.clone(),
                        name: "BadTool".into(),
                        input: serde_json::Value::Null,
                    },
                ],
            },
            ChatMessage {
                role: "user".into(),
                content: vec![ChatContent::ToolResult {
                    tool_use_id: bad_id.clone(),
                    content: "invalid arguments".into(),
                    is_error: true,
                }],
            },
        ];

        loop_.run_turn(&mut session).await.unwrap();

        let mut saw_retry_notice = false;
        let mut saw_error = false;
        let mut saw_text = false;
        while let Ok(event) = event_rx.try_recv() {
            match event {
                AgentEvent::LlmRetry { reason, .. } if reason.contains("malformed tool call") => {
                    saw_retry_notice = true;
                }
                AgentEvent::Error { .. } => saw_error = true,
                AgentEvent::MessageDelta { text, .. } if text.contains("recovered") => {
                    saw_text = true;
                }
                _ => {}
            }
        }
        assert!(
            saw_retry_notice,
            "expected a retry notice for the bad tool call"
        );
        assert!(!saw_error, "turn should not end in error after auto-retry");
        assert!(saw_text, "expected the recovered text after retry");
        std::fs::remove_dir_all(workspace).unwrap();
    }

    #[tokio::test]
    async fn falls_back_only_after_primary_and_fallback_retry_budgets() {
        let primary_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let primary_url = format!("http://{}", primary_listener.local_addr().unwrap());
        let primary_server = tokio::spawn(async move {
            for _ in 0..2 {
                let (mut socket, _) = primary_listener.accept().await.unwrap();
                let mut request = [0_u8; 8192];
                let _ = socket.read(&mut request).await.unwrap();
                let body = "primary unavailable";
                let response = format!(
                    "HTTP/1.1 503 Service Unavailable\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });

        let fallback_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let fallback_url = format!("http://{}", fallback_listener.local_addr().unwrap());
        let fallback_server = tokio::spawn(async move {
            for attempt in 1..=2 {
                let (mut socket, _) = fallback_listener.accept().await.unwrap();
                let mut request = [0_u8; 8192];
                let _ = socket.read(&mut request).await.unwrap();
                let (status, content_type, body) = if attempt == 1 {
                    (
                        "503 Service Unavailable",
                        "application/json",
                        "fallback retry",
                    )
                } else {
                    (
                        "200 OK",
                        "text/event-stream",
                        "data: {\"choices\":[{\"delta\":{\"content\":\"fallback recovered\"}}]}\n\
                         data: [DONE]\n",
                    )
                };
                let response = format!(
                    "HTTP/1.1 {status}\r\ncontent-type: {content_type}\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });

        let mut config = AppConfig {
            default_model: Some("primary".into()),
            fallback_model: Some("fallback".into()),
            loop_control: Some(LoopControlConfig {
                max_attempts_per_step: 2,
                rate_limit_retry_base_seconds: 0,
                reserved_context_size: 1_000,
                max_steps_per_turn: 1,
                auto_compact: false,
                compact_keep_last: 4,
                token_counting: "estimated".into(),
                ..Default::default()
            }),
            ..AppConfig::default()
        };
        for (name, base_url) in [("primary", primary_url), ("fallback", fallback_url)] {
            config.providers.insert(
                name.into(),
                ProviderConfig {
                    provider_type: "openai-chat".into(),
                    api_key: Some("token".into()),
                    base_url: Some(base_url),
                    custom_headers: HashMap::new(),
                    oauth: None,
                    first_token_timeout_ms: None,
                },
            );
            config.models.insert(
                name.into(),
                ModelConfig {
                    provider: name.into(),
                    model: format!("{name}-model"),
                    max_context_size: Some(16_000),
                    max_output_size: Some(1_000),
                    capabilities: Vec::new(),
                    display_name: None,
                    support_efforts: Vec::new(),
                    default_effort: None,
                    pricing: None,
                    experimental_adaptive_thinking: false,
                    experimental_visible_empty_retries: 0,
                    experimental_bad_toolcall_auto_retries: 0,
                    first_token_timeout_ms: None,
                },
            );
        }

        let (event_tx, mut event_rx) = mpsc::channel(64);
        let loop_ = AgentLoop::new(
            Arc::new(config),
            Arc::new(ToolRegistry::new()),
            Arc::new(Mutex::new(PermissionChain::new(
                PermissionMode::Auto,
                Vec::new(),
            ))),
            event_tx,
            Arc::new(Mutex::new(HashMap::new())),
        );
        let workspace =
            std::env::temp_dir().join(format!("kkagent-fallback-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&workspace).unwrap();
        let mut session = Session::new(
            "fallback-test".into(),
            workspace.clone(),
            PermissionMode::Auto,
            "primary".into(),
        );
        session.add_user_message("recover this step".into());

        loop_.run_turn(&mut session).await.unwrap();
        primary_server.await.unwrap();
        fallback_server.await.unwrap();
        let retry_reasons = std::iter::from_fn(|| event_rx.try_recv().ok())
            .filter_map(|event| match event {
                AgentEvent::LlmRetry {
                    reason, initial, ..
                } if initial => Some(reason),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(retry_reasons.len(), 3);
        assert!(retry_reasons[1].contains("switching to fallback fallback"));
        assert_eq!(session.get_model_alias(), "primary");
        assert!(session.messages.iter().any(|message| {
            message.content.iter().any(
                |content| matches!(content, ChatContent::Text { text } if text == "fallback recovered"),
            )
        }));
        std::fs::remove_dir_all(workspace).unwrap();
    }

    #[tokio::test]
    async fn returns_error_only_after_fallback_also_fails() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            for body in ["primary failed", "fallback failed"] {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0_u8; 8192];
                let _ = socket.read(&mut request).await.unwrap();
                let response = format!(
                    "HTTP/1.1 503 Service Unavailable\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });

        let mut config = AppConfig {
            default_model: Some("primary".into()),
            fallback_model: Some("fallback".into()),
            loop_control: Some(LoopControlConfig {
                max_attempts_per_step: 1,
                max_steps_per_turn: 1,
                auto_compact: false,
                ..Default::default()
            }),
            ..AppConfig::default()
        };
        config.providers.insert(
            "test".into(),
            ProviderConfig {
                provider_type: "openai-chat".into(),
                api_key: Some("token".into()),
                base_url: Some(base_url),
                custom_headers: HashMap::new(),
                oauth: None,
                first_token_timeout_ms: None,
            },
        );
        for alias in ["primary", "fallback"] {
            config.models.insert(
                alias.into(),
                ModelConfig {
                    provider: "test".into(),
                    model: alias.into(),
                    max_context_size: Some(16_000),
                    max_output_size: Some(1_000),
                    capabilities: Vec::new(),
                    display_name: None,
                    support_efforts: Vec::new(),
                    default_effort: None,
                    pricing: None,
                    experimental_adaptive_thinking: false,
                    experimental_visible_empty_retries: 0,
                    experimental_bad_toolcall_auto_retries: 0,
                    first_token_timeout_ms: None,
                },
            );
        }
        let (event_tx, _) = mpsc::channel(16);
        let loop_ = AgentLoop::new(
            Arc::new(config),
            Arc::new(ToolRegistry::new()),
            Arc::new(Mutex::new(PermissionChain::new(
                PermissionMode::Auto,
                Vec::new(),
            ))),
            event_tx,
            Arc::new(Mutex::new(HashMap::new())),
        );
        let workspace =
            std::env::temp_dir().join(format!("kkagent-fallback-error-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&workspace).unwrap();
        let mut session = Session::new(
            "fallback-error-test".into(),
            workspace.clone(),
            PermissionMode::Auto,
            "primary".into(),
        );
        session.add_user_message("fail only after both models".into());

        let error = loop_.run_turn(&mut session).await.unwrap_err();
        server.await.unwrap();
        assert!(error.to_string().contains("fallback failed"));
        std::fs::remove_dir_all(workspace).unwrap();
    }

    #[tokio::test]
    async fn steer_arriving_during_stream_continues_with_another_model_step() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let workspace =
            std::env::temp_dir().join(format!("kkagent-steer-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&workspace).unwrap();
        let mut session = Session::new(
            "steer-test".into(),
            workspace.clone(),
            PermissionMode::Auto,
            "test/model".into(),
        );
        session.add_user_message("start work".into());
        let mailbox = session.steer_mailbox.clone();

        let server = tokio::spawn(async move {
            for step in 0..2 {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = Vec::new();
                loop {
                    let mut chunk = [0_u8; 8_192];
                    let read = socket.read(&mut chunk).await.unwrap();
                    if read == 0 {
                        break;
                    }
                    request.extend_from_slice(&chunk[..read]);
                    let Some(header_end) = request.windows(4).position(|w| w == b"\r\n\r\n") else {
                        continue;
                    };
                    let headers = String::from_utf8_lossy(&request[..header_end]);
                    let content_len = headers
                        .lines()
                        .find_map(|line| {
                            line.to_ascii_lowercase()
                                .strip_prefix("content-length:")
                                .and_then(|value| value.trim().parse::<usize>().ok())
                        })
                        .unwrap_or(0);
                    if request.len() >= header_end + 4 + content_len {
                        break;
                    }
                }
                let request = String::from_utf8_lossy(&request);
                if step == 0 {
                    mailbox
                        .try_push(crate::session::SteerInput {
                            text: "focus on the failing test".into(),
                            images: Vec::new(),
                        })
                        .expect("the active turn should accept steer input");
                } else {
                    assert!(request.contains("focus on the failing test"));
                }
                let answer = if step == 0 { "initial" } else { "guided" };
                let body = format!(
                    "data: {{\"choices\":[{{\"delta\":{{\"content\":\"{answer}\"}}}}]}}\n\
                     data: [DONE]\n"
                );
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });

        let mut config = AppConfig {
            default_model: Some("test/model".into()),
            loop_control: Some(LoopControlConfig {
                max_attempts_per_step: 1,
                reserved_context_size: 1_000,
                max_steps_per_turn: 4,
                auto_compact: false,
                compact_keep_last: 4,
                token_counting: "estimated".into(),
                ..Default::default()
            }),
            ..AppConfig::default()
        };
        config.providers.insert(
            "test".into(),
            ProviderConfig {
                provider_type: "openai-chat".into(),
                api_key: Some("token".into()),
                base_url: Some(base_url),
                custom_headers: HashMap::new(),
                oauth: None,
                first_token_timeout_ms: None,
            },
        );
        config.models.insert(
            "test/model".into(),
            ModelConfig {
                provider: "test".into(),
                model: "test-model".into(),
                max_context_size: Some(16_000),
                max_output_size: Some(1_000),
                capabilities: Vec::new(),
                display_name: None,
                support_efforts: Vec::new(),
                default_effort: None,
                pricing: None,
                experimental_adaptive_thinking: false,
                experimental_visible_empty_retries: 0,
                experimental_bad_toolcall_auto_retries: 0,
                first_token_timeout_ms: None,
            },
        );
        let (event_tx, _) = mpsc::channel(64);
        let loop_ = AgentLoop::new(
            Arc::new(config),
            Arc::new(ToolRegistry::new()),
            Arc::new(Mutex::new(PermissionChain::new(
                PermissionMode::Auto,
                Vec::new(),
            ))),
            event_tx,
            Arc::new(Mutex::new(HashMap::new())),
        );

        loop_.run_turn(&mut session).await.unwrap();
        server.await.unwrap();
        assert!(!session.steer_mailbox.is_active());
        assert!(session.messages.iter().any(|message| {
            message
                .content
                .iter()
                .any(|content| matches!(content, ChatContent::Text { text } if text == "guided"))
        }));
        std::fs::remove_dir_all(workspace).unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn many_tool_steps_complete_without_recursive_stack_growth() {
        const TOOL_STEPS: usize = 32;
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            for step in 0..=TOOL_STEPS {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0_u8; 16_384];
                let _ = socket.read(&mut request).await.unwrap();
                let body = if step < TOOL_STEPS {
                    format!(
                        "data: {{\"choices\":[{{\"delta\":{{\"tool_calls\":[{{\"index\":0,\"id\":\"call-{step}\",\"function\":{{\"name\":\"MissingTool\",\"arguments\":\"{{\\\"step\\\":{step}}}\"}}}}]}}}}]}}\n\
                         data: [DONE]\n"
                    )
                } else {
                    "data: {\"choices\":[{\"delta\":{\"content\":\"completed\"}}]}\n\
                     data: [DONE]\n"
                        .into()
                };
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });

        let mut config = AppConfig {
            default_model: Some("test/model".into()),
            loop_control: Some(LoopControlConfig {
                max_attempts_per_step: 1,
                reserved_context_size: 1_000,
                max_steps_per_turn: 40,
                auto_compact: false,
                compact_keep_last: 4,
                token_counting: "estimated".into(),
                ..Default::default()
            }),
            ..AppConfig::default()
        };
        config.providers.insert(
            "test".into(),
            ProviderConfig {
                provider_type: "openai-chat".into(),
                api_key: Some("token".into()),
                base_url: Some(base_url),
                custom_headers: HashMap::new(),
                oauth: None,
                first_token_timeout_ms: None,
            },
        );
        config.models.insert(
            "test/model".into(),
            ModelConfig {
                provider: "test".into(),
                model: "test-model".into(),
                max_context_size: Some(1_000_000),
                max_output_size: Some(1_000),
                capabilities: vec!["tool_use".into()],
                display_name: None,
                support_efforts: Vec::new(),
                default_effort: None,
                pricing: None,
                experimental_adaptive_thinking: false,
                experimental_visible_empty_retries: 0,
                experimental_bad_toolcall_auto_retries: 0,
                first_token_timeout_ms: None,
            },
        );
        let (event_tx, _) = mpsc::channel(512);
        let loop_ = AgentLoop::new(
            Arc::new(config),
            Arc::new(ToolRegistry::new()),
            Arc::new(Mutex::new(PermissionChain::new(
                PermissionMode::Auto,
                Vec::new(),
            ))),
            event_tx,
            Arc::new(Mutex::new(HashMap::new())),
        );
        let workspace =
            std::env::temp_dir().join(format!("kkagent-many-steps-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&workspace).unwrap();
        let mut session = Session::new(
            "many-steps-test".into(),
            workspace.clone(),
            PermissionMode::Auto,
            "test/model".into(),
        );
        session.add_user_message("keep working".into());

        loop_.run_turn(&mut session).await.unwrap();
        server.await.unwrap();
        assert!(session.messages.iter().any(|message| {
            message
                .content
                .iter()
                .any(|content| matches!(content, ChatContent::Text { text } if text == "completed"))
        }));
        std::fs::remove_dir_all(workspace).unwrap();
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn visible_empty_after_tool_result_is_retried_when_experimental_flag_is_enabled() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let bodies = [
                "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call-1\",\"function\":{\"name\":\"MissingTool\",\"arguments\":\"{}\"}}]}}]}\n\
                 data: [DONE]\n",
                "data: {\"choices\":[{\"delta\":{\"reasoning_content\":\"still thinking\"}}]}\n\
                 data: [DONE]\n",
                "data: {\"choices\":[{\"delta\":{\"content\":\"completed after retry\"}}]}\n\
                 data: [DONE]\n",
            ];
            for body in bodies {
                let (mut socket, _) = listener.accept().await.unwrap();
                let mut request = [0_u8; 16_384];
                let _ = socket.read(&mut request).await.unwrap();
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                    body.len()
                );
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });

        let mut config = AppConfig {
            default_model: Some("test/model".into()),
            loop_control: Some(LoopControlConfig {
                max_attempts_per_step: 1,
                reserved_context_size: 1_000,
                max_steps_per_turn: 4,
                auto_compact: false,
                compact_keep_last: 4,
                token_counting: "estimated".into(),
                ..Default::default()
            }),
            ..AppConfig::default()
        };
        config.providers.insert(
            "test".into(),
            ProviderConfig {
                provider_type: "openai-chat".into(),
                api_key: Some("token".into()),
                base_url: Some(base_url),
                custom_headers: HashMap::new(),
                oauth: None,
                first_token_timeout_ms: None,
            },
        );
        config.models.insert(
            "test/model".into(),
            ModelConfig {
                provider: "test".into(),
                model: "test-model".into(),
                max_context_size: Some(16_000),
                max_output_size: Some(1_000),
                capabilities: vec!["tool_use".into()],
                display_name: None,
                support_efforts: Vec::new(),
                default_effort: None,
                pricing: None,
                experimental_adaptive_thinking: false,
                experimental_visible_empty_retries: 1,
                experimental_bad_toolcall_auto_retries: 0,
                first_token_timeout_ms: None,
            },
        );
        let (event_tx, _) = mpsc::channel(64);
        let loop_ = AgentLoop::new(
            Arc::new(config),
            Arc::new(ToolRegistry::new()),
            Arc::new(Mutex::new(PermissionChain::new(
                PermissionMode::Auto,
                Vec::new(),
            ))),
            event_tx,
            Arc::new(Mutex::new(HashMap::new())),
        );
        let workspace = std::env::temp_dir().join(format!(
            "kkagent-visible-empty-retry-{}",
            uuid::Uuid::new_v4()
        ));
        std::fs::create_dir_all(&workspace).unwrap();
        let mut session = Session::new(
            "visible-empty-retry-test".into(),
            workspace.clone(),
            PermissionMode::Auto,
            "test/model".into(),
        );
        session.add_user_message("use a tool, then finish".into());

        loop_.run_turn(&mut session).await.unwrap();
        server.await.unwrap();
        assert!(session.messages.iter().any(|message| {
            message.content.iter().any(
                |content| matches!(content, ChatContent::Text { text } if text == "completed after retry"),
            )
        }));
        assert!(!session.messages.iter().any(|message| {
            message.content.iter().any(
                |content| matches!(content, ChatContent::Thinking { thinking } if thinking == "still thinking"),
            )
        }));
        assert_eq!(
            session
                .messages
                .iter()
                .flat_map(|message| &message.content)
                .filter(|content| matches!(content, ChatContent::ToolResult { .. }))
                .count(),
            1,
            "the recovery retry must not execute the completed tool again"
        );
        std::fs::remove_dir_all(workspace).unwrap();
    }

    #[tokio::test]
    async fn provider_argument_object_error_rolls_back_and_stops_without_retry() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base_url = format!("http://{}", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = [0_u8; 8192];
            let _ = socket.read(&mut request).await.unwrap();
            let body = "status_code=400, Assistant tool call functions.BadTool:0.arguments must be a JSON object.";
            let response = format!(
                "HTTP/1.1 400 Bad Request\r\ncontent-type: text/plain\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
        });

        let mut config = AppConfig {
            default_model: Some("test/model".into()),
            loop_control: Some(LoopControlConfig {
                max_attempts_per_step: 3,
                reserved_context_size: 1_000,
                max_steps_per_turn: 4,
                auto_compact: true,
                compact_keep_last: 4,
                token_counting: "estimated".into(),
                ..Default::default()
            }),
            ..AppConfig::default()
        };
        config.providers.insert(
            "test".into(),
            ProviderConfig {
                provider_type: "openai-chat".into(),
                api_key: Some("token".into()),
                base_url: Some(base_url),
                custom_headers: HashMap::new(),
                oauth: None,
                first_token_timeout_ms: None,
            },
        );
        config.models.insert(
            "test/model".into(),
            ModelConfig {
                provider: "test".into(),
                model: "test-model".into(),
                max_context_size: Some(16_000),
                max_output_size: Some(1_000),
                capabilities: vec!["tool_use".into()],
                display_name: None,
                support_efforts: Vec::new(),
                default_effort: None,
                pricing: None,
                experimental_adaptive_thinking: false,
                experimental_visible_empty_retries: 0,
                experimental_bad_toolcall_auto_retries: 0,
                first_token_timeout_ms: None,
            },
        );
        let (event_tx, mut event_rx) = mpsc::channel(64);
        let loop_ = AgentLoop::new(
            Arc::new(config),
            Arc::new(ToolRegistry::new()),
            Arc::new(Mutex::new(PermissionChain::new(
                PermissionMode::Auto,
                Vec::new(),
            ))),
            event_tx,
            Arc::new(Mutex::new(HashMap::new())),
        );
        let workspace =
            std::env::temp_dir().join(format!("kkagent-tool-rollback-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&workspace).unwrap();
        let mut session = Session::new(
            "tool-rollback-test".into(),
            workspace.clone(),
            PermissionMode::Auto,
            "test/model".into(),
        );
        session.messages = vec![
            ChatMessage {
                role: "user".into(),
                content: vec![ChatContent::Text {
                    text: "do work".into(),
                }],
            },
            ChatMessage {
                role: "assistant".into(),
                content: vec![ChatContent::ToolUse {
                    id: "functions.BadTool:0".into(),
                    name: "BadTool".into(),
                    input: serde_json::Value::Null,
                }],
            },
            ChatMessage {
                role: "user".into(),
                content: vec![ChatContent::ToolResult {
                    tool_use_id: "functions.BadTool:0".into(),
                    content: "invalid arguments".into(),
                    is_error: true,
                }],
            },
        ];

        loop_.run_turn(&mut session).await.unwrap();
        server.await.unwrap();
        assert_eq!(session.messages.len(), 1);
        assert!(session.transcript_rewrite_required);
        let mut recovery_errors = Vec::new();
        let mut saw_idle = false;
        let mut saw_turn_end = false;
        while let Ok(event) = event_rx.try_recv() {
            match event {
                AgentEvent::Error { message, .. } => recovery_errors.push(message),
                AgentEvent::StatusUpdate {
                    status: SessionStatus::Idle,
                    ..
                } => saw_idle = true,
                AgentEvent::TurnEnd { .. } => saw_turn_end = true,
                _ => {}
            }
        }
        assert_eq!(recovery_errors.len(), 1);
        assert!(recovery_errors[0].contains("discarded that micro-step"));
        assert!(saw_idle);
        assert!(saw_turn_end);
        std::fs::remove_dir_all(workspace).unwrap();
    }

    struct ProbingTool {
        probe: std::sync::Arc<dyn Fn() + Send + Sync>,
    }

    #[async_trait::async_trait]
    impl kkagent_tools::Tool for ProbingTool {
        fn name(&self) -> &str {
            "Probe"
        }
        fn description(&self) -> &str {
            "records a probe observation"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({
                "type": "object",
                "properties": {},
            })
        }
        fn read_only(&self) -> bool {
            true
        }
        async fn execute(
            &self,
            _input: serde_json::Value,
            _ctx: &kkagent_tools::ToolContext,
        ) -> anyhow::Result<kkagent_tools::ToolOutput> {
            (self.probe)();
            Ok(kkagent_tools::ToolOutput::success("probed"))
        }
    }

    fn sse(body: &str) -> String {
        format!(
            "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
            body.len()
        )
    }

    /// Per-message persistence: within a running turn, every complete message
    /// (assistant step, tool result) must already be in the transcript DB at
    /// the moment the next step executes — the crash-recovery contract.
    #[tokio::test]
    async fn per_message_transcript_persistence_within_turn() {
        // Round 1: text + tool_use (chunked stream).
        let round1 = sse(&[
            "data: {\"choices\":[{\"delta\":{\"content\":\"running probe\"}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call-1\",\"type\":\"function\",\"function\":{\"name\":\"Probe\",\"arguments\":\"{}\"}}]}}]}\n\n",
            "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"tool_calls\"}]}\n\n",
            "data: [DONE]\n\n",
        ].concat());
        // Round 2: final text.
        let round2 = sse(&[
            "data: {\"choices\":[{\"delta\":{\"content\":\"all done\"}}]}\n\n",
            "data: [DONE]\n\n",
        ]
        .concat());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let mut round = 0;
            while let Ok((mut socket, _)) = listener.accept().await {
                round += 1;
                let mut buf = vec![0u8; 8192];
                let _ = tokio::time::timeout(std::time::Duration::from_secs(2), async {
                    use tokio::io::AsyncReadExt;
                    let _ = socket.read(&mut buf).await;
                })
                .await;
                let response = if round == 1 {
                    round1.clone()
                } else {
                    round2.clone()
                };
                use tokio::io::AsyncWriteExt;
                socket.write_all(response.as_bytes()).await.unwrap();
            }
        });

        let mut config = AppConfig {
            default_model: Some("test/model".into()),
            loop_control: Some(kkagent_config::LoopControlConfig {
                max_attempts_per_step: 3,
                max_steps_per_turn: 10,
                auto_compact: false,
                ..Default::default()
            }),
            ..AppConfig::default()
        };
        config.providers.insert(
            "test".into(),
            ProviderConfig {
                provider_type: "openai-chat".into(),
                api_key: Some("token".into()),
                base_url: Some(format!("http://{addr}")),
                custom_headers: HashMap::new(),
                oauth: None,
                first_token_timeout_ms: None,
            },
        );
        config.models.insert(
            "test/model".into(),
            ModelConfig {
                provider: "test".into(),
                model: "test-model".into(),
                max_context_size: Some(16_000),
                max_output_size: Some(1_000),
                capabilities: vec!["tool_use".into()],
                display_name: None,
                support_efforts: Vec::new(),
                default_effort: None,
                pricing: None,
                experimental_adaptive_thinking: false,
                experimental_visible_empty_retries: 0,
                experimental_bad_toolcall_auto_retries: 0,
                first_token_timeout_ms: None,
            },
        );
        let config = Arc::new(config);

        let db = crate::transcript::TranscriptDb::open_in_memory().unwrap();

        // Probe runs inside the tool-execution step: assert the DB already
        // contains the user prompt + the complete assistant step (with
        // thinking, text and tool_use) — chunk-level data never lands, but
        // complete messages do.
        let seen_in_db = Arc::new(std::sync::Mutex::new(Vec::<(String, usize)>::new()));
        let seen_clone = seen_in_db.clone();
        let db_for_probe = db.clone();
        let probe = Arc::new(move || {
            let records = db_for_probe
                .load_messages("persist-step-test")
                .expect("db read in probe");
            let summary: Vec<(String, usize)> = records
                .iter()
                .map(|r| (r.role.clone(), r.content_json.len()))
                .collect();
            *seen_clone.lock().unwrap() = summary;
        });

        let mut registry = kkagent_tools::ToolRegistry::new();
        registry.register(Arc::new(ProbingTool { probe }));

        let (event_tx, mut event_rx) = mpsc::channel(256);
        let loop_ = AgentLoop::new(
            config,
            Arc::new(registry),
            Arc::new(Mutex::new(PermissionChain::new(
                PermissionMode::Auto,
                Vec::new(),
            ))),
            event_tx,
            Arc::new(Mutex::new(HashMap::new())),
        )
        .with_transcript_db(db.clone());

        let workspace =
            std::env::temp_dir().join(format!("kkagent-persist-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&workspace).unwrap();
        db.create_session(
            "persist-step-test",
            "test/model",
            workspace.to_str().unwrap(),
        )
        .unwrap();
        let mut session = Session::new(
            "persist-step-test".into(),
            workspace.clone(),
            PermissionMode::Auto,
            "test/model".into(),
        );
        session.add_user_message("go".into());

        loop_.run_turn(&mut session).await.unwrap();
        server.abort();

        // Diagnostic: if the mid-turn probe never fired, dump the event log.
        let mut events: Vec<String> = Vec::new();
        while let Ok(event) = event_rx.try_recv() {
            let discriminant = match event {
                AgentEvent::ThinkingDelta { .. } => "ThinkingDelta",
                AgentEvent::MessageDelta { .. } => "MessageDelta",
                AgentEvent::ToolCall { ref tool_name, .. } => {
                    events.push(format!("ToolCall:{tool_name}"));
                    continue;
                }
                AgentEvent::ToolResult { .. } => "ToolResult",
                AgentEvent::TurnEnd { .. } => "TurnEnd",
                AgentEvent::Error { ref message, .. } => {
                    events.push(format!("Error:{message}"));
                    continue;
                }
                AgentEvent::StatusUpdate { .. } => "StatusUpdate",
                _ => "Other",
            };
            events.push(discriminant.into());
        }
        assert!(
            events.iter().any(|e| e.starts_with("ToolCall")),
            "probe tool was never called; events: {events:?}"
        );

        // During the tool step the DB held user + assistant(tool_use).
        let mid_turn = seen_in_db.lock().unwrap().clone();
        assert!(
            mid_turn.len() >= 2,
            "assistant step must be persisted before tool executes, got {mid_turn:?}"
        );
        assert_eq!(mid_turn[0].0, "user");
        assert_eq!(mid_turn[1].0, "assistant");
        assert!(
            mid_turn[1].1 > 50,
            "assistant step must contain full content, got len {}",
            mid_turn[1].1
        );

        // After the turn, DB also holds the tool result and final assistant.
        let final_records = db.load_messages("persist-step-test").unwrap();
        let roles: Vec<&str> = final_records.iter().map(|r| r.role.as_str()).collect();
        assert_eq!(roles, vec!["user", "assistant", "user", "assistant"]);
        assert!(final_records[1].content_json.contains("running probe"));
        assert!(final_records[1].content_json.contains("call-1"));
        assert!(final_records[1].content_json.contains("Probe"));
        assert!(final_records[2].content_json.contains("call-1"));
        assert!(final_records[3].content_json.contains("all done"));
        std::fs::remove_dir_all(workspace).unwrap();
    }
}
