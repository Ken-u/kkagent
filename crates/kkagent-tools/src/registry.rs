use std::collections::HashMap;
use std::sync::Arc;

use crate::Tool;

pub struct ToolRegistry {
    tools: HashMap<String, Arc<dyn Tool>>,
}

impl ToolRegistry {
    pub fn new() -> Self {
        Self {
            tools: HashMap::new(),
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
            })
            .collect()
    }
}

impl Default for ToolRegistry {
    fn default() -> Self {
        Self::new()
    }
}
