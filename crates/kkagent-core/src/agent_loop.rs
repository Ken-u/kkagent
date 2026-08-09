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

use crate::context_projector::{compact_messages, project, project_strict, ProjectOptions};
use crate::model_capability::ModelCapability;
use crate::permission::{PermissionChain, PermissionDecision};
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
        self.run_turn_inner(session, self.max_rounds)
    }

    fn run_turn_inner<'a>(
        &'a self,
        session: &'a mut Session,
        rounds_left: u32,
    ) -> futures::future::BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(async move { self.run_turn_body(session, rounds_left).await })
    }

    async fn run_turn_body(&self, session: &mut Session, rounds_left: u32) -> anyhow::Result<()> {
        let session_id = session.id.clone();
        tracing::info!("Starting turn for session {}", session_id);

        if session.is_interrupted() {
            return self.finish_interrupted(session).await;
        }

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

        // Keep permission chain in sync with session mode (/yolo /auto toggles)
        {
            let mut perm = self.permission.lock().await;
            perm.set_mode(session.permission_mode);
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
                    return self.finish_turn(session, false).await;
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
            .filter(|td| tool_allowed(session, &td.name))
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
                    &serde_json::json!({"session_id": session_id}),
                )
                .await;
        }

        let system_prompt = session.effective_system_prompt();
        let messages = self.prepare_messages(session, &tool_defs, &system_prompt);
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
                                tool.input = serde_json::from_str(&input)
                                    .unwrap_or(serde_json::Value::String(input));
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
                                tool.input = serde_json::from_str(&input)
                                    .unwrap_or(serde_json::Value::String(input));
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
            return self.finish_interrupted(session).await;
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
                return self
                    .run_turn_inner(session, rounds_left.saturating_sub(1))
                    .await;
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
                    return self.finish_interrupted(session).await;
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

                        let response = session.wait_approval(&approval_id).await;
                        match response.decision {
                            kkagent_protocol::ApprovalDecision::Approved => {
                                if response.scope == Some(kkagent_protocol::ApprovalScope::Session)
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
                                            let path =
                                                if std::path::Path::new(path_str).is_absolute() {
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
                                return self.finish_interrupted(session).await;
                            }
                            _ => {
                                Prepared::Done(ToolOutput::error("Tool call was rejected by user"))
                            }
                        }
                    }
                    PermissionDecision::Deny(reason) => {
                        Prepared::Done(ToolOutput::error(format!("Denied: {}", reason)))
                    }
                };
                prepared.push((tc.id.clone(), tc.name.clone(), prep));
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
                // ExitPlanMode success → leave plan mode
                if name == "ExitPlanMode" && !output.is_error {
                    session.plan_mode = false;
                    let _ = self
                        .event_tx
                        .send(AgentEvent::PlanModeChanged {
                            session_id: session_id.clone(),
                            enabled: false,
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

                if !output.is_error && session.plan_mode && (name == "Write" || name == "Edit") {
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

            let result_content: Vec<ChatContent> = tool_results
                .iter()
                .map(|(id, output)| ChatContent::ToolResult {
                    tool_use_id: id.clone(),
                    content: output.content.clone(),
                    is_error: output.is_error,
                })
                .collect();

            session.messages.push(ChatMessage {
                role: "user".into(),
                content: result_content,
            });

            if session.is_interrupted() {
                return self.finish_interrupted(session).await;
            }

            if dedupe_force_stop {
                tracing::warn!("Tool dedupe force-stop after repeated identical calls");
                return self.finish_turn(session, true).await;
            }

            tracing::info!(
                "Recursing into next turn ({} rounds left)",
                rounds_left.saturating_sub(1)
            );
            if rounds_left <= 1 {
                tracing::warn!("Agent turn limit reached for session {}", session_id);
                return self.finish_turn(session, true).await;
            }
            return self.run_turn_inner(session, rounds_left - 1).await;
        }

        self.finish_turn(session, true).await
    }

    async fn finish_interrupted(&self, session: &mut Session) -> anyhow::Result<()> {
        let session_id = session.id.clone();
        tracing::info!("Turn interrupted for session {}", session_id);
        session.note_turn_cancelled();
        let _ = self
            .event_tx
            .send(AgentEvent::Error {
                session_id: session_id.clone(),
                message: "Interrupted".into(),
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
                    &serde_json::json!({"session_id": session_id, "interrupted": true}),
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
                    &serde_json::json!({"session_id": session_id}),
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

    /// Project / auto-compact messages to fit model context budget.
    fn prepare_messages(
        &self,
        session: &mut Session,
        tools: &[ToolDef],
        system: &str,
    ) -> Vec<ChatMessage> {
        let reserved = self
            .config
            .loop_control
            .as_ref()
            .map(|l| l.reserved_context_size)
            .unwrap_or(50_000);
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

        let max_context = {
            let alias = session.get_model_alias();
            self.config
                .resolve_model(&alias)
                .and_then(|(m, _)| m.max_context_size)
                .unwrap_or(200_000)
        };

        let opts = ProjectOptions::default();
        // contextMemory: drop vacuous noise + loop-event markers before project.
        let _ = crate::context_memory::fold_vacuous(&mut session.messages);
        let projected = crate::context_memory::fold_loop_events(&session.build_messages());
        let mut messages = project(&projected, &opts);
        let mut req = session.token_counter.request_size(system, tools, &messages);

        if session
            .token_counter
            .needs_compaction(max_context, reserved, req)
        {
            messages = project_strict(&session.build_messages(), &opts);
            req = session.token_counter.request_size(system, tools, &messages);
        }

        if auto_compact
            && session
                .token_counter
                .needs_compaction(max_context, reserved, req)
            && session.messages.len() > keep_last
        {
            let digest =
                build_local_summary(&session.messages[..session.messages.len() - keep_last]);
            let dropped = compact_messages(&mut session.messages, keep_last, &digest);
            tracing::info!(
                "Auto-compacted session {}: dropped {} messages (est_tokens={})",
                session.id,
                dropped,
                req
            );
            messages = project(&session.build_messages(), &opts);
        }

        messages
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

#[derive(Debug, Clone)]
struct PendingToolCall {
    id: String,
    name: String,
    input: serde_json::Value,
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
        _ => {
            let na = candidate.components().collect::<Vec<_>>();
            let nb = plan.components().collect::<Vec<_>>();
            na == nb
        }
    };
    if !same {
        return None;
    }
    tokio::fs::read_to_string(plan).await.ok()
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
                &serde_json::json!({"tool": name, "input": input}),
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

    let ctx = ToolContext {
        working_dir,
        session_id,
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
    let dir = session.working_dir.join(".kkagent").join("tool-results");
    let _ = std::fs::create_dir_all(&dir);
    let id = uuid::Uuid::new_v4().to_string();
    let path = dir.join(format!("{}.txt", id));
    let _ = std::fs::write(&path, &output.content);
    let preview: String = output.content.chars().take(4000).collect();
    output.content = format!(
        "{preview}\n\n… tool result truncated ({tool_name}, {} chars). Full output saved to {} — use Read on that path if needed.",
        output.content.len(),
        path.display()
    );
    output
}

fn session_has_goal_reminder(session: &Session) -> bool {
    session.messages.iter().rev().take(6).any(|m| {
        m.content.iter().any(|c| match c {
            ChatContent::Text { text } => text.contains("Active goal:"),
            _ => false,
        })
    })
}

fn build_local_summary(messages: &[ChatMessage]) -> String {
    let mut out = String::from("Earlier conversation digest:\n");
    for m in messages.iter().take(40) {
        let text: String = m
            .content
            .iter()
            .filter_map(|c| match c {
                ChatContent::Text { text } => Some(text.as_str()),
                ChatContent::ToolUse { name, .. } => Some(name.as_str()),
                ChatContent::ToolResult { content, .. } => Some(content.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join(" ");
        if text.is_empty() {
            continue;
        }
        let snippet: String = text.chars().take(240).collect();
        out.push_str(&format!("[{}] {}\n", m.role, snippet));
    }
    if out.len() > 4_000 {
        out.chars().take(4_000).collect()
    } else {
        out
    }
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
}
