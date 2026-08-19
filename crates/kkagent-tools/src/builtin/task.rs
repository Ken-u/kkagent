use async_trait::async_trait;
use kkagent_protocol::subagent::{
    allowed_subagents_for, SubagentConfig, SubagentManager, SubagentStatus,
};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

use crate::builtin::bash::BackgroundShellManager;
use crate::{Tool, ToolContext, ToolOutput};

/// Fire-and-forget launcher: schedules the subagent and returns immediately.
pub type SubagentLaunchFn = Arc<dyn Fn(SubagentConfig) + Send + Sync>;

const MAX_AGENT_SWARM_SUBAGENTS: usize = 128;
const PROMPT_TEMPLATE_PLACEHOLDER: &str = "{{item}}";

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
    let requested_profile = input
        .get("profile")
        .or_else(|| input.get("subagent_type"))
        .and_then(|v| v.as_str())
        .map(String::from);

    if prompt.trim().is_empty() {
        return Ok(ToolOutput::error("Task prompt must not be empty"));
    }

    if let Some(resume) = resume_id {
        let previous = subagent_mgr.get_state(&resume).await;
        match subagent_mgr.resume(&resume).await {
            Ok(_) => {
                let profile = previous
                    .as_ref()
                    .and_then(|state| state.profile.clone())
                    .or(requested_profile);
                let subagents = previous
                    .as_ref()
                    .and_then(|state| state.subagents.clone())
                    .or_else(|| allowed_subagents_for(profile.as_deref().unwrap_or("general")));
                let mut cfg = SubagentConfig {
                    agent_id: resume.clone(),
                    description: desc.to_string(),
                    prompt: prompt.to_string(),
                    model,
                    working_dir: ctx.working_dir.to_string_lossy().to_string(),
                    profile,
                    subagents,
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
        subagents: allowed_subagents_for(requested_profile.as_deref().unwrap_or("general")),
        profile: requested_profile,
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

    async fn fetch_status(&self, id: &str) -> ToolOutput {
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
            return ToolOutput::success(out);
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
                return ToolOutput::success(out);
            }
        }
        ToolOutput::error(format!("Unknown task_id: {}", id))
    }

    async fn list(&self) -> ToolOutput {
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
            return ToolOutput::success("No tasks.");
        }
        ToolOutput::success(lines.join("\n"))
    }

    async fn stop(&self, id: &str) -> ToolOutput {
        if let Some(state) = self.subagent_mgr.get_state(id).await {
            let _ = self.subagent_mgr.stop(id).await;
            return ToolOutput::success(format!(
                "Stopped task {} ({})",
                state.agent_id, state.description
            ));
        }
        if let Some(bash) = &self.bash_shells {
            if bash.stop(id).await {
                return ToolOutput::success(format!("Stopped bash task {id}"));
            }
        }
        ToolOutput::error(format!("Unknown task_id: {id}"))
    }
}

#[async_trait]
impl Tool for TaskOutputTool {
    fn name(&self) -> &str {
        "TaskOutput"
    }
    fn description(&self) -> &str {
        "Manage background tasks by id (subagents and background Bash jobs): fetch status/result \
(default), list all, or stop one. This subsumes the former TaskList / TaskStop tools."
    }
    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "action": {
                    "type": "string",
                    "enum": ["status", "list", "stop"],
                    "description": "status (default) = fetch result of task_id; list = all tasks; stop = terminate task_id"
                },
                "task_id": {
                    "type": "string",
                    "description": "Subagent / task / shell id (for status and stop)"
                },
                "agent_id": {
                    "type": "string",
                    "description": "Alias for task_id"
                }
            }
        })
    }
    fn read_only(&self) -> bool {
        // status/list are read-only; the permission layer only treats this
        // entry as read-only for the default status action.
        true
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        let action = input.get("action").and_then(|v| v.as_str()).unwrap_or("");
        let id = input
            .get("task_id")
            .or_else(|| input.get("agent_id"))
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string();
        match action {
            "list" => Ok(self.list().await),
            "stop" => {
                if id.is_empty() {
                    return Ok(ToolOutput::error("Missing task_id for stop"));
                }
                Ok(self.stop(&id).await)
            }
            _ => {
                if id.is_empty() {
                    return Ok(ToolOutput::error(
                        "Missing task_id. Use action=list to see all tasks.",
                    ));
                }
                Ok(self.fetch_status(&id).await)
            }
        }
    }
}

/// Unified delegation tool (`Agent`) — replaces the former Task / Agent /
/// AgentSwarm trio. One name, four fan-out shapes:
///
/// - `prompt` — single subagent (sync by default, `run_in_background=true` for async)
/// - `agents[]` — named parallel fan-out with per-agent prompts
/// - `prompt_template` + `items[]` — templated parallel fan-out (`{{item}}` placeholder)
/// - `resume_agent_ids` map / `resume` — re-prompt finished agents
#[derive(Clone)]
pub struct AgentTool {
    subagent_mgr: Arc<SubagentManager>,
    launch: SubagentLaunchFn,
    allowed_subagents: Option<Vec<String>>,
    description: String,
}

impl AgentTool {
    pub fn new(subagent_mgr: Arc<SubagentManager>, launch: SubagentLaunchFn) -> Self {
        Self::with_allowed_subagents(subagent_mgr, launch, None)
    }

    pub fn with_allowed_subagents(
        subagent_mgr: Arc<SubagentManager>,
        launch: SubagentLaunchFn,
        allowed_subagents: Option<Vec<String>>,
    ) -> Self {
        let description = delegation_description(
            "Delegate a task to a subagent running in its own context. Single `prompt` = one agent \
(sync, or `run_in_background=true` for async). `agents[]` = parallel fan-out with per-agent \
prompts. `prompt_template` + `items[]` = templated fan-out. `resume` / `resume_agent_ids` = \
re-prompt finished agents. After launching, collect results with TaskOutput.",
            allowed_subagents.as_deref(),
        );
        Self {
            subagent_mgr,
            launch,
            allowed_subagents,
            description,
        }
    }

    fn default_profile_of(input: &Value) -> String {
        input
            .get("subagent_type")
            .or_else(|| input.get("profile"))
            .and_then(Value::as_str)
            .unwrap_or("coder")
            .to_string()
    }

    /// Reject a fan-out request before launching anything if any requested
    /// profile is outside the delegation allowlist.
    fn check_requested_profiles(&self, input: &Value) -> Result<(), String> {
        let default_profile = Self::default_profile_of(input);
        check_profile_allowed(&default_profile, &self.allowed_subagents)?;
        if let Some(agents) = input.get("agents").and_then(Value::as_array) {
            for agent in agents {
                let profile = agent
                    .get("profile")
                    .or_else(|| agent.get("subagent_type"))
                    .and_then(Value::as_str)
                    .unwrap_or(&default_profile);
                check_profile_allowed(profile, &self.allowed_subagents)?;
            }
        }
        Ok(())
    }

    /// Fire-and-forget fan-out (former AgentSwarmTool::execute).
    async fn execute_swarm(&self, input: Value, ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        if let Err(error) = self.check_requested_profiles(&input) {
            return Ok(ToolOutput::error(error));
        }
        let mut launched = Vec::new();
        let swarm_desc = input
            .get("description")
            .and_then(|v| v.as_str())
            .unwrap_or("swarm");
        let default_profile = Self::default_profile_of(&input);
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
                "profile": a.get("profile").cloned().unwrap_or(Value::String(default_profile.clone())),
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
            "Launched {} agents: {}\nUse TaskOutput to collect results.",
            launched.len(),
            launched.join(", ")
        )))
    }

    /// Single subagent (former AgentTool::execute): sync wait by default.
    async fn execute_single(
        &self,
        mut input: Value,
        ctx: &ToolContext,
    ) -> anyhow::Result<ToolOutput> {
        if input.get("profile").is_none() && input.get("subagent_type").is_none() {
            if let Some(obj) = input.as_object_mut() {
                obj.insert("profile".into(), Value::String("coder".into()));
            }
        }
        if let Err(error) = check_requested_profile(&input, &self.allowed_subagents, "coder") {
            return Ok(ToolOutput::error(error));
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

#[async_trait]
impl Tool for AgentTool {
    fn name(&self) -> &str {
        "Agent"
    }
    fn description(&self) -> &str {
        &self.description
    }
    fn disclosure(&self) -> crate::ToolDisclosure {
        crate::ToolDisclosure::Deferred
    }
    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "description": {"type": "string", "description": "Short description of the subagent / swarm"},
                "prompt": {"type": "string", "description": "The detailed task for a single subagent"},
                "profile": {
                    "type": "string",
                    "enum": ["general", "explore", "coder"],
                    "description": "Agent profile (default coder)"
                },
                "subagent_type": {"type": "string", "description": "Alias for profile"},
                "model": {"type": "string"},
                "resume": {
                    "type": "string",
                    "description": "Optional agent id to resume instead of spawning a new one"
                },
                "run_in_background": {
                    "type": "boolean",
                    "description": "If true, return immediately (default false — wait for completion)"
                },
                "resume_agent_ids": {
                    "type": "object",
                    "additionalProperties": {"type": "string"},
                    "description": "Map of agent_id → prompt used to resume that agent"
                },
                "prompt_template": {
                    "type": "string",
                    "description": "Prompt template; {{item}} is replaced per items entry"
                },
                "items": {
                    "type": "array",
                    "items": {"type": "string"},
                    "maxItems": 128
                },
                "agents": {
                    "type": "array",
                    "description": "Parallel fan-out: one subagent per entry",
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
        let swarm_mode = input.get("resume_agent_ids").is_some_and(|v| v.is_object())
            || input
                .get("agents")
                .is_some_and(|v| v.as_array().is_some_and(|a| !a.is_empty()))
            || input
                .get("prompt_template")
                .and_then(|v| v.as_str())
                .is_some_and(|t| !t.is_empty())
                && input.get("items").is_some_and(|v| v.is_array());
        if swarm_mode {
            self.execute_swarm(input, ctx).await
        } else {
            self.execute_single(input, ctx).await
        }
    }
}

fn check_requested_profile(
    input: &Value,
    allowed_subagents: &Option<Vec<String>>,
    default_profile: &str,
) -> Result<(), String> {
    let profile = input
        .get("profile")
        .or_else(|| input.get("subagent_type"))
        .and_then(Value::as_str)
        .unwrap_or(default_profile);
    check_profile_allowed(profile, allowed_subagents)
}

fn check_profile_allowed(
    requested_profile: &str,
    allowed_subagents: &Option<Vec<String>>,
) -> Result<(), String> {
    let Some(allowlist) = allowed_subagents else {
        return Ok(());
    };
    if allowlist
        .iter()
        .any(|allowed| allowed.eq_ignore_ascii_case(requested_profile))
    {
        return Ok(());
    }
    Err(format!(
        "Profile '{requested_profile}' is not in the allowed subagent allowlist: [{}]",
        allowlist.join(", ")
    ))
}

fn delegation_description(base: &str, allowed_subagents: Option<&[String]>) -> String {
    match allowed_subagents {
        None => format!("{base} Delegation profiles are unrestricted."),
        Some([]) => format!("{base} This caller cannot delegate to any subagent profile."),
        Some(allowlist) => format!(
            "{base} Allowed delegation profiles: {}.",
            allowlist.join(", ")
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    fn context() -> ToolContext {
        ToolContext {
            working_dir: std::env::temp_dir(),
            session_id: "parent-session".into(),
            turn_id: "test-turn".into(),
            plan_file_path: None,
            image: kkagent_config::ImageConfig::default(),
            tool_call_id: Some("tool-call".into()),
            interrupted: None,
            tools_config: kkagent_config::ToolsConfig::default(),
        }
    }

    fn recording_launcher() -> (SubagentLaunchFn, Arc<StdMutex<Vec<SubagentConfig>>>) {
        let launched = Arc::new(StdMutex::new(Vec::new()));
        let captured = launched.clone();
        let launch = Arc::new(move |config| {
            captured.lock().unwrap().push(config);
        });
        (launch, launched)
    }

    #[tokio::test]
    async fn coder_agent_allows_coder_and_rejects_general() {
        let manager = Arc::new(SubagentManager::new(4));
        let (launch, launched) = recording_launcher();
        let tool =
            AgentTool::with_allowed_subagents(manager, launch, allowed_subagents_for("coder"));
        assert!(tool.description().contains("coder, explore"));

        let allowed = tool
            .execute(
                serde_json::json!({
                    "description": "allowed",
                    "prompt": "do it",
                    "profile": "coder",
                    "run_in_background": true
                }),
                &context(),
            )
            .await
            .unwrap();
        assert!(!allowed.is_error);
        assert_eq!(launched.lock().unwrap().len(), 1);

        let denied = tool
            .execute(
                serde_json::json!({
                    "description": "denied",
                    "prompt": "do it",
                    "profile": "general",
                    "run_in_background": true
                }),
                &context(),
            )
            .await
            .unwrap();
        assert!(denied.is_error);
        assert!(denied
            .content
            .contains("not in the allowed subagent allowlist"));
        assert_eq!(launched.lock().unwrap().len(), 1);
    }

    #[tokio::test]
    async fn unrestricted_agent_accepts_any_profile() {
        let manager = Arc::new(SubagentManager::new(2));
        let (launch, launched) = recording_launcher();
        let tool = AgentTool::new(manager, launch);

        let output = tool
            .execute(
                serde_json::json!({
                    "description": "custom",
                    "prompt": "do it",
                    "profile": "specialized",
                    "run_in_background": true
                }),
                &context(),
            )
            .await
            .unwrap();
        assert!(!output.is_error);
        assert_eq!(
            launched.lock().unwrap()[0].profile.as_deref(),
            Some("specialized")
        );
    }

    #[test]
    fn swarm_rejects_a_disallowed_profile_before_launching_any_agent() {
        let manager = Arc::new(SubagentManager::new(4));
        let (launch, launched) = recording_launcher();
        let tool =
            AgentTool::with_allowed_subagents(manager, launch, allowed_subagents_for("coder"));
        let input = serde_json::json!({
            "agents": [
                {"description": "ok", "prompt": "one", "profile": "explore"},
                {"description": "no", "prompt": "two", "profile": "general"}
            ]
        });

        let error = tool.check_requested_profiles(&input).unwrap_err();
        assert!(error.contains("not in the allowed subagent allowlist"));
        assert!(launched.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn agents_array_fans_out_and_returns_ids() {
        let manager = Arc::new(SubagentManager::new(4));
        let (launch, launched) = recording_launcher();
        let tool = AgentTool::new(manager, launch);

        let output = tool
            .execute(
                serde_json::json!({
                    "agents": [
                        {"description": "one", "prompt": "first"},
                        {"description": "two", "prompt": "second"}
                    ]
                }),
                &context(),
            )
            .await
            .unwrap();
        assert!(!output.is_error);
        assert!(output.content.contains("Launched 2 agents"));
        assert_eq!(launched.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn templated_items_fan_out_with_placeholder_substituted() {
        let manager = Arc::new(SubagentManager::new(4));
        let (launch, launched) = recording_launcher();
        let tool = AgentTool::new(manager, launch);

        let output = tool
            .execute(
                serde_json::json!({
                    "description": "map",
                    "prompt_template": "explore module {{item}}",
                    "items": ["alpha", "beta"],
                }),
                &context(),
            )
            .await
            .unwrap();
        assert!(!output.is_error);
        let configs = launched.lock().unwrap();
        assert_eq!(configs.len(), 2);
        assert_eq!(configs[0].prompt, "explore module alpha");
        assert_eq!(configs[1].prompt, "explore module beta");
    }

    #[tokio::test]
    async fn task_output_actions_cover_status_list_stop() {
        let manager = Arc::new(SubagentManager::new(4));
        let (launch, _launched) = recording_launcher();
        let agent = AgentTool::new(manager.clone(), launch);
        agent
            .execute(
                serde_json::json!({
                    "description": "bg",
                    "prompt": "work",
                    "run_in_background": true
                }),
                &context(),
            )
            .await
            .unwrap();

        let tool = TaskOutputTool::new(manager);
        let list = tool
            .execute(serde_json::json!({"action": "list"}), &context())
            .await
            .unwrap();
        assert!(!list.is_error);
        assert!(list.content.contains("[agent/"), "{}", list.content);

        let unknown = tool
            .execute(
                serde_json::json!({"action": "stop", "task_id": "nope"}),
                &context(),
            )
            .await
            .unwrap();
        assert!(unknown.is_error);
        assert!(unknown.content.contains("Unknown task_id"));
    }
}
