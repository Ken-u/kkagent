//! Session subsystem — aligned with kimi-code `agent-core` / `agent-core-v2` session domains.
//!
//! Domains:
//! - store (index + workdir buckets + fork/archive)
//! - metadata / state / lifecycle / activity / interaction
//! - agentLifecycle / subagent / swarm batch
//! - todo / btw / cron / tool policy / terminal / process
//! - init / instructions / skill+profile catalogs / mcp / workspace / export / hooks

pub mod activity;
pub mod agent_lifecycle;
pub mod btw;
pub mod context;
pub mod cron;
pub mod export;
pub mod external_hooks;
pub mod init;
pub mod instructions;
pub mod interaction;
pub mod lifecycle;
pub mod log;
pub mod mcp;
pub mod metadata;
pub mod process;
pub mod profile_catalog;
pub mod runtime;
pub mod seed;
pub mod services;
pub mod session_tool_policy;
pub mod skill_catalog;
pub mod state;
pub mod store;
pub mod subagent;
pub mod swarm_batch;
pub mod terminal;
pub mod todo;
pub mod workspace_context;

pub use activity::{
    PendingInteraction, SessionActivityCause, SessionActivityState, SessionActivityView,
};
pub use agent_lifecycle::{AgentHandle, AgentLifecycleService, CreateAgentOptions, MAIN_AGENT_ID};
pub use btw::{
    BtwTurn, SessionBtwService, SIDE_QUESTION_SYSTEM_REMINDER, TOOL_CALL_DISABLED_MESSAGE,
};
pub use context::SessionContext;
pub use cron::{SessionCronJob, SessionCronService};
pub use export::{default_export_dir_name, export_session_directory, ExportManifest, ExportResult};
pub use external_hooks::{SessionExternalHooks, SessionHookEvent, HOOK_EVENT_TYPES};
pub use init::SessionInitService;
pub use instructions::{InstructionFile, SessionInstructionsProvider};
pub use interaction::{
    Interaction, InteractionKind, InteractionOrigin, SessionInteractionService, SharedInteraction,
};
pub use lifecycle::{SessionCloseReason, SessionCreateSource, SessionLifecycleHooks};
pub use log::SessionLogService;
pub use mcp::{McpConnectionView, SessionMcpHandle};
pub use metadata::{
    AgentKind, AgentMeta, SessionMeta, SessionMetaPatch, SessionMetadataService, TurnReason,
    SESSION_META_VERSION,
};
pub use process::{ProcessHandle, ProcessStatus, SessionProcessRunner};
pub use profile_catalog::{AgentProfileSummary, SessionAgentProfileCatalog};
pub use runtime::{messages_for_llm, plan_mode_reminder, FileChange, Session, TurnCheckpoint};
pub use seed::SessionSeed;
pub use services::SessionServices;
pub use session_tool_policy::{
    SessionToolPolicyDoc, SessionToolPolicyGate, SessionToolPolicyService,
};
pub use skill_catalog::{SessionSkillCatalog, SkillCatalogEntry};
pub use state::SessionStateService;
pub use store::{
    encode_work_dir_key, is_safe_session_id, normalize_work_dir, workspace_root_key, SessionStore,
    SessionSummary,
};
pub use subagent::{AgentRunRequest, AgentTaskEvent, SessionSubagentService};
pub use swarm_batch::{
    SessionSwarmBatchService, SessionSwarmRunResult, SessionSwarmTask, SwarmRunStatus,
    SwarmTaskKind,
};
pub use terminal::{SessionTerminalService, TerminalHandle};
pub use todo::{parse_todo_items, render_todo_list, SessionTodoService, TodoItem, TodoStatus};
pub use workspace_context::{SessionWorkspaceContext, WorkspaceInfo};
