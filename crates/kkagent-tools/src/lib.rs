pub mod accesses;
pub mod args_validator;
pub mod bash_ast;
pub mod builtin;
pub mod display;
pub mod git_worktree;
pub mod path_policy;
pub mod registry;
pub mod sandbox;
pub mod shell_safety;
pub mod web_providers;

pub use accesses::{infer_accesses, tool_accesses, ToolAccesses, ToolResourceAccess};
pub use bash_ast::{collect_commands, parse as parse_bash, pipes_into_shell};
pub use display::{builtin_display_schemas, render_chip, SummaryMode, ToolDisplaySchema};
pub use shell_safety::{analyze_shell_command, ShellRisk};

pub use builtin::cron::render_cron_fire_xml;
pub use builtin::{
    render_model_tool_skill_prompt, render_skill_loaded_block, render_user_slash_skill_prompt,
    BackgroundShellManager, BashOptions, BashTool, CronManager, SkillCatalog, SkillTool,
};
pub use registry::*;
pub use web_providers::WebServicesConfig;

use async_trait::async_trait;
use serde_json::Value;
use std::path::Path;

#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn parameters_schema(&self) -> Value;
    fn read_only(&self) -> bool {
        false
    }
    /// Per-call resource accesses (defaults to static inference).
    fn accesses(&self, input: &Value, working_dir: &Path) -> ToolAccesses {
        infer_accesses(self.name(), input, working_dir)
    }
    /// Approval rule subject (defaults to tool name).
    fn approval_rule(&self) -> &str {
        self.name()
    }
    /// Whether this tool is in the default-approve set (manual mode).
    fn default_approve(&self) -> bool {
        self.read_only()
    }
    /// Optional chip/summary schema override (falls back to global table).
    fn display_schema(&self) -> Option<ToolDisplaySchema> {
        None
    }
    async fn execute(&self, input: Value, ctx: &ToolContext) -> anyhow::Result<ToolOutput>;
}

#[derive(Debug, Clone)]
pub struct ToolContext {
    pub working_dir: std::path::PathBuf,
    pub session_id: String,
    /// Current plan file when the host supports file-backed plan mode.
    pub plan_file_path: Option<std::path::PathBuf>,
    pub image: kkagent_config::ImageConfig,
    /// Current tool_use id when available (for subagent mirroring).
    pub tool_call_id: Option<String>,
    /// Cooperative cancellation flag owned by the active session.
    pub interrupted: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, PartialEq, Eq)]
pub struct MediaOutput {
    pub media_type: String,
    pub data: String,
}

/// Steer / delivery payload injected into the next model turn (not shown as tool UI body).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolDelivery {
    pub message: String,
}

#[derive(Debug, Clone)]
pub struct ToolOutput {
    pub content: String,
    pub is_error: bool,
    /// Optional structured payload (e.g. todo items) for the agent loop / TUI.
    pub data: Option<Value>,
    /// Provider-neutral images to append after this tool result.
    pub images: Vec<MediaOutput>,
    /// When true, the agent loop should end the turn after applying this result.
    pub stop_turn: bool,
    /// Side channel for the model only (`<system>` block). Not sent to the TUI event.
    pub note: Option<String>,
    /// Optional steer message delivered after this tool result into the next turn.
    pub delivery: Option<ToolDelivery>,
}

impl ToolOutput {
    pub fn success(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            data: None,
            images: Vec::new(),
            stop_turn: false,
            note: None,
            delivery: None,
        }
    }

    pub fn success_with_data(content: impl Into<String>, data: Value) -> Self {
        Self {
            content: content.into(),
            is_error: false,
            data: Some(data),
            images: Vec::new(),
            stop_turn: false,
            note: None,
            delivery: None,
        }
    }

    pub fn error(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            data: None,
            images: Vec::new(),
            stop_turn: false,
            note: None,
            delivery: None,
        }
    }

    pub fn error_stop(content: impl Into<String>) -> Self {
        Self {
            content: content.into(),
            is_error: true,
            data: None,
            images: Vec::new(),
            stop_turn: true,
            note: None,
            delivery: None,
        }
    }

    pub fn with_image(mut self, media_type: impl Into<String>, data: impl Into<String>) -> Self {
        self.images.push(MediaOutput {
            media_type: media_type.into(),
            data: data.into(),
        });
        self
    }

    pub fn with_note(mut self, note: impl Into<String>) -> Self {
        self.note = Some(note.into());
        self
    }

    pub fn with_data(mut self, data: Value) -> Self {
        self.data = Some(data);
        self
    }

    pub fn with_delivery(mut self, message: impl Into<String>) -> Self {
        self.delivery = Some(ToolDelivery {
            message: message.into(),
        });
        self
    }

    /// Content projected to the model (content + optional `<system>` note).
    pub fn model_content(&self) -> String {
        match &self.note {
            Some(note) if !note.trim().is_empty() => {
                let note = note.trim();
                if self.content.is_empty() {
                    format!("<system>\n{note}\n</system>")
                } else if self.content.ends_with('\n') {
                    format!("{}<system>\n{note}\n</system>", self.content)
                } else {
                    format!("{}\n<system>\n{note}\n</system>", self.content)
                }
            }
            _ => self.content.clone(),
        }
    }
}

/// Built-in tools that do not require host-owned runtime managers.
pub fn register_core_tools(registry: &mut ToolRegistry) {
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
    registry.register(Arc::new(builtin::WritePlanTool));
    registry.register(Arc::new(builtin::ExitPlanModeTool));
    registry.register(Arc::new(builtin::SelectToolsTool::new()));
    registry.register(Arc::new(builtin::ReadMediaFileTool));
}

/// Register host-backed task and subagent tools with the caller's delegation policy.
pub fn register_subagent_tools(
    registry: &mut ToolRegistry,
    manager: std::sync::Arc<kkagent_protocol::subagent::SubagentManager>,
    launch: builtin::task::SubagentLaunchFn,
    allowed_subagents: Option<Vec<String>>,
) {
    use std::sync::Arc;

    registry.register(Arc::new(builtin::TaskTool::new(
        manager.clone(),
        launch.clone(),
    )));
    registry.register(Arc::new(builtin::AgentTool::with_allowed_subagents(
        manager.clone(),
        launch.clone(),
        allowed_subagents.clone(),
    )));
    registry.register(Arc::new(builtin::AgentSwarmTool::with_allowed_subagents(
        manager.clone(),
        launch,
        allowed_subagents,
    )));
    registry.register(Arc::new(builtin::TaskOutputTool::new(manager.clone())));
    registry.register(Arc::new(builtin::TaskListTool::new(manager.clone())));
    registry.register(Arc::new(builtin::TaskStopTool::new(manager)));
}

/// Backward-compatible main-agent core registration.
pub fn register_builtin_tools(registry: &mut ToolRegistry) {
    register_core_tools(registry);
}

pub struct ProfileToolSet;

impl ProfileToolSet {
    pub const GENERAL: &'static [&'static str] = &[
        "Read",
        "Write",
        "Edit",
        "Grep",
        "Glob",
        "Bash",
        "TaskList",
        "TaskOutput",
        "TaskStop",
        "CronCreate",
        "CronList",
        "CronDelete",
        "ReadMediaFile",
        "TodoList",
        "Skill",
        "WebSearch",
        "Agent",
        "AgentSwarm",
        "FetchURL",
        "AskUserQuestion",
        "EnterPlanMode",
        "ExitPlanMode",
        "CreateGoal",
        "GetGoal",
        "SetGoalBudget",
        "UpdateGoal",
    ];

    pub const CODER: &'static [&'static str] = &[
        "Agent",
        "AgentSwarm",
        "Bash",
        "CronCreate",
        "CronDelete",
        "CronList",
        "Edit",
        "EnterPlanMode",
        "ExitPlanMode",
        "Glob",
        "Grep",
        "Read",
        "ReadMediaFile",
        "Skill",
        "TaskList",
        "TaskOutput",
        "TaskStop",
        "TodoList",
        "WebSearch",
        "FetchURL",
        "AskUserQuestion",
    ];

    pub const EXPLORE: &'static [&'static str] = &[
        "Bash",
        "Read",
        "ReadMediaFile",
        "Glob",
        "Grep",
        "WebSearch",
        "FetchURL",
    ];

    pub fn for_profile(profile: &str) -> &'static [&'static str] {
        match profile.trim().to_ascii_lowercase().as_str() {
            "explore" => Self::EXPLORE,
            "coder" => Self::CODER,
            "agent" | "general" => Self::GENERAL,
            _ => Self::GENERAL,
        }
    }
}

pub fn retain_profile_tools(registry: &mut ToolRegistry, profile: &str) {
    registry.retain_names(ProfileToolSet::for_profile(profile));
}

#[cfg(test)]
mod profile_tool_tests {
    use super::*;
    use kkagent_protocol::subagent::SubagentManager;
    use std::sync::Arc;

    fn registry_for(profile: &str) -> ToolRegistry {
        let mut registry = ToolRegistry::new();
        register_core_tools(&mut registry);
        register_subagent_tools(
            &mut registry,
            Arc::new(SubagentManager::new(1)),
            Arc::new(|_| {}),
            kkagent_protocol::subagent::allowed_subagents_for(profile),
        );
        retain_profile_tools(&mut registry, profile);
        registry
    }

    fn names(registry: &ToolRegistry) -> Vec<&str> {
        registry
            .list()
            .into_iter()
            .map(|tool| tool.name())
            .collect()
    }

    #[test]
    fn explore_profile_is_read_only_and_cannot_delegate() {
        let registry = registry_for("explore");
        let names = names(&registry);

        assert!(names.contains(&"Read"));
        assert!(names.contains(&"Grep"));
        assert!(!names.contains(&"Write"));
        assert!(!names.contains(&"Edit"));
        assert!(!names.contains(&"Agent"));
        assert!(!names.contains(&"AgentSwarm"));
    }

    #[test]
    fn coder_profile_can_delegate_but_cannot_write_new_files() {
        let registry = registry_for("coder");
        let names = names(&registry);

        assert!(names.contains(&"Read"));
        assert!(names.contains(&"Edit"));
        assert!(names.contains(&"Agent"));
        assert!(names.contains(&"AgentSwarm"));
        assert!(!names.contains(&"Write"));
        assert!(!names.contains(&"Task"));
    }
}
