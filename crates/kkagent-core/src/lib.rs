pub mod activity_view;
pub mod agent_loop;
pub mod audit;
pub mod blob_store;
pub mod checkpoint_store;
pub mod context_breakdown;
pub mod context_memory;
pub mod context_projector;
pub mod conversation_turns;
pub mod dynamic_tools;
pub mod event_bus;
pub mod file_conflict;
pub mod full_compaction;
pub mod git_context;
pub mod legacy_migrate;
pub mod media_pipeline;
pub mod model_capability;
pub mod permission;
mod plan_filename;
pub mod plan_review;
pub mod plugin;
pub mod plugin_marketplace;
pub mod plugin_overrides;
pub mod replay;
pub mod scope_context;
pub mod session;
pub mod subagent_runtime;
pub mod swarm;
pub mod system_reminder;
/// Test isolation helpers — redirects the kkagent home during `cargo test`.
/// No-op outside test binaries that opt in via `install_test_home!`.
pub mod test_isolation;
pub mod token_counting;
pub mod tool_dedupe;
pub mod tool_policy;
pub mod tool_results;
pub mod tool_scheduler;
pub mod transcript;
pub mod trash;
pub mod undo_service;
pub mod usage;
pub mod vision_proxy;
pub mod workspace_registry;

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
    observe_context_overflow, resolve_compaction_model_alias, select_compaction_user_messages,
    summarize_history_with_llm, CompactionPolicy, CompactionResult, CompactionStrategy,
    DEFAULT_BLOCK_RATIO, DEFAULT_TRIGGER_RATIO, MAX_OVERFLOW_COMPACTION_ATTEMPTS,
};
pub use git_context::is_workspace_trusted;
pub use legacy_migrate::{migrate_legacy_home, MigrateReport};
pub use media_pipeline::{extract_at_paths, resolve_media, MediaLimits, MediaRef};
pub use model_capability::ModelCapability;
pub use permission::*;
pub use plugin::{
    LoadedPlugin, PluginDiagnostic, PluginInfo, PluginInterface, PluginManager, PluginManifest,
};
pub use plugin_marketplace::{
    InstalledPluginRecord, PluginMarketplace, PluginMarketplaceEntry, RegisteredPluginMarketplace,
};
pub use replay::ReplayBuilder;
pub use scope_context::ScopeContext;
pub use session::*;
pub use subagent_runtime::{run_subagent, run_subagent_mirrored, SubagentMirrorContext};
pub use swarm::{SwarmModeTrigger, SwarmService};
pub use token_counting::{ContextSize, TokenCounter, TokenCountingStrategy};
pub use tool_dedupe::{canonical_args, ToolDedupeTracker};
pub use tool_policy::{ToolPolicyLayers, ToolPolicyService};
pub use tool_results::{
    parse_result_filename, persist as persist_tool_result, sanitize_fragment, tool_results_root,
    PersistedToolResult, TOOL_RESULT_MAX_CHARS, TOOL_RESULT_PREVIEW_CHARS,
};
pub use tool_scheduler::{SchedulerStatus, ToolCallTask, ToolScheduler};
pub use transcript::{
    open_shared_sqlite, open_shared_sqlite_memory, IntegrityReport, IsolatedMessage, SharedSqlite,
    TranscriptDb,
};
pub use trash::{archive_session_to_trash, TrashSummary};
pub use undo_service::{UndoResult, UndoService};
pub use usage::UsageService;
pub use workspace_registry::{
    list_active_peers_default, resolve_workspace_identity, SessionRegistration,
    WorkspaceRegistryLease,
};

// Keep this crate's own tests (Session::new et al.) out of the real
// ~/.kkagent home. Must stay at the end of the file: the macro expands to an
// item that clippy requires before any test module.
crate::install_test_home!();
