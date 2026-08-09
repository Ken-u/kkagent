pub mod registry;
pub mod builtin;
pub mod path_policy;
pub mod accesses;
pub mod shell_safety;
pub mod bash_ast;
pub mod args_validator;
pub mod display;
pub mod git_worktree;

pub use accesses::{infer_accesses, tool_accesses, ToolAccesses, ToolResourceAccess};
pub use display::{builtin_display_schemas, render_chip, SummaryMode, ToolDisplaySchema};
pub use bash_ast::{collect_commands, parse as parse_bash, pipes_into_shell};
pub use shell_safety::{analyze_shell_command, ShellRisk};

pub use registry::*;
pub use builtin::{
    BackgroundShellManager, BashOptions, BashTool, SkillCatalog, CronManager, WebServicesConfig,
};
pub use builtin::cron::render_cron_fire_xml;

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
    /// Current tool_use id when available (for subagent mirroring).
    pub tool_call_id: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
    /// Optional structured payload (e.g. todo items) for the agent loop / TUI.
    pub data: Option<Value>,
}

impl ToolOutput {
    pub fn success(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            data: None,
        }
    }

    pub fn success_with_data(content: impl Into<String>, data: Value) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            data: Some(data),
        }
    }

    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            data: None,
        }
    }
}

pub fn register_builtin_tools(registry: &mut ToolRegistry) {
    use std::sync::Arc;
    registry.register(Arc::new(builtin::ReadTool));
    registry.register(Arc::new(builtin::WriteTool));
    registry.register(Arc::new(builtin::EditTool));
    registry.register(Arc::new(builtin::GrepTool));
    registry.register(Arc::new(builtin::GlobTool));
    registry.register(Arc::new(builtin::BashTool::default()));
    registry.register(Arc::new(builtin::TodoListTool::new()));
    registry.register(Arc::new(builtin::AskUserQuestionTool));
    registry.register(Arc::new(builtin::EnterPlanModeTool));
    registry.register(Arc::new(builtin::ExitPlanModeTool));
    registry.register(Arc::new(builtin::SelectToolsTool::new()));
    registry.register(Arc::new(builtin::ReadMediaFileTool));
}
