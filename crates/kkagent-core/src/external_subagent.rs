//! External subagent executor — runs plugin-declared subagent types.
//!
//! Two transports:
//!
//! - **ACP** — an external agent process (e.g. the Cursor CLI `agent acp`)
//!   driven over stdio JSON-RPC: `spawn` → `initialize` → `session/new` →
//!   `session/prompt` → collect text → `complete`/`fail` on the shared
//!   [`SubagentManager`] so TaskOutput/status/cancel keep working.
//! - **internal** — an in-process kkagent agent loop (kk model + kk tools)
//!   with an optional tool allowlist and plugin-private MCP servers. The MCP
//!   servers are started lazily per delegation and their tools are
//!   namespaced `<plugin>.<server>.<tool>` — zero main-session context cost
//!   until the subagent type is actually used.

use std::sync::Arc;
use std::time::Duration;

use kkagent_acp::{AcpClientOptions, ExternalProgress, PermissionPolicy};
use kkagent_protocol::subagent::SubagentConfig;
use kkagent_protocol::AgentEvent;
use tokio::sync::mpsc;

use crate::plugin::{PluginSubagentSpec, PluginSubagentTransportConfig};
use crate::subagent_runtime::SubagentMirrorContext;

/// Resolve the command for an external subagent: explicit manifest command,
/// else the Cursor CLI default (`agent acp`).
fn resolve_command(config: &PluginSubagentTransportConfig) -> Vec<String> {
    if config.command.is_empty() {
        vec!["agent".into(), "acp".into()]
    } else {
        config.command.clone()
    }
}

/// Run one external subagent to completion and return its final text.
///
/// `mirror`, when present, receives the same lifecycle events the built-in
/// in-process subagents emit (`SubagentSpawned`/`Started`/`Completed`/
/// `Failed` plus `SubagentChildEvent(MessageDelta)` for streamed text), so
/// the parent TUI renders external delegations identically.
pub async fn run_external_subagent(
    spec: &PluginSubagentSpec,
    sub_cfg: SubagentConfig,
    mirror: Option<SubagentMirrorContext>,
    ctx: ExternalRunContext,
) -> anyhow::Result<String> {
    match spec.transport.as_str() {
        "acp" => run_acp_subagent(spec, sub_cfg, mirror).await,
        "internal" => {
            crate::internal_subagent::run_internal_subagent(spec, sub_cfg, mirror, ctx).await
        }
        other => anyhow::bail!(
            "unsupported subagent transport `{other}` (supported: \"acp\", \"internal\")"
        ),
    }
}

/// Runtime handles the internal transport needs from the host process.
/// ACP subprocesses don't need any of these — they only touch the filesystem.
#[derive(Clone)]
pub struct ExternalRunContext {
    /// App config snapshot for the internal agent loop (model resolution,
    /// subagent limits, tools config).
    pub app_config: std::sync::Arc<kkagent_config::AppConfig>,
    /// Web services config for registering the Web tool (subject to `tools`).
    pub web: std::sync::Arc<kkagent_tools::WebServicesConfig>,
    /// Permission mode inherited from the parent turn.
    pub permission_mode: kkagent_protocol::PermissionMode,
    /// Parent interrupt flag, inherited so cancel propagates into the loop.
    pub interrupt: Option<Arc<std::sync::atomic::AtomicBool>>,
    /// Real nested launcher from the host: lets an internal subagent with
    /// `allowDelegation: true` actually spawn further subagents (through the
    /// same dispatch the root uses, so both built-in and plugin profiles are
    /// reachable). Without it nested delegation tools exist but no-op.
    pub launch: Option<kkagent_tools::builtin::task::SubagentLaunchFn>,
}

impl ExternalRunContext {
    /// Placeholder context for tests / transports that ignore it.
    pub fn detached(app_config: Arc<kkagent_config::AppConfig>) -> Self {
        let web = Arc::new(kkagent_tools::WebServicesConfig::from_app(&app_config));
        Self {
            app_config,
            web,
            permission_mode: kkagent_protocol::PermissionMode::Auto,
            interrupt: None,
            launch: None,
        }
    }
}

async fn run_acp_subagent(
    spec: &PluginSubagentSpec,
    sub_cfg: SubagentConfig,
    mirror: Option<SubagentMirrorContext>,
) -> anyhow::Result<String> {
    let cwd = spec
        .transport_config
        .cwd
        .as_ref()
        .map(std::path::PathBuf::from)
        .unwrap_or_else(|| std::path::PathBuf::from(&sub_cfg.working_dir));

    let mut env = std::collections::HashMap::new();
    for (k, v) in &spec.env {
        env.insert(k.clone(), v.clone());
    }

    let mut opts = AcpClientOptions {
        command: resolve_command(&spec.transport_config),
        cwd,
        env,
        request_timeout: Some(Duration::from_secs(spec.timeout_secs.unwrap_or(300))),
        permission: Some(if spec.auto_approve {
            PermissionPolicy::AutoApprove
        } else {
            PermissionPolicy::Deny
        }),
    };
    if let Some(mode) = &spec.transport_config.mode {
        // Remember the requested mode via an env hint for agents that support
        // mode selection at spawn; ACP proper selects modes in session/new.
        opts.env.insert("KKAGENT_ACP_MODE".into(), mode.clone());
    }

    let client = kkagent_acp::AcpClient::spawn(opts).await?;
    let result = run_on_client(&client, spec, &sub_cfg, &mirror).await;
    client.shutdown().await;
    result
}

/// Handshake → session → prompt over an already-spawned client. Split out so
/// tests can drive a stub client.
async fn run_on_client(
    client: &kkagent_acp::AcpClient,
    spec: &PluginSubagentSpec,
    sub_cfg: &SubagentConfig,
    mirror: &Option<SubagentMirrorContext>,
) -> anyhow::Result<String> {
    // Mirror lifecycle the same way run_subagent_mirrored does: Spawned
    // (with prompt) → Started → child MessageDelta stream → Completed/Failed.
    if let Some(m) = mirror {
        m.parent_event_tx
            .send(AgentEvent::SubagentSpawned {
                session_id: m.parent_session_id.clone(),
                subagent_id: sub_cfg.agent_id.clone(),
                subagent_name: spec.qualified_name(),
                parent_tool_call_id: m.parent_tool_call_id.clone(),
                description: Some(sub_cfg.description.clone()),
                model: sub_cfg.model.clone(),
                run_in_background: sub_cfg.run_in_background,
                prompt: Some(sub_cfg.prompt.clone()),
            })
            .await
            .ok();
        m.parent_event_tx
            .send(AgentEvent::SubagentStarted {
                session_id: m.parent_session_id.clone(),
                subagent_id: sub_cfg.agent_id.clone(),
            })
            .await
            .ok();
    }

    let outcome = run_prompt(client, spec, sub_cfg, mirror).await;
    if let Some(m) = mirror {
        let event = match &outcome {
            Ok(text) => AgentEvent::SubagentCompleted {
                session_id: m.parent_session_id.clone(),
                subagent_id: sub_cfg.agent_id.clone(),
                // Match the built-in path: first 400 chars as the preview.
                result_summary: text.chars().take(400).collect(),
                usage: None, // ACP does not expose token usage
            },
            Err(error) => AgentEvent::SubagentFailed {
                session_id: m.parent_session_id.clone(),
                subagent_id: sub_cfg.agent_id.clone(),
                error: error.to_string(),
            },
        };
        m.parent_event_tx.send(event).await.ok();
    }
    outcome
}

/// The actual ACP turn: authenticate → session → prompt → collect text.
async fn run_prompt(
    client: &kkagent_acp::AcpClient,
    spec: &PluginSubagentSpec,
    sub_cfg: &SubagentConfig,
    mirror: &Option<SubagentMirrorContext>,
) -> anyhow::Result<String> {
    // Authenticate only when the agent demands it and the manifest did not
    // opt out (env-injected credentials make the call redundant).
    if !spec.transport_config.skip_auth {
        client.authenticate().await?;
    }

    let cwd = std::path::PathBuf::from(&sub_cfg.working_dir);
    let session_id = client
        .start_session(&cwd, spec.transport_config.mode.as_deref())
        .await?;

    let prompt = match &spec.prompt_prefix {
        Some(prefix) if !prefix.is_empty() => format!("{prefix}\n\n{}", sub_cfg.prompt),
        _ => sub_cfg.prompt.clone(),
    };

    let (progress_tx, mut progress_rx) = mpsc::channel::<(String, ExternalProgress)>(256);
    let prompt_fut = client.prompt(&session_id, &prompt, progress_tx);
    tokio::pin!(prompt_fut);

    // Pump progress while the prompt turn runs: text deltas stream as child
    // MessageDelta events; ACP tool_call updates map onto ToolCall/ToolResult
    // so the TUI transcript shows the external agent's tool activity lines.
    let mut last_text = String::new();
    let result = loop {
        tokio::select! {
            outcome = &mut prompt_fut => break outcome,
            Some((_, progress)) = progress_rx.recv() => {
                let Some(m) = mirror else { continue };
                let child_event = match progress {
                    ExternalProgress::Text { delta } => {
                        last_text.push_str(&delta);
                        AgentEvent::MessageDelta {
                            session_id: sub_cfg.agent_id.clone(),
                            text: delta,
                        }
                    }
                    ExternalProgress::ToolCall {
                        tool_call_id,
                        title,
                        status,
                        ..
                    } => match status.as_deref() {
                        // Terminal statuses close the tool card with a state line.
                        Some("completed") => AgentEvent::ToolResult {
                            session_id: sub_cfg.agent_id.clone(),
                            tool_call_id,
                            tool_name: title,
                            output: "[ok]".into(),
                            is_error: false,
                        },
                        Some("failed") => AgentEvent::ToolResult {
                            session_id: sub_cfg.agent_id.clone(),
                            tool_call_id,
                            tool_name: title,
                            output: "[failed]".into(),
                            is_error: true,
                        },
                        // "pending"/"in_progress"/None: open (or refresh) the card.
                        _ => AgentEvent::ToolCall {
                            session_id: sub_cfg.agent_id.clone(),
                            tool_call_id,
                            tool_name: title,
                            input: serde_json::Value::Null,
                        },
                    },
                    ExternalProgress::Plan { .. } | ExternalProgress::Unknown { .. } => continue,
                };
                m.parent_event_tx
                    .send(AgentEvent::SubagentChildEvent {
                        session_id: m.parent_session_id.clone(),
                        subagent_id: sub_cfg.agent_id.clone(),
                        parent_tool_call_id: m.parent_tool_call_id.clone(),
                        event: Box::new(child_event),
                    })
                    .await
                    .ok();
            }
        }
    };

    let outcome = result?;
    // Prefer the accumulated stream; fall back to whatever the response
    // carried (some agents buffer instead of streaming).
    let final_text = if outcome.text.trim().is_empty() {
        last_text
    } else {
        outcome.text
    };
    if outcome.stop_reason == "cancelled" {
        anyhow::bail!("external subagent turn was cancelled");
    }
    if final_text.trim().is_empty() {
        return Ok("(external subagent finished with no text output)".into());
    }
    Ok(final_text)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::plugin::{PluginSubagentSpec, PluginSubagentTransportConfig};

    fn test_config(prompt: &str) -> SubagentConfig {
        SubagentConfig {
            agent_id: "test-agent".into(),
            description: "test".into(),
            prompt: prompt.into(),
            model: None,
            working_dir: ".".into(),
            profile: None,
            subagents: None,
            parent_session_id: None,
            parent_tool_call_id: None,
            run_in_background: false,
            depth: 1,
            parent_model: None,
        }
    }

    fn spec() -> PluginSubagentSpec {
        PluginSubagentSpec {
            name: "cursor".into(),
            plugin_id: "kk-test.cursor".into(),
            transport: "acp".into(),
            transport_config: PluginSubagentTransportConfig::default(),
            description: "Cursor CLI".into(),
            prompt_prefix: None,
            allow_delegation: false,
            auto_approve: true,
            env: Default::default(),
            timeout_secs: None,
            system_prompt: None,
            tools: Vec::new(),
            mcp_servers: Default::default(),
        }
    }

    #[test]
    fn rejects_unknown_transport() {
        let mut s = spec();
        s.transport = "http".into();
        let cfg = test_config("do the thing");
        let rt = tokio::runtime::Runtime::new().unwrap();
        let ctx = ExternalRunContext::detached(Arc::new(kkagent_config::AppConfig::default()));
        let err = rt
            .block_on(async { run_external_subagent(&s, cfg, None, ctx).await })
            .unwrap_err();
        assert!(err.to_string().contains("\"acp\", \"internal\""), "{err}");
    }

    #[test]
    fn resolve_command_defaults_to_cursor_cli() {
        let cmd = resolve_command(&PluginSubagentTransportConfig::default());
        assert_eq!(cmd, vec!["agent".to_string(), "acp".to_string()]);
        let custom = PluginSubagentTransportConfig {
            command: vec!["/opt/acp-agent".into()],
            ..Default::default()
        };
        assert_eq!(resolve_command(&custom), vec!["/opt/acp-agent".to_string()]);
    }

    #[test]
    fn prompt_prefix_is_prepended() {
        let mut s = spec();
        s.prompt_prefix = Some("Always answer in English.".into());
        let sub_cfg = test_config("do the thing");
        let prefix = s.prompt_prefix.clone().unwrap();
        let prompt = format!("{prefix}\n\n{}", sub_cfg.prompt);
        assert_eq!(prompt, "Always answer in English.\n\ndo the thing");
        let no_prefix = spec();
        assert!(no_prefix.prompt_prefix.is_none());
    }

    #[test]
    fn internal_transport_is_routed() {
        // An internal spec with no MCP servers and an allowlist that removes
        // every tool still runs — the loop answers from the model alone.
        let mut s = spec();
        s.transport = "internal".into();
        s.tools = vec!["Read".into()];
        let cfg = test_config("Reply with the single word: ok");
        let rt = tokio::runtime::Runtime::new().unwrap();
        let ctx = ExternalRunContext::detached(Arc::new(kkagent_config::AppConfig::default()));
        // No LLM credentials in test env — expect a clean failure, not a panic
        // or an "unsupported transport" bail.
        let outcome = rt.block_on(async { run_external_subagent(&s, cfg, None, ctx).await });
        match outcome {
            Ok(text) => assert!(!text.is_empty()),
            Err(err) => assert!(
                !err.to_string().contains("transport"),
                "unexpected transport error: {err}"
            ),
        }
    }
}
