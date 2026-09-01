//! Independent completion-judge agent for goal mode.
//!
//! When `[goal] judge_enabled` is on, a model-reported `Goal update complete`
//! is not trusted directly: [`run_goal_judge`] spins up a short-lived judge
//! agent (own scratch session, own model, `GoalJudge` + `Read` tools only)
//! that reviews the objective against transcript evidence and marks its
//! verdict via a `GoalJudge` toolcall.
//!
//! Failure semantics are fail-open on purpose: a judge turn that ends without
//! a toolcall, times out, or errors returns `Err` and the caller accepts the
//! original completion claim. The judge must never wedge goal mode.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};

use kkagent_config::AppConfig;
use kkagent_llm::ChatMessage;
use kkagent_protocol::goal::Goal;
use kkagent_protocol::PermissionMode;
use kkagent_tools::builtin::goal_judge::JudgeVerdict;
use kkagent_tools::builtin::{GoalJudgeTool, ReadTool};
use kkagent_tools::ToolRegistry;

use crate::agent_loop::AgentLoop;
use crate::full_compaction::resolve_compaction_model_alias;
use crate::permission::PermissionChain;
use crate::session::runtime::Session;

/// Outcome of a successful judge run (a `GoalJudge` toolcall was recorded).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalJudgeRecord {
    /// "approve" | "reject"
    pub verdict: String,
    /// Concrete missing evidence items (reject only).
    pub gaps: Vec<String>,
    /// Judge's one-or-two sentence rationale.
    pub summary: String,
    /// Model alias the judge ran on.
    pub model: String,
}

/// Maximum evidence message count handed to the judge.
pub const EVIDENCE_TAIL_MESSAGES: usize = 40;
/// Per-message text cap for the evidence window (characters).
const EVIDENCE_MESSAGE_CHAR_CAP: usize = 2_000;

/// Truncate a message's text content for the evidence window.
fn clip_message(message: &ChatMessage) -> ChatMessage {
    let content = message
        .content
        .iter()
        .filter(|c| !matches!(c, kkagent_llm::ChatContent::Image { .. }))
        .map(|c| match c {
            kkagent_llm::ChatContent::Text { text } => {
                let clipped: String = text.chars().take(EVIDENCE_MESSAGE_CHAR_CAP).collect();
                let clipped = if text.chars().count() > EVIDENCE_MESSAGE_CHAR_CAP {
                    format!("{clipped}\n…(truncated)")
                } else {
                    clipped
                };
                kkagent_llm::ChatContent::Text { text: clipped }
            }
            other => other.clone(),
        })
        .collect();
    ChatMessage {
        role: message.role.clone(),
        content,
        tools: None,
    }
}

fn build_judge_messages(goal: &Goal, evidence_tail: &[ChatMessage]) -> Vec<ChatMessage> {
    let mut evidence: Vec<String> = evidence_tail
        .iter()
        .rev()
        .take(EVIDENCE_TAIL_MESSAGES)
        .rev()
        .map(|m| {
            let clipped = clip_message(m);
            let text = clipped
                .content
                .iter()
                .map(|c| match c {
                    kkagent_llm::ChatContent::Text { text } => text.clone(),
                    kkagent_llm::ChatContent::ToolResult {
                        content, is_error, ..
                    } => format!(
                        "[tool_result{}] {content}",
                        if *is_error { " ERROR" } else { "" }
                    ),
                    _ => String::new(),
                })
                .filter(|s| !s.is_empty())
                .collect::<Vec<_>>()
                .join("\n");
            format!("<evidence role=\"{}\">\n{text}\n</evidence>", clipped.role)
        })
        .collect();
    if evidence.is_empty() {
        evidence.push("<evidence>(no transcript evidence available)</evidence>".into());
    }

    let criterion = goal
        .completion_criterion
        .as_ref()
        .map(|c| {
            format!(
                "\nCompletion criterion (stated validation):\n<untrusted_completion_criterion>\n{c}\n</untrusted_completion_criterion>"
            )
        })
        .unwrap_or_default();

    let prompt = format!(
        "You are an independent completion judge. Decide whether the transcript evidence \
proves the goal below is fully accomplished.\n\n\
Objective:\n{}\n{criterion}\n\n\
Rules:\n\
- Judge only from the evidence below (plus files you may Read yourself); never assume work happened \
without evidence.\n\
- Reject when any part of the objective is unfinished, unverified, or contradicted by the evidence; \
list each concrete gap.\n\
- Approve only when the evidence covers the whole objective and any stated validation passed.\n\
- When done, call the GoalJudge tool exactly once with your verdict.\n\n\
Evidence (most recent {EVIDENCE_TAIL_MESSAGES} messages):\n{}",
        goal.untrusted_objective_xml(),
        evidence.join("\n"),
    );

    vec![ChatMessage {
        role: "user".into(),
        content: vec![kkagent_llm::ChatContent::Text { text: prompt }],
        tools: None,
    }]
}

/// Resolve the judge model alias: explicit `judge_model` first, then the
/// compaction chain. Returns `Err` when nothing resolves.
fn resolve_judge_alias(config: &AppConfig) -> Result<String, String> {
    if let Some(alias) = config
        .goal
        .judge_model
        .as_ref()
        .map(|s| s.trim())
        .filter(|s| !s.is_empty())
    {
        if config.resolve_model(alias).is_some() {
            return Ok(alias.to_string());
        }
        return Err(format!("goal.judge_model `{alias}` does not resolve"));
    }
    resolve_compaction_model_alias(config, None).ok_or_else(|| {
        "no judge model available (set goal.judge_model, compaction_model, or default_model)"
            .to_string()
    })
}

/// Run the judge agent. `Ok` means a verdict toolcall was recorded; `Err`
/// means the judge could not produce a verdict (timeout / no toolcall /
/// runtime error) and the caller should fail open.
pub async fn run_goal_judge(
    config: Arc<AppConfig>,
    working_dir: PathBuf,
    goal: &Goal,
    evidence_tail: &[ChatMessage],
) -> Result<GoalJudgeRecord, String> {
    let alias = resolve_judge_alias(&config)?;
    let timeout = std::time::Duration::from_secs(config.goal.judge_timeout_secs.max(5));

    let verdict_slot: Arc<Mutex<Option<JudgeVerdict>>> = Arc::new(Mutex::new(None));
    let mut tools = ToolRegistry::new();
    tools.register(Arc::new(GoalJudgeTool::new(verdict_slot.clone())));
    tools.register(Arc::new(ReadTool));

    let (event_tx, _event_rx) = tokio::sync::mpsc::channel(16);
    let judge_loop = AgentLoop::new(
        config.clone(),
        Arc::new(tools),
        Arc::new(tokio::sync::Mutex::new(PermissionChain::new(
            PermissionMode::Auto,
            Vec::new(),
        ))),
        event_tx,
        Arc::new(tokio::sync::Mutex::new(HashMap::<
            String,
            tokio::task::AbortHandle,
        >::new())),
    );

    let mut session = Session::for_subagent(
        format!("judge-{}", uuid::Uuid::new_v4()),
        working_dir,
        PermissionMode::Auto,
        alias.clone(),
    );
    session.messages = build_judge_messages(goal, evidence_tail);

    let run = tokio::time::timeout(timeout, judge_loop.run_turn(&mut session)).await;
    match run {
        Ok(Ok(())) => {}
        Ok(Err(error)) => return Err(format!("judge agent failed: {error:#}")),
        Err(_) => return Err("judge agent timed out".into()),
    }

    let verdict = verdict_slot.lock().unwrap().take();
    match verdict {
        Some(v) => Ok(GoalJudgeRecord {
            verdict: v.verdict,
            gaps: v.gaps,
            summary: v.summary,
            model: alias,
        }),
        None => Err("judge agent ended without a GoalJudge verdict".into()),
    }
}

#[cfg(test)]
mod judge_unit_tests {
    use super::*;

    #[test]
    fn judge_slot_roundtrip() {
        let slot: Arc<Mutex<Option<JudgeVerdict>>> = Arc::new(Mutex::new(None));
        *slot.lock().unwrap() = Some(JudgeVerdict {
            verdict: "approve".into(),
            gaps: Vec::new(),
            summary: "ok".into(),
        });
        let taken = slot.lock().unwrap().take();
        assert!(taken.is_some());
        assert!(slot.lock().unwrap().is_none());
    }
}
