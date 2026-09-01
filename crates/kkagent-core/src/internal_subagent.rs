//! Internal plugin subagents — an in-process kkagent agent loop driven by a
//! plugin-declared subagent type (`transport: "internal"`).
//!
//! Unlike the built-in profiles (`run_subagent_mirrored`), an internal plugin
//! subagent can:
//!
//! - carry its own **system prompt** (`spec.system_prompt`);
//! - **bind a model alias at declaration time** (`spec.model`) — one of
//!   `default` / `fast` / `current` / `secondary`, expanded like every
//!   other symbolic token and overriding the delegation request's `model`;
//! - restrict its tool set to an **allowlist** (`spec.tools`) — core tools,
//!   the Web tool and plugin-private MCP tools all qualify for filtering;
//! - run **plugin-private MCP servers** (`spec.mcp_servers`), started lazily
//!   per delegation. Their tools are namespaced `<plugin>.<server>.<tool>`
//!   and never touch the main session's context.
//!
//! Everything else — model resolution, permission chain, event mirroring,
//! SubagentManager lifecycle — mirrors the built-in path.

use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tokio::task::AbortHandle;

use kkagent_protocol::subagent::SubagentConfig;
use kkagent_protocol::subagent::SubagentManager;
use kkagent_tools::{register_core_tools, register_subagent_tools, ToolRegistry};

use crate::agent_loop::AgentLoop;
use crate::external_subagent::ExternalRunContext;
use crate::permission::PermissionChain;
use crate::plugin::PluginSubagentSpec;
use crate::subagent_runtime::{extract_final_assistant_text, SubagentMirrorContext};

/// Max rounds for internal plugin subagents (wiki/RAG lookups rarely need
/// more; sits between explore's 16 and coder's 32).
const MAX_ROUNDS: u32 = 24;

/// Resolve the model an internal plugin subagent runs with.
///
/// A valid declaration-time alias binding (`spec.model`, one of
/// `default`/`fast`/`current`/`secondary`) wins over the delegation
/// request's `model` token; without a binding the standard subagent chain
/// applies (request token → `[subagent.default_models]` for `general` →
/// global `secondary_model` → `default_model`).
pub fn resolve_internal_subagent_model(
    spec: &PluginSubagentSpec,
    app_config: &kkagent_config::AppConfig,
    request_model: Option<&str>,
    parent_model: Option<&str>,
) -> String {
    match spec.model_alias() {
        Some(alias) => app_config.expand_model_alias_token(&alias, parent_model),
        None => app_config.resolve_subagent_model("general", request_model, parent_model),
    }
}

/// Run an internal plugin subagent to completion, returning final text.
pub async fn run_internal_subagent(
    spec: &PluginSubagentSpec,
    sub_cfg: SubagentConfig,
    mirror: Option<SubagentMirrorContext>,
    ctx: ExternalRunContext,
) -> anyhow::Result<String> {
    let app_config = ctx.app_config;

    // Model: a plugin-declared alias (`model` in the manifest, internal
    // transport only) is a declaration-time binding and wins over the
    // request's symbolic token; without one, resolution is the standard
    // chain (explicit token → `[subagent.default_models]` general →
    // secondary → default). Plugins cannot pin raw model ids — only the
    // aliases default/fast/current/secondary survive validation.
    let model = resolve_internal_subagent_model(
        spec,
        &app_config,
        sub_cfg.model.as_deref(),
        sub_cfg.parent_model.as_deref(),
    );

    let mut session = crate::session::Session::for_subagent(
        format!("sub-{}", sub_cfg.agent_id),
        std::path::PathBuf::from(&sub_cfg.working_dir),
        ctx.permission_mode,
        model.clone(),
    );
    if let Some(parent_flag) = ctx.interrupt {
        session.inherit_interrupted(parent_flag);
    }
    session.image_config = app_config.image.clone();
    session.attach_workspace_concurrency_guard();
    session.inject_workspace_instructions().await;
    // Generic subagent addon keeps the built-in framing, then the plugin's
    // own persona on top.
    session.system_prompt.push_str(
        "\n\n# Subagent profile: plugin\n\
You are a focused subagent registered by a plugin. Complete the delegated \
task with the tools available to you and return a concise final summary.",
    );
    if let Some(addon) = &spec.system_prompt {
        session.system_prompt.push_str("\n\n");
        session.system_prompt.push_str(addon);
    }
    session.add_user_message(sub_cfg.prompt.clone());

    let permission_rules = app_config
        .permission
        .as_ref()
        .map(|p| p.rules.clone())
        .unwrap_or_default();
    let permission = PermissionChain::new(ctx.permission_mode, permission_rules);
    let (event_tx, event_rx) = mpsc::channel(256);

    // --- Tool registry: core + Web + delegation, then plugin MCP + filter.
    let mut tools = ToolRegistry::new();
    register_core_tools(&mut tools);
    if let Some(web_tool) = kkagent_tools::builtin::WebTool::try_new(ctx.web.clone()) {
        tools.register(Arc::new(web_tool));
    }

    // Delegation tools: nested launches go through the host's real launcher
    // when `allowDelegation` is on (so the subagent can reach both built-in
    // and other plugin profiles), otherwise the tools are registered with an
    // empty allowlist so the model gets clear denials instead of guessing.
    let nested_manager = Arc::new(SubagentManager::new(
        app_config.subagent.effective_max_concurrent(),
    ));
    // Depth budget for nested launches: the delegating runtime (this
    // function) stamps the child's depth from its own, mirroring how
    // subagent_runtime stamps built-in nested launches. The host launcher
    // skips depth-0 handling for pre-stamped configs.
    let parent_depth = sub_cfg.depth;
    let max_depth = app_config.subagent.effective_max_depth();
    let depth_manager = nested_manager.clone();
    let host_launch = ctx.launch.clone();
    let nested_launch: kkagent_tools::builtin::task::SubagentLaunchFn = Arc::new(
        move |mut config: kkagent_protocol::subagent::SubagentConfig, interrupt| {
            let manager = depth_manager.clone();
            let agent_id = config.agent_id.clone();
            if let Err(reason) =
                kkagent_protocol::subagent::stamp_child_depth(&mut config, parent_depth, max_depth)
            {
                tracing::warn!("Rejected nested subagent {agent_id}: {reason}");
                tokio::spawn(async move {
                    manager.fail(&agent_id, reason).await;
                });
                return;
            }
            match &host_launch {
                Some(launch) => launch(config, interrupt),
                None => {
                    // No host wiring (tests / detached contexts): fail the
                    // entry so TaskOutput surfaces the limitation.
                    let manager = manager;
                    tokio::spawn(async move {
                        manager
                            .fail(&agent_id, "nested delegation is not available".into())
                            .await;
                    });
                }
            }
        },
    );
    // The delegation allowlist is driven by `allow_delegation` here, not by
    // profile policy.
    let allowed = if spec.allow_delegation {
        None
    } else {
        Some(Vec::new())
    };
    register_subagent_tools(
        &mut tools,
        nested_manager,
        nested_launch,
        allowed,
        app_config.tools.clone(),
        Vec::new(),
    );

    // Plugin-private MCP servers: lazy per-delegation manager. Server
    // runtime name follows the existing plugin convention
    // `plugin-<plugin_id>:<server>` so tool namespaces compress to
    // `<plugin_id>_<server>` (e.g. `wiki_search`) exactly like the main
    // session's plugin MCP tools.
    if !spec.mcp_servers.is_empty() {
        let plugin_id = spec.plugin_id.split('.').next().unwrap_or(&spec.plugin_id);
        let configs: Vec<kkagent_mcp::McpServerConfig> = spec
            .mcp_servers
            .iter()
            .map(|(name, cfg)| {
                kkagent_mcp::McpServerConfig::from_app(format!("plugin-{plugin_id}:{name}"), cfg)
            })
            .collect();
        let manager = Arc::new(kkagent_mcp::McpManager::new(configs));
        let _ = manager.connect_all().await;
        kkagent_mcp::register_mcp_tools(&mut tools, &manager).await;
    }

    // Allowlist filter — applies to core, Web, delegation and MCP tools.
    if !spec.tools.is_empty() {
        tools.retain_names(&spec.tools.iter().map(String::as_str).collect::<Vec<_>>());
    }

    // --- Event mirroring (identical shape to the built-in path).
    if let Some(m) = &mirror {
        let _ = m
            .parent_event_tx
            .send(kkagent_protocol::AgentEvent::SubagentSpawned {
                session_id: m.parent_session_id.clone(),
                subagent_id: sub_cfg.agent_id.clone(),
                subagent_name: spec.qualified_name(),
                parent_tool_call_id: m.parent_tool_call_id.clone(),
                description: Some(sub_cfg.description.clone()),
                model: Some(model.clone()),
                run_in_background: sub_cfg.run_in_background,
                prompt: Some(sub_cfg.prompt.clone()),
            })
            .await;
        let _ = m
            .parent_event_tx
            .send(kkagent_protocol::AgentEvent::SubagentStarted {
                session_id: m.parent_session_id.clone(),
                subagent_id: sub_cfg.agent_id.clone(),
            })
            .await;
    }
    let mirror_for_drain = mirror.clone();
    let child_id = sub_cfg.agent_id.clone();
    let drain_handle = tokio::spawn(async move {
        let mut event_rx = event_rx;
        while let Some(ev) = event_rx.recv().await {
            let Some(m) = &mirror_for_drain else { continue };
            let _ = m
                .parent_event_tx
                .send(kkagent_protocol::AgentEvent::SubagentChildEvent {
                    session_id: m.parent_session_id.clone(),
                    subagent_id: child_id.clone(),
                    parent_tool_call_id: m.parent_tool_call_id.clone(),
                    event: Box::new(ev),
                })
                .await;
        }
    });

    let abort_registry = Arc::new(Mutex::new(HashMap::<String, AbortHandle>::new()));
    let mut agent = AgentLoop::with_max_rounds(
        app_config.clone(),
        Arc::new(tools),
        Arc::new(Mutex::new(permission)),
        event_tx,
        abort_registry,
        MAX_ROUNDS,
    );
    if let Some(parent_session_id) = sub_cfg.parent_session_id.clone() {
        let store = crate::agent_loop::ToolResultStore::for_subagent(
            kkagent_config::default_config_dir(),
            parent_session_id,
        );
        agent = agent.with_tool_result_store(Arc::new(store));
    }

    let run_result = agent.run_turn(&mut session).await;
    let _ = std::fs::remove_dir_all(session.session_dir());
    drop(agent);
    let _ = drain_handle.await;

    match run_result {
        Ok(()) => {
            let result = extract_final_assistant_text(&session);
            if let Some(m) = &mirror {
                let summary: String = result.chars().take(400).collect();
                let _ = m
                    .parent_event_tx
                    .send(kkagent_protocol::AgentEvent::SubagentCompleted {
                        session_id: m.parent_session_id.clone(),
                        subagent_id: sub_cfg.agent_id.clone(),
                        result_summary: summary,
                        usage: None,
                        model: Some(session.get_model_alias()),
                    })
                    .await;
            }
            Ok(result)
        }
        Err(e) => {
            if let Some(m) = &mirror {
                let _ = m
                    .parent_event_tx
                    .send(kkagent_protocol::AgentEvent::SubagentFailed {
                        session_id: m.parent_session_id.clone(),
                        subagent_id: sub_cfg.agent_id.clone(),
                        error: e.to_string(),
                    })
                    .await;
            }
            Err(anyhow::anyhow!("internal plugin subagent failed: {e}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec_with_model(model: Option<&str>) -> PluginSubagentSpec {
        serde_json::from_value(serde_json::json!({
            "name": "search",
            "transport": "internal",
            "model": model
        }))
        .unwrap()
    }

    fn app_config_with_models(
        default: &str,
        fast: Option<&str>,
        secondary: Option<&str>,
    ) -> kkagent_config::AppConfig {
        kkagent_config::AppConfig {
            default_model: Some(default.to_string()),
            fast_model: fast.map(str::to_string),
            secondary_model: secondary.map(str::to_string),
            ..Default::default()
        }
    }

    #[test]
    fn model_alias_accepts_case_insensitive_aliases_only() {
        assert_eq!(
            spec_with_model(Some("fast")).model_alias().as_deref(),
            Some("fast")
        );
        assert_eq!(
            spec_with_model(Some("CURRENT")).model_alias().as_deref(),
            Some("current")
        );
        assert_eq!(
            spec_with_model(Some(" secondary "))
                .model_alias()
                .as_deref(),
            Some("secondary")
        );
        assert_eq!(spec_with_model(Some("test/model")).model_alias(), None);
        assert_eq!(spec_with_model(Some("")).model_alias(), None);
        assert_eq!(spec_with_model(None).model_alias(), None);
    }

    #[test]
    fn declared_alias_overrides_request_token() {
        let config =
            app_config_with_models("test/default", Some("test/fast"), Some("test/secondary"));
        // Declaration pins `fast` even though the request asks `current`.
        let model = resolve_internal_subagent_model(
            &spec_with_model(Some("fast")),
            &config,
            Some("current"),
            Some("test/session-model"),
        );
        assert_eq!(model, "test/fast");
    }

    #[test]
    fn declared_current_tracks_parent_session_model() {
        let config = app_config_with_models("test/default", None, None);
        let model = resolve_internal_subagent_model(
            &spec_with_model(Some("current")),
            &config,
            Some("default"),
            Some("test/session-model"),
        );
        assert_eq!(model, "test/session-model");
        // Without a parent model, `current` falls back to `default`.
        let model =
            resolve_internal_subagent_model(&spec_with_model(Some("current")), &config, None, None);
        assert_eq!(model, "test/default");
    }

    #[test]
    fn declared_secondary_and_default_fall_back_sensibly() {
        let config = app_config_with_models("test/default", None, None);
        assert_eq!(
            resolve_internal_subagent_model(
                &spec_with_model(Some("secondary")),
                &config,
                None,
                None
            ),
            "test/default"
        );
        assert_eq!(
            resolve_internal_subagent_model(
                &spec_with_model(Some("default")),
                &config,
                Some("fast"),
                None
            ),
            "test/default"
        );
    }

    #[test]
    fn raw_model_id_is_ignored_and_standard_resolution_applies() {
        let config =
            app_config_with_models("test/default", Some("test/fast"), Some("test/secondary"));
        // `model_alias()` filters raw ids out, so the request's `current`
        // token resolves normally (to the parent session model).
        let model = resolve_internal_subagent_model(
            &spec_with_model(Some("test/model")),
            &config,
            Some("current"),
            Some("test/session-model"),
        );
        assert_eq!(model, "test/session-model");
    }

    #[test]
    fn without_binding_standard_resolution_applies() {
        let config =
            app_config_with_models("test/default", Some("test/fast"), Some("test/secondary"));
        let model = resolve_internal_subagent_model(
            &spec_with_model(None),
            &config,
            Some("fast"),
            Some("test/session-model"),
        );
        assert_eq!(model, "test/fast");
    }
}
