use std::collections::BTreeMap;
use std::path::Path;
use std::sync::Arc;

use serde_json::Value;

use crate::{Tool, ToolAccesses, ToolContext, ToolDisclosure, ToolDisplaySchema, ToolOutput};

pub struct ToolRegistry {
    /// Name-keyed `BTreeMap`: iteration is deterministically sorted by name,
    /// so the wire `tools[]` prefix stays byte-stable across requests.
    /// Providers key their prompt cache on that prefix; a `HashMap` would
    /// reshuffle tool order on every register/retain and bust the cache.
    tools: BTreeMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: BTreeMap::new(),
        }
    }

    pub fn register(&mut self, tool: Arc<dyn Tool>) {
        self.tools.insert(tool.name().to_string(), tool);
    }

    /// Register a tool under an explicit name, replacing any existing entry.
    /// Used by plugin overrides to re-bind a bridged MCP tool onto a
    /// built-in tool's name (the stub keeps the original wire name).
    pub fn register_at(&mut self, name: &str, tool: Arc<dyn Tool>) {
        self.tools.insert(name.to_string(), tool);
    }

    pub fn get(&self, name: &str) -> Option<Arc<dyn Tool>> {
        self.tools.get(name).cloned()
    }

    pub fn list(&self) -> Vec<&dyn Tool> {
        self.tools.values().map(|t| t.as_ref()).collect()
    }

    pub fn retain_names(&mut self, allowed_names: &[&str]) {
        self.tools
            .retain(|name, _| allowed_names.iter().any(|allowed| name == allowed));
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
}
