use std::collections::BTreeMap;
use std::sync::Arc;

use crate::Tool;

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
