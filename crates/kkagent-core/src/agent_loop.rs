use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{mpsc, Mutex};
use tokio::task::AbortHandle;
use kkagent_protocol::{AgentEvent, SessionStatus};
use kkagent_llm::{LlmProvider, LlmRequest, StreamEvent, ChatMessage, ChatContent, ToolDef, ThinkingParams};
use kkagent_tools::{ToolContext, ToolOutput, ToolRegistry};
use kkagent_config::AppConfig;

use crate::permission::{PermissionChain, PermissionDecision};
use crate::session::Session;

pub struct AgentLoop {
    config: Arc<AppConfig>,
    provider: Arc<dyn LlmProvider>,
    tools: Arc<ToolRegistry>,
    permission: Arc<Mutex<PermissionChain>>,
    event_tx: mpsc::Sender<AgentEvent>,
    abort_registry: Arc<Mutex<HashMap<String, AbortHandle>>>,
}

impl AgentLoop {
    pub fn new(
        config: Arc<AppConfig>,
        provider: Arc<dyn LlmProvider>,
        tools: Arc<ToolRegistry>,
        permission: Arc<Mutex<PermissionChain>>,
        event_tx: mpsc::Sender<AgentEvent>,
        abort_registry: Arc<Mutex<HashMap<String, AbortHandle>>>,
    ) -> Self {
        Self {
            config,
            provider,
            tools,
            permission,
            event_tx,
            abort_registry,
        }
    }

    pub fn run_turn<'a>(
        &'a self,
        session: &'a mut Session,
    ) -> futures::future::BoxFuture<'a, anyhow::Result<()>> {
        Box::pin(self.run_turn_inner(session))
    }

    async fn run_turn_inner(
        &self,
        session: &mut Session,
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

        let model_alias = if session.model_alias.is_empty() {
            self.config.default_model_alias().unwrap_or("default").to_string()
        } else {
            session.model_alias.clone()
        };
        let (model_config, _) = self.config
            .resolve_model(&model_alias)
            .ok_or_else(|| anyhow::anyhow!("Model '{}' not found", model_alias))?;

        let tool_defs: Vec<ToolDef> = self.tools.tool_definitions().iter().map(|td| {
            ToolDef {
                name: td.name.clone(),
                description: td.description.clone(),
                input_schema: td.parameters.clone(),
            }
        }).collect();
        tracing::debug!("Sending {} tools to LLM", tool_defs.len());

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
        let provider = self.provider.clone();

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

            let mut tool_results = Vec::new();
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

                let output = match decision {
                    PermissionDecision::Approve => {
                        self.execute_tool(session, &tc.name, &tc.input).await
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
                                self.execute_tool(session, &tc.name, &tc.input).await
                            }
                            kkagent_protocol::ApprovalDecision::Cancelled => {
                                return self.finish_interrupted(session).await;
                            }
                            _ => ToolOutput::error("Tool call was rejected by user"),
                        }
                    }
                    PermissionDecision::Deny(reason) => {
                        ToolOutput::error(format!("Denied: {}", reason))
                    }
                };

                // ExitPlanMode success → leave plan mode
                if tc.name == "ExitPlanMode" && !output.is_error {
                    session.plan_mode = false;
                    let _ = self.event_tx.send(AgentEvent::PlanModeChanged {
                        session_id: session_id.clone(),
                        enabled: false,
                    }).await;
                }

                tracing::info!(
                    "Tool {} result: error={} len={}",
                    tc.name,
                    output.is_error,
                    output.content.len()
                );

                let _ = self.event_tx.send(AgentEvent::ToolResult {
                    session_id: session_id.clone(),
                    tool_call_id: tc.id.clone(),
                    tool_name: tc.name.clone(),
                    output: output.content.clone(),
                    is_error: output.is_error,
                }).await;

                tool_results.push((tc.id.clone(), output));
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

            tracing::info!("Recursing into next turn");
            return self.run_turn(session).await;
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
        // Snapshot files before mutating tools so undo can restore them.
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

        let tool = match self.tools.get(name) {
            Some(t) => t,
            None => return ToolOutput::error(format!("Unknown tool: {}", name)),
        };

        let ctx = ToolContext {
            working_dir: session.working_dir.clone(),
            session_id: session.id.clone(),
        };

        match tool.execute(input.clone(), &ctx).await {
            Ok(output) => output,
            Err(e) => ToolOutput::error(format!("Tool execution error: {}", e)),
        }
    }
}

#[derive(Debug, Clone)]
struct PendingToolCall {
    id: String,
    name: String,
    input: serde_json::Value,
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
