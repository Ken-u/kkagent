use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tokio::task::AbortHandle;
use std::collections::HashMap;

use kkagent_config::AppConfig;
use kkagent_protocol::{PermissionMode, subagent::SubagentConfig};
use kkagent_tools::{ToolRegistry, register_builtin_tools};

use crate::agent_loop::AgentLoop;
use crate::permission::PermissionChain;
use crate::session::Session;

/// Run a focused subagent to completion and return its final assistant text.
pub async fn run_subagent(
    app_config: Arc<AppConfig>,
    sub_cfg: SubagentConfig,
    permission_mode: PermissionMode,
) -> anyhow::Result<String> {
    let model = sub_cfg
        .model
        .clone()
        .filter(|m| !m.is_empty())
        .or_else(|| {
            app_config
                .secondary_model
                .clone()
                .filter(|m| !m.is_empty())
        })
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
        model,
    );
    session.inject_workspace_instructions().await;
    session.system_prompt.push_str(&profile_system_addon(&profile));
    session.add_user_message(sub_cfg.prompt.clone());

    let mut tools = ToolRegistry::new();
    register_builtin_tools(&mut tools);
    // Strip nested Task tools if present — builtins don't include them.

    let permission_rules = app_config
        .permission
        .as_ref()
        .map(|p| p.rules.clone())
        .unwrap_or_default();
    let permission = PermissionChain::new(PermissionMode::Auto, permission_rules);

    let (event_tx, mut event_rx) = mpsc::channel(64);
    // Optional mirror: drain events (parent can attach later).
    tokio::spawn(async move {
        while event_rx.recv().await.is_some() {}
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

    agent.run_turn(&mut session).await?;
    let result = extract_final_assistant_text(&session);
    persist_subagent_output(&sub_cfg, &result).await;
    Ok(result)
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
