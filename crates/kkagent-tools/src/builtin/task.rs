use async_trait::async_trait;
use kkagent_protocol::subagent::{SubagentConfig, SubagentManager, SubagentStatus};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

use crate::builtin::bash::BackgroundShellManager;
use crate::{Tool, ToolContext, ToolOutput};

/// Fire-and-forget launcher: schedules the subagent and returns immediately.
pub type SubagentLaunchFn = Arc<dyn Fn(SubagentConfig) + Send + Sync>;

const MAX_AGENT_SWARM_SUBAGENTS: usize = 128;
const PROMPT_TEMPLATE_PLACEHOLDER: &str = "{{item}}";

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
        spawn_subagent(&self.subagent_mgr, &self.launch, input, ctx, None).await
    }
}

async fn spawn_subagent(
    subagent_mgr: &SubagentManager,
    launch: &SubagentLaunchFn,
    input: Value,
    ctx: &ToolContext,
    resume_id: Option<String>,
) -> anyhow::Result<ToolOutput> {
    let desc = input
        .get("description")
        .and_then(|v| v.as_str())
        .unwrap_or("Unnamed task");
    let prompt = input.get("prompt").and_then(|v| v.as_str()).unwrap_or("");
    let model = input
        .get("model")
        .and_then(|v| v.as_str())
        .map(String::from);
    let profile = input
        .get("profile")
        .or_else(|| input.get("subagent_type"))
        .and_then(|v| v.as_str())
        .map(String::from);

    if prompt.trim().is_empty() {
        return Ok(ToolOutput::error("Task prompt must not be empty"));
    }

    if let Some(resume) = resume_id {
        match subagent_mgr.resume(&resume).await {
            Ok(_) => {
                let mut cfg = SubagentConfig {
                    agent_id: resume.clone(),
                    description: desc.to_string(),
                    prompt: prompt.to_string(),
                    model,
                    working_dir: ctx.working_dir.to_string_lossy().to_string(),
                    profile,
                    parent_session_id: Some(ctx.session_id.clone()),
                    parent_tool_call_id: ctx.tool_call_id.clone(),
                };
                if let Some(state) = subagent_mgr.get_state(&resume).await {
                    cfg.description = if desc == "Unnamed task" {
                        state.description
                    } else {
                        desc.to_string()
                    };
                }
                (launch)(cfg);
                return Ok(ToolOutput::success(format!(
                    "Resumed subagent id={resume}. Use TaskOutput to fetch results."
                )));
            }
            Err(e) => return Ok(ToolOutput::error(format!("Failed to resume: {e}"))),
        }
    }

    let agent_id = uuid::Uuid::new_v4().to_string();
    let mut working_dir = ctx.working_dir.to_string_lossy().to_string();
    if crate::git_worktree::worktree_enabled() {
        if let Ok(wt) =
            crate::git_worktree::create_worktree(&ctx.working_dir, &agent_id, None).await
        {
            working_dir = wt.path.display().to_string();
        }
    }

    let config = SubagentConfig {
        agent_id,
        description: desc.to_string(),
        prompt: prompt.to_string(),
        model,
        working_dir,
        profile,
        parent_session_id: Some(ctx.session_id.clone()),
        parent_tool_call_id: ctx.tool_call_id.clone(),
    };

    match subagent_mgr.spawn(config.clone()).await {
        Ok(agent_id) => {
            (launch)(config);
            Ok(ToolOutput::success(format!(
                "Subagent launched: {desc} (id={agent_id}). \
Use TaskOutput with this id to fetch results when ready; use TaskList to see status."
            )))
        }
        Err(e) => Ok(ToolOutput::error(format!(
            "Failed to launch subagent: {}",
            e
        ))),
    }
}

pub struct TaskOutputTool {
    subagent_mgr: Arc<SubagentManager>,
    bash_shells: Option<Arc<BackgroundShellManager>>,
}

impl TaskOutputTool {
    pub fn new(subagent_mgr: Arc<SubagentManager>) -> Self {
        Self {
            subagent_mgr,
            bash_shells: None,
        }
    }

    pub fn with_bash_shells(
        subagent_mgr: Arc<SubagentManager>,
        bash_shells: Arc<BackgroundShellManager>,
    ) -> Self {
        Self {
            subagent_mgr,
            bash_shells: Some(bash_shells),
        }
    }
}

#[async_trait]
impl Tool for TaskOutputTool {
    fn name(&self) -> &str {
        "TaskOutput"
    }
    fn description(&self) -> &str {
        "Get the status and result of a previously launched Task/Agent subagent or background Bash job by id."
    }
    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "task_id": {
                    "type": "string",
                    "description": "Subagent / task / shell id"
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
        if let Some(state) = self.subagent_mgr.get_state(id).await {
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
            return Ok(ToolOutput::success(out));
        }
        if let Some(bash) = &self.bash_shells {
            if let Some((description, _command, status, output, exit_code, running)) =
                bash.snapshot(id).await
            {
                let mut out = format!(
                    "task_id: {id}\nkind: bash\ndescription: {description}\nstatus: {status}"
                );
                if let Some(code) = exit_code {
                    out.push_str(&format!("\nexit_code: {code}"));
                }
                if !output.is_empty() {
                    out.push_str("\n\n");
                    out.push_str(&output);
                }
                if running {
                    out.push_str("\n\n(still running — call TaskOutput again later)");
                }
                return Ok(ToolOutput::success(out));
            }
        }
        Ok(ToolOutput::error(format!("Unknown task_id: {}", id)))
    }
}

pub struct TaskListTool {
    subagent_mgr: Arc<SubagentManager>,
    bash_shells: Option<Arc<BackgroundShellManager>>,
}

impl TaskListTool {
    pub fn new(subagent_mgr: Arc<SubagentManager>) -> Self {
        Self {
            subagent_mgr,
            bash_shells: None,
        }
    }

    pub fn with_bash_shells(
        subagent_mgr: Arc<SubagentManager>,
        bash_shells: Arc<BackgroundShellManager>,
    ) -> Self {
        Self {
            subagent_mgr,
            bash_shells: Some(bash_shells),
        }
    }
}

#[async_trait]
impl Tool for TaskListTool {
    fn name(&self) -> &str {
        "TaskList"
    }
    fn description(&self) -> &str {
        "List all Task/Agent subagents and background Bash jobs with their statuses."
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
        let mut lines: Vec<String> = self
            .subagent_mgr
            .list_all()
            .await
            .into_iter()
            .map(|t| {
                format!(
                    "- {} [agent/{:?}] {}{}",
                    t.agent_id,
                    t.status,
                    t.description,
                    if t.result.is_some() {
                        " (has result)"
                    } else {
                        ""
                    }
                )
            })
            .collect();
        if let Some(bash) = &self.bash_shells {
            for (id, desc, status, _running) in bash.list_jobs().await {
                lines.push(format!("- {id} [bash/{status}] {desc}"));
            }
        }
        if lines.is_empty() {
            return Ok(ToolOutput::success("No tasks."));
        }
        Ok(ToolOutput::success(lines.join("\n")))
    }
}

/// Agent tool — synchronous by default (kimi SubagentTool); optional background.
pub struct AgentTool {
    subagent_mgr: Arc<SubagentManager>,
    launch: SubagentLaunchFn,
}

impl AgentTool {
    pub fn new(subagent_mgr: Arc<SubagentManager>, launch: SubagentLaunchFn) -> Self {
        Self {
            subagent_mgr,
            launch,
        }
    }
}

#[async_trait]
impl Tool for AgentTool {
    fn name(&self) -> &str {
        "Agent"
    }
    fn description(&self) -> &str {
        "Run a profiled subagent (explore/coder/general). By default waits for completion and \
returns the result. Set run_in_background=true to detach and collect later via TaskOutput. \
Pass resume to continue an existing agent id."
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
                    "description": "Agent profile (default coder)"
                },
                "subagent_type": {
                    "type": "string",
                    "description": "Alias for profile"
                },
                "model": {"type": "string"},
                "resume": {
                    "type": "string",
                    "description": "Optional agent id to resume instead of spawning a new one"
                },
                "run_in_background": {
                    "type": "boolean",
                    "description": "If true, return immediately (default false — wait for completion)"
                }
            },
            "required": ["description", "prompt"]
        })
    }
    fn default_approve(&self) -> bool {
        true
    }
    async fn execute(&self, mut input: Value, ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        if input.get("profile").is_none() && input.get("subagent_type").is_none() {
            if let Some(obj) = input.as_object_mut() {
                obj.insert("profile".into(), Value::String("coder".into()));
            }
        }
        let resume = input
            .get("resume")
            .or_else(|| input.get("resume_id"))
            .and_then(|v| v.as_str())
            .map(String::from);
        let background = input
            .get("run_in_background")
            .and_then(|v| v.as_bool())
            .unwrap_or(false);

        let launched = spawn_subagent(&self.subagent_mgr, &self.launch, input, ctx, resume).await?;
        if launched.is_error || background {
            return Ok(launched);
        }

        let id = launched
            .content
            .split("id=")
            .nth(1)
            .and_then(|s| {
                s.split(|c: char| c == ')' || c == '.' || c.is_whitespace())
                    .next()
            })
            .unwrap_or("")
            .to_string();
        if id.is_empty() {
            return Ok(launched);
        }

        loop {
            if ctx
                .interrupted
                .as_ref()
                .is_some_and(|f| f.load(std::sync::atomic::Ordering::SeqCst))
            {
                let _ = self.subagent_mgr.stop(&id).await;
                return Ok(ToolOutput::error("Agent interrupted"));
            }
            match self.subagent_mgr.get_state(&id).await {
                Some(state)
                    if matches!(
                        state.status,
                        SubagentStatus::Complete
                            | SubagentStatus::Failed
                            | SubagentStatus::Cancelled
                    ) =>
                {
                    let mut out = format!(
                        "Agent {} finished ({:?})\ndescription: {}\n",
                        id, state.status, state.description
                    );
                    if let Some(r) = state.result {
                        out.push('\n');
                        out.push_str(&r);
                    }
                    if let Some(e) = state.error {
                        out.push_str("\nerror: ");
                        out.push_str(&e);
                    }
                    return Ok(ToolOutput::success(out));
                }
                None => return Ok(ToolOutput::error(format!("Agent {id} disappeared"))),
                _ => {
                    tokio::time::sleep(Duration::from_millis(200)).await;
                }
            }
        }
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
        "Launch multiple subagents in parallel. Pass `agents` array, or `prompt_template` + `items` \
(`{{item}}` placeholder). Optional `resume_agent_ids` map resumes existing agents first."
    }
    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "description": {"type": "string", "description": "Short description for the whole swarm"},
                "subagent_type": {"type": "string", "description": "Default profile for item-based spawns"},
                "profile": {"type": "string"},
                "model": {"type": "string"},
                "prompt_template": {
                    "type": "string",
                    "description": "Prompt template; {{item}} is replaced per items entry"
                },
                "items": {
                    "type": "array",
                    "items": {"type": "string"},
                    "maxItems": 128
                },
                "resume_agent_ids": {
                    "type": "object",
                    "additionalProperties": {"type": "string"},
                    "description": "Map of agent_id → prompt used to resume that agent"
                },
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
            }
        })
    }
    fn default_approve(&self) -> bool {
        true
    }
    async fn execute(&self, input: Value, ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        let mut launched = Vec::new();
        let swarm_desc = input
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("swarm");
        let default_profile = input
            .get("subagent_type")
            .or_else(|| input.get("profile"))
            .and_then(|v| v.as_str())
            .unwrap_or("coder");
        let default_model = input
            .get("model")
            .and_then(|v| v.as_str())
            .map(String::from);

        if let Some(map) = input.get("resume_agent_ids").and_then(|v| v.as_object()) {
            for (agent_id, prompt_v) in map {
                let prompt = prompt_v.as_str().unwrap_or("");
                if prompt.is_empty() {
                    continue;
                }
                let resume_input = serde_json::json!({
                    "description": format!("{swarm_desc}:{agent_id}"),
                    "prompt": prompt,
                    "profile": default_profile,
                });
                match spawn_subagent(
                    &self.subagent_mgr,
                    &self.launch,
                    resume_input,
                    ctx,
                    Some(agent_id.clone()),
                )
                .await?
                {
                    out if out.is_error => return Ok(out),
                    _ => launched.push(agent_id.clone()),
                }
            }
        }

        if let (Some(template), Some(items)) = (
            input.get("prompt_template").and_then(|v| v.as_str()),
            input.get("items").and_then(|v| v.as_array()),
        ) {
            if items.len() > MAX_AGENT_SWARM_SUBAGENTS {
                return Ok(ToolOutput::error(format!(
                    "items exceeds max {MAX_AGENT_SWARM_SUBAGENTS}"
                )));
            }
            for (i, item) in items.iter().enumerate() {
                let item_s = item.as_str().unwrap_or("").trim();
                if item_s.is_empty() {
                    continue;
                }
                let prompt = template.replace(PROMPT_TEMPLATE_PLACEHOLDER, item_s);
                let agent_input = serde_json::json!({
                    "description": format!("{swarm_desc}[{i}]"),
                    "prompt": prompt,
                    "profile": default_profile,
                    "model": default_model,
                });
                match spawn_subagent(&self.subagent_mgr, &self.launch, agent_input, ctx, None)
                    .await?
                {
                    out if out.is_error => {
                        return Ok(ToolOutput::error(format!(
                            "Failed after launching {}: {}",
                            launched.join(", "),
                            out.content
                        )));
                    }
                    out => {
                        if let Some(id) = out.content.split("id=").nth(1).and_then(|s| {
                            s.split(|c: char| c == ')' || c == '.' || c.is_whitespace())
                                .next()
                        }) {
                            launched.push(id.to_string());
                        }
                    }
                }
            }
        }

        let agents = input
            .get("agents")
            .and_then(|v| v.as_array())
            .cloned()
            .unwrap_or_default();
        for a in agents {
            let desc = a
                .get("description")
                .and_then(|v| v.as_str())
                .unwrap_or("swarm agent");
            let prompt = a.get("prompt").and_then(|v| v.as_str()).unwrap_or("");
            if prompt.is_empty() {
                continue;
            }
            let agent_input = serde_json::json!({
                "description": desc,
                "prompt": prompt,
                "profile": a.get("profile").cloned().unwrap_or(Value::String(default_profile.into())),
                "model": a.get("model").cloned().unwrap_or(Value::Null),
            });
            match spawn_subagent(&self.subagent_mgr, &self.launch, agent_input, ctx, None).await? {
                out if out.is_error => {
                    return Ok(ToolOutput::error(format!(
                        "Failed after launching {}: {}",
                        launched.join(", "),
                        out.content
                    )));
                }
                out => {
                    if let Some(id) = out.content.split("id=").nth(1).and_then(|s| {
                        s.split(|c: char| c == ')' || c == '.' || c.is_whitespace())
                            .next()
                    }) {
                        launched.push(id.to_string());
                    }
                }
            }
        }

        if launched.is_empty() {
            return Ok(ToolOutput::error(
                "No agents launched. Provide agents[], or prompt_template+items, and/or resume_agent_ids.",
            ));
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
    bash_shells: Option<Arc<BackgroundShellManager>>,
}

impl TaskStopTool {
    pub fn new(subagent_mgr: Arc<SubagentManager>) -> Self {
        Self {
            subagent_mgr,
            bash_shells: None,
        }
    }

    pub fn with_bash_shells(
        subagent_mgr: Arc<SubagentManager>,
        bash_shells: Arc<BackgroundShellManager>,
    ) -> Self {
        Self {
            subagent_mgr,
            bash_shells: Some(bash_shells),
        }
    }
}

#[async_trait]
impl Tool for TaskStopTool {
    fn name(&self) -> &str {
        "TaskStop"
    }
    fn description(&self) -> &str {
        "Stop a running Task/Agent subagent or background Bash job by id."
    }
    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "task_id": {
                    "type": "string",
                    "description": "Subagent / task / shell id"
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
        if let Ok(state) = self.subagent_mgr.stop(id).await {
            return Ok(ToolOutput::success(format!(
                "Stopped task {} ({})",
                state.agent_id, state.description
            )));
        }
        if let Some(bash) = &self.bash_shells {
            if bash.stop(id).await {
                return Ok(ToolOutput::success(format!("Stopped bash task {id}")));
            }
        }
        Ok(ToolOutput::error(format!("Unknown task_id: {id}")))
    }
}
