use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use kkagent_protocol::{ToolAllowlist, ToolGate};
use serde_json::Value;

use crate::{Tool, ToolAccesses, ToolContext, ToolDisclosure, ToolDisplaySchema, ToolOutput};

/// Why [`ToolRegistry::register_unique`] refused a tool.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RegisterError {
    #[error("tool `{0}` is not permitted by the tool allowlist")]
    NotPermitted(String),
    #[error("a tool named `{0}` is already registered")]
    NameTaken(String),
}

pub struct ToolRegistry {
    /// Name-keyed `BTreeMap`: iteration is deterministically sorted by name,
    /// so the wire `tools[]` prefix stays byte-stable across requests.
    /// Providers key their prompt cache on that prefix; a `HashMap` would
    /// reshuffle tool order on every register/retain and bust the cache.
    tools: BTreeMap<String, Arc<dyn Tool>>,
    /// Optional strict ceiling on which tool names may live in this registry.
    /// Once set it can only be narrowed. Every insertion path funnels through
    /// it, so plugin overrides, MCP bridging, dynamic loading and host tool
    /// injection cannot add a tool the role does not allow.
    allowlist: Option<ToolAllowlist>,
    /// Optional pre-execution reviewer (the embedding host). It is consulted
    /// at the single point where a tool call turns into an execution, after
    /// kkagent's own permission chain has let the call through.
    gate: Option<Arc<dyn ToolGate>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: BTreeMap::new(),
            allowlist: None,
            gate: None,
        }
    }

    /// Put a host review gate in front of every tool of this registry.
    pub fn set_gate(&mut self, gate: Arc<dyn ToolGate>) {
        self.gate = Some(gate);
    }

    /// The host review gate, if one is installed.
    pub fn gate(&self) -> Option<&Arc<dyn ToolGate>> {
        self.gate.as_ref()
    }

    /// Restrict this registry to `allowlist`. Combined with any existing
    /// ceiling by intersection (a ceiling can be narrowed, never widened) and
    /// prunes tools that are no longer permitted.
    pub fn set_allowlist(&mut self, allowlist: &ToolAllowlist) {
        let narrowed = match &self.allowlist {
            Some(existing) => existing.intersect(allowlist),
            None => allowlist.clone(),
        };
        self.tools.retain(|name, _| narrowed.allows(name));
        self.allowlist = Some(narrowed);
    }

    /// The active ceiling, if the registry is restricted.
    pub fn allowlist(&self) -> Option<&ToolAllowlist> {
        self.allowlist.as_ref()
    }

    /// Whether a tool named `name` may be listed, fetched or executed.
    /// Unrestricted registries permit every name.
    pub fn permits(&self, name: &str) -> bool {
        self.allowlist
            .as_ref()
            .is_none_or(|allowlist| allowlist.allows(name))
    }

    /// Register a tool. A tool the allowlist does not permit is dropped.
    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        let name = tool.name().to_string();
        self.register_at(&name, tool);
    }

    /// Whether a tool is registered under `name`.
    pub fn contains(&self, name: &str) -> bool {
        self.tools.contains_key(name)
    }

    /// Register a tool only if its name is free. Unlike [`Self::register`]
    /// this never replaces an existing tool, so an injected tool cannot take
    /// over a built-in's name. Respects the allowlist like every insertion.
    pub fn register_unique(&mut self, tool: Arc<dyn Tool>) -> Result<(), RegisterError> {
        let name = tool.name().to_string();
        if !self.permits(&name) {
            return Err(RegisterError::NotPermitted(name));
        }
        if self.tools.contains_key(&name) {
            return Err(RegisterError::NameTaken(name));
        }
        self.tools.insert(name, tool);
        Ok(())
    }

    /// Register a tool under an explicit name, replacing any existing entry.
    /// Used by plugin overrides to re-bind a bridged MCP tool onto a
    /// built-in tool's name (the stub keeps the original wire name).
    pub fn register_at(&mut self, name: &str, tool: Arc<dyn Tool>) {
        if !self.permits(name) {
            tracing::debug!(tool = name, "tool rejected by registry allowlist");
            return;
        }
        self.tools.insert(name.to_string(), tool);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        if !self.permits(name) {
            return None;
        }
        self.tools.get(name).cloned()
    }

    pub fn list(&self) -> Vec<&dyn Tool> {
        self.tools.values().map(|t| t.as_ref()).collect()
    }

    pub fn retain_names(&mut self, allowed_names: &[&str]) {
        self.tools
            .retain(|name, _| allowed_names.iter().any(|allowed| name == allowed));
    }

    /// Remove a tool by wire name (e.g. pruning delegation tools once the
    /// subagent nesting budget is exhausted). Returns whether it existed.
    pub fn remove(&mut self, name: &str) -> bool {
        self.tools.remove(name).is_some()
    }

    pub fn tool_definitions(&self) -> Vec<kkagent_protocol::tools::ToolDefinition> {
        self.tools
            .values()
            .map(|t| kkagent_protocol::tools::ToolDefinition {
                name: t.name().to_string(),
                description: t.description().to_string(),
                parameters: t.parameters_schema(),
                read_only: t.read_only(),
                disclosure: t.disclosure(),
            })
            .collect()
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}

/// Identity-preserving adapter for plugin tool overrides.
///
/// The *behavior surface* (description, schema, execution, display) comes
/// from `replacement`; the *identity and policy surface* — wire name,
/// `read_only`, disclosure posture, resource-access inference, approval
/// rule, default-approve — is inherited from `original`, so an overridden
/// built-in is indistinguishable from the original for the model,
/// permission chain, and progressive disclosure.
pub struct OverrideTool {
    original: Arc<dyn Tool>,
    replacement: Arc<dyn Tool>,
}

impl OverrideTool {
    pub fn new(original: Arc<dyn Tool>, replacement: Arc<dyn Tool>) -> Self {
        Self {
            original,
            replacement,
        }
    }
}

#[async_trait::async_trait]
impl Tool for OverrideTool {
    fn name(&self) -> &str {
        self.original.name()
    }

    fn description(&self) -> &str {
        self.replacement.description()
    }

    fn parameters_schema(&self) -> Value {
        self.replacement.parameters_schema()
    }

    fn read_only(&self) -> bool {
        self.original.read_only()
    }

    fn disclosure(&self) -> ToolDisclosure {
        self.original.disclosure()
    }

    fn normalize_input(&self, input: Value) -> Value {
        self.replacement.normalize_input(input)
    }

    fn accesses(&self, input: &Value, working_dir: &Path) -> ToolAccesses {
        self.original.accesses(input, working_dir)
    }

    fn approval_rule(&self) -> &str {
        self.original.approval_rule()
    }

    fn default_approve(&self) -> bool {
        self.original.default_approve()
    }

    fn display_schema(&self) -> Option<ToolDisplaySchema> {
        self.replacement.display_schema()
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        self.replacement.execute(input, ctx).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{ToolContext, ToolOutput};
    use serde_json::Value;

    struct StubTool(&'static str);

    #[async_trait::async_trait]
    impl Tool for StubTool {
        fn name(&self) -> &str {
            self.0
        }
        fn description(&self) -> &str {
            "stub"
        }
        fn parameters_schema(&self) -> Value {
            serde_json::json!({"type": "object"})
        }
        async fn execute(&self, _input: Value, _ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
            unimplemented!("stub tool is never executed")
        }
    }

    /// Wire `tools[]` must be name-sorted so provider prompt caches are not
    /// invalidated by iteration-order churn.
    #[test]
    fn tool_definitions_are_sorted_by_name() {
        let mut registry = ToolRegistry::new();
        for name in ["mcp__x", "alpha", "Zulu", "Beta"] {
            registry.register(Arc::new(StubTool(name)));
        }
        let names: Vec<String> = registry
            .tool_definitions()
            .into_iter()
            .map(|d| d.name)
            .collect();
        assert_eq!(names, vec!["Beta", "Zulu", "alpha", "mcp__x"]);
    }

    fn names_of(registry: &ToolRegistry) -> Vec<String> {
        registry
            .tool_definitions()
            .into_iter()
            .map(|d| d.name)
            .collect()
    }

    #[test]
    fn allowlist_prunes_existing_tools_and_filters_later_registrations() {
        let mut registry = ToolRegistry::new();
        for name in ["Read", "Bash", "mcp__gh__issues", "mcp__other__x"] {
            registry.register(Arc::new(StubTool(name)));
        }
        registry.set_allowlist(&ToolAllowlist::new(["Read", "mcp__gh__*"]));
        assert_eq!(names_of(&registry), vec!["Read", "mcp__gh__issues"]);

        // Later registrations (MCP bridge, host injection) are filtered too.
        registry.register(Arc::new(StubTool("Bash")));
        registry.register(Arc::new(StubTool("mcp__gh__prs")));
        registry.register(Arc::new(StubTool("mcp__other__y")));
        assert_eq!(
            names_of(&registry),
            vec!["Read", "mcp__gh__issues", "mcp__gh__prs"]
        );
        assert!(registry.get("Bash").is_none());
        assert!(registry.get("Read").is_some());
        assert!(registry.permits("Read"));
        assert!(!registry.permits("Bash"));
    }

    #[test]
    fn register_at_cannot_rebind_a_disallowed_name() {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(StubTool("Read")));
        registry.set_allowlist(&ToolAllowlist::new(["Read"]));
        // A plugin override re-binding a bridged tool onto `Bash` is refused.
        registry.register_at("Bash", Arc::new(StubTool("mcp__plugin__bash")));
        assert!(registry.get("Bash").is_none());
        assert_eq!(names_of(&registry), vec!["Read"]);
    }

    #[test]
    fn allowlist_can_only_be_narrowed() {
        let mut registry = ToolRegistry::new();
        for name in ["Read", "Grep", "Bash"] {
            registry.register(Arc::new(StubTool(name)));
        }
        registry.set_allowlist(&ToolAllowlist::new(["Read", "Grep"]));
        // Attempting to widen back to everything keeps the original ceiling.
        registry.set_allowlist(&ToolAllowlist::new(["*"]));
        registry.register(Arc::new(StubTool("Bash")));
        assert_eq!(names_of(&registry), vec!["Grep", "Read"]);
        registry.set_allowlist(&ToolAllowlist::new(["Read"]));
        assert_eq!(names_of(&registry), vec!["Read"]);
    }

    #[test]
    fn register_unique_never_replaces_and_respects_the_allowlist() {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(StubTool("Bash")));
        registry.set_allowlist(&ToolAllowlist::new(["Bash", "host_*"]));
        assert_eq!(
            registry.register_unique(Arc::new(StubTool("Bash"))),
            Err(RegisterError::NameTaken("Bash".into()))
        );
        assert_eq!(
            registry.register_unique(Arc::new(StubTool("Read"))),
            Err(RegisterError::NotPermitted("Read".into()))
        );
        assert_eq!(
            registry.register_unique(Arc::new(StubTool("host_task"))),
            Ok(())
        );
        assert!(registry.contains("host_task"));
        assert!(!registry.contains("Read"));
        assert_eq!(names_of(&registry), vec!["Bash", "host_task"]);
    }

    #[test]
    fn empty_allowlist_denies_everything() {
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(StubTool("Read")));
        registry.set_allowlist(&ToolAllowlist::deny_all());
        registry.register(Arc::new(StubTool("Read")));
        assert!(names_of(&registry).is_empty());
    }

    #[test]
    fn unrestricted_registry_permits_everything() {
        let registry = ToolRegistry::new();
        assert!(registry.allowlist().is_none());
        assert!(registry.permits("whatever"));
    }
}
