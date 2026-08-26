//! Standalone `AgentSwarm` tool — synchronous batch fan-out.
//!
//! Complements the unified `Agent` tool with kimi-code's swarm semantics:
//! - burst-then-throttle launch ramp (5 immediate, then one per 700 ms)
//! - provider rate-limit recovery: requeue with exponential backoff,
//!   capacity shrink (at most one per 2 s, floor 1) and recovery after 3 min
//! - wall-clock timeout that detaches (never kills) still-running subagents
//! - interrupt (user Esc) detaches instead of aborting the children
//! - kimi-code style XML result rendering with per-agent outcome + resume hint

use async_trait::async_trait;
use kkagent_config::ToolsConfig;
use kkagent_protocol::subagent::{
    allowed_subagents_for, SubagentConfig, SubagentManager, SubagentStatus,
};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::watch;

use crate::builtin::task::{check_profile_allowed, SubagentLaunchFn};
use crate::{Tool, ToolContext, ToolOutput};

pub const MAX_AGENT_SWARM_SUBAGENTS: usize = 128;
const PROMPT_TEMPLATE_PLACEHOLDER: &str = "{{item}}";

/// Burst-then-throttle launch ramp (kimi-code `agentRunBatch`).
const INITIAL_LAUNCH_LIMIT: usize = 5;
const INITIAL_LAUNCH_INTERVAL: Duration = Duration::from_millis(700);

/// Provider rate-limit recovery knobs (kimi-code `agentRunBatch`).
const RATE_LIMIT_RETRY_BASE: Duration = Duration::from_millis(3000);
const RATE_LIMIT_RETRY_FACTOR: u32 = 2;
const RATE_LIMIT_MAX_RETRIES: u32 = 4;
const CAPACITY_SHRINK_INTERVAL: Duration = Duration::from_millis(2000);
const CAPACITY_RECOVERY_INTERVAL: Duration = Duration::from_secs(180);

/// Safety net when no timeout is configured.
const HARD_CEILING: Duration = Duration::from_secs(60 * 60);

/// One planned child run.
#[derive(Debug, Clone)]
struct SwarmSpec {
    kind: SwarmSpecKind,
    /// Known upfront for resume specs; assigned at spawn time otherwise.
    agent_id: Option<String>,
    item: Option<String>,
    description: String,
    prompt: String,
    profile: Option<String>,
    model: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq)]
enum SwarmSpecKind {
    Spawn,
    Resume,
}

/// Final per-agent outcome for rendering.
#[derive(Debug, Clone)]
struct SwarmResult {
    spec_kind: SwarmSpecKind,
    agent_id: Option<String>,
    item: Option<String>,
    status: &'static str,
    body: String,
}

/// Scheduler state for one not-yet-terminal child.
struct SwarmSlot {
    spec_index: usize,
    spec_kind: SwarmSpecKind,
    item: Option<String>,
    config: SubagentConfig,
    /// `Some` once the manager knows this agent (resume specs start with it).
    agent_id: Option<String>,
    retries: u32,
    next_launch_at: Option<Instant>,
    launched: bool,
}

/// Detect provider rate-limit flavored failures so the run can be requeued.
fn is_rate_limit_error(message: &str) -> bool {
    let message = message.to_ascii_lowercase();
    [
        "rate limit",
        "429",
        "too many requests",
        "overloaded",
        "quota",
    ]
    .iter()
    .any(|needle| message.contains(needle))
}

/// True when the manager rejected the launch because the concurrency
/// ceiling is saturated (our own backpressure — requeue with backoff).
fn is_capacity_error(message: &str) -> bool {
    message.contains("Maximum concurrent subagents reached")
}

fn escape_xml_attribute(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// kimi-code `renderSwarmResults` format.
fn render_swarm_results(results: &[SwarmResult]) -> String {
    let completed = results.iter().filter(|r| r.status == "completed").count();
    let failed = results.iter().filter(|r| r.status == "failed").count();
    let aborted = results.iter().filter(|r| r.status == "aborted").count();
    let should_render_resume_hint = results.iter().any(|r| r.status != "completed")
        && results.iter().any(|r| r.agent_id.is_some());

    let mut summary_parts = Vec::new();
    if completed > 0 {
        summary_parts.push(format!("completed: {completed}"));
    }
    if failed > 0 {
        summary_parts.push(format!("failed: {failed}"));
    }
    if aborted > 0 {
        summary_parts.push(format!("aborted: {aborted}"));
    }

    let mut lines = vec![
        "<agent_swarm_result>".to_string(),
        format!("<summary>{}</summary>", summary_parts.join(", ")),
    ];
    if should_render_resume_hint {
        lines.push(
            "<resume_hint>Call AgentSwarm with resume_agent_ids using the agent_id values \
             in this result to continue unfinished work.</resume_hint>"
                .to_string(),
        );
    }
    for result in results {
        let mode = if result.spec_kind == SwarmSpecKind::Resume {
            r#" mode="resume""#
        } else {
            ""
        };
        let agent_id = result
            .agent_id
            .as_deref()
            .map(|id| format!(r#" agent_id="{id}""#))
            .unwrap_or_default();
        let item = result
            .item
            .as_deref()
            .map(|item| format!(r#" item="{}""#, escape_xml_attribute(item)))
            .unwrap_or_default();
        let state = match result.status {
            "completed" => "complete",
            "aborted" => "cancelled",
            _ => "failed",
        };
        lines.push(format!(
            "<subagent{agent_id}{mode}{item} state=\"{state}\" outcome=\"{}\">{}</subagent>",
            result.status, result.body
        ));
    }
    lines.push("</agent_swarm_result>".to_string());
    lines.join("\n")
}

/// Parse + validate the request into specs before launching anything.
fn create_swarm_specs(input: &Value) -> Result<Vec<SwarmSpec>, String> {
    let mut specs: Vec<SwarmSpec> = Vec::new();

    if let Some(map) = input.get("resume_agent_ids").and_then(Value::as_object) {
        for (agent_id, prompt_v) in map {
            let prompt = prompt_v.as_str().unwrap_or("");
            if prompt.trim().is_empty() {
                continue;
            }
            specs.push(SwarmSpec {
                kind: SwarmSpecKind::Resume,
                agent_id: Some(agent_id.clone()),
                item: None,
                description: format!("resume {agent_id}"),
                prompt: prompt.to_string(),
                profile: None,
                model: None,
            });
        }
    }

    if let (Some(template), Some(items)) = (
        input.get("prompt_template").and_then(Value::as_str),
        input.get("items").and_then(Value::as_array),
    ) {
        for item_v in items {
            let item = item_v.as_str().unwrap_or("");
            let prompt = template.replace(PROMPT_TEMPLATE_PLACEHOLDER, item);
            specs.push(SwarmSpec {
                kind: SwarmSpecKind::Spawn,
                agent_id: None,
                item: Some(item.to_string()),
                description: String::new(),
                prompt,
                profile: None,
                model: None,
            });
        }
    }

    if let Some(agents) = input.get("agents").and_then(Value::as_array) {
        for agent in agents {
            let prompt = agent.get("prompt").and_then(Value::as_str).unwrap_or("");
            if prompt.trim().is_empty() {
                continue;
            }
            specs.push(SwarmSpec {
                kind: SwarmSpecKind::Spawn,
                agent_id: None,
                item: None,
                description: agent
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or("")
                    .to_string(),
                prompt: prompt.to_string(),
                profile: agent
                    .get("profile")
                    .or_else(|| agent.get("subagent_type"))
                    .and_then(Value::as_str)
                    .map(String::from),
                model: agent.get("model").and_then(Value::as_str).map(String::from),
            });
        }
    }

    if specs.len() > MAX_AGENT_SWARM_SUBAGENTS {
        return Err(format!(
            "AgentSwarm exceeds max {MAX_AGENT_SWARM_SUBAGENTS} subagents"
        ));
    }

    let resume_count = specs
        .iter()
        .filter(|s| s.kind == SwarmSpecKind::Resume)
        .count();
    if resume_count == 0 && specs.len() < 2 {
        return Err(
            "AgentSwarm requires at least 2 subagents (prompt_template + items, or agents[]). \
             Use Agent for single delegation."
                .to_string(),
        );
    }

    // Distinct subagent prompts (kimi-code duplicate guard, applied to spawns).
    let mut seen_prompts: HashMap<&str, usize> = HashMap::new();
    for (index, spec) in specs.iter().enumerate() {
        if spec.kind != SwarmSpecKind::Spawn {
            continue;
        }
        let key = spec.prompt.trim();
        if let Some(previous_index) = seen_prompts.get(key) {
            return Err(format!(
                "Duplicate subagent prompts from items {} and {}. AgentSwarm requires distinct subagents.",
                previous_index + 1,
                index + 1
            ));
        }
        seen_prompts.insert(key, index + 1);
    }

    Ok(specs)
}

/// Standalone batch delegation tool.
#[derive(Clone)]
pub struct AgentSwarmTool {
    subagent_mgr: Arc<SubagentManager>,
    launch: SubagentLaunchFn,
    allowed_subagents: Option<Vec<String>>,
    tools_config: ToolsConfig,
}

impl AgentSwarmTool {
    pub fn new(
        subagent_mgr: Arc<SubagentManager>,
        launch: SubagentLaunchFn,
        allowed_subagents: Option<Vec<String>>,
        tools_config: ToolsConfig,
    ) -> Self {
        Self {
            subagent_mgr,
            launch,
            allowed_subagents,
            tools_config,
        }
    }

    async fn execute_swarm(&self, input: &Value, ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        let default_profile = input
            .get("subagent_type")
            .or_else(|| input.get("profile"))
            .and_then(Value::as_str)
            .unwrap_or("coder")
            .to_string();

        let specs = match create_swarm_specs(input) {
            Ok(specs) => specs,
            Err(error) => return Ok(ToolOutput::error(error)),
        };

        // Reject the whole batch before launching anything if any requested
        // profile (top-level default or per-agent) is outside the allowlist.
        if let Err(error) = check_profile_allowed(&default_profile, &self.allowed_subagents) {
            return Ok(ToolOutput::error(error));
        }
        for spec in &specs {
            if let Some(profile) = &spec.profile {
                if let Err(error) = check_profile_allowed(profile, &self.allowed_subagents) {
                    return Ok(ToolOutput::error(error));
                }
            }
        }

        let swarm_description = input
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or("Unnamed swarm");
        let top_model = input.get("model").and_then(Value::as_str).map(String::from);

        // ---- scheduler state -------------------------------------------------
        let base_capacity = self.subagent_mgr.max_concurrent().max(1);
        let mut capacity = base_capacity;
        let mut last_shrink_at: Option<Instant> = None;
        let mut last_rate_limit_at: Option<Instant> = None;
        let mut watch_rx: watch::Receiver<u64> = self.subagent_mgr.subscribe();
        let timeout = self.tools_config.effective_subagent_timeout_secs();
        let deadline = timeout.map(|secs| Instant::now() + Duration::from_secs(secs));

        let mut slots: Vec<SwarmSlot> = Vec::with_capacity(specs.len());
        let mut results: Vec<Option<SwarmResult>> = vec![None; specs.len()];

        for (spec_index, spec) in specs.iter().enumerate() {
            let profile = spec
                .profile
                .clone()
                .or_else(|| Some(default_profile.clone()))
                .filter(|p| !p.is_empty());
            let agent_id = match &spec.agent_id {
                Some(id) => id.clone(),
                None => uuid::Uuid::new_v4().to_string(),
            };
            let mut prompt = spec.prompt.clone();
            // Resume continuity (kimi-code semantics): the re-prompted agent
            // keeps context of its prior run's outcome.
            if spec.kind == SwarmSpecKind::Resume {
                if let Some(previous) = self.subagent_mgr.get_state(&agent_id).await {
                    if let Some(result) = previous.result {
                        let trimmed = result.trim();
                        if !trimmed.is_empty() {
                            prompt = format!(
                                "Your previous run produced:\n{trimmed}\n\nContinue with \
                                 this new instruction:\n{prompt}"
                            );
                        }
                    }
                }
            }
            let mut working_dir = ctx.working_dir.to_string_lossy().to_string();
            if crate::git_worktree::worktree_enabled() {
                if let Ok(wt) =
                    crate::git_worktree::create_worktree(&ctx.working_dir, &agent_id, None).await
                {
                    working_dir = wt.path.display().to_string();
                }
            }
            let description = if spec.description.is_empty() {
                format!(
                    "{swarm_description} #{} ({})",
                    spec_index + 1,
                    profile.as_deref().unwrap_or("coder")
                )
            } else {
                spec.description.clone()
            };
            slots.push(SwarmSlot {
                spec_index,
                spec_kind: spec.kind,
                item: spec.item.clone(),
                config: SubagentConfig {
                    agent_id,
                    description,
                    prompt,
                    model: spec.model.clone().or_else(|| top_model.clone()),
                    working_dir,
                    subagents: allowed_subagents_for(profile.as_deref().unwrap_or("general")),
                    profile: profile.clone(),
                    parent_session_id: Some(ctx.session_id.clone()),
                    parent_tool_call_id: ctx.tool_call_id.clone(),
                    // Swarm children are managed by the batch; on tool-side
                    // timeout / interrupt they detach instead of dying.
                    run_in_background: true,
                },
                agent_id: spec.agent_id.clone(),
                retries: 0,
                next_launch_at: None,
                launched: false,
            });
        }

        let start = Instant::now();
        let mut launched_count = 0usize;

        loop {
            let now = Instant::now();

            // Capacity recovery after 3 quiet minutes.
            if capacity < base_capacity
                && last_rate_limit_at
                    .map(|t| now.duration_since(t) >= CAPACITY_RECOVERY_INTERVAL)
                    .unwrap_or(false)
            {
                capacity = base_capacity;
            }

            // ---- launch phase (burst-then-throttle ramp) --------------------
            let mut in_flight = 0usize;
            for slot in &slots {
                if slot.launched
                    && slot.next_launch_at.is_none()
                    && self
                        .subagent_mgr
                        .get_state(&slot.config.agent_id)
                        .await
                        .map(|s| s.status == SubagentStatus::Running)
                        .unwrap_or(false)
                {
                    in_flight += 1;
                }
            }

            for slot in slots.iter_mut() {
                if results[slot.spec_index].is_some() {
                    continue;
                }
                // Already launched and not waiting out a requeue backoff —
                // its outcome is collected by the collect phase below; never
                // relaunch it here (a completed child would otherwise be
                // resumed in an infinite loop).
                if slot.launched {
                    continue;
                }
                if let Some(at) = slot.next_launch_at {
                    if now < at {
                        continue;
                    }
                    slot.next_launch_at = None;
                }
                if in_flight >= capacity {
                    break;
                }

                // Resume specs (and requeued runs) relaunch through `resume`;
                // fresh spawns go through `spawn`.
                let already_known = slot.agent_id.is_some();
                let launch_result = if already_known {
                    self.subagent_mgr
                        .resume(&slot.config.agent_id)
                        .await
                        .map(|_| ())
                        .map_err(|e| e.to_string())
                } else {
                    self.subagent_mgr
                        .spawn(slot.config.clone())
                        .await
                        .map(|_| ())
                        .map_err(|e| e.to_string())
                };

                match launch_result {
                    Ok(()) => {
                        let config = slot.config.clone();
                        (self.launch)(config);
                        slot.launched = true;
                        slot.agent_id = Some(slot.config.agent_id.clone());
                        launched_count += 1;
                        in_flight += 1;
                        // Throttle after the initial burst.
                        if launched_count > INITIAL_LAUNCH_LIMIT {
                            tokio::time::sleep(INITIAL_LAUNCH_INTERVAL).await;
                        }
                    }
                    Err(error) => {
                        if is_capacity_error(&error) || is_rate_limit_error(&error) {
                            slot.retries += 1;
                            if slot.retries > RATE_LIMIT_MAX_RETRIES {
                                results[slot.spec_index] = Some(SwarmResult {
                                    spec_kind: slot.spec_kind,
                                    agent_id: slot.agent_id.clone(),
                                    item: slot.item.clone(),
                                    status: "failed",
                                    body: format!("rate-limit retries exhausted: {error}"),
                                });
                                continue;
                            }
                            let backoff = RATE_LIMIT_RETRY_BASE
                                * RATE_LIMIT_RETRY_FACTOR.pow(slot.retries - 1);
                            slot.next_launch_at = Some(Instant::now() + backoff);
                            last_rate_limit_at = Some(Instant::now());
                            // Shrink capacity at most once per 2 s, floor 1.
                            if capacity > 1
                                && last_shrink_at
                                    .map(|t| {
                                        Instant::now().duration_since(t) >= CAPACITY_SHRINK_INTERVAL
                                    })
                                    .unwrap_or(true)
                            {
                                capacity -= 1;
                                last_shrink_at = Some(Instant::now());
                            }
                        } else {
                            results[slot.spec_index] = Some(SwarmResult {
                                spec_kind: slot.spec_kind,
                                agent_id: slot.agent_id.clone(),
                                item: slot.item.clone(),
                                status: "failed",
                                body: error,
                            });
                        }
                    }
                }
            }

            // ---- collect phase ------------------------------------------------
            let mut pending_ids = Vec::new();
            for slot in slots.iter_mut() {
                if results[slot.spec_index].is_some() {
                    continue;
                }
                // Still waiting out a backoff — do not double-requeue.
                if slot.next_launch_at.is_some() {
                    pending_ids.push(slot.spec_index);
                    continue;
                }
                let Some(agent_id) = slot.agent_id.clone() else {
                    pending_ids.push(slot.spec_index);
                    continue;
                };
                let Some(state) = self.subagent_mgr.get_state(&agent_id).await else {
                    pending_ids.push(slot.spec_index);
                    continue;
                };
                match state.status {
                    SubagentStatus::Complete => {
                        results[slot.spec_index] = Some(SwarmResult {
                            spec_kind: slot.spec_kind,
                            agent_id: Some(agent_id),
                            item: slot.item.clone(),
                            status: "completed",
                            body: state.result.unwrap_or_default(),
                        });
                    }
                    SubagentStatus::Failed => {
                        let error = state.error.clone().unwrap_or_default();
                        if is_rate_limit_error(&error) && slot.retries < RATE_LIMIT_MAX_RETRIES {
                            // Requeue: mark for relaunch with exponential backoff.
                            slot.retries += 1;
                            let backoff = RATE_LIMIT_RETRY_BASE
                                * RATE_LIMIT_RETRY_FACTOR.pow(slot.retries - 1);
                            slot.next_launch_at = Some(Instant::now() + backoff);
                            slot.launched = false;
                            last_rate_limit_at = Some(Instant::now());
                            if capacity > 1
                                && last_shrink_at
                                    .map(|t| {
                                        Instant::now().duration_since(t) >= CAPACITY_SHRINK_INTERVAL
                                    })
                                    .unwrap_or(true)
                            {
                                capacity -= 1;
                                last_shrink_at = Some(Instant::now());
                            }
                        } else {
                            results[slot.spec_index] = Some(SwarmResult {
                                spec_kind: slot.spec_kind,
                                agent_id: Some(agent_id),
                                item: slot.item.clone(),
                                status: "failed",
                                body: error,
                            });
                        }
                    }
                    SubagentStatus::Cancelled => {
                        results[slot.spec_index] = Some(SwarmResult {
                            spec_kind: slot.spec_kind,
                            agent_id: Some(agent_id),
                            item: slot.item.clone(),
                            status: "aborted",
                            body: "cancelled".to_string(),
                        });
                    }
                    _ => pending_ids.push(slot.spec_index),
                }
            }

            if pending_ids.is_empty() {
                let rendered: Vec<SwarmResult> = results.into_iter().flatten().collect();
                debug_assert_eq!(rendered.len(), specs.len());
                return Ok(ToolOutput::success(render_swarm_results(&rendered)));
            }

            // ---- wait phase ---------------------------------------------------
            let next_launch = slots.iter().filter_map(|s| s.next_launch_at).min();
            let mut sleep = Duration::from_secs(1);
            if let Some(at) = next_launch {
                sleep = sleep.min(at.saturating_duration_since(Instant::now()));
            }
            tokio::select! {
                changed = watch_rx.changed() => {
                    if changed.is_err() {
                        // Manager dropped — fall through and re-evaluate.
                    }
                }
                _ = tokio::time::sleep(sleep) => {}
            }

            // Timeout → detach (children keep running in the background).
            if let Some(dl) = deadline {
                if Instant::now() >= dl {
                    let running_ids: Vec<String> = slots
                        .iter()
                        .filter(|s| results[s.spec_index].is_none() && s.agent_id.is_some())
                        .map(|s| s.config.agent_id.clone())
                        .collect();
                    return Ok(detach_output(&running_ids, "timed out"));
                }
            }

            // Interrupt → detach instead of killing the children.
            if let Some(flag) = &ctx.interrupted {
                if flag.load(std::sync::atomic::Ordering::Relaxed) {
                    let running_ids: Vec<String> = slots
                        .iter()
                        .filter(|s| results[s.spec_index].is_none() && s.agent_id.is_some())
                        .map(|s| s.config.agent_id.clone())
                        .collect();
                    return Ok(detach_output(&running_ids, "interrupted by user"));
                }
            }

            // Safety net: never spin forever even with no timeout configured.
            if timeout.is_none() && start.elapsed() > HARD_CEILING {
                let running_ids: Vec<String> = slots
                    .iter()
                    .filter(|s| results[s.spec_index].is_none() && s.agent_id.is_some())
                    .map(|s| s.config.agent_id.clone())
                    .collect();
                return Ok(detach_output(&running_ids, "exceeded 1h hard ceiling"));
            }
        }
    }
}

fn detach_output(running_ids: &[String], reason: &str) -> ToolOutput {
    let ids = running_ids.join(", ");
    ToolOutput::success(format!(
        "AgentSwarm {reason}. {len} subagent(s) detached and still running in the \
         background (ids: {ids}). Use TaskOutput with these ids to fetch results \
         when ready, or resume them later with resume_agent_ids.",
        len = running_ids.len()
    ))
}

#[async_trait]
impl Tool for AgentSwarmTool {
    fn name(&self) -> &str {
        "AgentSwarm"
    }

    fn description(&self) -> &str {
        "Parallel fan-out: launch a batch of subagents (one Agent scope each) through \
         prompt_template + items[] (templated), agents[] (per-agent prompts), or \
         resume_agent_ids (re-prompt finished agents). Waits for all of them and \
         returns per-agent results as XML. On timeout or user interrupt the children \
         detach and keep running — collect them later with TaskOutput / resume_agent_ids."
    }

    fn parameters_schema(&self) -> Value {
        serde_json::json!({
            "type": "object",
            "properties": {
                "description": {
                    "type": "string",
                    "description": "Short description of the subagent / swarm"
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
                            "profile": {
                                "type": "string",
                                "enum": ["general", "explore", "coder"]
                            },
                            "model": {"type": "string"}
                        },
                        "required": ["prompt"]
                    }
                },
                "resume_agent_ids": {
                    "type": "object",
                    "description": "Map of agent_id to a new prompt; re-prompts finished agents",
                    "additionalProperties": {"type": "string"}
                },
                "profile": {
                    "type": "string",
                    "enum": ["general", "explore", "coder"],
                    "description": "Default subagent profile for spawned members (default: coder)"
                },
                "model": {
                    "type": "string",
                    "description": "Default model override for spawned members"
                }
            }
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        self.execute_swarm(&input, ctx).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ToolContext;
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

    #[expect(dead_code)]
    fn recording_launcher() -> (SubagentLaunchFn, Arc<StdMutex<Vec<SubagentConfig>>>) {
        let launched = Arc::new(StdMutex::new(Vec::new()));
        let captured = launched.clone();
        let launch = Arc::new(move |config: SubagentConfig| {
            // Simulate an instant successful run: Pending -> Complete.
            let manager = captured.clone();
            let id = config.agent_id.clone();
            let result = format!("done: {}", config.prompt);
            captured.lock().unwrap().push(config);
            let _ = (manager, id, result);
        });
        (launch, launched)
    }

    /// Manager + launcher pair that auto-completes children shortly after
    /// launch, simulating the runtime side of the subagent lifecycle.
    struct AutoComplete {
        manager: Arc<SubagentManager>,
        launched: Arc<StdMutex<Vec<SubagentConfig>>>,
    }

    impl AutoComplete {
        fn new(max_concurrent: usize) -> Self {
            let manager = Arc::new(SubagentManager::new(max_concurrent));
            let launched = Arc::new(StdMutex::new(Vec::new()));
            Self { manager, launched }
        }

        fn launcher(&self) -> SubagentLaunchFn {
            let manager = self.manager.clone();
            let launched = self.launched.clone();
            Arc::new(move |config: SubagentConfig| {
                launched.lock().unwrap().push(config.clone());
                let manager = manager.clone();
                let id = config.agent_id.clone();
                let result = format!("done: {}", config.prompt);
                tokio::spawn(async move {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                    let _ = manager.complete(&id, result).await;
                });
            })
        }
    }

    #[tokio::test]
    async fn swarm_waits_and_renders_xml() {
        let auto = AutoComplete::new(8);
        let tool = AgentSwarmTool::new(
            auto.manager.clone(),
            auto.launcher(),
            None,
            kkagent_config::ToolsConfig::default(),
        );

        let output = tool
            .execute(
                serde_json::json!({
                    "description": "explore stuff",
                    "prompt_template": "explore {{item}}",
                    "items": ["alpha", "beta"]
                }),
                &context(),
            )
            .await
            .unwrap();

        assert!(!output.is_error);
        assert!(output.content.contains("<agent_swarm_result>"));
        assert!(output.content.contains("<summary>completed: 2</summary>"));
        assert!(output.content.contains("done: explore alpha"));
        assert!(output.content.contains("done: explore beta"));
        assert!(!output.content.contains("<resume_hint>"));
        assert_eq!(auto.launched.lock().unwrap().len(), 2);
    }

    #[tokio::test]
    async fn swarm_resume_reprompts_finished_agents() {
        let auto = AutoComplete::new(8);
        // Seed one finished agent.
        let config = SubagentConfig {
            agent_id: "agent-1".into(),
            description: "first".into(),
            prompt: "first run".into(),
            model: None,
            working_dir: std::env::temp_dir().to_string_lossy().to_string(),
            subagents: None,
            profile: Some("coder".into()),
            parent_session_id: None,
            parent_tool_call_id: None,
            run_in_background: false,
        };
        auto.manager.spawn(config).await.unwrap();
        auto.manager
            .complete("agent-1", "prior findings".into())
            .await;

        let tool = AgentSwarmTool::new(
            auto.manager.clone(),
            auto.launcher(),
            None,
            kkagent_config::ToolsConfig::default(),
        );

        let output = tool
            .execute(
                serde_json::json!({
                    "description": "resume batch",
                    "resume_agent_ids": {
                        "agent-1": "continue",
                        "agent-missing": "wont run"
                    }
                }),
                &context(),
            )
            .await
            .unwrap();

        assert!(!output.is_error);
        // agent-missing fails fast (unknown id), agent-1 resumes to completion.
        assert!(output
            .content
            .contains("<summary>completed: 1, failed: 1</summary>"));
        assert!(output
            .content
            .contains(r#"agent_id="agent-1" mode="resume""#));
        // Prior-run context is injected into the resumed prompt.
        let launched = auto.launched.lock().unwrap();
        assert!(launched
            .iter()
            .any(|c| c.prompt.contains("prior findings") && c.prompt.contains("continue")));
    }

    #[tokio::test]
    async fn swarm_rejects_denied_profile_before_launching() {
        let auto = AutoComplete::new(8);
        let tool = AgentSwarmTool::new(
            auto.manager.clone(),
            auto.launcher(),
            allowed_subagents_for("coder"),
            kkagent_config::ToolsConfig::default(),
        );

        let output = tool
            .execute(
                serde_json::json!({
                    "description": "denied",
                    "profile": "general",
                    "prompt_template": "explore {{item}}",
                    "items": ["alpha", "beta"]
                }),
                &context(),
            )
            .await
            .unwrap();

        assert!(output.is_error);
        assert!(output
            .content
            .contains("not in the allowed subagent allowlist"));
        assert!(auto.launched.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn swarm_timeout_detaches_instead_of_killing() {
        let manager = Arc::new(SubagentManager::new(8));
        let launched = Arc::new(StdMutex::new(Vec::new()));
        let captured = launched.clone();
        let launch: SubagentLaunchFn = Arc::new(move |config: SubagentConfig| {
            // Never completes — stays Running.
            captured.lock().unwrap().push(config);
        });
        let tools_config = kkagent_config::ToolsConfig {
            subagent_timeout_secs: Some(1),
            ..kkagent_config::ToolsConfig::default()
        };
        let tool = AgentSwarmTool::new(manager.clone(), launch, None, tools_config);

        let start = Instant::now();
        let output = tool
            .execute(
                serde_json::json!({
                    "description": "slow swarm",
                    "prompt_template": "explore {{item}}",
                    "items": ["alpha", "beta"]
                }),
                &context(),
            )
            .await
            .unwrap();

        assert!(!output.is_error);
        assert!(output.content.contains("timed out"));
        assert!(output.content.contains("detached"));
        assert!(start.elapsed() < Duration::from_secs(10));
        // Children were spawned (and still Running — not killed).
        let running = manager
            .list_all()
            .await
            .iter()
            .filter(|s| s.status == SubagentStatus::Running)
            .count();
        assert_eq!(running, 2);
    }

    #[tokio::test]
    async fn swarm_interrupt_detaches() {
        let manager = Arc::new(SubagentManager::new(8));
        let launched = Arc::new(StdMutex::new(Vec::new()));
        let captured = launched.clone();
        let launch: SubagentLaunchFn = Arc::new(move |config: SubagentConfig| {
            // Never completes — stays Running.
            captured.lock().unwrap().push(config);
        });
        let flag = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let mut ctx = context();
        ctx.interrupted = Some(flag.clone());
        let tool = AgentSwarmTool::new(
            manager.clone(),
            launch,
            None,
            kkagent_config::ToolsConfig::default(),
        );

        // Trigger the interrupt shortly after the swarm starts waiting.
        let flag_timer = flag.clone();
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(100)).await;
            flag_timer.store(true, std::sync::atomic::Ordering::Relaxed);
        });

        let output = tool
            .execute(
                serde_json::json!({
                    "description": "slow swarm",
                    "prompt_template": "explore {{item}}",
                    "items": ["alpha", "beta"]
                }),
                &ctx,
            )
            .await
            .unwrap();

        assert!(!output.is_error);
        assert!(output.content.contains("interrupted by user"));
        let running = manager
            .list_all()
            .await
            .iter()
            .filter(|s| s.status == SubagentStatus::Running)
            .count();
        assert_eq!(running, 2);
    }

    #[tokio::test]
    async fn swarm_requeues_on_capacity_error() {
        // Capacity 1 with 3 specs: launches must be serialized and all
        // eventually complete (no perma-failure on capacity errors).
        let auto = AutoComplete::new(1);
        let tool = AgentSwarmTool::new(
            auto.manager.clone(),
            auto.launcher(),
            None,
            kkagent_config::ToolsConfig::default(),
        );

        let output = tool
            .execute(
                serde_json::json!({
                    "description": "tight capacity",
                    "prompt_template": "explore {{item}}",
                    "items": ["one", "two", "three"]
                }),
                &context(),
            )
            .await
            .unwrap();

        assert!(!output.is_error);
        assert!(output.content.contains("<summary>completed: 3</summary>"));
    }

    #[test]
    fn rejects_single_subagent_swarm() {
        let input = serde_json::json!({
            "prompt_template": "do {{item}}",
            "items": ["one thing"]
        });
        let err = create_swarm_specs(&input).unwrap_err();
        assert!(err.contains("at least 2 subagents"));
    }

    #[test]
    fn rejects_duplicate_prompts() {
        let input = serde_json::json!({
            "prompt_template": "same",
            "items": ["a", "b"]
        });
        let err = create_swarm_specs(&input).unwrap_err();
        assert!(err.contains("Duplicate subagent prompts"));
    }

    #[test]
    fn resume_only_swarm_is_allowed() {
        let input = serde_json::json!({
            "resume_agent_ids": {"agent-1": "continue", "agent-2": "also continue"}
        });
        let specs = create_swarm_specs(&input).unwrap();
        assert_eq!(specs.len(), 2);
        assert!(specs.iter().all(|s| s.kind == SwarmSpecKind::Resume));
    }

    #[test]
    fn templates_items() {
        let input = serde_json::json!({
            "prompt_template": "explore {{item}} now",
            "items": ["alpha", "beta"]
        });
        let specs = create_swarm_specs(&input).unwrap();
        assert_eq!(specs.len(), 2);
        assert_eq!(specs[0].prompt, "explore alpha now");
        assert_eq!(specs[1].item.as_deref(), Some("beta"));
    }

    #[test]
    fn renders_kimi_style_xml() {
        let results = vec![
            SwarmResult {
                spec_kind: SwarmSpecKind::Spawn,
                agent_id: Some("agent-1".into()),
                item: Some("alpha".into()),
                status: "completed",
                body: "found 3 files".into(),
            },
            SwarmResult {
                spec_kind: SwarmSpecKind::Spawn,
                agent_id: Some("agent-2".into()),
                item: Some("be<ta".into()),
                status: "failed",
                body: "boom".into(),
            },
        ];
        let xml = render_swarm_results(&results);
        assert!(xml.starts_with("<agent_swarm_result>"));
        assert!(xml.contains("<summary>completed: 1, failed: 1</summary>"));
        assert!(xml.contains("agent_id=\"agent-1\""));
        assert!(xml.contains("item=\"be&lt;ta\""));
        assert!(xml.contains("outcome=\"completed\">found 3 files"));
        assert!(xml.contains("<resume_hint>"));
        assert!(xml.ends_with("</agent_swarm_result>"));
    }

    #[test]
    fn rate_limit_detection() {
        assert!(is_rate_limit_error("HTTP 429 Too Many Requests"));
        assert!(is_rate_limit_error("provider rate limit exceeded"));
        assert!(!is_rate_limit_error("file not found"));
    }
}
