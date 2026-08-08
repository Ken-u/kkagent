use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex};
use tokio::task::AbortHandle;
use kkagent_protocol::{AgentEvent, SessionStatus};
use kkagent_llm::{LlmRequest, StreamEvent, ChatMessage, ChatContent, ToolDef, ThinkingParams, create_provider};
use kkagent_tools::{infer_accesses, ToolContext, ToolOutput, ToolRegistry};
use kkagent_config::AppConfig;

use crate::permission::{PermissionChain, PermissionDecision};
use crate::session::Session;
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
        Box::pin(async move {
            self.run_turn_body(session, rounds_left).await
        })
    }

    async fn run_turn_body(
        &self,
        session: &mut Session,
        rounds_left: u32,
    ) -> anyhow::Result<()> {
        let session_id = session.id.clone();
        tracing::info!("Starting turn for session {}", session_id);

        if session.is_interrupted() {
            return self.finish_interrupted(session).await;
        }

        let _ = self.event_tx.send(AgentEvent::TurnStart {
            session_id: session_id.clone(),
        }).await;

        let _ = self.event_tx.send(AgentEvent::StatusUpdate {
            session_id: session_id.clone(),
            status: SessionStatus::Thinking,
        }).await;

        // Keep permission chain in sync with session mode (/yolo /auto toggles)
        {
            let mut perm = self.permission.lock().await;
            perm.set_mode(session.permission_mode);
        }

        let model_alias = {
            let alias = session.get_model_alias();
            if alias.is_empty() {
                self.config.default_model_alias().unwrap_or("default").to_string()
            } else {
                alias
            }
        };
        let (model_config, provider_config) = self.config
            .resolve_model(&model_alias)
            .ok_or_else(|| anyhow::anyhow!("Model '{}' not found", model_alias))?;
        tracing::info!(
            "Using model alias={} id={}",
            model_alias,
            model_config.model
        );
        let provider = create_provider(provider_config, model_config);

        let tool_defs: Vec<ToolDef> = self
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
        tracing::debug!("Sending {} tools to LLM", tool_defs.len());

        // Todo reminder injection
        session.turns_since_todo = session.turns_since_todo.saturating_add(1);
        if session.turns_since_todo >= 8 {
            session.add_user_message(
                "<system-reminder>\nThe TodoList tool has not been updated recently. \
If you are working on multi-step tasks, consider updating TodoList. \
Do not mention this reminder to the user.\n</system-reminder>".into(),
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

        let messages = session.build_messages();
        tracing::debug!("Conversation has {} messages", messages.len());

        let thinking = self.config.thinking.as_ref().and_then(|t| {
            if t.enabled {
                Some(ThinkingParams { budget_tokens: 10000 })
            } else {
                None
            }
        });

        let request = LlmRequest {
            model: model_config.model.clone(),
            messages,
            tools: tool_defs,
            max_tokens: model_config.max_output_size.unwrap_or(8192) as u32,
            system: Some(session.effective_system_prompt()),
            thinking,
        };

        let (stream_tx, mut stream_rx) = mpsc::channel::<StreamEvent>(256);

        let handle = tokio::spawn(async move {
            if let Err(e) = provider.stream_chat(request, stream_tx).await {
                tracing::error!("LLM stream error: {}", e);
            }
        });
        self.abort_registry
            .lock()
            .await
            .insert(session_id.clone(), handle.abort_handle());

        let mut assistant_text = String::new();
        let mut thinking_text = String::new();
        let mut tool_calls: Vec<PendingToolCall> = Vec::new();
        let mut current_tool: Option<PendingToolCall> = None;
        let mut tool_input_buf = String::new();
        let mut interrupted = false;

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
                        let _ = self.event_tx.send(AgentEvent::MessageDelta {
                            session_id: session_id.clone(),
                            text,
                        }).await;
                    }
                    StreamEvent::ThinkingDelta(text) => {
                        thinking_text.push_str(&text);
                        let _ = self.event_tx.send(AgentEvent::ThinkingDelta {
                            session_id: session_id.clone(),
                            text,
                        }).await;
                    }
                    StreamEvent::ToolUseStart { id, name } => {
                        tracing::info!("Tool use start: {} ({})", name, id);
                        tool_input_buf.clear();
                        current_tool = Some(PendingToolCall {
                            id, name, input: serde_json::Value::Null,
                        });
                    }
                    StreamEvent::ToolUseInputDelta(json_chunk) => {
                        tool_input_buf.push_str(&json_chunk);
                    }
                    StreamEvent::ToolUseEnd => {
                        if let Some(mut tool) = current_tool.take() {
                            tool.input = serde_json::from_str(&tool_input_buf)
                                .unwrap_or(serde_json::Value::String(tool_input_buf.clone()));
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
                        }
                        tool_input_buf.clear();
                    }
                    StreamEvent::MessageEnd { usage } => {
                        tracing::debug!(
                            "Message end: in={} out={}",
                            usage.input_tokens,
                            usage.output_tokens
                        );
                        let _ = self.event_tx.send(AgentEvent::UsageUpdate {
                            session_id: session_id.clone(),
                            usage: kkagent_protocol::TokenUsage {
                                input_tokens: usage.input_tokens,
                                output_tokens: usage.output_tokens,
                                cache_creation_input_tokens: usage.cache_creation_input_tokens,
                                cache_read_input_tokens: usage.cache_read_input_tokens,
                            },
                        }).await;
                    }
                    StreamEvent::Error(msg) => {
                        tracing::error!("Stream error: {}", msg);
                        let _ = self.event_tx.send(AgentEvent::Error {
                            session_id: session_id.clone(),
                            message: msg,
                        }).await;
                    }
                },
                Ok(None) => break,
                Err(_) => continue, // timeout — re-check interrupt
            }
        }

        self.abort_registry.lock().await.remove(&session_id);

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

        tracing::info!(
            "Stream done: text_len={} tool_calls={}",
            assistant_text.len(),
            tool_calls.len()
        );

        // Record assistant message
        let mut content = Vec::new();
        if !thinking_text.is_empty() {
            content.push(ChatContent::Thinking { thinking: thinking_text });
        }
        if !assistant_text.is_empty() {
            content.push(ChatContent::Text { text: assistant_text });
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
            let _ = self.event_tx.send(AgentEvent::StatusUpdate {
                session_id: session_id.clone(),
                status: SessionStatus::ToolExecuting,
            }).await;

            // Phase 1: permissions + interactive tools (serial). Phase 2: conflict-aware parallel exec.
            enum Prepared {
                Done(ToolOutput),
                Ready { name: String, input: serde_json::Value },
            }
            let mut prepared: Vec<(String, String, Prepared)> = Vec::new();

            for tc in &tool_calls {
                if session.is_interrupted() {
                    return self.finish_interrupted(session).await;
                }

                let _ = self.event_tx.send(AgentEvent::ToolCall {
                    session_id: session_id.clone(),
                    tool_call_id: tc.id.clone(),
                    tool_name: tc.name.clone(),
                    input: tc.input.clone(),
                }).await;

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
                                if let Some(path_str) = tc.input.get("path").and_then(|v| v.as_str()) {
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
                        let _ = self.event_tx.send(AgentEvent::ApprovalRequested {
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
                        }).await;

                        let _ = self.event_tx.send(AgentEvent::StatusUpdate {
                            session_id: session_id.clone(),
                            status: SessionStatus::WaitingApproval,
                        }).await;

                        let response = session.wait_approval(&approval_id).await;
                        match response.decision {
                            kkagent_protocol::ApprovalDecision::Approved => {
                                if response.scope == Some(kkagent_protocol::ApprovalScope::Session) {
                                    let mut perm = self.permission.lock().await;
                                    perm.record_session_approval(&tc.name, &tc.input);
                                }
                                if tc.name == "AskUserQuestion" {
                                    Prepared::Done(self.execute_tool(session, &tc.name, &tc.input).await)
                                } else {
                                    if tc.name == "Write" || tc.name == "Edit" {
                                        if let Some(path_str) = tc.input.get("path").and_then(|v| v.as_str()) {
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
                            kkagent_protocol::ApprovalDecision::Cancelled => {
                                return self.finish_interrupted(session).await;
                            }
                            _ => Prepared::Done(ToolOutput::error("Tool call was rejected by user")),
                        }
                    }
                    PermissionDecision::Deny(reason) => {
                        Prepared::Done(ToolOutput::error(format!("Denied: {}", reason)))
                    }
                };
                prepared.push((tc.id.clone(), tc.name.clone(), prep));
            }

            let _ = self.event_tx.send(AgentEvent::StatusUpdate {
                session_id: session_id.clone(),
                status: SessionStatus::ToolExecuting,
            }).await;

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
                                execute_tool_parallel(
                                    tools,
                                    hooks,
                                    working_dir,
                                    sid,
                                    enabled.as_ref(),
                                    &name,
                                    &input,
                                    Some(tool_call_id),
                                )
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
                            .unwrap_or_else(|| ToolOutput::error("scheduler missing result"))
                    }
                };
                resolved.push((id, name, output));
            }

            let mut tool_results = Vec::new();
            for (id, name, output) in resolved {
                // ExitPlanMode success → leave plan mode
                if name == "ExitPlanMode" && !output.is_error {
                    session.plan_mode = false;
                    let _ = self.event_tx.send(AgentEvent::PlanModeChanged {
                        session_id: session_id.clone(),
                        enabled: false,
                    }).await;
                }
                if name == "EnterPlanMode" && !output.is_error {
                    session.plan_mode = true;
                    let _ = self.event_tx.send(AgentEvent::PlanModeChanged {
                        session_id: session_id.clone(),
                        enabled: true,
                    }).await;
                }
                if name == "SelectTools" && !output.is_error {
                    if let Some(data) = &output.data {
                        if data.get("tools").map(|v| v.is_null()).unwrap_or(false) {
                            session.enabled_tools = None;
                        } else if let Some(arr) = data.get("tools").and_then(|v| v.as_array()) {
                            session.enabled_tools = Some(
                                arr.iter()
                                    .filter_map(|v| v.as_str().map(String::from))
                                    .collect(),
                            );
                        }
                    }
                }

                let output = truncate_tool_output(session, &name, output);

                tracing::info!(
                    "Tool {} result: error={} len={}",
                    name,
                    output.is_error,
                    output.content.len()
                );

                let _ = self.event_tx.send(AgentEvent::ToolResult {
                    session_id: session_id.clone(),
                    tool_call_id: id.clone(),
                    tool_name: name.clone(),
                    output: output.content.clone(),
                    is_error: output.is_error,
                }).await;

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

                if !output.is_error
                    && session.plan_mode
                    && (name == "Write" || name == "Edit")
                {
                    // Reconstruct input from tool call list
                    if let Some(tc) = tool_calls.iter().find(|t| t.id == id) {
                        if let Some(content) =
                            read_plan_file_if_matched(session, &tc.input).await
                        {
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

            let result_content: Vec<ChatContent> = tool_results.iter().map(|(id, output)| {
                ChatContent::ToolResult {
                    tool_use_id: id.clone(),
                    content: output.content.clone(),
                    is_error: output.is_error,
                }
            }).collect();

            session.messages.push(ChatMessage {
                role: "user".into(),
                content: result_content,
            });

            if session.is_interrupted() {
                return self.finish_interrupted(session).await;
            }

            tracing::info!("Recursing into next turn ({} rounds left)", rounds_left.saturating_sub(1));
            if rounds_left <= 1 {
                tracing::warn!("Agent turn limit reached for session {}", session_id);
                let _ = self.event_tx.send(AgentEvent::StatusUpdate {
                    session_id: session_id.clone(),
                    status: SessionStatus::Idle,
                }).await;
                let _ = self.event_tx.send(AgentEvent::TurnEnd {
                    session_id: session_id.clone(),
                }).await;
                return Ok(());
            }
            return self.run_turn_inner(session, rounds_left - 1).await;
        }

        let _ = self.event_tx.send(AgentEvent::StatusUpdate {
            session_id: session_id.clone(),
            status: SessionStatus::Idle,
        }).await;

        let _ = self.event_tx.send(AgentEvent::TurnEnd {
            session_id: session_id.clone(),
        }).await;

        session.commit_turn();
        tracing::info!("Turn completed for session {}", session_id);
        Ok(())
    }

    async fn finish_interrupted(&self, session: &mut Session) -> anyhow::Result<()> {
        let session_id = session.id.clone();
        tracing::info!("Turn interrupted for session {}", session_id);
        session.commit_turn();
        let _ = self.event_tx.send(AgentEvent::Error {
            session_id: session_id.clone(),
            message: "Interrupted".into(),
        }).await;
        let _ = self.event_tx.send(AgentEvent::StatusUpdate {
            session_id: session_id.clone(),
            status: SessionStatus::Idle,
        }).await;
        let _ = self.event_tx.send(AgentEvent::TurnEnd {
            session_id,
        }).await;
        Ok(())
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
        execute_tool_parallel(
            Arc::clone(&self.tools),
            self.hooks.clone(),
            session.working_dir.clone(),
            session.id.clone(),
            session.enabled_tools.as_ref(),
            name,
            input,
            None,
        )
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
                parts.push(format!("Selected: {}", labels.first().cloned().unwrap_or_default()));
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

async fn read_plan_file_if_matched(
    session: &Session,
    input: &serde_json::Value,
) -> Option<String> {
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

async fn execute_tool_parallel(
    tools: Arc<ToolRegistry>,
    hooks: Option<Arc<kkagent_mcp::HookManager>>,
    working_dir: std::path::PathBuf,
    session_id: String,
    enabled_tools: Option<&std::collections::HashSet<String>>,
    name: &str,
    input: &serde_json::Value,
    tool_call_id: Option<String>,
) -> ToolOutput {
    if !tool_allowed_set(enabled_tools, name) {
        return ToolOutput::error(format!(
            "Tool `{name}` is not in the current SelectTools allowlist"
        ));
    }

    if let Some(hooks) = &hooks {
        let _ = hooks
            .fire(
                kkagent_mcp::hooks::HookEvent::PreToolCall,
                &serde_json::json!({"tool": name, "input": input}),
            )
            .await;
    }

    let tool = match tools.get(name) {
        Some(t) => t,
        None => return ToolOutput::error(format!("Unknown tool: {}", name)),
    };

    let ctx = ToolContext {
        working_dir,
        session_id,
        tool_call_id,
    };

    match tool.execute(input.clone(), &ctx).await {
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
