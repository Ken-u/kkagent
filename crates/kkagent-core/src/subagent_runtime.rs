use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tokio::task::AbortHandle;

use kkagent_config::AppConfig;
use kkagent_protocol::{subagent::SubagentConfig, AgentEvent, PermissionMode};
use kkagent_tools::{register_builtin_tools, ToolRegistry};

use crate::agent_loop::AgentLoop;
use crate::permission::PermissionChain;
use crate::session::Session;

/// Parent context used to mirror child agent events into the parent TUI.
#[derive(Clone)]
pub struct SubagentMirrorContext {
    pub parent_session_id: String,
    pub parent_tool_call_id: String,
    pub parent_event_tx: mpsc::Sender<AgentEvent>,
}

/// Run a focused subagent to completion and return its final assistant text.
pub async fn run_subagent(
    app_config: Arc<AppConfig>,
    sub_cfg: SubagentConfig,
    permission_mode: PermissionMode,
) -> anyhow::Result<String> {
    run_subagent_mirrored(app_config, sub_cfg, permission_mode, None).await
}

pub async fn run_subagent_mirrored(
    app_config: Arc<AppConfig>,
    sub_cfg: SubagentConfig,
    permission_mode: PermissionMode,
    mirror: Option<SubagentMirrorContext>,
) -> anyhow::Result<String> {
    let model = sub_cfg
        .model
        .clone()
        .filter(|m| !m.is_empty())
        .or_else(|| app_config.secondary_model.clone().filter(|m| !m.is_empty()))
        .or_else(|| app_config.default_model_alias().map(|s| s.to_string()))
        .unwrap_or_else(|| "default".into());

    let profile = sub_cfg
        .profile
        .as_deref()
        .unwrap_or("general")
        .to_lowercase();

    let mut session = Session::new(
        format!("sub-{}", sub_cfg.agent_id),
        PathBuf::from(&sub_cfg.working_dir),
        permission_mode,
        model.clone(),
    );
    session.inject_workspace_instructions().await;
    session
        .system_prompt
        .push_str(&profile_system_addon(&profile));
    session.add_user_message(sub_cfg.prompt.clone());

    let mut tools = ToolRegistry::new();
    register_builtin_tools(&mut tools);

    let permission_rules = app_config
        .permission
        .as_ref()
        .map(|p| p.rules.clone())
        .unwrap_or_default();
    let permission = PermissionChain::new(PermissionMode::Auto, permission_rules);

    let (event_tx, mut event_rx) = mpsc::channel(256);

    if let Some(m) = &mirror {
        let _ = m
            .parent_event_tx
            .send(AgentEvent::SubagentSpawned {
                session_id: m.parent_session_id.clone(),
                subagent_id: sub_cfg.agent_id.clone(),
                subagent_name: profile.clone(),
                parent_tool_call_id: m.parent_tool_call_id.clone(),
                description: Some(sub_cfg.description.clone()),
                model: Some(model.clone()),
                run_in_background: true,
            })
            .await;
        let _ = m
            .parent_event_tx
            .send(AgentEvent::SubagentStarted {
                session_id: m.parent_session_id.clone(),
                subagent_id: sub_cfg.agent_id.clone(),
            })
            .await;
    }

    let mirror_for_drain = mirror.clone();
    let child_id = sub_cfg.agent_id.clone();
    tokio::spawn(async move {
        while let Some(ev) = event_rx.recv().await {
            if let Some(m) = &mirror_for_drain {
                // Mirror nested lifecycle/content under parent tool call.
                let nested = match &ev {
                    AgentEvent::MessageDelta { .. }
                    | AgentEvent::ThinkingDelta { .. }
                    | AgentEvent::ToolCall { .. }
                    | AgentEvent::ToolResult { .. }
                    | AgentEvent::StatusUpdate { .. }
                    | AgentEvent::Error { .. }
                    | AgentEvent::TodoUpdated { .. } => Some(ev.clone()),
                    _ => None,
                };
                if let Some(child_ev) = nested {
                    let _ = m
                        .parent_event_tx
                        .send(AgentEvent::SubagentChildEvent {
                            session_id: m.parent_session_id.clone(),
                            subagent_id: child_id.clone(),
                            parent_tool_call_id: m.parent_tool_call_id.clone(),
                            event: Box::new(child_ev),
                        })
                        .await;
                }
            }
        }
    });

    let abort_registry = Arc::new(Mutex::new(HashMap::<String, AbortHandle>::new()));
    let max_rounds = match profile.as_str() {
        "explore" => 16,
        "coder" => 32,
        _ => 24,
    };
    let agent = AgentLoop::with_max_rounds(
        app_config,
        Arc::new(tools),
        Arc::new(Mutex::new(permission)),
        event_tx,
        abort_registry,
        max_rounds,
    );

    let run_result = agent.run_turn(&mut session).await;
    match run_result {
        Ok(()) => {
            let result = extract_final_assistant_text(&session);
            persist_subagent_output(&sub_cfg, &result).await;
            if let Some(m) = &mirror {
                let summary: String = result.chars().take(400).collect();
                let _ = m
                    .parent_event_tx
                    .send(AgentEvent::SubagentCompleted {
                        session_id: m.parent_session_id.clone(),
                        subagent_id: sub_cfg.agent_id.clone(),
                        result_summary: summary,
                    })
                    .await;
            }
            Ok(result)
        }
        Err(e) => {
            if let Some(m) = &mirror {
                let _ = m
                    .parent_event_tx
                    .send(AgentEvent::SubagentFailed {
                        session_id: m.parent_session_id.clone(),
                        subagent_id: sub_cfg.agent_id.clone(),
                        error: e.to_string(),
                    })
                    .await;
            }
            Err(e)
        }
    }
}

fn profile_system_addon(profile: &str) -> String {
    match profile {
        "explore" => "\n\n# Subagent profile: explore\n\
You are a read-only explorer. Prefer Glob/Grep/Read. Do not modify files. \
Produce a structured map of findings with paths. Do not launch Task subagents.".into(),
        "coder" => "\n\n# Subagent profile: coder\n\
You are an implementation agent. Make concrete code changes with Write/Edit. \
Keep changes focused. Do not launch Task subagents.".into(),
        _ => "\n\n# Subagent mode\n\
You are a focused subagent. Complete the assigned task thoroughly, use tools as needed, \
then finish with a concise report. Do not ask the user clarifying questions. Do not launch Task subagents.".into(),
    }
}

async fn persist_subagent_output(cfg: &SubagentConfig, result: &str) {
    let dir = PathBuf::from(&cfg.working_dir)
        .join(".kkagent")
        .join("tasks");
    let _ = tokio::fs::create_dir_all(&dir).await;
    let path = dir.join(format!("{}.md", cfg.agent_id));
    let body = format!(
        "# Task {}\n\n**description:** {}\n**profile:** {}\n\n---\n\n{}\n",
        cfg.agent_id,
        cfg.description,
        cfg.profile.as_deref().unwrap_or("general"),
        result
    );
    let _ = tokio::fs::write(path, body).await;
}

fn extract_final_assistant_text(session: &Session) -> String {
    for msg in session.messages.iter().rev() {
        if msg.role != "assistant" {
            continue;
        }
        let mut text = String::new();
        for block in &msg.content {
            if let kkagent_llm::ChatContent::Text { text: t } = block {
                if !text.is_empty() {
                    text.push('\n');
                }
                text.push_str(t);
            }
        }
        if !text.trim().is_empty() {
            return text;
        }
    }
    "(subagent finished with no text output)".into()
}
