use async_trait::async_trait;
use serde_json::Value;
use std::sync::Arc;
use kkagent_protocol::subagent::{SubagentManager, SubagentConfig, SubagentStatus};

use crate::{Tool, ToolContext, ToolOutput};

/// Fire-and-forget launcher: schedules the subagent and returns immediately.
pub type SubagentLaunchFn = Arc<dyn Fn(SubagentConfig) + Send + Sync>;

pub struct TaskTool {
    subagent_mgr: Arc<SubagentManager>,
    launch: SubagentLaunchFn,
}

impl TaskTool {
    pub fn new(subagent_mgr: Arc<SubagentManager>, launch: SubagentLaunchFn) -> Self {
        Self {
            subagent_mgr,
            launch,
        }
    }
}

#[async_trait]
impl Tool for TaskTool {
    fn name(&self) -> &str {
        "Task"
    }
    fn description(&self) -> &str {
        "Launch a subagent to handle a complex or broad exploration task in its own context. \
Use for parallel codebase mapping, multi-file investigations, or long-running research. \
After launching, continue other work and collect results with TaskOutput / TaskList."
    }
    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "description": {
                    "type": "string",
                    "description": "Short description of what the subagent will do"
                },
                "prompt": {
                    "type": "string",
                    "description": "The detailed task for the subagent to perform"
                },
                "model": {
                    "type": "string",
                    "description": "Optional model alias override for the subagent"
                },
                "profile": {
                    "type": "string",
                    "enum": ["general", "explore", "coder"],
                    "description": "Subagent profile (default general)"
                }
            },
            "required": ["description", "prompt"]
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        let desc = input
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("Unnamed task");
        let prompt = input
            .get("prompt")
            .and_then(|v| v.as_str())
            .unwrap_or("");
        let model = input
            .get("model")
            .and_then(|v| v.as_str())
            .map(String::from);
        let profile = input
            .get("profile")
            .and_then(|v| v.as_str())
            .map(String::from);

        if prompt.trim().is_empty() {
            return Ok(ToolOutput::error("Task prompt must not be empty"));
        }

        let config = SubagentConfig {
            agent_id: uuid::Uuid::new_v4().to_string(),
            description: desc.to_string(),
            prompt: prompt.to_string(),
            model,
            working_dir: ctx.working_dir.to_string_lossy().to_string(),
            profile,
            parent_session_id: Some(ctx.session_id.clone()),
            parent_tool_call_id: ctx.tool_call_id.clone(),
        };

        match self.subagent_mgr.spawn(config.clone()).await {
            Ok(agent_id) => {
                (self.launch)(config);
                Ok(ToolOutput::success(format!(
                    "Subagent launched: {desc} (id={agent_id}). \
Use TaskOutput with this id to fetch results when ready; use TaskList to see status."
                )))
            }
            Err(e) => Ok(ToolOutput::error(format!("Failed to launch subagent: {}", e))),
        }
    }
}

pub struct TaskOutputTool {
    subagent_mgr: Arc<SubagentManager>,
}

impl TaskOutputTool {
    pub fn new(subagent_mgr: Arc<SubagentManager>) -> Self {
        Self { subagent_mgr }
    }
}

#[async_trait]
impl Tool for TaskOutputTool {
    fn name(&self) -> &str {
        "TaskOutput"
    }
    fn description(&self) -> &str {
        "Get the status and result of a previously launched Task subagent by id."
    }
    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "task_id": {
                    "type": "string",
                    "description": "Subagent / task id returned by the Task tool"
                },
                "agent_id": {
                    "type": "string",
                    "description": "Alias for task_id"
                }
            }
        })
    }
    fn read_only(&self) -> bool {
        true
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        let id = input
            .get("task_id")
            .or_else(|| input.get("agent_id"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if id.is_empty() {
            return Ok(ToolOutput::error("Missing task_id"));
        }
        match self.subagent_mgr.get_state(id).await {
            Some(state) => {
                let mut out = format!(
                    "task_id: {}\ndescription: {}\nstatus: {:?}\nturns_used: {}",
                    state.agent_id, state.description, state.status, state.turns_used
                );
                if let Some(ref r) = state.result {
                    out.push_str("\n\nresult:\n");
                    out.push_str(r);
                }
                if let Some(ref e) = state.error {
                    out.push_str("\n\nerror:\n");
                    out.push_str(e);
                }
                if state.status == SubagentStatus::Running {
                    out.push_str("\n\n(still running — call TaskOutput again later)");
                }
                Ok(ToolOutput::success(out))
            }
            None => Ok(ToolOutput::error(format!("Unknown task_id: {}", id))),
        }
    }
}

pub struct TaskListTool {
    subagent_mgr: Arc<SubagentManager>,
}

impl TaskListTool {
    pub fn new(subagent_mgr: Arc<SubagentManager>) -> Self {
        Self { subagent_mgr }
    }
}

#[async_trait]
impl Tool for TaskListTool {
    fn name(&self) -> &str {
        "TaskList"
    }
    fn description(&self) -> &str {
        "List all Task subagents and their statuses."
    }
    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {}
        })
    }
    fn read_only(&self) -> bool {
        true
    }

    async fn execute(&self, _input: Value, _ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        let all = self.subagent_mgr.list_all().await;
        if all.is_empty() {
            return Ok(ToolOutput::success("No tasks."));
        }
        let lines: Vec<String> = all
            .into_iter()
            .map(|t| {
                format!(
                    "- {} [{}] {}{}",
                    t.agent_id,
                    format!("{:?}", t.status).to_lowercase(),
                    t.description,
                    if t.result.is_some() { " (has result)" } else { "" }
                )
            })
            .collect();
        Ok(ToolOutput::success(lines.join("\n")))
    }
}

/// Agent tool — Task with explicit profile (explore/coder/general).
pub struct AgentTool {
    inner: TaskTool,
}

impl AgentTool {
    pub fn new(subagent_mgr: Arc<SubagentManager>, launch: SubagentLaunchFn) -> Self {
        Self {
            inner: TaskTool::new(subagent_mgr, launch),
        }
    }
}

#[async_trait]
impl Tool for AgentTool {
    fn name(&self) -> &str {
        "Agent"
    }
    fn description(&self) -> &str {
        "Launch a profiled subagent (explore/coder/general). Prefer explore for codebase mapping, \
coder for implementation. Collect results with TaskOutput."
    }
    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "description": {"type": "string"},
                "prompt": {"type": "string"},
                "profile": {
                    "type": "string",
                    "enum": ["general", "explore", "coder"],
                    "description": "Agent profile (default explore)"
                },
                "model": {"type": "string"}
            },
            "required": ["description", "prompt"]
        })
    }
    async fn execute(&self, mut input: Value, ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        if input.get("profile").is_none() {
            if let Some(obj) = input.as_object_mut() {
                obj.insert("profile".into(), Value::String("explore".into()));
            }
        }
        self.inner.execute(input, ctx).await
    }
}

/// Launch multiple subagents in parallel.
pub struct AgentSwarmTool {
    subagent_mgr: Arc<SubagentManager>,
    launch: SubagentLaunchFn,
}

impl AgentSwarmTool {
    pub fn new(subagent_mgr: Arc<SubagentManager>, launch: SubagentLaunchFn) -> Self {
        Self {
            subagent_mgr,
            launch,
        }
    }
}

#[async_trait]
impl Tool for AgentSwarmTool {
    fn name(&self) -> &str {
        "AgentSwarm"
    }
    fn description(&self) -> &str {
        "Launch multiple subagents in parallel. Pass `agents` array of {description, prompt, profile?}."
    }
    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "agents": {
                    "type": "array",
                    "items": {
                        "type": "object",
                        "properties": {
                            "description": {"type": "string"},
                            "prompt": {"type": "string"},
                            "profile": {"type": "string"},
                            "model": {"type": "string"}
                        },
                        "required": ["description", "prompt"]
                    }
                }
            },
            "required": ["agents"]
        })
    }
    async fn execute(&self, input: Value, ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        let agents = input
            .get("agents")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        if agents.is_empty() {
            return Ok(ToolOutput::error("agents array is empty"));
        }
        let mut launched = Vec::new();
        for a in agents {
            let desc = a
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("swarm agent");
            let prompt = a.get("prompt").and_then(|v| v.as_str()).unwrap_or("");
            if prompt.is_empty() {
                continue;
            }
            let config = SubagentConfig {
                agent_id: uuid::Uuid::new_v4().to_string(),
                description: desc.to_string(),
                prompt: prompt.to_string(),
                model: a.get("model").and_then(|v| v.as_str()).map(String::from),
                working_dir: ctx.working_dir.to_string_lossy().to_string(),
                profile: a
                    .get("profile")
                    .and_then(|v| v.as_str())
                    .map(String::from)
                    .or_else(|| Some("explore".into())),
                parent_session_id: Some(ctx.session_id.clone()),
                parent_tool_call_id: ctx.tool_call_id.clone(),
            };
            match self.subagent_mgr.spawn(config.clone()).await {
                Ok(id) => {
                    (self.launch)(config);
                    launched.push(id);
                }
                Err(e) => {
                    return Ok(ToolOutput::error(format!(
                        "Failed after launching {}: {}",
                        launched.join(", "),
                        e
                    )));
                }
            }
        }
        Ok(ToolOutput::success(format!(
            "Launched {} agents: {}\nUse TaskOutput / TaskList to collect results.",
            launched.len(),
            launched.join(", ")
        )))
    }
}

pub struct TaskStopTool {
    subagent_mgr: Arc<SubagentManager>,
}

impl TaskStopTool {
    pub fn new(subagent_mgr: Arc<SubagentManager>) -> Self {
        Self { subagent_mgr }
    }
}

#[async_trait]
impl Tool for TaskStopTool {
    fn name(&self) -> &str {
        "TaskStop"
    }
    fn description(&self) -> &str {
        "Stop a running Task subagent by id. Use TaskList to find running tasks."
    }
    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "task_id": {
                    "type": "string",
                    "description": "Subagent / task id returned by the Task tool"
                },
                "agent_id": {
                    "type": "string",
                    "description": "Alias for task_id"
                }
            }
        })
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        let id = input
            .get("task_id")
            .or_else(|| input.get("agent_id"))
            .and_then(|v| v.as_str())
            .unwrap_or("");
        if id.is_empty() {
            return Ok(ToolOutput::error("Missing task_id"));
        }
        match self.subagent_mgr.stop(id).await {
            Ok(state) => Ok(ToolOutput::success(format!(
                "Stopped task {} ({})",
                state.agent_id, state.description
            ))),
            Err(e) => Ok(ToolOutput::error(e.to_string())),
        }
    }
}
