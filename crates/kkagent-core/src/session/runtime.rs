use kkagent_llm::{ChatContent, ChatMessage};
use kkagent_protocol::{ApprovalResponse, PermissionMode, QuestionResponse};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio::sync::{mpsc, oneshot};

use crate::session::instructions::SessionInstructionsProvider;
use crate::session::lifecycle::SessionCreateSource;
use crate::session::metadata::{SessionMeta, SessionMetaPatch, TurnReason};
use crate::session::services::SessionServices;
use crate::session::store::{encode_work_dir_key, SessionStore};

/// Pre-write snapshot so undo can restore files.
#[derive(Debug, Clone)]
pub struct FileChange {
    pub path: PathBuf,
    /// `None` means the file did not exist before the write.
    pub previous: Option<Vec<u8>>,
}

#[derive(Debug, Clone)]
pub struct TurnCheckpoint {
    /// Index of the user message that started this turn.
    pub message_start_index: usize,
    pub file_changes: Vec<FileChange>,
}

const PLAN_MODE_META_KEY: &str = "planMode";
const PLAN_ID_META_KEY: &str = "planId";
const PENDING_PLAN_REVIEW_META_KEY: &str = "pendingPlanReview";
const TODOS_META_KEY: &str = "todos";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionPlanState {
    pub enabled: bool,
    pub id: String,
    pub path: PathBuf,
    pub content: Option<String>,
}

pub struct Session {
    pub id: String,
    pub title: Option<String>,
    pub messages: Vec<ChatMessage>,
    pub system_prompt: String,
    pub working_dir: PathBuf,
    pub image_config: kkagent_config::ImageConfig,
    /// Shared Arc so `/permission` can update mid-turn while the session is out of the map.
    pub permission_mode: Arc<std::sync::Mutex<PermissionMode>>,
    pub plan_mode: bool,
    /// Desired plan mode remains reachable while the live Session is owned by
    /// the agent loop and temporarily absent from the server session map.
    pub plan_mode_requested: Arc<AtomicBool>,
    /// Readable id used as the plan filename (`<id>.md`).
    pub plan_id: String,
    /// Only this file may be written/edited while plan_mode is on.
    pub plan_file_path: PathBuf,
    /// Model alias from config (e.g. "local/claude-opus-4-8").
    /// Shared Arc so `/model` can update mid-turn while the session is out of the map.
    pub model_alias: Arc<std::sync::Mutex<String>>,
    /// How many messages have already been written to the transcript DB.
    pub persisted_message_count: usize,
    /// The in-memory history was compacted and must atomically replace the DB transcript.
    pub transcript_rewrite_required: bool,
    pub approval_waiters: HashMap<String, oneshot::Sender<ApprovalResponse>>,
    approval_rx: mpsc::Receiver<ApprovalResponse>,
    pub approval_tx: mpsc::Sender<ApprovalResponse>,
    question_rx: mpsc::Receiver<QuestionResponse>,
    pub question_tx: mpsc::Sender<QuestionResponse>,
    /// Set by session.interrupt — agent loop checks between stream/tool steps.
    pub interrupted: Arc<AtomicBool>,
    /// Index of the current turn's user message (set by `begin_turn`).
    turn_message_start: Option<usize>,
    /// File mutations during the in-flight turn.
    pub current_turn_changes: Vec<FileChange>,
    /// Completed turns available for undo (most recent last).
    pub undo_stack: Vec<TurnCheckpoint>,
    /// Progressive tool disclosure filter (None = all tools).
    pub enabled_tools: Option<std::collections::HashSet<String>>,
    /// Turns since last TodoList write (for reminder).
    pub turns_since_todo: u32,
    /// Cross-turn tool dedupe tracker.
    pub tool_dedupe: crate::tool_dedupe::ToolDedupeTracker,
    /// Session token counter (measured anchors + estimates).
    pub token_counter: crate::token_counting::TokenCounter,
    /// Token count right after the last successful compaction (re-compact guard).
    pub last_compacted_tokens: Option<u64>,
    /// Consecutive provider-overflow compact recoveries in the current turn.
    pub consecutive_overflow_compacts: u32,
    /// Observed max context after a provider overflow (may be below configured).
    pub observed_max_context: Option<u64>,
    /// Layered tool activation policy (SelectTools + workspace/session disables).
    pub tool_policy: crate::tool_policy::ToolPolicyService,
    /// Swarm mode roster / enter-exit.
    pub swarm: crate::swarm::SwarmService,
    /// Aggregated token/step usage.
    pub usage: crate::usage::UsageService,
    /// Session-scoped services (metadata, agents, activity, store paths, …).
    pub services: SessionServices,
}

impl Session {
    pub fn new(
        id: String,
        working_dir: PathBuf,
        permission_mode: PermissionMode,
        model_alias: String,
    ) -> Self {
        Self::new_with_source(
            id,
            working_dir,
            permission_mode,
            model_alias,
            SessionCreateSource::Startup,
            None,
        )
    }

    pub fn resume(
        id: String,
        working_dir: PathBuf,
        permission_mode: PermissionMode,
        model_alias: String,
    ) -> Self {
        Self::new_with_source(
            id,
            working_dir,
            permission_mode,
            model_alias,
            SessionCreateSource::Resume,
            None,
        )
    }

    pub fn new_with_source(
        id: String,
        working_dir: PathBuf,
        permission_mode: PermissionMode,
        model_alias: String,
        source: SessionCreateSource,
        hooks: Option<Arc<kkagent_mcp::HookManager>>,
    ) -> Self {
        let (approval_tx, approval_rx) = mpsc::channel(16);
        let (question_tx, question_rx) = mpsc::channel(16);
        let (session_dir, workspace_id) = resolve_session_dir(&id, &working_dir, source);
        let mut services = SessionServices::bootstrap(
            &id,
            working_dir.clone(),
            session_dir,
            workspace_id,
            true,
            source,
            hooks,
        )
        .unwrap_or_else(|e| {
            tracing::warn!("session services bootstrap failed: {e}; using ephemeral dir");
            let ephemeral = std::env::temp_dir().join("kkagent-sessions").join(&id);
            let _ = std::fs::create_dir_all(&ephemeral);
            SessionServices::bootstrap(
                &id,
                working_dir.clone(),
                ephemeral,
                encode_work_dir_key(&working_dir),
                true,
                source,
                None,
            )
            .expect("ephemeral session bootstrap")
        });

        let title = services.metadata.read().title.clone();
        let plan_state = plan_state_from_metadata(
            &id,
            &working_dir,
            &services.context.session_dir,
            Some(services.metadata.read()),
            true,
        );
        let restored_todos = todo_items_from_metadata(Some(services.metadata.read()));
        services
            .todos
            .set_todos(todo_service_items(&restored_todos));
        if services
            .metadata
            .read()
            .custom
            .get(PLAN_ID_META_KEY)
            .and_then(|value| value.as_str())
            .and_then(valid_plan_id)
            .is_none()
            && (plan_state.enabled || plan_state.content.is_some())
        {
            let mut custom = services.metadata.read().custom.clone();
            custom.insert(PLAN_ID_META_KEY.into(), plan_state.id.clone().into());
            if let Err(error) = services.metadata.update(
                SessionMetaPatch {
                    custom: Some(custom),
                    ..Default::default()
                },
                false,
            ) {
                tracing::warn!(%error, "failed to persist restored plan id");
            }
        }

        Self {
            id,
            title,
            messages: Vec::new(),
            system_prompt: default_system_prompt(),
            working_dir,
            image_config: kkagent_config::ImageConfig::default(),
            permission_mode: Arc::new(std::sync::Mutex::new(permission_mode)),
            plan_mode: plan_state.enabled,
            plan_mode_requested: Arc::new(AtomicBool::new(plan_state.enabled)),
            plan_id: plan_state.id,
            plan_file_path: plan_state.path,
            model_alias: Arc::new(std::sync::Mutex::new(model_alias)),
            persisted_message_count: 0,
            transcript_rewrite_required: false,
            approval_waiters: HashMap::new(),
            approval_rx,
            approval_tx,
            question_rx,
            question_tx,
            interrupted: Arc::new(AtomicBool::new(false)),
            turn_message_start: None,
            current_turn_changes: Vec::new(),
            undo_stack: Vec::new(),
            enabled_tools: None,
            turns_since_todo: 0,
            tool_dedupe: crate::tool_dedupe::ToolDedupeTracker::new(),
            token_counter: crate::token_counting::TokenCounter::new(
                crate::token_counting::TokenCountingStrategy::MeasuredPlusEstimated,
            ),
            last_compacted_tokens: None,
            consecutive_overflow_compacts: 0,
            observed_max_context: None,
            tool_policy: crate::tool_policy::ToolPolicyService::new(),
            swarm: crate::swarm::SwarmService::new(),
            usage: crate::usage::UsageService::new(),
            services,
        }
    }

    pub fn session_dir(&self) -> &std::path::Path {
        &self.services.context.session_dir
    }

    pub fn set_title_persisted(&mut self, title: impl Into<String>) -> anyhow::Result<()> {
        let title = title.into();
        self.title = Some(title.clone());
        self.services.metadata.set_title(title)
    }

    /// Change plan mode and persist the state before reporting it to clients.
    pub fn set_plan_mode_persisted(&mut self, enabled: bool) -> anyhow::Result<()> {
        if enabled && !self.plan_mode {
            let plans_dir = self
                .services
                .context
                .session_dir
                .join("agents")
                .join("main")
                .join("plans");
            self.plan_id = crate::plan_filename::generate_plan_id(&plans_dir, "plan");
            self.plan_file_path = plans_dir.join(format!("{}.md", self.plan_id));
        }
        if enabled {
            if let Some(parent) = self.plan_file_path.parent() {
                std::fs::create_dir_all(parent)?;
            }
        }
        let mut custom = self.services.metadata.read().custom.clone();
        custom.insert(PLAN_MODE_META_KEY.into(), enabled.into());
        custom.insert(PLAN_ID_META_KEY.into(), self.plan_id.clone().into());
        if !enabled {
            custom.remove(PENDING_PLAN_REVIEW_META_KEY);
        }
        self.services.metadata.update(
            SessionMetaPatch {
                custom: Some(custom),
                ..Default::default()
            },
            true,
        )?;
        self.plan_mode = enabled;
        self.plan_mode_requested.store(enabled, Ordering::SeqCst);
        Ok(())
    }

    pub fn request_plan_mode(&self, enabled: bool) {
        self.plan_mode_requested.store(enabled, Ordering::SeqCst);
    }

    /// Apply a mode change requested while this session was owned by a running
    /// agent loop. Returns true when the persisted mode changed.
    pub fn sync_requested_plan_mode(&mut self) -> anyhow::Result<bool> {
        let requested = self.plan_mode_requested.load(Ordering::SeqCst);
        if requested == self.plan_mode {
            return Ok(false);
        }
        if let Err(error) = self.set_plan_mode_persisted(requested) {
            self.plan_mode_requested
                .store(self.plan_mode, Ordering::SeqCst);
            return Err(error);
        }
        Ok(true)
    }

    pub fn pending_plan_review(&self) -> Option<kkagent_protocol::ApprovalRequest> {
        pending_plan_review_from_metadata(Some(self.services.metadata.read()))
    }

    pub fn set_pending_plan_review(
        &mut self,
        request: Option<kkagent_protocol::ApprovalRequest>,
    ) -> anyhow::Result<()> {
        let mut custom = self.services.metadata.read().custom.clone();
        if let Some(request) = request {
            custom.insert(
                PENDING_PLAN_REVIEW_META_KEY.into(),
                serde_json::to_value(request)?,
            );
        } else {
            custom.remove(PENDING_PLAN_REVIEW_META_KEY);
        }
        self.services.metadata.update(
            SessionMetaPatch {
                custom: Some(custom),
                ..Default::default()
            },
            true,
        )
    }

    pub fn todo_items(&self) -> Vec<kkagent_protocol::TodoItemEvent> {
        self.services
            .todos
            .get_todos()
            .into_iter()
            .enumerate()
            .map(|(index, item)| kkagent_protocol::TodoItemEvent {
                id: (index + 1).to_string(),
                content: item.title,
                status: match item.status {
                    crate::session::todo::TodoStatus::Pending => "pending",
                    crate::session::todo::TodoStatus::InProgress => "in_progress",
                    crate::session::todo::TodoStatus::Done => "completed",
                    crate::session::todo::TodoStatus::Cancelled => "cancelled",
                }
                .into(),
            })
            .collect()
    }

    pub fn set_todos_persisted(
        &mut self,
        items: Vec<kkagent_protocol::TodoItemEvent>,
    ) -> anyhow::Result<()> {
        self.services.todos.set_todos(todo_service_items(&items));
        let mut custom = self.services.metadata.read().custom.clone();
        custom.insert(TODOS_META_KEY.into(), serde_json::to_value(&items)?);
        self.services.metadata.update(
            SessionMetaPatch {
                custom: Some(custom),
                ..Default::default()
            },
            true,
        )
    }

    /// Finalize `YYYY-MM-DD_<plan-name>.md` from the Markdown H1 written by
    /// the agent. The plan stays in the same session-scoped plans directory.
    pub fn finalize_plan_filename(&mut self, content: &str) -> anyhow::Result<()> {
        let title = crate::plan_filename::markdown_plan_title(content)?;
        let Some(plans_dir) = self.plan_file_path.parent().map(PathBuf::from) else {
            anyhow::bail!("plan file has no parent directory");
        };
        let base_id = crate::plan_filename::plan_id_base(title);
        let next_id = if crate::plan_filename::plan_id_matches_base(&self.plan_id, &base_id) {
            self.plan_id.clone()
        } else if plans_dir.join(format!("{base_id}.md")).exists() {
            crate::plan_filename::generate_plan_id(&plans_dir, title)
        } else {
            base_id
        };
        if next_id == self.plan_id {
            return Ok(());
        }

        let previous_id = self.plan_id.clone();
        let previous_path = self.plan_file_path.clone();
        let next_path = plans_dir.join(format!("{next_id}.md"));
        std::fs::rename(&previous_path, &next_path)?;
        self.plan_id = next_id;
        self.plan_file_path = next_path.clone();

        let mut custom = self.services.metadata.read().custom.clone();
        custom.insert(PLAN_ID_META_KEY.into(), self.plan_id.clone().into());
        if let Err(error) = self.services.metadata.update(
            SessionMetaPatch {
                custom: Some(custom),
                ..Default::default()
            },
            true,
        ) {
            if let Err(rollback_error) = std::fs::rename(&next_path, &previous_path) {
                tracing::error!(%rollback_error, "failed to roll back plan filename");
            }
            self.plan_id = previous_id;
            self.plan_file_path = previous_path;
            return Err(error);
        }
        Ok(())
    }

    pub fn plan_state(&self) -> SessionPlanState {
        SessionPlanState {
            enabled: self.plan_mode,
            id: self.plan_id.clone(),
            path: self.plan_file_path.clone(),
            content: read_nonempty_plan(&self.plan_file_path),
        }
    }

    pub fn note_turn_started(&mut self) {
        self.services.mark_turn_started();
        self.begin_turn();
    }

    pub fn note_turn_completed(&mut self) {
        self.commit_turn();
        self.services.mark_turn_ended(TurnReason::Completed);
    }

    pub fn note_turn_cancelled(&mut self) {
        self.commit_turn();
        self.services.mark_turn_ended(TurnReason::Cancelled);
    }

    pub fn begin_turn(&mut self) {
        self.turn_message_start = Some(self.messages.len().saturating_sub(1));
        self.current_turn_changes.clear();
    }

    pub fn commit_turn(&mut self) {
        if let Some(start) = self.turn_message_start.take() {
            self.undo_stack.push(TurnCheckpoint {
                message_start_index: start,
                file_changes: std::mem::take(&mut self.current_turn_changes),
            });
        }
    }

    /// Snapshot file contents before Write/Edit (once per path per turn).
    pub async fn record_pre_change(&mut self, path: PathBuf) {
        if self.current_turn_changes.iter().any(|c| c.path == path) {
            return;
        }
        let previous = tokio::fs::read(&path).await.ok();
        self.current_turn_changes
            .push(FileChange { path, previous });
    }

    /// Undo the last completed turn: restore files + truncate messages.
    /// Returns the new message count.
    pub fn undo_last_turn(&mut self) -> anyhow::Result<usize> {
        let cp = self
            .undo_stack
            .pop()
            .ok_or_else(|| anyhow::anyhow!("Nothing to undo"))?;

        for change in cp.file_changes.into_iter().rev() {
            match change.previous {
                Some(bytes) => {
                    if let Some(parent) = change.path.parent() {
                        let _ = std::fs::create_dir_all(parent);
                    }
                    std::fs::write(&change.path, bytes)?;
                }
                None => {
                    let _ = std::fs::remove_file(&change.path);
                }
            }
        }

        self.messages.truncate(cp.message_start_index);
        self.persisted_message_count = self.persisted_message_count.min(self.messages.len());
        Ok(self.messages.len())
    }

    pub fn clear_interrupt(&self) {
        self.interrupted.store(false, Ordering::SeqCst);
    }

    pub fn request_interrupt(&self) {
        self.interrupted.store(true, Ordering::SeqCst);
        // Unblock any in-flight approval wait
        let _ = self.approval_tx.try_send(ApprovalResponse {
            approval_id: String::new(),
            decision: kkagent_protocol::ApprovalDecision::Cancelled,
            scope: None,
            feedback: Some("interrupted".into()),
            selected_label: None,
        });
        let _ = self.question_tx.try_send(QuestionResponse {
            question_id: String::new(),
            selected_option_ids: Vec::new(),
            free_text: None,
            cancelled: true,
        });
    }

    pub fn is_interrupted(&self) -> bool {
        self.interrupted.load(Ordering::SeqCst)
    }

    pub fn get_model_alias(&self) -> String {
        self.model_alias
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
    }

    pub fn set_model_alias(&self, alias: impl Into<String>) {
        *self.model_alias.lock().unwrap_or_else(|e| e.into_inner()) = alias.into();
    }

    pub fn get_permission_mode(&self) -> PermissionMode {
        *self
            .permission_mode
            .lock()
            .unwrap_or_else(|e| e.into_inner())
    }

    pub fn set_permission_mode(&self, mode: PermissionMode) {
        *self
            .permission_mode
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = mode;
    }

    pub fn add_user_message(&mut self, text: String) {
        let mut text = text;
        let mut media_content = Vec::new();
        // Resolve image mentions into real multimodal blocks. Invalid, oversized,
        // and out-of-workspace paths stay as plain user text.
        let limits = crate::media_pipeline::MediaLimits::default();
        for p in crate::media_pipeline::extract_at_paths(&text) {
            let path = if PathBuf::from(&p).is_absolute() {
                PathBuf::from(&p)
            } else {
                self.working_dir.join(&p)
            };
            match crate::media_pipeline::resolve_media(&path, &limits) {
                Ok(m) if m.kind == crate::media_pipeline::MediaKind::Image => {
                    match crate::media_pipeline::load_workspace_image(
                        &path,
                        &self.working_dir,
                        &limits,
                        &self.image_config,
                    ) {
                        Ok(image) => {
                            text.push_str(&format!(
                                "\n<image-attached name=\"{}\" bytes=\"{}\"/>",
                                m.path
                                    .file_name()
                                    .and_then(|name| name.to_str())
                                    .unwrap_or("image"),
                                m.bytes
                            ));
                            media_content.push(image);
                        }
                        Err(e) => tracing::debug!("image attach skipped for {p}: {e}"),
                    }
                }
                Ok(m) if m.kind == crate::media_pipeline::MediaKind::Video => {
                    match crate::media_pipeline::load_workspace_video(
                        &path,
                        &self.working_dir,
                        &limits,
                    ) {
                        Ok(video) => {
                            text.push_str(&format!(
                                "\n<video-attached name=\"{}\" bytes=\"{}\"/>",
                                m.path
                                    .file_name()
                                    .and_then(|name| name.to_str())
                                    .unwrap_or("video"),
                                m.bytes
                            ));
                            media_content.push(video);
                        }
                        Err(e) => tracing::debug!("video attach skipped for {p}: {e}"),
                    }
                }
                Ok(_) => tracing::debug!("non-image media attach skipped for {p}"),
                Err(e) => {
                    tracing::debug!("media resolve skipped for {p}: {e}");
                }
            }
        }
        let _ = {
            if !kkagent_protocol::is_harness_only_user_text(&text) {
                let visible = kkagent_protocol::visible_user_text(&text);
                let for_prompt = if visible.is_empty() {
                    text.as_str()
                } else {
                    visible.as_str()
                };
                self.services.metadata.set_last_prompt(for_prompt)
            } else {
                Ok(())
            }
        };
        let mut content = vec![ChatContent::Text { text }];
        content.extend(media_content);
        self.messages.push(ChatMessage {
            role: "user".into(),
            content,
        });
    }

    pub fn add_user_message_with_images(
        &mut self,
        text: String,
        images: Vec<(String, String)>,
    ) -> anyhow::Result<()> {
        self.add_user_message(text);
        let message = self
            .messages
            .last_mut()
            .ok_or_else(|| anyhow::anyhow!("user message was not created"))?;
        for (_media_type, data) in images {
            let image =
                kkagent_tools::builtin::media::normalize_user_image(&data, &self.image_config)?;
            message.content.push(ChatContent::Image {
                media_type: image.media_type,
                data: image.data,
            });
        }
        Ok(())
    }

    pub fn build_messages(&self) -> Vec<ChatMessage> {
        messages_for_llm(self)
    }

    pub fn effective_system_prompt(&self) -> String {
        // Plan-mode constraints are injected as a fresh `<system-reminder>` user
        // message each turn (kimi-code style), not baked into the system prompt.
        self.system_prompt.clone()
    }

    pub async fn wait_approval(&mut self, approval_id: &str) -> ApprovalResponse {
        loop {
            if self.is_interrupted() {
                return ApprovalResponse {
                    approval_id: approval_id.to_string(),
                    decision: kkagent_protocol::ApprovalDecision::Cancelled,
                    scope: None,
                    feedback: Some("interrupted".into()),
                    selected_label: None,
                };
            }
            match tokio::time::timeout(
                std::time::Duration::from_millis(200),
                self.approval_rx.recv(),
            )
            .await
            {
                Ok(Some(resp)) => {
                    if resp.approval_id == approval_id
                        || resp.approval_id.is_empty()
                        || matches!(resp.decision, kkagent_protocol::ApprovalDecision::Cancelled)
                    {
                        return resp;
                    }
                }
                Ok(None) => {
                    return ApprovalResponse {
                        approval_id: approval_id.to_string(),
                        decision: kkagent_protocol::ApprovalDecision::Rejected,
                        scope: None,
                        feedback: Some("approval channel closed".into()),
                        selected_label: None,
                    };
                }
                Err(_) => continue, // timeout — re-check interrupt
            }
        }
    }

    pub fn submit_approval(&self, response: ApprovalResponse) {
        let _ = self.approval_tx.try_send(response);
    }

    pub async fn wait_question(&mut self, question_id: &str) -> QuestionResponse {
        loop {
            if self.is_interrupted() {
                return QuestionResponse {
                    question_id: question_id.to_string(),
                    selected_option_ids: Vec::new(),
                    free_text: None,
                    cancelled: true,
                };
            }
            match tokio::time::timeout(
                std::time::Duration::from_millis(200),
                self.question_rx.recv(),
            )
            .await
            {
                Ok(Some(resp)) => {
                    if resp.question_id == question_id
                        || resp.question_id.is_empty()
                        || resp.cancelled
                    {
                        return resp;
                    }
                }
                Ok(None) => {
                    return QuestionResponse {
                        question_id: question_id.to_string(),
                        selected_option_ids: Vec::new(),
                        free_text: Some("question channel closed".into()),
                        cancelled: true,
                    };
                }
                Err(_) => continue,
            }
        }
    }

    pub fn submit_question(&self, response: QuestionResponse) {
        let _ = self.question_tx.try_send(response);
    }

    /// Inject the server-authoritative workspace root using Kimi's working-directory framing.
    pub fn inject_working_directory_context(&mut self) {
        self.system_prompt
            .push_str(&working_directory_context(&self.working_dir));
    }

    /// Append AGENTS.md / `.kkagent/AGENTS.md` into the system prompt (kimi-style workspace instructions).
    pub async fn inject_workspace_instructions(&mut self) {
        if let Some(file) = SessionInstructionsProvider::load(&self.working_dir).await {
            self.system_prompt
                .push_str(&SessionInstructionsProvider::format_for_system_prompt(
                    &file,
                ));
        }
    }

    pub fn inject_git_context(&mut self) {
        self.inject_git_context_with_trust(None);
    }

    pub fn inject_git_context_with_trust(
        &mut self,
        trust: Option<&kkagent_config::WorkspaceTrust>,
    ) {
        if let Some(ctx) =
            crate::git_context::collect_git_context_with_trust(&self.working_dir, trust)
        {
            self.system_prompt.push_str(&ctx);
        }
    }

    pub fn inject_date_reminder(&mut self) {
        let today = chrono::Local::now().format("%Y-%m-%d (%A)");
        self.system_prompt
            .push_str(&format!("\n\n# Date\n\nToday's date is {today}.\n"));
    }
}

fn working_directory_context(working_dir: &std::path::Path) -> String {
    format!(
        r#"

# Working Environment

## Working Directory

The current working directory is `{working_dir}`. This should be considered as the project root if you are instructed to perform tasks on the project. Tools may require absolute paths for some parameters; if so, you must use absolute paths for those parameters.

For files inside this project root, prefer paths relative to the working directory in tool parameters and shell commands. Do not repeat the project root with `cd` or `git -C` when the command can run in the session's working directory. Use an absolute path only when a tool requires it or the user explicitly authorizes access outside the working directory.
"#,
        working_dir = working_dir.display()
    )
}

fn valid_plan_id(value: &str) -> Option<&str> {
    if value.is_empty()
        || value.len() > 120
        || !value
            .chars()
            .all(|ch| ch.is_alphanumeric() || matches!(ch, '-' | '_'))
    {
        return None;
    }
    Some(value)
}

fn plan_state_from_metadata(
    session_id: &str,
    working_dir: &std::path::Path,
    session_dir: &std::path::Path,
    metadata: Option<&SessionMeta>,
    migrate_legacy: bool,
) -> SessionPlanState {
    let plans_dir = session_dir.join("agents").join("main").join("plans");
    let legacy = working_dir
        .join(".kkagent")
        .join("plans")
        .join(format!("{session_id}.md"));
    let id = metadata
        .and_then(|meta| meta.custom.get(PLAN_ID_META_KEY))
        .and_then(|value| value.as_str())
        .and_then(valid_plan_id)
        .map(str::to_string)
        .or_else(|| {
            legacy.is_file().then(|| {
                valid_plan_id(session_id)
                    .unwrap_or("legacy-plan")
                    .to_string()
            })
        })
        .unwrap_or_else(|| crate::plan_filename::generate_plan_id(&plans_dir, "plan"));
    let path = plans_dir.join(format!("{id}.md"));

    if migrate_legacy && !path.exists() && legacy.is_file() {
        let migration = path
            .parent()
            .map(std::fs::create_dir_all)
            .transpose()
            .and_then(|_| std::fs::copy(&legacy, &path).map(|_| ()));
        match migration {
            Ok(()) => tracing::info!(
                from = %legacy.display(),
                to = %path.display(),
                "migrated legacy plan file"
            ),
            Err(error) => tracing::warn!(
                from = %legacy.display(),
                to = %path.display(),
                %error,
                "failed to migrate legacy plan file"
            ),
        }
    }

    SessionPlanState {
        enabled: metadata
            .and_then(|meta| meta.custom.get(PLAN_MODE_META_KEY))
            .and_then(|value| value.as_bool())
            .unwrap_or(false),
        id,
        content: read_nonempty_plan(&path),
        path,
    }
}

fn read_nonempty_plan(path: &std::path::Path) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .filter(|content| !content.trim().is_empty())
}

fn pending_plan_review_from_metadata(
    metadata: Option<&SessionMeta>,
) -> Option<kkagent_protocol::ApprovalRequest> {
    metadata?
        .custom
        .get(PENDING_PLAN_REVIEW_META_KEY)
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
}

fn todo_items_from_metadata(
    metadata: Option<&SessionMeta>,
) -> Vec<kkagent_protocol::TodoItemEvent> {
    metadata
        .and_then(|metadata| metadata.custom.get(TODOS_META_KEY))
        .cloned()
        .and_then(|value| serde_json::from_value(value).ok())
        .unwrap_or_default()
}

fn todo_service_items(
    items: &[kkagent_protocol::TodoItemEvent],
) -> Vec<crate::session::todo::TodoItem> {
    items
        .iter()
        .filter_map(|item| {
            let title = item.content.trim();
            if title.is_empty() {
                return None;
            }
            let status = match item.status.as_str() {
                "in_progress" | "in-progress" => crate::session::todo::TodoStatus::InProgress,
                "done" | "completed" => crate::session::todo::TodoStatus::Done,
                "cancelled" | "canceled" => crate::session::todo::TodoStatus::Cancelled,
                _ => crate::session::todo::TodoStatus::Pending,
            };
            Some(crate::session::todo::TodoItem {
                title: title.to_string(),
                status,
            })
        })
        .collect()
}

fn load_persisted_metadata(
    session_id: &str,
    working_dir: &std::path::Path,
) -> (PathBuf, Option<SessionMeta>) {
    let store = SessionStore::open_default();
    let session_dir = store
        .get(session_id)
        .ok()
        .map(|summary| PathBuf::from(summary.session_dir))
        .or_else(|| store.session_dir_for(session_id, working_dir).ok())
        .unwrap_or_else(|| store.sessions_dir.join(session_id));
    let metadata = std::fs::read_to_string(session_dir.join("state.json"))
        .ok()
        .and_then(|text| serde_json::from_str::<SessionMeta>(&text).ok());
    (session_dir, metadata)
}

/// Read plan state without constructing a second live `Session`. This is used
/// while an active session has temporarily moved into the agent loop.
pub fn load_persisted_plan_state(
    session_id: &str,
    working_dir: &std::path::Path,
) -> SessionPlanState {
    let (session_dir, metadata) = load_persisted_metadata(session_id, working_dir);
    plan_state_from_metadata(
        session_id,
        working_dir,
        &session_dir,
        metadata.as_ref(),
        true,
    )
}

pub fn load_persisted_pending_plan_review(
    session_id: &str,
    working_dir: &std::path::Path,
) -> Option<kkagent_protocol::ApprovalRequest> {
    let (_, metadata) = load_persisted_metadata(session_id, working_dir);
    pending_plan_review_from_metadata(metadata.as_ref())
}

pub fn load_persisted_todos(
    session_id: &str,
    working_dir: &std::path::Path,
) -> Vec<kkagent_protocol::TodoItemEvent> {
    let (_, metadata) = load_persisted_metadata(session_id, working_dir);
    todo_items_from_metadata(metadata.as_ref())
}

fn resolve_session_dir(
    id: &str,
    working_dir: &std::path::Path,
    source: SessionCreateSource,
) -> (PathBuf, String) {
    let store = SessionStore::open_default();
    let workspace_id = encode_work_dir_key(working_dir);
    match source {
        SessionCreateSource::Startup => match store.create(id, working_dir) {
            Ok(summary) => (PathBuf::from(summary.session_dir), workspace_id),
            Err(_) => {
                // Already indexed — reuse.
                if let Ok(summary) = store.get(id) {
                    (PathBuf::from(summary.session_dir), workspace_id)
                } else {
                    let dir = store
                        .session_dir_for(id, working_dir)
                        .unwrap_or_else(|_| store.sessions_dir.join(&workspace_id).join(id));
                    let _ = std::fs::create_dir_all(&dir);
                    (dir, workspace_id)
                }
            }
        },
        SessionCreateSource::Resume | SessionCreateSource::Fork => {
            if let Ok(summary) = store.get(id) {
                (PathBuf::from(summary.session_dir), workspace_id)
            } else {
                match store.create(id, working_dir) {
                    Ok(summary) => (PathBuf::from(summary.session_dir), workspace_id),
                    Err(_) => {
                        let dir = store
                            .session_dir_for(id, working_dir)
                            .unwrap_or_else(|_| store.sessions_dir.join(&workspace_id).join(id));
                        let _ = std::fs::create_dir_all(&dir);
                        (dir, workspace_id)
                    }
                }
            }
        }
    }
}

fn default_system_prompt() -> String {
    // Aligned with kimi-code `profile/default/system.md` (trimmed for CLI v1 scope).
    r#"You are kkagent (Kimi Code CLI compatible), an interactive general AI agent running on a user's computer.

Your primary goal is to help users with software engineering tasks by taking action — use the tools available to you to make real changes on the user's system. You should also answer questions when asked. Always adhere strictly to the following system instructions and the user's requirements.

# Language

Write in the user's language unless they explicitly ask for a different one. Determine it from their most recent messages — if they switch languages mid-session, switch with them. This applies to everything user-visible: your replies, your reasoning and thinking, progress notes before and between tool calls, and questions you ask. Keep code, commands, identifiers, file paths, and technical terms in their original form.

# Prompt and Tool Use

For simple questions/greetings that do not involve any information in the working directory, you may simply reply directly. For anything else, default to taking action with tools. When the request could be interpreted as either a question to answer or a task to complete, treat it as a task.

When handling the user's request, if it involves creating, modifying, or running code or files, you MUST use the appropriate tools available to you to make actual changes — do not just describe the solution in text. When calling tools, do not provide detailed explanations or chain-of-thought. For non-trivial or multi-step tasks, first emit one short user-visible sentence describing what you will do next, then call the tool(s).

For broad codebase exploration (map a module, find call sites across many files, compare alternatives), prefer launching a `Task` subagent with a focused prompt so work can proceed in parallel; then use `TaskOutput` / `TaskList` to collect results. Do the exploration yourself only when it is a small, single-path lookup.

When a dedicated tool fits the job, reach for it before raw shell: `Read` a known path, `Glob` to find files by name, and `Grep` to search file contents.

Your text replies render as Markdown in the user's terminal. Use light Markdown: short paragraphs, `-` bullets, backticks for code/paths, fenced blocks for multi-line code. Do not use emoji unless the user does first.

You have the capability to output any number of tool calls in a single response. If you anticipate making multiple non-interfering tool calls, make them in parallel.

Tool calls run behind the user's permission settings. A rejected or denied call means the user or their policy declined that specific action — adjust your approach. Do not retry the same call unchanged.

# General Guidelines for Coding

When working on an existing codebase:
- Understand it by reading with tools (`Read`, `Glob`, `Grep`) before making changes.
- Make MINIMAL changes to achieve the goal.
- Keep edits scoped to the files and modules the request actually implies.
- Make new code read like the code around it.

DO NOT run `git commit`, `git push`, `git reset`, `git rebase` or other git mutations unless explicitly asked. Ask for confirmation each time.

Weigh reversibility and blast radius before destructive actions (`rm -rf`, dropping databases, force-pushing). Confirm first when the action is hard to undo or reaches beyond the local workspace.

# Context Management

When the conversation grows long, older turns may be compacted into a summary. Treat that summary as an accurate record of what already happened: do not redo work it reports as done.

Tool results and user messages may include `<system-reminder>` tags. These are authoritative system directives that you MUST follow — they may override normal behavior (e.g., restricting you to read-only actions during plan mode).
"#
    .to_string()
}

/// Mirrors kimi-code plan-mode injector `fullReminder`.
pub fn plan_mode_reminder() -> String {
    r#"<system-reminder>
Plan mode is active. You MUST NOT make edits through normal file tools or otherwise make changes to the system unless a tool request is explicitly approved. Prefer read-only tools. Use WritePlan only for the host-managed plan document; it does not accept a path. Use Bash only when needed; Bash follows the normal permission mode and rules. This supersedes any other instructions you have received. TaskStop, CronCreate, and CronDelete are also blocked in plan mode — call ExitPlanMode first if you need them.

Workflow:
  1. Understand — explore the codebase with Glob, Grep, Read.
  2. Design — converge on the best approach; consider trade-offs but aim for a single recommendation.
  3. Review — re-read key files to verify understanding.
  4. Write Plan — call WritePlan with the complete Markdown document. Call it again with the complete revised document after feedback. Never pass or invent a plan path.
  5. Exit — call ExitPlanMode for user approval (user chooses 执行 / 修改意见 / 拒绝).

## Plan file format
The first line MUST be a level-1 Markdown title: `# <plan name>`. The host uses that title to finalize the filename as `YYYY-MM-DD_<plan-name>.md` when ExitPlanMode is called. Write the rest as structured Markdown with concrete ordered steps and validation.

## Handling multiple approaches
Keep it focused: at most 2-3 meaningfully different approaches. Do NOT pad with minor variations — if one approach is clearly superior, just propose that one.
When the best approach depends on user preferences, constraints, or context you don't have, use AskUserQuestion to clarify first. This helps you write a better, more targeted plan rather than dumping multiple options for the user to sort through.
When you do include multiple approaches in the plan, you MUST pass them as the `options` parameter when calling ExitPlanMode, so the user can select which approach to execute at approval time.
NEVER write multiple approaches in the plan and call ExitPlanMode without the `options` parameter — the user will only see the default approval controls with no way to choose a specific approach.

AskUserQuestion is for clarifying missing requirements or user preferences that affect the plan.
Never ask about plan approval via text or AskUserQuestion.
Your turn must end with either AskUserQuestion (to clarify requirements or preferences) or ExitPlanMode (to request plan approval). Do NOT end your turn any other way.
Do NOT use AskUserQuestion to ask about plan approval or reference "the plan" — the user cannot see the plan until you call ExitPlanMode.
</system-reminder>"#
        .into()
}

/// Build LLM-facing messages; when plan mode is on, append a fresh system-reminder
/// (kimi injects these into the conversation, not only the system prompt).
pub fn messages_for_llm(session: &Session) -> Vec<ChatMessage> {
    let mut messages = session.messages.clone();
    if session.plan_mode {
        messages.push(ChatMessage {
            role: "user".into(),
            content: vec![ChatContent::Text {
                text: plan_mode_reminder(),
            }],
        });
    }
    messages
}

#[cfg(test)]
mod working_directory_tests {
    use super::*;

    #[test]
    fn context_names_the_root_and_prefers_relative_paths() {
        let root = std::path::Path::new("/workspace/project");
        let context = working_directory_context(root);
        assert!(context.contains("`/workspace/project`"));
        assert!(context.contains("prefer paths relative to the working directory"));
        assert!(context.contains("git -C"));
    }

    #[test]
    fn plan_reminder_uses_host_managed_write_plan_tool() {
        let reminder = plan_mode_reminder();
        assert!(reminder.contains("WritePlan"));
        assert!(reminder.contains("does not accept a path"));
        assert!(!reminder.contains("Plan file:"));
    }

    #[test]
    fn restores_pending_plan_review_and_todos_from_metadata() {
        let working_dir = PathBuf::from("/workspace/project");
        let mut metadata = SessionMeta::new("session-restore", &working_dir);
        let request = kkagent_protocol::ApprovalRequest {
            approval_id: "approval-1".into(),
            session_id: "session-restore".into(),
            tool_call_id: "tool-1".into(),
            tool_name: "ExitPlanMode".into(),
            action: "review".into(),
            tool_input_display: Some(serde_json::json!({
                "kind": "plan_review",
                "plan": "# Restore",
                "path": "/plans/restore.md",
            })),
            created_at: chrono::Utc::now(),
        };
        let todos = vec![
            kkagent_protocol::TodoItemEvent {
                id: "1".into(),
                content: "First".into(),
                status: "completed".into(),
            },
            kkagent_protocol::TodoItemEvent {
                id: "2".into(),
                content: "Second".into(),
                status: "in_progress".into(),
            },
        ];
        metadata.custom.insert(
            PENDING_PLAN_REVIEW_META_KEY.into(),
            serde_json::to_value(&request).unwrap(),
        );
        metadata
            .custom
            .insert(TODOS_META_KEY.into(), serde_json::to_value(&todos).unwrap());

        assert_eq!(
            pending_plan_review_from_metadata(Some(&metadata))
                .unwrap()
                .approval_id,
            "approval-1"
        );
        let restored_todos = todo_items_from_metadata(Some(&metadata));
        assert_eq!(restored_todos.len(), 2);
        assert_eq!(restored_todos[1].content, "Second");
        assert_eq!(
            todo_service_items(&restored_todos)[1].status,
            crate::session::todo::TodoStatus::InProgress
        );
    }

    #[test]
    fn restores_active_plan_from_session_scoped_path() {
        let root =
            std::env::temp_dir().join(format!("kkagent-plan-restore-{}", uuid::Uuid::new_v4()));
        let working_dir = root.join("work");
        let session_dir = root.join("session");
        let plan_id = "2026-08-11_resume_plan";
        let plan_path = session_dir
            .join("agents")
            .join("main")
            .join("plans")
            .join(format!("{plan_id}.md"));
        std::fs::create_dir_all(plan_path.parent().unwrap()).unwrap();
        std::fs::write(&plan_path, "# Resume plan\n\nKeep this content.\n").unwrap();
        let mut metadata = SessionMeta::new("session-1", &working_dir);
        metadata
            .custom
            .insert(PLAN_MODE_META_KEY.into(), true.into());
        metadata
            .custom
            .insert(PLAN_ID_META_KEY.into(), plan_id.into());

        let restored = plan_state_from_metadata(
            "session-1",
            &working_dir,
            &session_dir,
            Some(&metadata),
            true,
        );
        assert!(restored.enabled);
        assert_eq!(restored.id, plan_id);
        assert_eq!(restored.path, plan_path);
        assert_eq!(
            restored.content.as_deref(),
            Some("# Resume plan\n\nKeep this content.\n")
        );
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn migrates_workspace_plan_without_removing_legacy_file() {
        let root =
            std::env::temp_dir().join(format!("kkagent-plan-migration-{}", uuid::Uuid::new_v4()));
        let working_dir = root.join("work");
        let session_dir = root.join("session");
        let legacy = working_dir
            .join(".kkagent")
            .join("plans")
            .join("session-2.md");
        std::fs::create_dir_all(legacy.parent().unwrap()).unwrap();
        std::fs::write(&legacy, "# Legacy plan\n").unwrap();

        let restored =
            plan_state_from_metadata("session-2", &working_dir, &session_dir, None, true);
        assert_eq!(
            restored.path,
            session_dir.join("agents/main/plans").join("session-2.md")
        );
        assert_eq!(restored.content.as_deref(), Some("# Legacy plan\n"));
        assert!(legacy.exists());
        std::fs::remove_dir_all(root).unwrap();
    }
}
