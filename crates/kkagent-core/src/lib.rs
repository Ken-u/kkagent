pub mod activity_view;
pub mod agent_loop;
pub mod blob_store;
pub mod context_breakdown;
pub mod context_memory;
pub mod context_projector;
pub mod conversation_turns;
pub mod event_bus;
pub mod file_conflict;
pub mod full_compaction;
pub mod git_context;
pub mod media_pipeline;
pub mod model_capability;
pub mod permission;
mod plan_filename;
pub mod plan_review;
pub mod plugin;
pub mod replay;
pub mod scope_context;
pub mod session;
pub mod subagent_runtime;
pub mod swarm;
pub mod system_reminder;
pub mod token_counting;
pub mod tool_dedupe;
pub mod tool_policy;
pub mod tool_scheduler;
pub mod transcript;
pub mod undo_service;
pub mod usage;

pub use activity_view::{ActivityItem, ActivityView};
pub use agent_loop::*;
pub use blob_store::{resolve_media_refs, BlobStore};
pub use context_breakdown::ContextBreakdown;
pub use context_memory::{fold_loop_events, fold_vacuous, CompactionHandoff};
pub use context_projector::{
    build_compaction_digest, compact_cut_index, compact_messages, fold_old_media, project,
    project_strict, repair_tool_exchanges, ProjectOptions,
};
pub use conversation_turns::{editable_turns, EditableTurn};
pub use event_bus::EventBus;
pub use file_conflict::FileConflictTracker;
pub use full_compaction::{
    apply_compaction, compact_full, compact_full_async, is_real_user_input,
    observe_context_overflow, select_compaction_user_messages, summarize_history_with_llm,
    CompactionPolicy, CompactionResult, CompactionStrategy, DEFAULT_BLOCK_RATIO,
    DEFAULT_TRIGGER_RATIO, MAX_OVERFLOW_COMPACTION_ATTEMPTS,
};
pub use git_context::{collect_git_context, collect_git_context_with_trust, is_workspace_trusted};
pub use media_pipeline::{extract_at_paths, resolve_media, MediaLimits, MediaRef};
pub use model_capability::ModelCapability;
pub use permission::*;
pub use plugin::{LoadedPlugin, PluginManager, PluginManifest};
pub use replay::ReplayBuilder;
pub use scope_context::ScopeContext;
pub use session::*;
pub use subagent_runtime::{run_subagent, run_subagent_mirrored, SubagentMirrorContext};
pub use swarm::{SwarmModeTrigger, SwarmService};
pub use token_counting::{ContextSize, TokenCounter, TokenCountingStrategy};
pub use tool_dedupe::{canonical_args, ToolDedupeTracker};
pub use tool_policy::{ToolPolicyLayers, ToolPolicyService};
pub use tool_scheduler::{ToolCallTask, ToolScheduler};
pub use transcript::{
    open_shared_sqlite, open_shared_sqlite_memory, IntegrityReport, IsolatedMessage, SharedSqlite,
    TranscriptDb,
};
pub use undo_service::{UndoResult, UndoService};
pub use usage::UsageService;
