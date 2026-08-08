pub mod registry;
pub mod builtin;

pub use registry::*;

use async_trait::async_trait;
use serde_json::Value;

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters_schema(&self) -> Value;
    fn read_only(&self) -> bool { false }
    async fn execute(&self, input: Value, ctx: &ToolContext) -> anyhow::Result<ToolOutput>;
}

#[derive(Debug, Clone)]
pub struct ToolContext {
    pub working_dir: std::path::PathBuf,
    pub session_id: String,
}

#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
}

impl ToolOutput {
    pub fn success(content: impl Into<String>) -> Self {
        Self { content: content.into(), is_error: false }
    }

    pub fn error(content: impl Into<String>) -> Self {
        Self { content: content.into(), is_error: true }
    }
}

pub fn register_builtin_tools(registry: &mut ToolRegistry) {
    use std::sync::Arc;
    registry.register(Arc::new(builtin::ReadTool));
    registry.register(Arc::new(builtin::WriteTool));
    registry.register(Arc::new(builtin::EditTool));
    registry.register(Arc::new(builtin::GrepTool));
    registry.register(Arc::new(builtin::GlobTool));
    registry.register(Arc::new(builtin::BashTool));
    registry.register(Arc::new(builtin::TodoListTool::new()));
    registry.register(Arc::new(builtin::ExitPlanModeTool));
}
