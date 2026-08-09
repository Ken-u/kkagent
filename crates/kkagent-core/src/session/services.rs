//! Aggregated session-scoped services bag (agent-core-v2 session domains).

use crate::session::activity::SessionActivityView;
use crate::session::agent_lifecycle::AgentLifecycleService;
use crate::session::btw::SessionBtwService;
use crate::session::context::SessionContext;
use crate::session::cron::SessionCronService;
use crate::session::external_hooks::SessionExternalHooks;
use crate::session::init::SessionInitService;
use crate::session::interaction::SessionInteractionService;
use crate::session::lifecycle::{SessionCloseReason, SessionCreateSource, SessionLifecycleHooks};
use crate::session::log::SessionLogService;
use crate::session::mcp::SessionMcpHandle;
use crate::session::metadata::{AgentKind, AgentMeta, SessionMetadataService, TurnReason};
use crate::session::process::SessionProcessRunner;
use crate::session::profile_catalog::SessionAgentProfileCatalog;
use crate::session::session_tool_policy::SessionToolPolicyGate;
use crate::session::skill_catalog::SessionSkillCatalog;
use crate::session::state::SessionStateService;
use crate::session::subagent::SessionSubagentService;
use crate::session::swarm_batch::SessionSwarmBatchService;
use crate::session::terminal::SessionTerminalService;
use crate::session::todo::SessionTodoService;
use crate::session::workspace_context::SessionWorkspaceContext;
use std::path::PathBuf;
use std::sync::Arc;

/// Everything that lives at Session scope besides the LLM message runtime.
pub struct SessionServices {
    pub context: SessionContext,
    pub metadata: SessionMetadataService,
    pub state: SessionStateService,
    pub lifecycle: SessionLifecycleHooks,
    pub activity: SessionActivityView,
    pub interaction: Arc<SessionInteractionService>,
    pub agents: AgentLifecycleService,
    pub todos: SessionTodoService,
    pub btw: SessionBtwService,
    pub tool_policy_gate: SessionToolPolicyGate,
    pub cron: SessionCronService,
    pub swarm_batch: SessionSwarmBatchService,
    pub subagent: SessionSubagentService,
    pub terminal: SessionTerminalService,
    pub process: SessionProcessRunner,
    pub init: SessionInitService,
    pub skills: SessionSkillCatalog,
    pub profiles: SessionAgentProfileCatalog,
    pub workspace: SessionWorkspaceContext,
    pub mcp: SessionMcpHandle,
    pub log: SessionLogService,
    pub external_hooks: SessionExternalHooks,
    pub create_source: SessionCreateSource,
}

impl SessionServices {
    pub fn bootstrap(
        session_id: &str,
        working_dir: PathBuf,
        session_dir: PathBuf,
        workspace_id: String,
        trusted: bool,
        source: SessionCreateSource,
        hooks: Option<Arc<kkagent_mcp::HookManager>>,
    ) -> anyhow::Result<Self> {
        let context = SessionContext::new(
            session_id,
            workspace_id,
            session_dir.clone(),
            working_dir.clone(),
        );
        // Store layer may already have written state.json; always load-or-create.
        let metadata =
            SessionMetadataService::load_or_create(&session_dir, session_id, &working_dir)?;
        let agents = AgentLifecycleService::new();
        let tool_policy_gate = SessionToolPolicyGate::default();
        let _ = tool_policy_gate.session_policy.load_from_dir(&session_dir);
        let cron = SessionCronService::new();
        let _ = cron.load(&session_dir);

        let mut svc = Self {
            context,
            metadata,
            state: SessionStateService::new(),
            lifecycle: SessionLifecycleHooks::new(),
            activity: SessionActivityView::new(),
            interaction: Arc::new(SessionInteractionService::new()),
            agents,
            todos: SessionTodoService::new(),
            btw: SessionBtwService::new(),
            tool_policy_gate,
            cron,
            swarm_batch: SessionSwarmBatchService::new(),
            subagent: SessionSubagentService::new(),
            terminal: SessionTerminalService::new(),
            process: SessionProcessRunner::new(),
            init: SessionInitService::new(),
            skills: SessionSkillCatalog::new(),
            profiles: SessionAgentProfileCatalog::new(),
            workspace: SessionWorkspaceContext::new(working_dir, trusted),
            mcp: SessionMcpHandle::new(),
            log: SessionLogService::new(),
            external_hooks: SessionExternalHooks::new(hooks),
            create_source: source,
        };

        // Register main agent into durable metadata (no-op if identical on resume).
        if let Some(main) = svc.agents.get(crate::session::agent_lifecycle::MAIN_AGENT_ID) {
            let meta = AgentMeta {
                kind: Some(AgentKind::Main),
                ..svc.agents.to_agent_meta(&main)
            };
            let _ = svc.metadata.register_agent(&main.id, meta);
        }
        svc.log.info(format!(
            "session bootstrap source={:?} dir={}",
            source,
            session_dir.display()
        ));
        Ok(svc)
    }

    pub async fn on_created(&self) {
        self.lifecycle.fire_created(self.create_source).await;
        self.external_hooks
            .fire(
                crate::session::external_hooks::SessionHookEvent::SessionStart,
                SessionExternalHooks::payload_session_start(
                    &self.context.session_id,
                    &self.context.cwd.to_string_lossy(),
                    match self.create_source {
                        SessionCreateSource::Startup => "startup",
                        SessionCreateSource::Resume => "resume",
                        SessionCreateSource::Fork => "fork",
                    },
                ),
            )
            .await;
    }

    pub async fn on_close(&self, reason: SessionCloseReason) {
        self.lifecycle.fire_will_close(reason).await;
        let _ = self.tool_policy_gate.session_policy.persist(&self.context.session_dir);
        let _ = self.cron.persist(&self.context.session_dir);
        let _ = self.log.flush_to_file(&self.context.session_dir);
        self.external_hooks
            .fire(
                crate::session::external_hooks::SessionHookEvent::SessionEnd,
                serde_json::json!({
                    "session_id": self.context.session_id,
                    "reason": match reason {
                        SessionCloseReason::Exit => "exit",
                        SessionCloseReason::Archive => "archive",
                    },
                }),
            )
            .await;
    }

    pub fn mark_turn_started(&self) {
        self.activity.set_main_turn_active(true);
    }

    pub fn mark_turn_ended(&mut self, reason: TurnReason) {
        self.activity.set_last_turn_reason(reason.clone());
        let _ = self.metadata.set_last_turn_reason(reason);
    }
}
