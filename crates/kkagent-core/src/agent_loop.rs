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
use tokio::task::AbortHandle;

use crate::context_projector::{fold_old_media, project, project_strict, ProjectOptions};
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
use crate::tool_scheduler::{box_start, ToolCallTask, ToolScheduler};

pub struct AgentLoop {
    config: Arc<AppConfig>,
    tools: Arc<ToolRegistry>,
    permission: Arc<Mutex<PermissionChain>>,
    event_tx: mpsc::Sender<AgentEvent>,
    abort_registry: Arc<Mutex<HashMap<String, AbortHandle>>>,
    /// Max LLM rounds per top-level run_turn (tool recursion counts).
    max_rounds: u32,
    hooks: Option<Arc<kkagent_mcp::HookManager>>,
    goal_mgr: Option<Arc<GoalManager>>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TurnStep {
    Continue,
    Done,
}

const TOOL_RESULT_INLINE_MAX: usize = 32_000;

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
            .unwrap_or(64);
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
            max_rounds: max_rounds.max(1),
            hooks: None,
            goal_mgr: None,
        }
    }

    pub fn run_turn<'a>(
        &'a self,
        session: &'a mut Session,
    ) -> futures::future::BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move {
            let mut rounds_left = self.max_rounds;
            loop {
                match self.run_turn_step(session).await? {
                    TurnStep::Done => return Ok(()),
                    TurnStep::Continue if rounds_left <= 1 => {
                        tracing::warn!("Agent turn limit reached for session {}", session.id);
                        self.finish_turn(session, true).await?;
                        return Ok(());
                    }
                    TurnStep::Continue => {
                        rounds_left -= 1;
                        tracing::info!("Continuing turn ({} rounds left)", rounds_left);
                    }
                }
            }
        })
    }

    async fn run_turn_step(&self, session: &mut Session) -> anyhow::Result<TurnStep> {
        session.image_config = self.config.image.clone();
        let session_id = session.id.clone();
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
        let (model_config, provider_config) = self
            .config
            .resolve_model(&model_alias)
            .ok_or_else(|| anyhow::anyhow!("Model '{}' not found", model_alias))?;
        tracing::info!(
            "Using model alias={} id={}",
            model_alias,
            model_config.model
        );
        let capability = ModelCapability::from_model(model_config);

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
                        .fail_goal("Goal budget exhausted (turns/tokens/wall-clock)")
                        .await;
                    session.add_user_message(format!(
                        "<system-reminder>\nActive goal budget exhausted \
(turns={}/{:?} tokens={}/{:?}). Stop autonomous work and summarize progress.\n</system-reminder>",
                        goal.turns_used,
                        goal.budget.turn_budget,
                        goal.tokens_used,
                        goal.budget.token_budget
                    ));
                    let _ = self
                        .event_tx
                        .send(AgentEvent::Error {
                            session_id: session_id.clone(),
                            message: "Goal budget exhausted".into(),
                        })
                        .await;
                    self.finish_turn(session, false).await?;
                    return Ok(TurnStep::Done);
                }
                if goal.status == kkagent_protocol::goal::GoalStatus::Active
                    && !session_has_goal_reminder(session)
                {
                    session.add_user_message(format!(
                        "<system-reminder>\nActive goal: {}\n\
Progress: turns={}/{:?} tokens={}/{:?}. Continue working toward this goal.\n</system-reminder>",
                        goal.description,
                        goal.turns_used,
                        goal.budget.turn_budget,
                        goal.tokens_used,
                        goal.budget.token_budget
                    ));
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
        let mut messages = self.prepare_messages(session, &tool_defs, &system_prompt);
        tracing::debug!("Conversation has {} messages (projected)", messages.len());

        let thinking = self.config.thinking.as_ref().and_then(|t| {
            if t.enabled || capability.thinking {
                Some(ThinkingParams {
                    budget_tokens: 10000,
                })
            } else {
                None
            }
        });

        let max_attempts = self
            .config
            .loop_control
            .as_ref()
            .map(|l| l.max_attempts_per_step)
            .unwrap_or(3)
            .max(1);

        let mut assistant_text = String::new();
        let mut thinking_text = String::new();
        let mut tool_calls: Vec<PendingToolCall> = Vec::new();
        let mut interrupted = false;
        let mut terminal_stream_error: Option<String> = None;
        let mut rejected_tool_call_recovery: Option<ToolCallRollback> = None;

        for attempt in 1..=max_attempts {
            assistant_text.clear();
            thinking_text.clear();
            tool_calls.clear();
            let mut stream_failed = false;
            let mut last_stream_error: Option<String> = None;

            let request = LlmRequest {
                model: model_config.model.clone(),
                messages: messages.clone(),
                tools: tool_defs.clone(),
                max_tokens: model_config.max_output_size.unwrap_or(8192) as u32,
                system: Some(system_prompt.clone()),
                thinking,
            };

            let (stream_tx, mut stream_rx) = mpsc::channel::<StreamEvent>(256);
            let provider = create_provider(provider_config, model_config)?;
            let stream_error_tx = stream_tx.clone();
            let handle = tokio::spawn(async move {
                if let Err(e) = provider.stream_chat(request, stream_tx).await {
                    tracing::error!("LLM stream error: {}", e);
                    let _ = stream_error_tx
                        .send(StreamEvent::Error(e.to_string()))
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
                        StreamEvent::MessageEnd { usage } => {
                            for (_, (mut tool, input)) in active_tools.drain() {
                                (tool.input, tool.input_error) = parse_tool_arguments(&input);
                                tool_calls.push(tool);
                            }
                            got_message_end = true;
                            tracing::debug!(
                                "Message end: in={} out={}",
                                usage.input_tokens,
                                usage.output_tokens
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
                            tracing::error!("Stream error: {}", msg);
                            last_stream_error = Some(msg);
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
                        rejected_tool_call_recovery = Some(rollback);
                        break;
                    }
                }
            }
            if failed
                && empty
                && attempt < max_attempts
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
                    .resolve_model(&session.get_model_alias())
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
                    continue;
                }
            }
            if failed && empty && attempt < max_attempts {
                tracing::warn!(
                    "LLM step retry {}/{} ({})",
                    attempt,
                    max_attempts,
                    last_stream_error
                        .as_deref()
                        .unwrap_or("empty/incomplete stream")
                );
                let exponent = attempt.saturating_sub(1).min(5);
                let delay_ms = 200_u64.saturating_mul(1_u64 << exponent);
                tokio::time::sleep(Duration::from_millis(delay_ms)).await;
                continue;
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
            }
            self.finish_interrupted(session).await?;
            return Ok(TurnStep::Done);
        }

        if let Some(rollback) = rejected_tool_call_recovery {
            let message = format!(
                "Model rejected malformed tool call `{}`; discarded that micro-step and stopped. Send `continue` to resume. ({})",
                rollback.tool_call_id,
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

                let decision = {
                    let perm = self.permission.lock().await;
                    perm.evaluate(
                        &tc.name,
                        &tc.input,
                        &session.working_dir,
                        session.plan_mode,
                        Some(&session.plan_file_path),
                    )
                };
                tracing::info!("Permission for {}: {:?}", tc.name, decision);

                let prep = match decision {
                    PermissionDecision::Approve => {
                        if tc.name == "AskUserQuestion" {
                            Prepared::Done(self.execute_tool(session, &tc.name, &tc.input).await)
                        } else if tc.name == "ExitPlanMode" {
                            Prepared::Done(self.auto_exit_plan_mode(session, &tc.input).await)
                        } else {
                            if tc.name == "Write" || tc.name == "Edit" {
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
                                    if response.scope
                                        == Some(kkagent_protocol::ApprovalScope::Session)
                                    {
                                        let mut perm = self.permission.lock().await;
                                        perm.record_session_approval(&tc.name, &tc.input);
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
                                _ => Prepared::Done(ToolOutput::error(
                                    "Tool call was rejected by user",
                                )),
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
                            kkagent_protocol::ApprovalDecision::Approved => {
                                if response.scope == Some(kkagent_protocol::ApprovalScope::Session)
                                {
                                    let mut perm = self.permission.lock().await;
                                    perm.record_session_approval(name, input);
                                }
                            }
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
                    let image = self.config.image.clone();
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
                            async move {
                                execute_tool_parallel(ParallelToolRequest {
                                    tools,
                                    hooks,
                                    working_dir,
                                    session_id: sid,
                                    image,
                                    enabled_tools: enabled,
                                    name,
                                    input,
                                    tool_call_id: Some(tool_call_id),
                                    interrupted,
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
            let mut resolved: Vec<(String, String, ToolOutput)> = Vec::new();
            for (i, (id, name, prep)) in prepared.into_iter().enumerate() {
                let output = match prep {
                    Prepared::Done(o) => o,
                    Prepared::Ready { .. } => {
                        let _ = ready_indices.contains(&i);
                        parallel_iter
                            .next()
                            .unwrap_or_else(|| Err("scheduler missing result".into()))
                            .unwrap_or_else(ToolOutput::error)
                    }
                };
                resolved.push((id, name, output));
            }

            let mut tool_results = Vec::new();
            for (id, name, output) in resolved {
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
                    session.plan_mode = true;
                    let _ = self
                        .event_tx
                        .send(AgentEvent::PlanModeChanged {
                            session_id: session_id.clone(),
                            enabled: true,
                        })
                        .await;
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

                let output = truncate_tool_output(session, &name, output);

                tracing::info!(
                    "Tool {} result: error={} len={}",
                    name,
                    output.is_error,
                    output.content.len()
                );

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

                if !output.is_error && name == "TodoList" {
                    session.turns_since_todo = 0;
                    if let Some(items) = todo_items_from_output(&output) {
                        let _ = self
                            .event_tx
                            .send(AgentEvent::TodoUpdated {
                                session_id: session_id.clone(),
                                items,
                            })
                            .await;
                    }
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

            // Steer / delivery messages land after tool results (next model turn).
            for msg in deliveries {
                session.add_user_message(msg);
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

        self.finish_turn(session, true).await?;
        Ok(TurnStep::Done)
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
                        goal_mgr.fail_goal("Goal budget exhausted after turn").await;
                    }
                }
            }
        }
        session.note_turn_completed();
        // Sync sticky todos into session services when present in tool results.
        if let Some(last) = session.messages.iter().rev().find(|m| {
            m.content
                .iter()
                .any(|c| matches!(c, kkagent_llm::ChatContent::ToolResult { .. }))
        }) {
            for part in &last.content {
                if let kkagent_llm::ChatContent::ToolResult { content, .. } = part {
                    if let Ok(v) = serde_json::from_str::<serde_json::Value>(content) {
                        if let Some(todos) = v.get("todos").or_else(|| v.get("items")) {
                            let items = crate::session::todo::parse_todo_items(todos);
                            if !items.is_empty() {
                                session.services.todos.set_todos(items);
                            }
                        }
                    }
                }
            }
        }
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

        let projected = project(&session.build_messages(), &ProjectOptions::default());
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
        session.undo_stack.clear();
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
        let projected = crate::context_memory::fold_loop_events(&session.build_messages());
        let mut messages = project(&projected, &opts);
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
            session.undo_stack.clear();
            let after = session
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
            messages = project(&session.build_messages(), &opts);
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
        let path = session.plan_file_path.display().to_string();
        let plan = tokio::fs::read_to_string(&session.plan_file_path)
            .await
            .unwrap_or_default();
        if plan.trim().is_empty() {
            return ToolOutput::error(format!(
                "No plan file found. Write your plan to {path} first, then call ExitPlanMode."
            ));
        }
        let _ = self
            .event_tx
            .send(AgentEvent::PlanFileUpdated {
                session_id: session.id.clone(),
                path: path.clone(),
                content: plan.clone(),
            })
            .await;
        let _ = input;
        session.plan_mode = false;
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
        let path = session.plan_file_path.display().to_string();
        let plan = tokio::fs::read_to_string(&session.plan_file_path)
            .await
            .unwrap_or_default();
        if plan.trim().is_empty() {
            return Ok(ToolOutput::error(format!(
                "No plan file found. Write your plan to {path} first, then call ExitPlanMode."
            )));
        }

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
        let _ = self
            .event_tx
            .send(AgentEvent::ApprovalRequested {
                session_id: session_id.to_string(),
                request: kkagent_protocol::ApprovalRequest {
                    approval_id: approval_id.clone(),
                    session_id: session_id.to_string(),
                    tool_call_id: tc.id.clone(),
                    tool_name: "ExitPlanMode".into(),
                    action: "Ready to build with this plan?".into(),
                    tool_input_display: Some(display.to_display_json()),
                    created_at: chrono::Utc::now(),
                },
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

        if response.decision == kkagent_protocol::ApprovalDecision::Cancelled {
            return Ok(ToolOutput::success(
                "Plan approval dismissed. Plan mode remains active.",
            ));
        }

        let (output, exit) = resolve_exit_plan_approval(&response, &display);
        if exit {
            session.plan_mode = false;
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
            image: self.config.image.clone(),
            enabled_tools: session.enabled_tools.clone(),
            name: name.to_string(),
            input: input.clone(),
            tool_call_id: None,
            interrupted: session.interrupted.clone(),
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
        let background = input
            .get("background")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

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

        if background {
            // Park without blocking the agent turn — deliver a steer when answered elsewhere.
            return ToolOutput::success(format!(
                "Background question parked (id={question_id}). Continue other work; the answer will arrive as a follow-up delivery."
            ))
            .with_delivery(format!(
                "<system>Background AskUserQuestion {question_id} is waiting for the user. Do not block on it.</system>"
            ));
        }

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

fn skill_activated_from_output(
    session_id: &str,
    output: &ToolOutput,
) -> Option<AgentEvent> {
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
            "SelectTools" | "AskUserQuestion" | "TodoList" | "ExitPlanMode" | "EnterPlanMode"
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
        }
    }
}

struct ParallelToolRequest {
    tools: Arc<ToolRegistry>,
    hooks: Option<Arc<kkagent_mcp::HookManager>>,
    working_dir: std::path::PathBuf,
    session_id: String,
    image: kkagent_config::ImageConfig,
    enabled_tools: Option<std::collections::HashSet<String>>,
    name: String,
    input: serde_json::Value,
    tool_call_id: Option<String>,
    interrupted: Arc<std::sync::atomic::AtomicBool>,
}

async fn execute_tool_parallel(request: ParallelToolRequest) -> ToolOutput {
    let ParallelToolRequest {
        tools,
        hooks,
        working_dir,
        session_id,
        image,
        enabled_tools,
        name,
        input,
        tool_call_id,
        interrupted,
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

    if let Err(e) = kkagent_tools::args_validator::validate_against_schema(
        &tool.parameters_schema(),
        &input,
    ) {
        return ToolOutput::error(e.message);
    }

    let ctx = ToolContext {
        working_dir,
        session_id,
        image,
        tool_call_id,
        interrupted: Some(interrupted),
    };

    match tool.execute(input, &ctx).await {
        Ok(output) => output,
        Err(e) => ToolOutput::error(format!("Tool execution error: {}", e)),
    }
}

fn truncate_tool_output(session: &Session, tool_name: &str, mut output: ToolOutput) -> ToolOutput {
    if output.content.len() <= TOOL_RESULT_INLINE_MAX {
        return output;
    }
    let full_len = output.content.chars().count();
    let preview: String = output.content.chars().take(4000).collect();
    output.content = match persist_tool_output(&session.working_dir, &output.content) {
        Ok(path) => format!(
            "{preview}\n\n… tool result truncated ({tool_name}, {full_len} chars). Full output saved to {} — use Read on that path if needed.",
            path.display()
        ),
        Err(error) => format!(
            "{preview}\n\n… tool result truncated ({tool_name}, {full_len} chars). The full output could not be saved: {error}"
        ),
    };
    output
}

fn persist_tool_output(
    working_dir: &std::path::Path,
    content: &str,
) -> Result<std::path::PathBuf, String> {
    let root = std::fs::canonicalize(working_dir)
        .map_err(|error| format!("workspace is unavailable ({error})"))?;
    let app_dir = root.join(".kkagent");
    reject_symlink(&app_dir)?;
    std::fs::create_dir_all(&app_dir)
        .map_err(|error| format!("cannot create .kkagent directory ({error})"))?;
    let dir = app_dir.join("tool-results");
    reject_symlink(&dir)?;
    std::fs::create_dir_all(&dir)
        .map_err(|error| format!("cannot create tool-results directory ({error})"))?;

    let canonical_dir = std::fs::canonicalize(&dir)
        .map_err(|error| format!("cannot resolve tool-results directory ({error})"))?;
    if !canonical_dir.starts_with(&root) {
        return Err("tool-results directory escapes the workspace".into());
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&app_dir, std::fs::Permissions::from_mode(0o700));
        let _ = std::fs::set_permissions(&canonical_dir, std::fs::Permissions::from_mode(0o700));
    }

    let path = canonical_dir.join(format!("{}.txt", uuid::Uuid::new_v4()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&path)
        .map_err(|error| format!("cannot create result file ({error})"))?;
    std::io::Write::write_all(&mut file, content.as_bytes())
        .map_err(|error| format!("cannot write result file ({error})"))?;
    Ok(path)
}

fn reject_symlink(path: &std::path::Path) -> Result<(), String> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_symlink() => Err(format!(
            "refusing symlinked output directory {}",
            path.display()
        )),
        Ok(_) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(format!("cannot inspect {} ({error})", path.display())),
    }
}

fn session_has_goal_reminder(session: &Session) -> bool {
    session.messages.iter().rev().take(6).any(|m| {
        m.content.iter().any(|c| match c {
            ChatContent::Text { text } => text.contains("Active goal:"),
            _ => false,
        })
    })
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
    fn persists_large_tool_results_inside_workspace() {
        let workspace =
            std::env::temp_dir().join(format!("kkagent-output-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&workspace).unwrap();
        let path = persist_tool_output(&workspace, "full output").unwrap();
        assert!(path.starts_with(std::fs::canonicalize(&workspace).unwrap()));
        assert_eq!(std::fs::read_to_string(path).unwrap(), "full output");
        std::fs::remove_dir_all(workspace).unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn refuses_symlinked_tool_result_directory() {
        use std::os::unix::fs::symlink;
        let base =
            std::env::temp_dir().join(format!("kkagent-output-link-{}", uuid::Uuid::new_v4()));
        let workspace = base.join("workspace");
        let outside = base.join("outside");
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        symlink(&outside, workspace.join(".kkagent")).unwrap();
        let error = persist_tool_output(&workspace, "must not escape").unwrap_err();
        assert!(error.contains("symlinked"));
        assert!(std::fs::read_dir(&outside).unwrap().next().is_none());
        std::fs::remove_dir_all(base).unwrap();
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
                ChatContent::Text { text } => text.contains("compacted to free up context")
                    || text.contains("Earlier conversation digest"),
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
        while let Ok(event) = event_rx.try_recv() {
            if let AgentEvent::Error { message, .. } = event {
                errors.push(message);
            }
        }
        assert!(errors.is_empty());
        assert!(session.messages.iter().any(|message| {
            message
                .content
                .iter()
                .any(|content| matches!(content, ChatContent::Text { text } if text == "recovered"))
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
}
