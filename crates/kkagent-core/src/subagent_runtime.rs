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
        .filter(|m| !m.is_empty())
        .or_else(|| app_config.default_model_alias().map(|s| s.to_string()))
        .unwrap_or_else(|| "default".into());

    let mut session = Session::new(
        format!("sub-{}", sub_cfg.agent_id),
        PathBuf::from(&sub_cfg.working_dir),
        permission_mode,
        model,
    );
    session.system_prompt.push_str(
        "\n\n# Subagent mode\n\
You are a focused subagent launched by the main agent. Complete the assigned task thoroughly, \
use tools as needed, then finish with a concise report of findings. Do not ask clarifying \
questions to a user — make reasonable assumptions and state them. Do not launch further Task subagents.",
    );
    session.add_user_message(sub_cfg.prompt);

    let mut tools = ToolRegistry::new();
    // Builtins only — no nested Task / Goal to keep the subagent bounded.
    register_builtin_tools(&mut tools);

    let permission_rules = app_config
        .permission
        .as_ref()
        .map(|p| p.rules.clone())
        .unwrap_or_default();
    // Prefer Auto for subagents so they don't block on interactive approvals.
    let permission = PermissionChain::new(PermissionMode::Auto, permission_rules);

    let (event_tx, mut event_rx) = mpsc::channel(64);
    tokio::spawn(async move {
        while event_rx.recv().await.is_some() {}
    });

    let abort_registry = Arc::new(Mutex::new(HashMap::<String, AbortHandle>::new()));
    let agent = AgentLoop::with_max_rounds(
        app_config,
        Arc::new(tools),
        Arc::new(Mutex::new(permission)),
        event_tx,
        abort_registry,
        24,
    );

    agent.run_turn(&mut session).await?;
    Ok(extract_final_assistant_text(&session))
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
