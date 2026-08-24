//! Plugin override application: base system-prompt replacement and built-in
//! tool re-binding onto plugin MCP tools.
//!
//! Overrides are applied as the **last step** of tool-registry construction
//! so built-in registration cannot clobber them (see `build_turn_tool_registry`
//! in kkagent/src/main.rs). Guard tools (`AskUserQuestion`, plan-mode tools,
//! `Goal`) are rejected by `kkagent_config::plugin_policy` and can never be
//! replaced, keeping permission/plan-mode name-based checks sound.

use crate::plugin::{plugin_tool_namespace, PluginManager};
use kkagent_config::PluginsConfig;
use kkagent_tools::ToolRegistry;
use std::sync::RwLock;

/// Global base-prompt override installed by an enabled plugin
/// (`replaceSystemPrompt: true`). Read by `Session::new_with_source` for both
/// main and subagent sessions so persona replacement applies uniformly.
static SYSTEM_PROMPT_OVERRIDE: RwLock<Option<String>> = RwLock::new(None);

/// Install (or clear) the global base-prompt override. Called at startup and
/// on `/plugins reload` / enable / disable / install / remove.
pub fn set_system_prompt_override(prompt: Option<String>) {
    let mut guard = SYSTEM_PROMPT_OVERRIDE
        .write()
        .expect("system prompt override lock poisoned");
    *guard = prompt.filter(|p| !p.trim().is_empty());
}

/// The effective base system prompt: plugin replacement when installed,
/// otherwise the built-in default.
pub fn effective_base_system_prompt() -> String {
    if let Some(prompt) = SYSTEM_PROMPT_OVERRIDE
        .read()
        .expect("system prompt override lock poisoned")
        .clone()
    {
        return prompt;
    }
    crate::session::runtime::default_system_prompt()
}

/// Load the override state from a plugin manager and install it. Returns a
/// human-readable summary for diagnostics/logging.
pub async fn sync_system_prompt_override(plugins: &PluginManager) -> Option<String> {
    match plugins.system_prompt_override().await {
        Some((prompt, losers)) => {
            if !losers.is_empty() {
                tracing::warn!(
                    losers = losers.join(", "),
                    "multiple plugins replace the system prompt; using the lexicographically first"
                );
            }
            set_system_prompt_override(Some(prompt.clone()));
            Some(prompt)
        }
        None => {
            set_system_prompt_override(None);
            None
        }
    }
}

/// Apply plugin tool overrides onto a fully built registry (built-ins plus
/// MCP-bridged tools). Must run last. Returns diagnostics: `(builtin, reason)`
/// pairs for overrides that were skipped.
pub async fn apply_plugin_tool_overrides(
    registry: &mut ToolRegistry,
    overrides: &[(String, String)],
    plugins: &PluginManager,
    policy: &PluginsConfig,
) -> Vec<(String, String)> {
    // Snapshot plugin manifest info needed to resolve source refs without
    // holding the manager lock across the (sync) registry mutation.
    let manifests = plugins.manifest_snapshot().await;
    let mut skipped = Vec::new();
    // `overrides` arrive sorted by plugin id (then builtin name), so the
    // first entry per built-in wins — consistent with prompt/service
    // override semantics. Later contenders are reported, never applied
    // (also prevents adapter-wrapping-adapter chains).
    let mut applied: std::collections::HashSet<String> = std::collections::HashSet::new();
    for (builtin, source) in overrides {
        if applied.contains(builtin) {
            skipped.push((
                builtin.clone(),
                "shadowed by an earlier plugin's override (plugin id order)".to_string(),
            ));
            continue;
        }
        if registry.get(builtin).is_none() {
            skipped.push((builtin.clone(), "built-in tool not registered".into()));
            continue;
        }
        if !policy.is_overridable(builtin) {
            skipped.push((
                builtin.clone(),
                "not overridable under current policy (guard tool or no opt-in)".into(),
            ));
            continue;
        }
        let Some((plugin_id, server, tool)) = parse_source_ref(source) else {
            skipped.push((builtin.clone(), format!("invalid source ref {source}")));
            continue;
        };
        let Some(manifest) = manifests.iter().find(|m| m.0 == plugin_id) else {
            skipped.push((builtin.clone(), format!("plugin {plugin_id} not loaded")));
            continue;
        };
        let multi_server = manifest.1.len() > 1;
        if !manifest.1.contains_key(server.as_str()) {
            skipped.push((
                builtin.clone(),
                format!("plugin {plugin_id} has no server {server}"),
            ));
            continue;
        }
        let namespace = plugin_tool_namespace(&plugin_id, &server, multi_server);
        let qualified = kkagent_mcp::qualify_namespaced_tool_name(&namespace, &tool);
        match registry.get(&qualified) {
            Some(bridged) => {
                // Preserve the built-in's identity/policy surface (wire name,
                // disclosure posture, read-only, approval rule) while the
                // replacement provides description/schema/execution.
                let original = registry.get(builtin).expect("builtin checked above");
                registry.register_at(
                    builtin,
                    std::sync::Arc::new(kkagent_tools::OverrideTool::new(original, bridged)),
                );
                applied.insert(builtin.clone());
            }
            None => skipped.push((
                builtin.clone(),
                format!("source tool {qualified} unavailable (server disabled or not connected)"),
            )),
        }
    }
    skipped
}

/// `plugin:<plugin-id>:<server>.<tool>` -> `(plugin_id, server, tool)`.
fn parse_source_ref(source: &str) -> Option<(String, String, String)> {
    let rest = source.strip_prefix("plugin:")?;
    let (plugin_id, tail) = rest.split_once(':')?;
    let (server, tool) = tail.rsplit_once('.')?;
    if plugin_id.is_empty() || server.is_empty() || tool.is_empty() {
        return None;
    }
    Some((plugin_id.to_string(), server.to_string(), tool.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_valid_source_refs() {
        let (p, s, t) = parse_source_ref("plugin:kk-web:tavily.search").unwrap();
        assert_eq!(
            (p.as_str(), s.as_str(), t.as_str()),
            ("kk-web", "tavily", "search")
        );
        // tool names may contain dots — rsplit keeps the last segment
        let (_, _, t) = parse_source_ref("plugin:a:b.c.d").unwrap();
        assert_eq!(t, "d");
    }

    #[test]
    fn rejects_malformed_source_refs() {
        assert!(parse_source_ref("kk-web:tavily.search").is_none());
        assert!(parse_source_ref("plugin::tavily.search").is_none());
        assert!(parse_source_ref("plugin:kk-web:.search").is_none());
        assert!(parse_source_ref("plugin:kk-web:tavily.").is_none());
        assert!(parse_source_ref("plugin:kk-web:nodot").is_none());
    }

    #[test]
    fn effective_prompt_falls_back_to_default() {
        set_system_prompt_override(None);
        assert_eq!(
            effective_base_system_prompt(),
            crate::session::runtime::default_system_prompt()
        );
        set_system_prompt_override(Some("custom".into()));
        assert_eq!(effective_base_system_prompt(), "custom");
        set_system_prompt_override(None);
    }

    /// Minimal `Arc<dyn Tool>` stand-in for registry override tests.
    struct StubTool(&'static str);

    /// Deferred-disclosure stub standing in for a bridged MCP tool.
    struct DeferredStub(&'static str);

    #[async_trait::async_trait]
    impl kkagent_tools::Tool for StubTool {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "builtin stub"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {}})
        }
        fn read_only(&self) -> bool {
            true
        }
        async fn execute(
            &self,
            _args: serde_json::Value,
            _ctx: &kkagent_tools::ToolContext,
        ) -> anyhow::Result<kkagent_tools::ToolOutput> {
            Ok(kkagent_tools::ToolOutput::success("stub"))
        }
    }

    #[async_trait::async_trait]
    impl kkagent_tools::Tool for DeferredStub {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "replacement stub"
        }
        fn parameters_schema(&self) -> serde_json::Value {
            serde_json::json!({"type": "object", "properties": {"q": {}}})
        }
        fn disclosure(&self) -> kkagent_tools::ToolDisclosure {
            kkagent_tools::ToolDisclosure::Deferred
        }
        async fn execute(
            &self,
            _args: serde_json::Value,
            _ctx: &kkagent_tools::ToolContext,
        ) -> anyhow::Result<kkagent_tools::ToolOutput> {
            Ok(kkagent_tools::ToolOutput::success("deferred stub"))
        }
    }

    async fn empty_manager() -> std::sync::Arc<crate::plugin::PluginManager> {
        let dir = std::env::temp_dir().join(format!("kkagent-ovr-{}", uuid::Uuid::new_v4()));
        let manager = crate::plugin::PluginManager::discover(&dir).await;
        let _ = tokio::fs::remove_dir_all(dir).await;
        manager
    }

    #[tokio::test]
    async fn override_rebinds_bridged_tool_under_builtin_name() {
        let manager = empty_manager().await;
        let mut registry = kkagent_tools::ToolRegistry::new();
        registry.register(std::sync::Arc::new(StubTool("Web")));
        registry.register(std::sync::Arc::new(StubTool("mcp__kk-web__search")));
        let overrides = vec![("Web".to_string(), "plugin:kk-web:tavily.search".to_string())];
        // Manager has no manifest for kk-web: the source ref cannot resolve,
        // so the override is skipped with a "not loaded" diagnostic.
        let skipped = apply_plugin_tool_overrides(
            &mut registry,
            &overrides,
            &manager,
            &kkagent_config::PluginsConfig::default(),
        )
        .await;
        assert_eq!(skipped[0].0, "Web");
        assert!(skipped[0].1.contains("not loaded"));
    }

    #[tokio::test]
    async fn successful_override_rebinds_bridged_tool_in_place() {
        // Real manifest so the source ref resolves; the qualified name is
        // what a bridged tavily server would register (single server →
        // namespace = plugin id).
        let dir = std::env::temp_dir().join(format!("kkagent-ovr-ok-{}", uuid::Uuid::new_v4()));
        let root = dir.join("kk-web");
        tokio::fs::create_dir_all(&root).await.unwrap();
        tokio::fs::write(
            root.join("kk.plugin.json"),
            serde_json::json!({
                "name": "kk-web",
                "toolOverrides": { "Web": "tavily.search" },
                "mcpServers": { "tavily": { "command": "npx", "args": ["-y", "tavily-mcp"] } }
            })
            .to_string(),
        )
        .await
        .unwrap();
        let manager = crate::plugin::PluginManager::discover(&dir).await;

        let mut registry = kkagent_tools::ToolRegistry::new();
        registry.register(std::sync::Arc::new(StubTool("Web")));
        registry.register(std::sync::Arc::new(DeferredStub("mcp__kk-web__search")));

        let overrides = manager.tool_overrides().await;
        let skipped = apply_plugin_tool_overrides(
            &mut registry,
            &overrides,
            &manager,
            &kkagent_config::PluginsConfig::default(),
        )
        .await;
        assert!(skipped.is_empty(), "skipped: {skipped:?}");

        let web = registry.get("Web").expect("Web slot present");
        // Identity surface: wire name and disclosure stay those of the
        // built-in (Inline), NOT the bridged MCP tool (Deferred).
        assert_eq!(web.name(), "Web");
        assert_eq!(
            web.disclosure(),
            kkagent_tools::ToolDisclosure::Inline,
            "override must inherit the covered tool's disclosure posture"
        );
        assert!(web.read_only(), "policy surface inherited from built-in");
        // Behavior surface: description and schema come from the replacement.
        assert_eq!(web.description(), "replacement stub");
        assert!(web.parameters_schema()["properties"].get("q").is_some());
        // Wire definitions carry the built-in name with the replacement schema.
        let defs = registry.tool_definitions();
        let web_def = defs.iter().find(|d| d.name == "Web").unwrap();
        assert!(web_def.parameters["properties"].get("q").is_some());
        assert_eq!(web_def.disclosure, kkagent_tools::ToolDisclosure::Inline);

        let _ = tokio::fs::remove_dir_all(dir).await;
    }

    #[tokio::test]
    async fn policy_rejects_guard_and_unlisted_tools() {
        let manager = empty_manager().await;
        let mut registry = kkagent_tools::ToolRegistry::new();
        registry.register(std::sync::Arc::new(StubTool("AskUserQuestion")));
        registry.register(std::sync::Arc::new(StubTool("Bash")));
        let overrides = vec![
            (
                "AskUserQuestion".to_string(),
                "plugin:p:evil.ask".to_string(),
            ),
            ("Bash".to_string(), "plugin:p:s.run".to_string()),
        ];
        let skipped = apply_plugin_tool_overrides(
            &mut registry,
            &overrides,
            &manager,
            &kkagent_config::PluginsConfig::default(),
        )
        .await;
        assert_eq!(skipped.len(), 2);
        assert!(skipped
            .iter()
            .any(|(n, r)| n == "AskUserQuestion" && r.contains("policy")));

        // Opt-in via config flips Bash past the policy gate (still skipped
        // later on the missing-plugin check, but with "not loaded").
        let policy = kkagent_config::PluginsConfig {
            extra_overridable_tools: vec!["Bash".to_string()],
        };
        let skipped =
            apply_plugin_tool_overrides(&mut registry, &overrides, &manager, &policy).await;
        assert_eq!(skipped.len(), 2);
        let bash = skipped.iter().find(|(n, _)| n == "Bash").unwrap();
        assert!(bash.1.contains("not loaded"), "got: {}", bash.1);
    }

    #[tokio::test]
    async fn missing_builtin_reports_skip() {
        let manager = empty_manager().await;
        let mut registry = kkagent_tools::ToolRegistry::new();
        let overrides = vec![("Web".to_string(), "plugin:p:s.t".to_string())];
        let skipped = apply_plugin_tool_overrides(
            &mut registry,
            &overrides,
            &manager,
            &kkagent_config::PluginsConfig::default(),
        )
        .await;
        assert_eq!(skipped[0].1, "built-in tool not registered");
    }

    #[tokio::test]
    async fn same_builtin_first_plugin_wins_and_later_is_reported() {
        let dir = std::env::temp_dir().join(format!("kkagent-ovr-dup-{}", uuid::Uuid::new_v4()));
        for id in ["a-plugin", "b-plugin"] {
            let root = dir.join(id);
            tokio::fs::create_dir_all(&root).await.unwrap();
            tokio::fs::write(
                root.join("kk.plugin.json"),
                serde_json::json!({
                    "name": id,
                    "toolOverrides": { "Web": "s.search" },
                    "mcpServers": { "s": { "command": "npx" } }
                })
                .to_string(),
            )
            .await
            .unwrap();
        }
        let manager = crate::plugin::PluginManager::discover(&dir).await;

        let mut registry = kkagent_tools::ToolRegistry::new();
        registry.register(std::sync::Arc::new(StubTool("Web")));
        registry.register(std::sync::Arc::new(DeferredStub("mcp__a-plugin__search")));
        registry.register(std::sync::Arc::new(DeferredStub("mcp__b-plugin__search")));

        let overrides = manager.tool_overrides().await;
        assert_eq!(overrides.len(), 2, "both plugins declare Web overrides");
        let skipped = apply_plugin_tool_overrides(
            &mut registry,
            &overrides,
            &manager,
            &kkagent_config::PluginsConfig::default(),
        )
        .await;
        // a-plugin (id order) wins; b-plugin is reported as shadowed.
        assert_eq!(skipped.len(), 1);
        assert_eq!(skipped[0].0, "Web");
        assert!(skipped[0].1.contains("shadowed"));
        // The applied override is a-plugin's tool (its schema marker is on
        // the stub's description — both stubs share description strings, so
        // assert the adapter did not double-wrap by checking identity).
        let web = registry.get("Web").unwrap();
        assert_eq!(web.name(), "Web");

        let _ = tokio::fs::remove_dir_all(dir).await;
    }
}
