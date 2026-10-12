//! ACP agents as **session backends**.
//!
//! A plugin subagent declared with `transport: "acp"` (for example the Cursor
//! CLI behind `agent acp`) is also selectable through `/model` as the runtime
//! alias `acp:<plugin>.<name>`. While a session uses such an alias, every turn
//! runs on the external agent and kkagent only renders it (see
//! `agent_loop/acp_turn.rs`).
//!
//! This module owns everything that outlives a single turn:
//!
//! - [`AcpBackendHub`]: one external process + ACP session per
//!   *(kkagent session, agent)*, created lazily, reused across turns, rebuilt
//!   when the process dies or the working directory changes, reclaimed after
//!   30 idle minutes, on session deletion and on server exit.
//! - [`InteractionBridge`]: carries the agent's reverse requests (permission,
//!   `cursor/ask_question`, `cursor/create_plan`) to the turn runner, which
//!   owns the `&mut Session` needed to wait for the user.
//! - The **transcript stays the source of truth**; the ACP session is a cache.
//!   The hub remembers a [`SyncMark`] (message count + tail fingerprint) of
//!   what the agent has seen. Before each prompt it is validated against the
//!   live transcript: still matching → only the messages added since (for
//!   example turns another model produced) are folded into a bounded context
//!   preface; no longer matching (undo / edit / restore / compact) → a fresh
//!   ACP session is created and the preface re-introduces the history.
//! - Activity summaries: each ACP turn leaves one harness-only
//!   `<system-reminder>` message that starts with `[acp-activity …]` and lists
//!   the tools the agent ran, so any model taking the session over knows what
//!   happened without ever seeing a tool block it did not produce.

use anyhow::Context as _;
use kkagent_acp::{
    config_option_by_category, AcpAskQuestionOutcome, AcpAskQuestionRequest, AcpClient,
    AcpClientOptions, AcpConfigOption, AcpCreatePlanOutcome, AcpCreatePlanRequest,
    AcpPermissionOutcome, AcpPermissionRequest, AcpRequestHandler,
};
use kkagent_llm::{ChatContent, ChatMessage};
use std::collections::hash_map::DefaultHasher;
use std::collections::HashMap;
use std::future::Future;
use std::hash::{Hash, Hasher};
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex as StdMutex};
use std::time::{Duration, Instant};
use tokio::sync::{mpsc, oneshot, Mutex, OwnedMutexGuard};

use crate::plugin::{PluginManager, PluginSubagentSpec};

/// Start of the activity-summary header line.
pub const ACP_ACTIVITY_MARKER: &str = "[acp-activity";
/// Idle timeout (no output from the agent) when the manifest has no
/// `timeoutSecs`.
pub const DEFAULT_ACP_IDLE_TIMEOUT: Duration = Duration::from_secs(600);
/// A backend process nobody used for this long is reclaimed.
pub const ACP_IDLE_REAP_AFTER: Duration = Duration::from_secs(30 * 60);
/// Handshake / `session/new` / `set_mode` request timeout.
const SETUP_REQUEST_TIMEOUT: Duration = Duration::from_secs(120);
/// Budget for the context preface (chars).
const PREFACE_MAX_CHARS: usize = 12_000;
/// Longest slice of one message quoted in the preface (chars).
const PREFACE_MESSAGE_MAX_CHARS: usize = 1_500;
/// Most tool lines listed in one activity summary.
pub const ACTIVITY_MAX_LINES: usize = 40;

// ---------------------------------------------------------------------------
// Interaction bridge
// ---------------------------------------------------------------------------

/// A reverse request waiting for the user, handed to the turn runner.
#[derive(Debug)]
pub enum AcpInteraction {
    Permission {
        request: AcpPermissionRequest,
        reply: oneshot::Sender<AcpPermissionOutcome>,
    },
    Question {
        request: AcpAskQuestionRequest,
        reply: oneshot::Sender<AcpAskQuestionOutcome>,
    },
    Plan {
        request: AcpCreatePlanRequest,
        reply: oneshot::Sender<AcpCreatePlanOutcome>,
    },
}

/// The [`AcpRequestHandler`] installed on every backend client. While a turn
/// runs, [`InteractionBridge::attach`] routes requests to its runner; between
/// turns nobody is listening and every request is answered `Cancelled`.
#[derive(Default)]
pub struct InteractionBridge {
    sender: StdMutex<Option<mpsc::Sender<AcpInteraction>>>,
}

/// Detaches the runner from the bridge when dropped.
pub struct InteractionAttachment {
    bridge: Arc<InteractionBridge>,
}

impl Drop for InteractionAttachment {
    fn drop(&mut self) {
        *lock(&self.bridge.sender) = None;
    }
}

impl InteractionBridge {
    /// Route reverse requests to `tx` until the returned guard is dropped.
    pub fn attach(self: &Arc<Self>, tx: mpsc::Sender<AcpInteraction>) -> InteractionAttachment {
        *lock(&self.sender) = Some(tx);
        InteractionAttachment {
            bridge: self.clone(),
        }
    }

    fn current(&self) -> Option<mpsc::Sender<AcpInteraction>> {
        lock(&self.sender).clone()
    }
}

#[async_trait::async_trait]
impl AcpRequestHandler for InteractionBridge {
    async fn request_permission(&self, request: AcpPermissionRequest) -> AcpPermissionOutcome {
        let Some(tx) = self.current() else {
            return AcpPermissionOutcome::Cancelled;
        };
        let (reply, answer) = oneshot::channel();
        if tx
            .send(AcpInteraction::Permission { request, reply })
            .await
            .is_err()
        {
            return AcpPermissionOutcome::Cancelled;
        }
        answer.await.unwrap_or(AcpPermissionOutcome::Cancelled)
    }

    async fn ask_question(&self, request: AcpAskQuestionRequest) -> AcpAskQuestionOutcome {
        let Some(tx) = self.current() else {
            return AcpAskQuestionOutcome::Cancelled;
        };
        let (reply, answer) = oneshot::channel();
        if tx
            .send(AcpInteraction::Question { request, reply })
            .await
            .is_err()
        {
            return AcpAskQuestionOutcome::Cancelled;
        }
        answer.await.unwrap_or(AcpAskQuestionOutcome::Cancelled)
    }

    async fn create_plan(&self, request: AcpCreatePlanRequest) -> AcpCreatePlanOutcome {
        let Some(tx) = self.current() else {
            return AcpCreatePlanOutcome::Cancelled;
        };
        let (reply, answer) = oneshot::channel();
        if tx
            .send(AcpInteraction::Plan { request, reply })
            .await
            .is_err()
        {
            return AcpCreatePlanOutcome::Cancelled;
        }
        answer.await.unwrap_or(AcpCreatePlanOutcome::Cancelled)
    }
}

fn lock<T>(mutex: &StdMutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|e| e.into_inner())
}

// ---------------------------------------------------------------------------
// Sync mark
// ---------------------------------------------------------------------------

/// What the ACP session has seen of the transcript: how many messages and a
/// fingerprint of the last one. In-memory only.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SyncMark {
    pub count: usize,
    tail: u64,
}

impl SyncMark {
    /// Mark covering all of `messages` (`None` when empty).
    pub fn of(messages: &[ChatMessage]) -> Option<Self> {
        let last = messages.last()?;
        Some(Self {
            count: messages.len(),
            tail: fingerprint(last),
        })
    }

    /// True while `messages` still starts with the history this mark was
    /// taken over: long enough, and the message at the mark is unchanged.
    pub fn matches(&self, messages: &[ChatMessage]) -> bool {
        self.count > 0
            && messages
                .get(self.count - 1)
                .is_some_and(|message| fingerprint(message) == self.tail)
    }
}

fn fingerprint(message: &ChatMessage) -> u64 {
    let mut hasher = DefaultHasher::new();
    message.role.hash(&mut hasher);
    for part in &message.content {
        match part {
            ChatContent::Text { text } => {
                0u8.hash(&mut hasher);
                text.hash(&mut hasher);
            }
            ChatContent::Image { media_type, data } => {
                1u8.hash(&mut hasher);
                media_type.hash(&mut hasher);
                data.len().hash(&mut hasher);
            }
            ChatContent::Video {
                media_type,
                path,
                filename,
            } => {
                2u8.hash(&mut hasher);
                media_type.hash(&mut hasher);
                path.hash(&mut hasher);
                filename.hash(&mut hasher);
            }
            ChatContent::ToolUse { id, name, input } => {
                3u8.hash(&mut hasher);
                id.hash(&mut hasher);
                name.hash(&mut hasher);
                input.to_string().hash(&mut hasher);
            }
            ChatContent::ToolResult {
                tool_use_id,
                content,
                is_error,
            } => {
                4u8.hash(&mut hasher);
                tool_use_id.hash(&mut hasher);
                content.hash(&mut hasher);
                is_error.hash(&mut hasher);
            }
            ChatContent::Thinking { thinking } => {
                5u8.hash(&mut hasher);
                thinking.len().hash(&mut hasher);
            }
        }
    }
    hasher.finish()
}

// ---------------------------------------------------------------------------
// Transcript helpers: prompt text, preface, activity summaries
// ---------------------------------------------------------------------------

fn message_texts(message: &ChatMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|part| match part {
            ChatContent::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn has_tool_result(message: &ChatMessage) -> bool {
    message
        .content
        .iter()
        .any(|part| matches!(part, ChatContent::ToolResult { .. }))
}

/// Index where the run of user messages at the end of `messages` begins: the
/// input the agent has not answered yet (tool results do not count).
///
/// The `[acp-activity]` summary an ACP turn leaves right behind its answer is
/// a user-role message too, but it belongs to the answered history: leaving
/// it in the group would push the group start below the sync mark and force
/// a pointless session rebuild on every turn that ran tools.
pub fn trailing_user_group_start(messages: &[ChatMessage]) -> usize {
    let mut start = messages
        .iter()
        .rposition(|message| message.role != "user" || has_tool_result(message))
        .map_or(0, |index| index + 1);
    while start < messages.len() && is_activity_summary_text(&message_texts(&messages[start])) {
        start += 1;
    }
    start
}

/// The text to send to the agent for the unanswered user messages: their
/// visible text (harness injections stripped), blank-line separated. Falls
/// back to the raw text when only harness content is pending, and notes
/// attachments the first version cannot forward.
pub fn prompt_text(unanswered: &[ChatMessage]) -> String {
    let mut parts: Vec<String> = Vec::new();
    let mut attachments = 0usize;
    for message in unanswered {
        let raw = message_texts(message);
        let visible = kkagent_protocol::harness::visible_user_text(&raw);
        if !visible.is_empty() {
            parts.push(visible);
        }
        attachments += message
            .content
            .iter()
            .filter(|part| matches!(part, ChatContent::Image { .. } | ChatContent::Video { .. }))
            .count();
    }
    if parts.is_empty() {
        if let Some(raw) = unanswered
            .iter()
            .rev()
            .map(message_texts)
            .find(|text| !text.trim().is_empty())
        {
            parts.push(raw.trim().to_string());
        }
    }
    let mut text = parts.join("\n\n");
    if attachments > 0 {
        if !text.is_empty() {
            text.push_str("\n\n");
        }
        text.push_str(&format!(
            "[{attachments} image/video attachment(s) were not forwarded to this agent]"
        ));
    }
    text
}

fn truncate_chars(text: &str, max: usize) -> String {
    if text.chars().count() <= max {
        return text.to_string();
    }
    let mut out: String = text.chars().take(max).collect();
    out.push('…');
    out
}

/// One transcript message rendered for the preface; `None` for messages that
/// carry nothing worth quoting (harness injections, schema-only messages).
fn render_for_preface(message: &ChatMessage) -> Option<String> {
    if message.is_schema_only() || message.role == "system" {
        return None;
    }
    let raw = message_texts(message);
    if message.role == "user" && is_activity_summary_text(&raw) {
        let lines: Vec<&str> = raw
            .lines()
            .filter(|line| line.starts_with("- ") || line.starts_with('…'))
            .take(8)
            .collect();
        return (!lines.is_empty()).then(|| format!("[agent tool activity]\n{}", lines.join("\n")));
    }
    let mut blocks: Vec<String> = Vec::new();
    let visible = if message.role == "user" {
        kkagent_protocol::harness::visible_user_text(&raw)
    } else {
        raw.trim().to_string()
    };
    if !visible.is_empty() {
        blocks.push(truncate_chars(&visible, PREFACE_MESSAGE_MAX_CHARS));
    }
    for part in &message.content {
        match part {
            ChatContent::Image { .. } => blocks.push("[image]".into()),
            ChatContent::Video { filename, .. } => blocks.push(format!("[video: {filename}]")),
            ChatContent::ToolUse { name, input, .. } => blocks.push(format!(
                "[tool {name}] {}",
                truncate_chars(&input.to_string(), 160)
            )),
            ChatContent::ToolResult {
                content, is_error, ..
            } => blocks.push(format!(
                "[tool result{}] {}",
                if *is_error { " (error)" } else { "" },
                truncate_chars(content.trim(), 200)
            )),
            ChatContent::Text { .. } | ChatContent::Thinking { .. } => {}
        }
    }
    if blocks.is_empty() {
        return None;
    }
    Some(format!("[{}]\n{}", message.role, blocks.join("\n")))
}

/// Bounded context preface for messages the ACP session has not seen. Newest
/// messages win when the budget runs out. `None` when nothing is worth
/// quoting.
pub fn build_preface(unseen: &[ChatMessage]) -> Option<String> {
    let rendered: Vec<String> = unseen.iter().filter_map(render_for_preface).collect();
    if rendered.is_empty() {
        return None;
    }
    let mut kept: Vec<&String> = Vec::new();
    let mut used = 0usize;
    for entry in rendered.iter().rev() {
        let cost = entry.chars().count() + 2;
        if used + cost > PREFACE_MAX_CHARS && !kept.is_empty() {
            break;
        }
        used += cost;
        kept.push(entry);
    }
    kept.reverse();
    let omitted = rendered.len() - kept.len();
    let mut out = String::from(
        "<kkagent-context>\nThis session was attached after earlier conversation turns that you \
         did not see (kkagent history, possibly produced by other models). Treat the history \
         below as background; the user's new message follows this block.\n",
    );
    if omitted > 0 {
        out.push_str(&format!("\n[{omitted} older message(s) omitted]\n"));
    }
    for entry in kept {
        out.push('\n');
        out.push_str(entry);
        out.push('\n');
    }
    out.push_str("</kkagent-context>");
    Some(out)
}

/// One tool the external agent ran during a turn.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ToolActivity {
    pub kind: String,
    pub title: String,
    pub status: String,
    pub locations: Vec<String>,
}

/// The harness-only transcript message summarizing a turn's tool activity, or
/// `None` when the agent ran nothing. `agent_id` is `<plugin>.<name>`,
/// `agent_name` the display name (`cursor`).
pub fn format_activity_summary(
    agent_id: &str,
    agent_name: &str,
    activity: &[ToolActivity],
) -> Option<String> {
    if activity.is_empty() {
        return None;
    }
    let mut body = format!(
        "{ACP_ACTIVITY_MARKER} agent={agent_id} name={agent_name}]\n\
         The external agent \"{agent_name}\" ran these tools while answering the previous \
         message. Their inputs, outputs and diffs are not recorded here: inspect the \
         workspace (Read, `git diff`) for the real state of the files.\n"
    );
    for item in activity.iter().take(ACTIVITY_MAX_LINES) {
        let kind = if item.kind.is_empty() {
            "tool"
        } else {
            &item.kind
        };
        let title = truncate_chars(item.title.trim(), 160);
        let mut line = format!("- {kind}");
        if !title.is_empty() {
            line.push_str(&format!(" · {title}"));
        }
        if !item.status.is_empty() {
            line.push_str(&format!(" ({})", item.status));
        }
        if !item.locations.is_empty() {
            let shown: Vec<&str> = item.locations.iter().take(3).map(String::as_str).collect();
            line.push_str(&format!(" — {}", shown.join(", ")));
            if item.locations.len() > 3 {
                line.push_str(&format!(" (+{} more)", item.locations.len() - 3));
            }
        }
        line.push('\n');
        body.push_str(&line);
    }
    if activity.len() > ACTIVITY_MAX_LINES {
        body.push_str(&format!(
            "… and {} more tool call(s)\n",
            activity.len() - ACTIVITY_MAX_LINES
        ));
    }
    Some(crate::system_reminder::wrap(body.trim_end()))
}

fn is_activity_summary_text(text: &str) -> bool {
    kkagent_protocol::harness::is_harness_only_user_text(text) && text.contains(ACP_ACTIVITY_MARKER)
}

/// Display names of the ACP agents that ran turns inside `messages`, in
/// order of first appearance (found through the activity summaries).
pub fn external_agents_in(messages: &[ChatMessage]) -> Vec<String> {
    let mut agents: Vec<String> = Vec::new();
    for message in messages.iter().filter(|m| m.role == "user") {
        let text = message_texts(message);
        if !is_activity_summary_text(&text) {
            continue;
        }
        let Some(start) = text.find(ACP_ACTIVITY_MARKER) else {
            continue;
        };
        let header = text[start..].lines().next().unwrap_or_default();
        let header = header.trim_end_matches(']');
        let field = |key: &str| {
            header
                .split_whitespace()
                .find_map(|token| token.strip_prefix(key))
                .map(str::to_string)
        };
        if let Some(name) = field("name=").or_else(|| field("agent=")) {
            if !agents.contains(&name) {
                agents.push(name);
            }
        }
    }
    agents
}

/// Tool name used in events for an ACP tool call: `<agent>/<kind>`. The
/// namespace keeps the TUI's name-based rendering (Read/Grep/Glob folding,
/// Write/Edit change tracking, Bash result parsing) away from tools it
/// cannot see the effects of.
pub fn acp_tool_name(agent_name: &str, kind: Option<&str>) -> String {
    let kind = kind
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .unwrap_or("tool");
    format!("{agent_name}/{kind}")
}

// ---------------------------------------------------------------------------
// Hub
// ---------------------------------------------------------------------------

/// Starts (or connects) an ACP client. Replaceable for tests.
pub type AcpConnectFuture = Pin<Box<dyn Future<Output = anyhow::Result<Arc<AcpClient>>> + Send>>;
pub type AcpConnector = Arc<dyn Fn(AcpClientOptions) -> AcpConnectFuture + Send + Sync>;

fn process_connector() -> AcpConnector {
    Arc::new(|options| Box::pin(AcpClient::spawn(options)))
}

type EntryKey = (String, String);

struct Connected {
    client: Arc<AcpClient>,
    bridge: Arc<InteractionBridge>,
    acp_session_id: String,
    cwd: PathBuf,
    synced: Option<SyncMark>,
    mode: Option<String>,
    available_modes: Vec<String>,
    /// Session config options as the agent last reported them.
    config_options: Vec<AcpConfigOption>,
    /// Model id the agent reported for the session, variant suffix included
    /// (`gpt-6.1-sol[high]`).
    model_raw: Option<String>,
    /// Thinking level the agent could not take for this ACP session; asked
    /// again only when the request changes, so it is reported once.
    refused_level: Option<String>,
}

impl Connected {
    /// Adopt what a fresh `session/new` reported.
    fn adopt_info(&mut self, info: kkagent_acp::AcpSessionInfo) {
        self.mode = info.current_mode.clone();
        self.available_modes = info.available_modes.clone();
        self.model_raw = info.current_model_raw();
        self.config_options = info.config_options;
        self.refused_level = None;
    }
}

/// What an ACP agent session runs on, as far as the agent reported it.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct AcpAgentSnapshot {
    /// Model id without variant parameters (`claude-sonnet-5-5`).
    pub model: Option<String>,
    /// Thinking level in effect: the agent's thinking-level option, else the
    /// bare level of a variant id (`gpt-6.1-sol[high]`).
    pub thought_level: Option<String>,
    /// Levels the agent lets a client choose; empty when it offers none (the
    /// level then only follows the agent's own settings).
    pub thought_levels: Vec<String>,
}

/// Thinking levels from lowest to highest, as agents spell them.
const LEVEL_SCALE: [&str; 8] = [
    "none", "minimal", "low", "medium", "high", "xhigh", "max", "ultra",
];

/// A thinking-level spelling in the form of [`LEVEL_SCALE`] (`Extra High` →
/// `xhigh`, `off` → `none`); unknown spellings are only lower-cased.
fn canonical_level(token: &str) -> String {
    let flat: String = token
        .trim()
        .chars()
        .filter(|c| !matches!(c, '-' | '_' | ' '))
        .collect::<String>()
        .to_ascii_lowercase();
    match flat.as_str() {
        "off" | "disabled" => "none",
        "min" => "minimal",
        "med" | "mid" => "medium",
        "extrahigh" | "veryhigh" => "xhigh",
        "maximum" => "max",
        other => other,
    }
    .to_string()
}

/// Pick the level `offered` by an agent that best matches `requested`: the
/// same level when it is offered, else the nearest on [`LEVEL_SCALE`] (ties
/// go to the lower level). The flag says whether the match is exact. `None`
/// when `requested` is not a known level and nothing offered equals it.
pub fn match_thought_level(requested: &str, offered: &[String]) -> Option<(String, bool)> {
    let wanted = canonical_level(requested);
    if let Some(same) = offered.iter().find(|o| canonical_level(o) == wanted) {
        return Some((same.clone(), true));
    }
    let rank = |level: &str| {
        LEVEL_SCALE
            .iter()
            .position(|known| *known == canonical_level(level))
    };
    let wanted_rank = rank(requested)?;
    offered
        .iter()
        .filter_map(|level| rank(level).map(|r| (r, level)))
        .min_by_key(|(r, _)| (r.abs_diff(wanted_rank), *r))
        .map(|(_, level)| (level.clone(), false))
}

/// Result of [`AcpLease::ensure_thought_level`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ThoughtLevelOutcome {
    /// Nothing to do: the agent already runs at the level, or the same
    /// request was turned down before and has been reported.
    Unchanged,
    /// The agent now runs at `to` (was `from`). `exact` is false when `to`
    /// is only the closest level the agent offers to the request.
    Applied {
        from: Option<String>,
        to: String,
        exact: bool,
    },
    /// The agent offers no thinking-level option.
    Unsupported,
    /// The agent offers no matching level or rejected the change.
    Refused { reason: String },
}

impl ThoughtLevelOutcome {
    /// What to tell the user about asking `agent` for `requested`, if
    /// anything.
    pub fn note(&self, agent: &str, requested: &str) -> Option<String> {
        match self {
            Self::Unchanged => None,
            Self::Applied { exact: true, .. } => None,
            Self::Applied { to, .. } => Some(format!(
                "{agent} has no thinking level '{requested}'; it now uses '{to}', the closest it offers."
            )),
            Self::Unsupported => Some(format!(
                "{agent} does not let clients choose a thinking level, so '{requested}' has no \
                 effect on it; change it in the agent's own settings."
            )),
            Self::Refused { reason } => Some(format!(
                "{agent} did not take thinking level '{requested}': {reason}"
            )),
        }
    }
}

#[derive(Default)]
struct EntryState {
    conn: Option<Connected>,
}

struct BackendEntry {
    state: Arc<Mutex<EntryState>>,
    /// Mirror of the live client, reachable without the turn lock so a
    /// session deletion can kill a running turn's process.
    client: StdMutex<Option<Arc<AcpClient>>>,
    last_used: StdMutex<Instant>,
}

impl BackendEntry {
    fn touch(&self) {
        *lock(&self.last_used) = Instant::now();
    }
}

/// Process + ACP-session registry for ACP-backed sessions.
pub struct AcpBackendHub {
    plugins: Arc<PluginManager>,
    connector: AcpConnector,
    entries: Mutex<HashMap<EntryKey, Arc<BackendEntry>>>,
    reaper_started: AtomicBool,
    idle_reap_after: Duration,
}

impl AcpBackendHub {
    pub fn new(plugins: Arc<PluginManager>) -> Arc<Self> {
        Self::with_connector(plugins, process_connector())
    }

    /// Hub with a custom client factory (in-process stub agents in tests).
    pub fn with_connector(plugins: Arc<PluginManager>, connector: AcpConnector) -> Arc<Self> {
        Arc::new(Self {
            plugins,
            connector,
            entries: Mutex::new(HashMap::new()),
            reaper_started: AtomicBool::new(false),
            idle_reap_after: ACP_IDLE_REAP_AFTER,
        })
    }

    /// Manifest of the ACP agent behind `agent_id` (`<plugin>.<name>`), or a
    /// readable error when its plugin is disabled or gone.
    pub async fn resolve(&self, agent_id: &str) -> anyhow::Result<PluginSubagentSpec> {
        self.plugins.acp_backend(agent_id).await.ok_or_else(|| {
            anyhow::anyhow!(
                "ACP agent '{agent_id}' is not available: its plugin is disabled, removed, or no \
                 longer declares it as a `transport: \"acp\"` subagent. Pick another model with \
                 /model."
            )
        })
    }

    /// Take the (session, agent) backend for one turn, starting the process
    /// and the ACP session on first use. Turns of the same pair are
    /// serialized; the lease is released by dropping it.
    pub async fn checkout(
        self: &Arc<Self>,
        session_id: &str,
        agent_id: &str,
        cwd: &Path,
    ) -> anyhow::Result<AcpLease> {
        let spec = self.resolve(agent_id).await?;
        self.ensure_reaper();
        let entry = {
            let mut entries = self.entries.lock().await;
            entries
                .entry((session_id.to_string(), agent_id.to_string()))
                .or_insert_with(|| {
                    Arc::new(BackendEntry {
                        state: Arc::new(Mutex::new(EntryState::default())),
                        client: StdMutex::new(None),
                        last_used: StdMutex::new(Instant::now()),
                    })
                })
                .clone()
        };
        entry.touch();
        let mut state = entry.state.clone().lock_owned().await;
        if state.conn.as_ref().is_some_and(|c| !c.client.is_alive()) {
            if let Some(dead) = state.conn.take() {
                dead.client.shutdown().await;
            }
        }
        if state.conn.is_none() {
            let connected = self.connect(&spec, cwd).await?;
            *lock(&entry.client) = Some(connected.client.clone());
            state.conn = Some(connected);
        }
        Ok(AcpLease {
            entry,
            state,
            spec,
            agent_id: agent_id.to_string(),
        })
    }

    async fn connect(&self, spec: &PluginSubagentSpec, cwd: &Path) -> anyhow::Result<Connected> {
        let bridge = Arc::new(InteractionBridge::default());
        let process_cwd = spec
            .transport_config
            .cwd
            .as_ref()
            .map(PathBuf::from)
            .unwrap_or_else(|| cwd.to_path_buf());
        let options = AcpClientOptions {
            command: crate::external_subagent::resolve_command(&spec.transport_config),
            cwd: process_cwd,
            env: spec
                .env
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect(),
            request_timeout: Some(SETUP_REQUEST_TIMEOUT),
            permission: None,
            handler: Some(bridge.clone()),
            // Lets Cursor offer the thinking level as an option of its own,
            // which `/effort` drives.
            parameterized_model_picker: true,
        };
        let client = (self.connector)(options).await?;
        let setup = async {
            if !spec.transport_config.skip_auth {
                client.authenticate().await.context("ACP authenticate")?;
            }
            client
                .start_session_info(cwd)
                .await
                .context("ACP session/new")
        }
        .await;
        let info = match setup {
            Ok(info) => info,
            Err(error) => {
                client.shutdown().await;
                return Err(error);
            }
        };
        let mut connected = Connected {
            client,
            bridge,
            acp_session_id: info.session_id.clone(),
            cwd: cwd.to_path_buf(),
            synced: None,
            mode: None,
            available_modes: Vec::new(),
            config_options: Vec::new(),
            model_raw: None,
            refused_level: None,
        };
        connected.adopt_info(info);
        Ok(connected)
    }

    /// Kill every backend of `session_id` (session deleted / closed).
    pub async fn close_session(&self, session_id: &str) {
        let removed: Vec<Arc<BackendEntry>> = {
            let mut entries = self.entries.lock().await;
            let keys: Vec<EntryKey> = entries
                .keys()
                .filter(|(sid, _)| sid == session_id)
                .cloned()
                .collect();
            keys.into_iter()
                .filter_map(|key| entries.remove(&key))
                .collect()
        };
        for entry in removed {
            shutdown_entry(&entry).await;
        }
    }

    /// Kill every backend (server exit).
    pub async fn close_all(&self) {
        let removed: Vec<Arc<BackendEntry>> = {
            let mut entries = self.entries.lock().await;
            entries.drain().map(|(_, entry)| entry).collect()
        };
        for entry in removed {
            shutdown_entry(&entry).await;
        }
    }

    /// Number of live (session, agent) backends (diagnostics / tests).
    pub async fn len(&self) -> usize {
        self.entries.lock().await.len()
    }

    pub async fn is_empty(&self) -> bool {
        self.len().await == 0
    }

    /// Reclaim backends nobody used for the idle period (and are not mid-turn).
    pub async fn reap_idle(&self) {
        let stale: Vec<(EntryKey, Arc<BackendEntry>)> = {
            let entries = self.entries.lock().await;
            entries
                .iter()
                .filter(|(_, entry)| {
                    lock(&entry.last_used).elapsed() >= self.idle_reap_after
                        && entry.state.try_lock().is_ok()
                })
                .map(|(key, entry)| (key.clone(), entry.clone()))
                .collect()
        };
        for (key, entry) in stale {
            self.entries.lock().await.remove(&key);
            shutdown_entry(&entry).await;
        }
    }

    fn ensure_reaper(self: &Arc<Self>) {
        if self.reaper_started.swap(true, Ordering::SeqCst) {
            return;
        }
        let weak = Arc::downgrade(self);
        tokio::spawn(async move {
            loop {
                tokio::time::sleep(Duration::from_secs(60)).await;
                let Some(hub) = weak.upgrade() else {
                    break;
                };
                hub.reap_idle().await;
            }
        });
    }
}

async fn shutdown_entry(entry: &BackendEntry) {
    let client = lock(&entry.client).take();
    if let Some(client) = client {
        client.shutdown().await;
    }
}

/// Result of [`AcpLease::prepare`].
#[derive(Debug, Clone)]
pub struct PreparedPrompt {
    /// Preface (if any) followed by the user's text: what goes into
    /// `session/prompt`.
    pub text: String,
    /// A fresh ACP session replaced a stale one (history rewritten, cwd
    /// changed).
    pub rebuilt: bool,
    /// The prompt carries a context preface.
    pub has_preface: bool,
}

/// Exclusive use of one (session, agent) backend for a turn.
pub struct AcpLease {
    entry: Arc<BackendEntry>,
    state: OwnedMutexGuard<EntryState>,
    spec: PluginSubagentSpec,
    agent_id: String,
}

impl Drop for AcpLease {
    fn drop(&mut self) {
        self.entry.touch();
    }
}

impl AcpLease {
    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }

    /// Display name of the agent (`cursor`).
    pub fn agent_name(&self) -> String {
        let mut spec = self.spec.clone();
        spec.plugin_id = self.agent_id.clone();
        spec.qualified_name()
    }

    pub fn spec(&self) -> &PluginSubagentSpec {
        &self.spec
    }

    /// Seconds of silence tolerated from the agent during a turn.
    pub fn idle_timeout(&self) -> Duration {
        self.spec
            .timeout_secs
            .map(Duration::from_secs)
            .unwrap_or(DEFAULT_ACP_IDLE_TIMEOUT)
    }

    fn conn(&self) -> &Connected {
        self.state
            .conn
            .as_ref()
            .expect("lease always holds a connection")
    }

    fn conn_mut(&mut self) -> &mut Connected {
        self.state
            .conn
            .as_mut()
            .expect("lease always holds a connection")
    }

    pub fn client(&self) -> Arc<AcpClient> {
        self.conn().client.clone()
    }

    pub fn acp_session_id(&self) -> String {
        self.conn().acp_session_id.clone()
    }

    pub fn bridge(&self) -> Arc<InteractionBridge> {
        self.conn().bridge.clone()
    }

    /// Mode the agent is currently in, as far as we know.
    pub fn mode(&self) -> Option<String> {
        self.conn().mode.clone()
    }

    pub fn note_mode(&mut self, mode: &str) {
        self.conn_mut().mode = Some(mode.to_string());
    }

    /// Switch the agent to `mode` when it supports it and is not there yet.
    /// Failures only get logged: a stale mode must not fail the turn.
    pub async fn ensure_mode(&mut self, mode: &str) {
        let (client, session_id) = {
            let conn = self.conn();
            if conn.mode.as_deref() == Some(mode) {
                return;
            }
            if !conn.available_modes.is_empty() && !conn.available_modes.iter().any(|m| m == mode) {
                return;
            }
            (conn.client.clone(), conn.acp_session_id.clone())
        };
        match client.set_mode(&session_id, mode).await {
            Ok(()) => self.conn_mut().mode = Some(mode.to_string()),
            Err(error) => {
                tracing::warn!(agent = %self.agent_id, %mode, "ACP session/set_mode failed: {error:#}");
            }
        }
    }

    /// The model and thinking level the agent reported for its session.
    pub fn agent_snapshot(&self) -> AcpAgentSnapshot {
        let conn = self.conn();
        let level = config_option_by_category(&conn.config_options, "thought_level");
        AcpAgentSnapshot {
            model: conn
                .model_raw
                .as_deref()
                .and_then(kkagent_acp::base_model_id),
            thought_level: level.and_then(|option| option.current.clone()).or_else(|| {
                conn.model_raw
                    .as_deref()
                    .and_then(kkagent_acp::variant_level_of)
            }),
            thought_levels: level.map(AcpConfigOption::value_ids).unwrap_or_default(),
        }
    }

    /// Ask the agent to think at `requested` (`low` … `max`, as `/effort`
    /// spells them), mapped onto the levels the agent offers. Only the
    /// agent's thinking-level option is touched. Failures never fail the
    /// turn: they come back as an outcome the caller reports.
    ///
    /// Agents may keep the choice as their own default (Cursor does).
    pub async fn ensure_thought_level(&mut self, requested: &str) -> ThoughtLevelOutcome {
        let requested = requested.trim().to_ascii_lowercase();
        if requested.is_empty() || self.conn().refused_level.as_deref() == Some(requested.as_str())
        {
            return ThoughtLevelOutcome::Unchanged;
        }
        let option = config_option_by_category(&self.conn().config_options, "thought_level")
            .filter(|option| !option.values.is_empty())
            .cloned();
        let Some(option) = option else {
            self.conn_mut().refused_level = Some(requested);
            return ThoughtLevelOutcome::Unsupported;
        };
        let offered = option.value_ids();
        let Some((target, exact)) = match_thought_level(&requested, &offered) else {
            self.conn_mut().refused_level = Some(requested);
            return ThoughtLevelOutcome::Refused {
                reason: format!("it offers {}.", offered.join(", ")),
            };
        };
        if option.current.as_deref() == Some(target.as_str()) {
            return ThoughtLevelOutcome::Unchanged;
        }
        let (client, session_id) = {
            let conn = self.conn();
            (conn.client.clone(), conn.acp_session_id.clone())
        };
        match client
            .set_config_option(&session_id, &option.id, &target)
            .await
        {
            Ok(updated) => {
                let conn = self.conn_mut();
                if updated.is_empty() {
                    // The agent confirmed without a list: patch what we cache.
                    if let Some(cached) = conn.config_options.iter_mut().find(|o| o.id == option.id)
                    {
                        cached.current = Some(target.clone());
                    }
                } else {
                    if let Some(model) = kkagent_acp::current_model_raw_of(&updated, None) {
                        conn.model_raw = Some(model);
                    }
                    conn.config_options = updated;
                }
                ThoughtLevelOutcome::Applied {
                    from: option.current.clone(),
                    to: target,
                    exact,
                }
            }
            Err(error) => {
                tracing::warn!(
                    agent = %self.agent_id, level = %target,
                    "ACP session/set_config_option failed: {error:#}"
                );
                self.conn_mut().refused_level = Some(requested);
                ThoughtLevelOutcome::Refused {
                    reason: format!("{error:#}"),
                }
            }
        }
    }

    /// Decide what the agent has to be told before the user's new message.
    ///
    /// `messages` is the live transcript ending with the unanswered user
    /// message(s). The sync mark is validated against it: still matching →
    /// only messages added since the mark go into the preface; stale or
    /// missing on an already-used session → a fresh ACP session is created
    /// and the whole preceding history goes in (bounded).
    pub async fn prepare(
        &mut self,
        messages: &[ChatMessage],
        cwd: &Path,
    ) -> anyhow::Result<PreparedPrompt> {
        let group_start = trailing_user_group_start(messages);
        let prompt = prompt_text(&messages[group_start..]);
        let history = &messages[..group_start];

        let cwd_changed = self.conn().cwd != cwd;
        let mark_valid = self
            .conn()
            .synced
            .as_ref()
            .is_some_and(|mark| mark.count <= group_start && mark.matches(history));
        let used_before = self.conn().synced.is_some();
        let rebuilt = cwd_changed || (used_before && !mark_valid);
        if rebuilt {
            self.rebuild_session(cwd).await?;
        }
        let unseen: &[ChatMessage] = if mark_valid && !rebuilt {
            let covered = self.conn().synced.map_or(0, |mark| mark.count);
            &history[covered..]
        } else {
            history
        };
        let preface = build_preface(unseen);
        let has_preface = preface.is_some();
        let text = match preface {
            Some(preface) => format!("{preface}\n\n{prompt}"),
            None => prompt,
        };
        Ok(PreparedPrompt {
            text,
            rebuilt,
            has_preface,
        })
    }

    async fn rebuild_session(&mut self, cwd: &Path) -> anyhow::Result<()> {
        let client = self.conn().client.clone();
        let info = client
            .start_session_info(cwd)
            .await
            .context("ACP session/new (rebuild)")?;
        let conn = self.conn_mut();
        conn.acp_session_id = info.session_id.clone();
        conn.cwd = cwd.to_path_buf();
        conn.synced = None;
        conn.adopt_info(info);
        Ok(())
    }

    /// Record that the agent has now seen all of `messages`.
    pub fn mark_synced(&mut self, messages: &[ChatMessage]) {
        self.conn_mut().synced = SyncMark::of(messages);
    }

    /// Forget what the agent has seen (the next turn starts a fresh ACP
    /// session with a full preface).
    pub fn clear_synced(&mut self) {
        self.conn_mut().synced = None;
    }

    /// Drop the connection after a failed or abandoned turn: the process is
    /// killed and the next checkout starts a new one. Consumes the lease —
    /// there is nothing left to use it for.
    pub async fn invalidate(mut self) {
        if let Some(conn) = self.state.conn.take() {
            conn.client.shutdown().await;
        }
        *lock(&self.entry.client) = None;
    }
}

#[cfg(test)]
pub(crate) mod testing;
#[cfg(test)]
mod tests;
