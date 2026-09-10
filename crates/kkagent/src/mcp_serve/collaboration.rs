//! MCP orchestration metadata shares the existing transcript database. Session
//! messages, checkpoints, tool execution and permissions stay in the core.
use super::*;
use kkagent_tools::{Tool, ToolContext};
use serde::{Deserialize, Serialize};

#[derive(Clone)]
pub(super) struct CollaborationStore(kkagent_core::transcript::SharedSqlite);

impl CollaborationStore {
    pub fn new(db: &TranscriptDb) -> Self {
        Self(db.shared())
    }

    pub fn put(&self, kind: &str, id: &str, value: &Value) -> Result<(), String> {
        let db = self.0.lock().map_err(|e| e.to_string())?;
        db.execute_batch("CREATE TABLE IF NOT EXISTS mcp_collaboration (kind TEXT NOT NULL, id TEXT NOT NULL, value TEXT NOT NULL, PRIMARY KEY(kind,id))").map_err(|e| e.to_string())?;
        db.execute("INSERT INTO mcp_collaboration VALUES (?1,?2,?3) ON CONFLICT(kind,id) DO UPDATE SET value=excluded.value", [kind, id, &value.to_string()]).map_err(|e| e.to_string())?;
        Ok(())
    }

    pub fn list(&self, kind: &str) -> Result<Vec<(String, Value)>, String> {
        let db = self.0.lock().map_err(|e| e.to_string())?;
        db.execute_batch("CREATE TABLE IF NOT EXISTS mcp_collaboration (kind TEXT NOT NULL, id TEXT NOT NULL, value TEXT NOT NULL, PRIMARY KEY(kind,id))").map_err(|e| e.to_string())?;
        let mut stmt = db
            .prepare("SELECT id,value FROM mcp_collaboration WHERE kind=?1 ORDER BY id")
            .map_err(|e| e.to_string())?;
        let rows = stmt
            .query_map([kind], |r| {
                Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?))
            })
            .map_err(|e| e.to_string())?;
        rows.map(|r| {
            let (id, value) = r.map_err(|e| e.to_string())?;
            Ok((id, serde_json::from_str(&value).map_err(|e| e.to_string())?))
        })
        .collect()
    }
}

#[derive(Clone, Serialize, Deserialize)]
pub(super) struct TaskRecord {
    pub id: String,
    pub session_id: String,
    pub resume: bool,
    pub description: String,
    pub prompt: String,
    pub workspace: PathBuf,
    pub run_dir: PathBuf,
    pub branch: Option<String>,
    pub base_commit: Option<String>,
    pub plan_title: Option<String>,
    pub plan: Option<String>,
    pub plan_ref: Option<Value>,
    pub phase: TaskPhase,
    pub summary: TaskSummary,
    pub error: Option<String>,
    pub review: String,
    #[serde(default)]
    pub reviewed_snapshot: Option<String>,
    #[serde(default)]
    pub event_sequence: u64,
    #[serde(default)]
    pub recent_events: Vec<String>,
    #[serde(default)]
    pub progress: Progress,
    /// Persisted so resumed MCP tasks keep routing decisions after restart.
    #[serde(default)]
    pub via_standalone: bool,
}

impl TaskRecord {
    pub fn into_task(self) -> McpTask {
        let interrupted = !self.phase.terminal();
        McpTask {
            id: self.id,
            session_id: self.session_id,
            description: self.description,
            prompt: self.prompt,
            plan_title: self.plan_title,
            plan: self.plan,
            plan_ref: StdMutex::new(self.plan_ref),
            origin_workspace: self.workspace,
            worktree: Mutex::new(self.branch.map(|branch| {
                kkagent_tools::git_worktree::WorktreeInfo {
                    path: self.run_dir.clone(),
                    branch,
                }
            })),
            isolated: false,
            run_dir: Mutex::new(self.run_dir),
            base_commit: StdMutex::new(self.base_commit),
            via_standalone: self.via_standalone,
            interrupt: Arc::new(AtomicBool::new(false)),
            mailbox: SessionSteerMailbox::default(),
            question_tx: StdMutex::new(None),
            approval_tx: StdMutex::new(None),
            runner_notify: Notify::new(),
            runner: StdMutex::new(RunnerSlot::initial()),
            activity: StdMutex::new(ActivityStamp::default()),
            phase: StdMutex::new(if interrupted {
                TaskPhase::Failed
            } else {
                self.phase
            }),
            progress: StdMutex::new(self.progress),
            recent_events: StdMutex::new(self.recent_events),
            pending_question: StdMutex::new(None),
            pending_approval: StdMutex::new(None),
            pending_instructions: StdMutex::new(Vec::new()),
            summary: StdMutex::new(self.summary),
            error: StdMutex::new(if interrupted {
                Some("server restarted; resume explicitly with continue_task".into())
            } else {
                self.error
            }),
            started_at: Instant::now(),
            finished_at: StdMutex::new(Some(Instant::now())),
            review: StdMutex::new(self.review),
            reviewed_snapshot: StdMutex::new(self.reviewed_snapshot),
            event_sequence: std::sync::atomic::AtomicU64::new(self.event_sequence),
            resume: self.resume,
        }
    }
}

impl McpServer {
    pub(super) async fn persist_task(&self, task: &McpTask) -> Result<(), String> {
        persist_task(&self.store, task).await
    }

    pub(super) async fn tool_search(
        &self,
        name: &str,
        args: &Value,
    ) -> Result<(String, Option<Value>), String> {
        let workspace = self.resolve_workspace_arg(args).await?;
        let mut tools_config = self.config.tools.clone();
        tools_config.path_guard_mode = "strict".into();
        tools_config.additional_dirs.clear();
        tools_config.sensitive_path_check = true;
        let ctx = ToolContext {
            working_dir: workspace.clone(),
            tools_config,
            session_id: "mcp-inspect".into(),
            turn_id: String::new(),
            plan_file_path: None,
            image: self.config.image.clone(),
            tool_call_id: None,
            interrupted: None,
            model_alias: None,
        };
        let mut input = json!({});
        for key in [
            "pattern",
            "path",
            "glob",
            "case_insensitive",
            "context",
            "offset",
        ] {
            if let Some(v) = args.get(key) {
                input[key] = v.clone();
            }
        }
        if let Some(context) = input["context"].as_u64() {
            input["context"] = json!(context.min(20));
        }
        let limit = args
            .get("limit")
            .and_then(Value::as_u64)
            .unwrap_or(200)
            .clamp(1, 2000);
        input["head_limit"] = json!(limit);
        if name == "Grep" {
            input["output_mode"] = json!("content");
        }
        let output = match name {
            "Glob" => kkagent_tools::builtin::GlobTool.execute(input, &ctx).await,
            _ => kkagent_tools::builtin::GrepTool.execute(input, &ctx).await,
        }
        .map_err(|e| e.to_string())?;
        if output.is_error {
            return Err(output.content);
        }
        let mut text = output.content;
        if let Some(note) = &output.note {
            text.push_str("\n\n");
            text.push_str(note);
        }
        // Do not emit metadata-only structuredContent: OpenAI hosts drop
        // content[] when structuredContent is present, and Glob/Grep store
        // the match list only in the text body (ToolOutput.data is empty).
        Ok((text, None))
    }

    pub(super) async fn tool_get_plan(
        &self,
        args: &Value,
    ) -> Result<(String, Option<Value>), String> {
        let id = args["plan_id"].as_str().ok_or("plan_id is required")?;
        let plans = self.plans.lock().await;
        let plan = plans
            .get(id)
            .ok_or_else(|| format!("unknown plan_id: {id}"))?;
        let version = args["plan_version"].as_u64().unwrap_or(plan.version);
        // Prefer on-disk markdown (default Plan mode alignment); fall back to
        // legacy in-DB revision bodies from older releases.
        let body = read_stored_plan_body(plan, version)?;
        Ok((
            body.clone(),
            Some(json!({
                "plan_id": id,
                "plan_version": version,
                "title": plan.title,
                "workspace": plan.workspace,
                "path": plan.revision_paths.get(&version).unwrap_or(&plan.path),
                "text": body,
            })),
        ))
    }

    pub(super) async fn plan_instruction(
        &self,
        args: &Value,
    ) -> Result<Option<(String, Value)>, String> {
        if args.get("plan_id").is_none() {
            return Ok(None);
        }
        let id = args["plan_id"].as_str().ok_or("plan_id is required")?;
        let plans = self.plans.lock().await;
        let plan = plans
            .get(id)
            .ok_or_else(|| format!("unknown plan_id: {id}"))?
            .clone();
        drop(plans);
        let version = args["plan_version"].as_u64().unwrap_or(plan.version);
        // Immutable revision: unknown versions must fail, never silently fall
        // back to latest (delegate already uses read_stored_plan_body).
        let _body = read_stored_plan_body(&plan, version)?;
        let path = plan
            .revision_paths
            .get(&version)
            .cloned()
            .or_else(|| {
                (version == plan.version && !plan.path.as_os_str().is_empty())
                    .then(|| plan.path.clone())
            })
            .ok_or_else(|| format!("unknown plan_version: {version} for plan {id}"))?;
        let metadata = plan_ref_value(id, version, &plan.title, Some(&path));
        let instruction = build_initial_prompt_sync(
            "",
            Some(PlanPromptRef {
                title: &plan.title,
                path: &path,
                plan_id: id,
                plan_version: version,
            }),
            &[],
        );
        Ok(Some((instruction, metadata)))
    }

    pub(super) async fn tool_session_context(
        &self,
        args: &Value,
    ) -> Result<(String, Option<Value>), String> {
        let id = args["session_id"]
            .as_str()
            .ok_or("session_id is required")?;
        let record = self
            .transcript
            .find_session_by_prefix(id)
            .map_err(|e| e.to_string())?
            .ok_or("unknown session_id")?;
        let messages = self
            .transcript
            .load_messages(&record.session_id)
            .map_err(|e| e.to_string())?;
        let parsed = super::super::messages_from_records(&messages);
        let offset = args["offset"].as_u64().unwrap_or(0) as usize;
        let limit = args["limit"].as_u64().unwrap_or(10).clamp(1, 50) as usize;
        let entries: Vec<Value> = parsed
            .iter()
            .rev()
            .filter(|m| matches!(m.role.as_str(), "user" | "assistant"))
            .filter_map(|m| {
                let text = m
                    .content
                    .iter()
                    .filter_map(|c| match c {
                        kkagent_llm::ChatContent::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                (!text.is_empty()).then(|| json!({"role":m.role,"text":clip_chars(&text,8000)}))
            })
            .collect();
        let page: Vec<_> = entries.iter().skip(offset).take(limit).cloned().collect();
        let payload = json!({"session_id":record.session_id,"workspace":record.working_dir,"title":record.title,"messages":page,"order":"newest_first","next_offset":(offset+limit<entries.len()).then_some(offset+limit)});
        Ok((serde_json::to_string(&payload).unwrap(), Some(payload)))
    }
}

pub(super) async fn persist_task(store: &CollaborationStore, task: &McpTask) -> Result<(), String> {
    let run_dir = task.run_dir.lock().await.clone();
    let branch = task
        .worktree
        .lock()
        .await
        .as_ref()
        .map(|w| w.branch.clone());
    let record = TaskRecord {
        id: task.id.clone(),
        session_id: task.session_id.clone(),
        resume: task.resume,
        description: task.description.clone(),
        prompt: task.prompt.clone(),
        workspace: task.origin_workspace.clone(),
        run_dir,
        branch,
        base_commit: task
            .base_commit
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone(),
        plan_title: task.plan_title.clone(),
        plan: task.plan.clone(),
        plan_ref: task
            .plan_ref
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone(),
        phase: task.phase(),
        summary: task.summary_snapshot(),
        error: task.error.lock().unwrap_or_else(|e| e.into_inner()).clone(),
        review: task
            .review
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone(),
        reviewed_snapshot: task
            .reviewed_snapshot
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone(),
        event_sequence: task.event_sequence.load(Ordering::SeqCst),
        recent_events: task
            .recent_events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone(),
        progress: task
            .progress
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone(),
        via_standalone: task.via_standalone,
    };
    store.put(
        "task",
        &task.id,
        &serde_json::to_value(record).map_err(|e| e.to_string())?,
    )
}

pub(super) async fn resolve_revision(dir: &Path, revision: &str) -> Result<String, String> {
    let out = tokio::process::Command::new("git")
        .args([
            "rev-parse",
            "--verify",
            "--end-of-options",
            &format!("{revision}^{{commit}}"),
        ])
        .current_dir(dir)
        .output()
        .await
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(format!("invalid git revision: {revision}"));
    }
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_string())
}

pub(super) async fn inspect_diff(
    workspace: &Path,
    args: &Value,
) -> Result<(Vec<Value>, Option<Value>), String> {
    if args.get("head").is_some() && args.get("base").is_none() {
        return Err("base is required with head".into());
    }
    let mut base = resolve_revision(workspace, args["base"].as_str().unwrap_or("HEAD")).await?;
    let head = match args["head"].as_str() {
        Some(h) => Some(resolve_revision(workspace, h).await?),
        None => None,
    };
    if args["merge_base"].as_bool().unwrap_or(false) {
        let head = head.as_ref().ok_or("merge_base requires base and head")?;
        let out = tokio::process::Command::new("git")
            .args(["merge-base", &base, head])
            .current_dir(workspace)
            .output()
            .await
            .map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err("cannot find merge base".into());
        }
        base = String::from_utf8_lossy(&out.stdout).trim().to_string();
    }
    let mut cmd = tokio::process::Command::new("git");
    cmd.args([
        "--no-pager",
        "diff",
        "--no-ext-diff",
        "--no-textconv",
        "--no-color",
    ]);
    match args["format"].as_str().unwrap_or("patch") {
        "stat" => {
            cmd.arg("--stat");
        }
        "files" => {
            cmd.arg("--name-status");
        }
        "patch" => {}
        _ => return Err("format must be patch, stat or files".into()),
    }
    cmd.arg(&base);
    if let Some(h) = &head {
        cmd.arg(h);
    }
    cmd.arg("--");
    if let Some(path) = args["path"].as_str() {
        cmd.arg(path);
    }
    let out = cmd
        .current_dir(workspace)
        .output()
        .await
        .map_err(|e| e.to_string())?;
    if !out.status.success() {
        return Err(String::from_utf8_lossy(&out.stderr).into_owned());
    }
    let text = String::from_utf8_lossy(&out.stdout);
    let offset = args["offset"].as_u64().unwrap_or(0) as usize;
    let limit = args["limit"].as_u64().unwrap_or(400).clamp(1, 5000) as usize;
    let lines: Vec<_> = text.lines().collect();
    // `offset`/`limit` are line-based, but the page is also byte-capped
    // (MAX_INSPECT_BYTES). With a single diff line longer than the cap
    // (minified bundles, lockfiles), line-only accounting breaks the offset
    // contract: the skipped-looking line is dropped byte-wise and never
    // appears on any page. Split oversized lines into cap-sized chunks so
    // paging through next_offset is lossless.
    const CHUNK: usize = 64 * 1024;
    let mut expanded: Vec<&str> = Vec::with_capacity(lines.len());
    for line in &lines {
        if line.len() <= CHUNK {
            expanded.push(line);
        } else {
            let bytes = line.as_bytes();
            let mut start = 0;
            while start < bytes.len() {
                let mut end = (start + CHUNK).min(bytes.len());
                while end > start && !line.is_char_boundary(end) {
                    end -= 1;
                }
                expanded.push(&line[start..end]);
                start = end;
            }
        }
    }
    let mut page = String::new();
    let mut count = 0;
    for line in expanded.iter().skip(offset).take(limit) {
        if page.len() + line.len() + 1 > MAX_INSPECT_BYTES {
            break;
        }
        page.push_str(line);
        page.push('\n');
        count += 1;
    }
    // Forward progress: an oversized first line after byte-cap must still
    // consume one source unit so next_offset cannot equal offset forever.
    if count == 0 && offset < expanded.len() {
        let line = expanded[offset];
        let mut end = line.len().min(MAX_INSPECT_BYTES.saturating_sub(1));
        while end > 0 && !line.is_char_boundary(end) {
            end -= 1;
        }
        page.push_str(&line[..end]);
        if end < line.len() {
            page.push_str("\n…[truncated]");
        }
        page.push('\n');
        count = 1;
    }
    let more = offset + count < expanded.len();
    // Include the patch body in structuredContent — OpenAI hosts prefer it
    // over content[] when both are present.
    Ok((
        vec![json!({"type":"text","text":page.clone()})],
        Some(json!({
            "workspace": workspace,
            "kind": "diff",
            "base": base,
            "head": head,
            "truncated": more,
            "next_offset": more.then_some(offset + count),
            "total_lines": expanded.len(),
            "text": page,
        })),
    ))
}

impl McpServer {
    /// Guarantee a runner is serving `task`: spawn one iff the slot says the
    /// previous runner exited. The decision is taken under the slot lock —
    /// the same lock the runner holds for its final pre-exit instruction
    /// check — so exactly one of "the live runner consumes the instruction"
    /// and "a fresh runner is spawned to consume it" is true. This is what
    /// makes an accepted `continue_task` reliable; nothing keys on
    /// `AbortHandle::is_finished()` (asynchronously updated) or on notify
    /// delivery.
    pub(super) fn ensure_runner(self: &Arc<Self>, task: &Arc<McpTask>) -> bool {
        let mut slot = task.runner.lock().unwrap_or_else(|e| e.into_inner());
        if !slot.exited {
            return false;
        }
        slot.exited = false;
        slot.waiting = false;
        let ctx = RunnerCtx {
            config: self.config.clone(),
            web: self.web.clone(),
            queue: self.queue.clone(),
            transcript: self.transcript.clone(),
            store: self.store.clone(),
        };
        let handle = tokio::spawn(run_task(ctx, Arc::clone(task)));
        slot.abort = Some(handle.abort_handle());
        true
    }

    pub(super) async fn resume_task(&self, args: &Value) -> Result<Arc<McpTask>, String> {
        if args.get("task_id").is_some() {
            return Err("pass either session_id or task_id, not both".into());
        }
        if args.get("decision").is_some() {
            return Err("decision is only valid with task_id".into());
        }
        let id = args["session_id"]
            .as_str()
            .ok_or("session_id must be a string")?;
        if let Some(task) = self
            .tasks
            .lock()
            .await
            .values()
            .find(|t| t.session_id == id)
            .cloned()
        {
            return Ok(task);
        }
        let record = self
            .transcript
            .find_session_by_prefix(id)
            .map_err(|e| e.to_string())?
            .ok_or_else(|| format!("unknown session_id: {id}"))?;
        if let Some(task) = self
            .tasks
            .lock()
            .await
            .values()
            .find(|t| t.session_id == record.session_id)
            .cloned()
        {
            return Ok(task);
        }
        let workspace = std::fs::canonicalize(&record.working_dir)
            .map_err(|e| format!("session workspace unavailable: {e}"))?;
        if !is_trusted(&self.config, &workspace) {
            return Err(format!(
                "{} is not a trusted workspace",
                workspace.display()
            ));
        }
        let task = TaskRecord {
            id: uuid::Uuid::new_v4().to_string(),
            session_id: record.session_id,
            resume: true,
            description: record.title.unwrap_or_else(|| "Resumed session".into()),
            prompt: String::new(),
            workspace: workspace.clone(),
            run_dir: workspace.clone(),
            branch: None,
            base_commit: git_head(&workspace).await,
            plan_title: None,
            plan: None,
            plan_ref: None,
            phase: TaskPhase::Completed,
            summary: TaskSummary::default(),
            error: None,
            review: "pending".into(),
            reviewed_snapshot: None,
            event_sequence: 0,
            recent_events: Vec::new(),
            progress: Progress::default(),
            // Historical sessions live on the shared server when MCP is attached.
            via_standalone: self.rpc.is_some(),
        }
        .into_task();
        let task = Arc::new(task);
        self.persist_task(&task).await?;
        self.tasks
            .lock()
            .await
            .insert(task.id.clone(), task.clone());
        Ok(task)
    }
}

pub(super) fn extend_definitions(definitions: &mut Vec<Value>) {
    for tool in definitions.iter_mut() {
        let name = tool["name"].as_str().unwrap().to_string();
        if matches!(
            name.as_str(),
            "delegate" | "continue_task" | "write_plan" | "cancel"
        ) {
            tool["inputSchema"]["properties"]["request_id"] = json!({"type":"string","description":"Unique retry key; reuse only with exactly the same arguments"});
        }
        if matches!(name.as_str(), "delegate" | "continue_task") {
            tool["inputSchema"]["properties"]["plan_id"] = json!({"type":"string","description":"Apply an orchestrator plan explicitly to this execution"});
            tool["inputSchema"]["properties"]["plan_version"] = json!({"type":"integer","minimum":1,"description":"Immutable plan version; defaults to latest"});
        }
        match name.as_str() {
            "inspect" => {
                let p = &mut tool["inputSchema"]["properties"];
                p["base"] = json!({"type":"string","description":"Diff base revision; defaults to HEAD against the worktree"});
                p["head"] = json!({"type":"string","description":"Compare two commits; requires base. Omit to compare with the worktree"});
                p["merge_base"] = json!({"type":"boolean","description":"Compare head against its merge base with base"});
                p["format"] = json!({"type":"string","enum":["patch","stat","files"]});
            }
            "continue_task" => {
                tool["description"]=json!("Continue an existing task or resume a historical session with its messages and checkpoints. Supply exactly one of task_id/session_id. Return immediately and poll get_progress. decision answers a pending question/permission on task_id; plan_id applies an explicit plan revision. review records acceptance or requests changes without launching a turn; inspect get_result first.");
                tool["inputSchema"]["properties"]["session_id"] = json!({"type":"string","description":"Historical session id or unique prefix; returns task_id for subsequent calls"});
                tool["inputSchema"]
                    .as_object_mut()
                    .unwrap()
                    .remove("required");
                tool["inputSchema"]["oneOf"] = json!([{"required":["task_id"],"not":{"required":["session_id"]}},{"required":["session_id"],"not":{"required":["task_id"]}}]);
                tool["inputSchema"]["properties"]["review"] = json!({"type":"object","properties":{"accepted":{"type":"boolean"},"head":{"type":"string","description":"Reviewed commit SHA from get_result"},"snapshot":{"type":"string","description":"Change detector from get_result, including uncommitted and untracked files"}},"required":["accepted","head","snapshot"],"additionalProperties":false});
                tool["inputSchema"]["properties"]["decision"]["properties"]["approval_id"] =
                    json!({"type":"string","description":"Expected pending approval id"});
            }
            "get_progress" => {
                tool["inputSchema"]["properties"]["after_event"] = json!({"type":"integer","minimum":0,"description":"Return only newer recent events; events_lost signals a retention gap"});
            }
            "write_plan" => {
                tool["description"]=json!("Persist a versioned orchestrator plan. Reusing plan_id creates an immutable new version. get_plan reads any version. delegate/continue_task explicitly apply a selected version; editing never changes running work implicitly.");
            }
            _ => {}
        }
    }
    for (name,description,properties,required) in [
        ("Glob","Find files using kkagent's built-in Glob; honors ignore rules and workspace confinement. Narrow pattern/path if results are truncated.",json!({"workspace":{"type":"string"},"pattern":{"type":"string"},"path":{"type":"string"},"limit":{"type":"integer","minimum":1,"maximum":2000}}),json!(["pattern"])),
        ("Grep","Search code using kkagent's built-in Grep (ripgrep). Returns content with line numbers; offset and limit page the results.",json!({"workspace":{"type":"string"},"pattern":{"type":"string"},"path":{"type":"string"},"glob":{"type":"string"},"case_insensitive":{"type":"boolean"},"context":{"type":"integer","minimum":0,"maximum":20},"offset":{"type":"integer","minimum":0},"limit":{"type":"integer","minimum":1,"maximum":2000}}),json!(["pattern"])),
        ("get_plan","Read a persisted plan version; omitted plan_version selects the latest.",json!({"plan_id":{"type":"string"},"plan_version":{"type":"integer","minimum":1}}),json!(["plan_id"])),
        ("get_session_context","Read bounded user/assistant text from a historical session, newest first. Tool transcripts and thinking are not included.",json!({"session_id":{"type":"string"},"offset":{"type":"integer","minimum":0},"limit":{"type":"integer","minimum":1,"maximum":50}}),json!(["session_id"]))
    ] {
        definitions.push(json!({"name":name,"description":description,"inputSchema":{"type":"object","properties":properties,"required":required,"additionalProperties":false},"annotations":{"readOnlyHint":true,"destructiveHint":false,"openWorldHint":false}}));
    }
}

/// A change detector, not an authentication token. Include untracked file bytes
/// because git diff alone omits newly created source files.
pub(super) async fn review_snapshot(dir: &Path) -> Result<String, String> {
    let dir = dir.to_path_buf();
    tokio::task::spawn_blocking(move || {
        use std::hash::{Hash, Hasher};
        use std::io::Read;
        let mut hash = std::collections::hash_map::DefaultHasher::new();
        for args in [
            vec!["rev-parse", "HEAD"],
            vec![
                "diff",
                "--no-ext-diff",
                "--no-textconv",
                "--binary",
                "HEAD",
                "--",
            ],
            vec![
                "diff",
                "--cached",
                "--no-ext-diff",
                "--no-textconv",
                "--binary",
                "HEAD",
                "--",
            ],
        ] {
            let out = std::process::Command::new("git")
                .args(args)
                .current_dir(&dir)
                .output()
                .map_err(|e| e.to_string())?;
            if !out.status.success() {
                return Err("review snapshots require a git repository with a commit".into());
            }
            out.stdout.hash(&mut hash);
        }
        let out = std::process::Command::new("git")
            .args(["ls-files", "--others", "--exclude-standard", "-z"])
            .current_dir(&dir)
            .output()
            .map_err(|e| e.to_string())?;
        if !out.status.success() {
            return Err("cannot list untracked files".into());
        }
        for raw in out.stdout.split(|b| *b == 0).filter(|p| !p.is_empty()) {
            raw.hash(&mut hash);
            #[cfg(unix)]
            let path = {
                use std::os::unix::ffi::OsStrExt;
                dir.join(std::ffi::OsStr::from_bytes(raw))
            };
            #[cfg(not(unix))]
            let path = dir.join(std::str::from_utf8(raw).map_err(|e| e.to_string())?);
            if std::fs::symlink_metadata(&path)
                .map_err(|e| e.to_string())?
                .file_type()
                .is_symlink()
            {
                std::fs::read_link(path)
                    .map_err(|e| e.to_string())?
                    .hash(&mut hash);
                continue;
            }
            let mut file = std::fs::File::open(path).map_err(|e| e.to_string())?;
            let mut buffer = [0u8; 65536];
            loop {
                let size = file.read(&mut buffer).map_err(|e| e.to_string())?;
                if size == 0 {
                    break;
                }
                hash.write(&buffer[..size]);
            }
        }
        Ok(format!("{:016x}", hash.finish()))
    })
    .await
    .map_err(|e| e.to_string())?
}

#[cfg(test)]
mod tests {
    use super::*;

    fn server_at(dir: &Path, db: TranscriptDb) -> Arc<McpServer> {
        Arc::new(
            McpServer::new(
                Arc::new(kkagent_config::AppConfig {
                    trusted_workspaces: vec![dir.to_string_lossy().into_owned()],
                    ..Default::default()
                }),
                db,
                None,
            )
            .unwrap(),
        )
    }
    async fn call(server: &Arc<McpServer>, name: &str, args: Value) -> Value {
        server
            .tools_call(&json!({"name":name,"arguments":args}))
            .await
            .unwrap()
    }
    fn git(dir: &Path, args: &[&str]) -> String {
        let out = std::process::Command::new("git")
            .args(args)
            .current_dir(dir)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().into()
    }
    fn repo() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        git(dir.path(), &["init"]);
        git(dir.path(), &["config", "user.name", "test"]);
        git(dir.path(), &["config", "user.email", "test@example.com"]);
        std::fs::write(dir.path().join("code.rs"), "fn before() {}\n").unwrap();
        git(dir.path(), &["add", "."]);
        git(dir.path(), &["commit", "-m", "base"]);
        dir
    }

    #[tokio::test]
    async fn versioned_plans_and_retry_receipts_survive_restart() {
        let dir = tempfile::tempdir().unwrap();
        let db = TranscriptDb::open(&dir.path().join("state.db")).unwrap();
        let server = server_at(dir.path(), db.clone());
        let original = call(
            &server,
            "write_plan",
            json!({"plan":"first","request_id":"r1"}),
        )
        .await;
        let again = call(
            &server,
            "write_plan",
            json!({"plan":"first","request_id":"r1"}),
        )
        .await;
        assert_eq!(original, again);
        let id = &original["structuredContent"]["plan_id"];
        let second = call(&server, "write_plan", json!({"plan_id":id,"plan":"second"})).await;
        assert_eq!(second["structuredContent"]["plan_version"], 2);
        drop(server);
        let server = server_at(dir.path(), db);
        assert_eq!(
            call(
                &server,
                "write_plan",
                json!({"plan":"first","request_id":"r1"})
            )
            .await,
            original
        );
        let old = call(&server, "get_plan", json!({"plan_id":id,"plan_version":1})).await;
        assert_eq!(old["content"][0]["text"], "first");
        let latest = call(&server, "get_plan", json!({"plan_id":id})).await;
        assert_eq!(latest["content"][0]["text"], "second");
        assert!(server
            .tools_call(
                &json!({"name":"write_plan","arguments":{"plan":"different","request_id":"r1"}})
            )
            .await
            .is_err());
    }

    #[tokio::test]
    async fn resume_reuses_session_and_recovers_task_metadata() {
        let dir = tempfile::tempdir().unwrap();
        let db = TranscriptDb::open_in_memory().unwrap();
        db.create_session(
            "previous-session",
            "old-model",
            dir.path().to_str().unwrap(),
        )
        .unwrap();
        db.append_message(
            "previous-session",
            "user",
            r#"[{"type":"text","text":"previous objective"}]"#,
            None,
        )
        .unwrap();
        let server = server_at(dir.path(), db.clone());
        let permits = server
            .queue
            .clone()
            .acquire_many_owned(server.queue.available_permits() as u32)
            .await
            .unwrap();
        let resumed = call(
            &server,
            "continue_task",
            json!({"session_id":"previous","instruction":"continue","request_id":"continue-1"}),
        )
        .await;
        assert_eq!(resumed["isError"], false, "{resumed}");
        assert_eq!(
            resumed["structuredContent"]["session_id"],
            "previous-session"
        );
        let id = resumed["structuredContent"]["task_id"].as_str().unwrap();
        let task = server.require_task(&json!({"task_id":id})).await.unwrap();
        assert!(same_dir(task.run_dir.lock().await.as_path(), dir.path()));
        assert_eq!(server.tasks.lock().await.len(), 1);
        assert_eq!(
            server
                .transcript
                .load_messages("previous-session")
                .unwrap()
                .len(),
            1
        );
        let repeated = call(
            &server,
            "continue_task",
            json!({"session_id":"previous","instruction":"continue","request_id":"continue-1"}),
        )
        .await;
        assert_eq!(repeated, resumed);
        task.abort_runner();
        drop(permits);
        let restored = server_at(dir.path(), db);
        let task = restored.require_task(&json!({"task_id":id})).await.unwrap();
        assert_eq!(task.session_id, "previous-session");
        assert_eq!(task.phase(), TaskPhase::Failed);
        assert!(task
            .error
            .lock()
            .unwrap()
            .as_ref()
            .unwrap()
            .contains("restarted"));
        let context = call(
            &restored,
            "get_session_context",
            json!({"session_id":"previous"}),
        )
        .await;
        assert_eq!(
            context["structuredContent"]["messages"][0]["text"],
            "previous objective"
        );
    }

    #[tokio::test]
    async fn diffs_compare_commits_and_page_without_losing_lines() {
        let _subprocess = crate::mcp_serve::SUBPROCESS_TEST_LOCK.lock().await;
        let dir = repo();
        let base = git(dir.path(), &["rev-parse", "HEAD"]);
        std::fs::write(dir.path().join("code.rs"), "fn after() {}\n").unwrap();
        git(dir.path(), &["commit", "-am", "change"]);
        let head = git(dir.path(), &["rev-parse", "HEAD"]);
        let server = server_at(dir.path(), TranscriptDb::open_in_memory().unwrap());
        let first = call(
            &server,
            "inspect",
            json!({"kind":"diff","base":base,"head":head,"limit":2}),
        )
        .await;
        assert_eq!(first["isError"], false);
        assert_eq!(first["structuredContent"]["next_offset"], 2);
        let rest = call(
            &server,
            "inspect",
            json!({"kind":"diff","base":base,"head":head,"offset":2}),
        )
        .await;
        assert!(rest["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("+fn after"));
        let invalid = call(
            &server,
            "inspect",
            json!({"kind":"diff","base":"--output=/tmp/nope","head":"HEAD"}),
        )
        .await;
        assert_eq!(invalid["isError"], true);
        let default = call(&server, "inspect", json!({"kind":"diff"})).await;
        assert_eq!(default["content"][0]["text"], "");
    }

    /// The inspect tool schema must advertise base/head/merge_base/format
    /// (the delegation API relies on `inspect(kind=diff, base, head)` to
    /// review commits without shelling out to git), and git-revision
    /// arguments like `HEAD^` must resolve.
    #[tokio::test]
    async fn inspect_schema_exposes_base_head_and_accepts_revisions() {
        let _subprocess = crate::mcp_serve::SUBPROCESS_TEST_LOCK.lock().await;
        let dir = repo();
        std::fs::write(dir.path().join("code.rs"), "fn after() {}\n").unwrap();
        git(dir.path(), &["commit", "-am", "change"]);
        let server = server_at(dir.path(), TranscriptDb::open_in_memory().unwrap());
        let response = server
            .handle_message(r#"{"jsonrpc":"2.0","id":1,"method":"tools/list"}"#)
            .await
            .unwrap();
        let parsed: Value = serde_json::from_str(&response).unwrap();
        let inspect = parsed["result"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "inspect")
            .expect("inspect tool listed")
            .clone();
        for property in ["base", "head", "merge_base", "format"] {
            assert!(
                inspect["inputSchema"]["properties"].get(property).is_some(),
                "inspect schema missing `{property}`: {inspect}"
            );
        }
        // Review the latest commit exactly like an orchestrator would.
        let review = call(
            &server,
            "inspect",
            json!({"kind":"diff","base":"HEAD^","head":"HEAD"}),
        )
        .await;
        assert_eq!(review["isError"], false, "{review}");
        assert!(review["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("+fn after"));
        assert_eq!(review["structuredContent"]["kind"], "diff");
        assert!(review["structuredContent"]["text"]
            .as_str()
            .unwrap()
            .contains("+fn after"));
    }

    #[tokio::test]
    async fn built_in_search_is_confined_and_has_line_numbers() {
        let _subprocess = crate::mcp_serve::SUBPROCESS_TEST_LOCK.lock().await;
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("code.rs"), "fn searchable() {}\n").unwrap();
        let server = server_at(dir.path(), TranscriptDb::open_in_memory().unwrap());
        let glob = call(&server, "Glob", json!({"pattern":"*.rs"})).await;
        assert_eq!(glob["isError"], false);
        assert!(glob["content"][0]["text"]
            .as_str()
            .unwrap()
            .contains("code.rs"));
        // Metadata-only structuredContent would hide the match list on OpenAI
        // hosts that drop content[] when structuredContent is set.
        assert!(glob.get("structuredContent").is_none());
        if std::process::Command::new("rg")
            .arg("--version")
            .output()
            .is_ok()
        {
            let grep = call(&server, "Grep", json!({"pattern":"searchable"})).await;
            assert_eq!(grep["isError"], false);
            assert!(grep["content"][0]["text"].as_str().unwrap().contains("1:"));
            assert!(grep.get("structuredContent").is_none());
        }
        let escape = call(&server, "Glob", json!({"pattern":"*","path":".."})).await;
        assert_eq!(escape["isError"], true);
    }

    #[tokio::test]
    async fn review_snapshot_detects_uncommitted_and_untracked_edits() {
        let _subprocess = crate::mcp_serve::SUBPROCESS_TEST_LOCK.lock().await;
        let dir = repo();
        let initial = review_snapshot(dir.path()).await.unwrap();
        std::fs::write(dir.path().join("new.rs"), "new code").unwrap();
        let added = review_snapshot(dir.path()).await.unwrap();
        assert_ne!(initial, added);
        std::fs::write(dir.path().join("new.rs"), "changed code").unwrap();
        assert_ne!(added, review_snapshot(dir.path()).await.unwrap());
        std::fs::remove_file(dir.path().join("new.rs")).unwrap();
        assert_eq!(initial, review_snapshot(dir.path()).await.unwrap());
        std::fs::write(dir.path().join("code.rs"), "changed tracked\n").unwrap();
        assert_ne!(initial, review_snapshot(dir.path()).await.unwrap());
    }
    #[tokio::test]
    async fn resumed_worker_sends_history_and_releases_slot_between_turns() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel::<Value>();
        let app=axum::Router::new().fallback(axum::routing::post(move |axum::Json(body):axum::Json<Value>| {
            let tx=tx.clone(); async move {
                tx.send(body).unwrap();
                ([("content-type","text/event-stream")], "data: {\"choices\":[{\"delta\":{\"content\":\"worker completed\"}}]}\n\ndata: [DONE]\n\n")
            }
        }));
        let http = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let dir = tempfile::tempdir().unwrap();
        let db = TranscriptDb::open_in_memory().unwrap();
        let id = format!("mcp-test-{}", uuid::Uuid::new_v4());
        db.create_session(&id, "test/model", dir.path().to_str().unwrap())
            .unwrap();
        db.append_message(
            &id,
            "user",
            r#"[{"type":"text","text":"original-session-objective"}]"#,
            None,
        )
        .unwrap();
        let mut config = kkagent_config::AppConfig {
            trusted_workspaces: vec![dir.path().to_string_lossy().into()],
            default_model: Some("test/model".into()),
            ..Default::default()
        };
        config.providers.insert("test".into(),serde_json::from_value(json!({"type":"openai-chat","api_key":"test","base_url":format!("http://{addr}"),"request_timeout_ms":5000})).unwrap());
        config.models.insert("test/model".into(),serde_json::from_value(json!({"provider":"test","model":"test-model","max_context_size":128000,"max_output_size":1000,"capabilities":["tool_use"]})).unwrap());
        let server = Arc::new(McpServer::new(Arc::new(config), db.clone(), None).unwrap());
        let slots = server.queue.available_permits();
        let result = call(
            &server,
            "continue_task",
            json!({"session_id":id,"instruction":"first-followup"}),
        )
        .await;
        assert_eq!(result["isError"], false, "{result}");
        let task = server
            .require_task(&json!({"task_id":result["structuredContent"]["task_id"]}))
            .await
            .unwrap();
        for (turn, instruction) in [(1, "first-followup"), (2, "second-followup")] {
            if turn == 2 {
                call(
                    &server,
                    "continue_task",
                    json!({"task_id":task.id,"instruction":instruction}),
                )
                .await;
            }
            let request = tokio::time::timeout(std::time::Duration::from_secs(60), rx.recv())
                .await
                .unwrap()
                .unwrap();
            let messages = request["messages"].to_string();
            assert!(
                messages.contains("original-session-objective"),
                "{messages}"
            );
            assert!(messages.contains(instruction), "{messages}");
            if turn == 2 {
                assert!(messages.contains("worker completed"));
            }
            tokio::time::timeout(std::time::Duration::from_secs(60), async {
                while !task.phase().terminal() {
                    tokio::time::sleep(std::time::Duration::from_millis(10)).await;
                }
            })
            .await
            .unwrap();
            assert_eq!(
                task.phase(),
                TaskPhase::Completed,
                "{:?}",
                task.error.lock().unwrap()
            );
            assert_eq!(server.queue.available_permits(), slots);
        }
        let records = db.load_messages(&id).unwrap();
        assert!(records
            .iter()
            .any(|r| r.role == "assistant" && r.content_json.contains("worker completed")));
        task.abort_runner();
        http.abort();
        let _ = kkagent_core::session::store::SessionStore::open_default().delete(&id);
    }

    /// Regression for the lost-wakeup race between a turn completing and an
    /// immediately following continue_task: every accepted instruction must
    /// eventually reach the model, no matter how the timing interleaves
    /// (runner parked, runner exiting, runner gone). Loops to shake out
    /// scheduling-dependent windows.
    #[tokio::test]
    async fn continue_right_after_completion_is_never_lost() {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel::<Value>();
        let app = axum::Router::new().fallback(
            axum::routing::post(move |axum::Json(body): axum::Json<Value>| {
                let tx = tx.clone();
                async move {
                    tx.send(body).unwrap();
                    (
                        [("content-type", "text/event-stream")],
                        "data: {\"choices\":[{\"delta\":{\"content\":\"worker completed\"}}]}\n\ndata: [DONE]\n\n",
                    )
                }
            }),
        );
        let http = tokio::spawn(async move {
            axum::serve(listener, app).await.unwrap();
        });
        let dir = tempfile::tempdir().unwrap();
        let db = TranscriptDb::open_in_memory().unwrap();
        let id = format!("mcp-test-{}", uuid::Uuid::new_v4());
        db.create_session(&id, "test/model", dir.path().to_str().unwrap())
            .unwrap();
        db.append_message(
            &id,
            "user",
            r#"[{"type":"text","text":"original-session-objective"}]"#,
            None,
        )
        .unwrap();
        let mut config = kkagent_config::AppConfig {
            trusted_workspaces: vec![dir.path().to_string_lossy().into()],
            default_model: Some("test/model".into()),
            ..Default::default()
        };
        config.providers.insert("test".into(),serde_json::from_value(json!({"type":"openai-chat","api_key":"test","base_url":format!("http://{addr}"),"request_timeout_ms":5000})).unwrap());
        config.models.insert("test/model".into(),serde_json::from_value(json!({"provider":"test","model":"test-model","max_context_size":128000,"max_output_size":1000,"capabilities":["tool_use"]})).unwrap());
        let server = Arc::new(McpServer::new(Arc::new(config), db.clone(), None).unwrap());
        let first = call(
            &server,
            "continue_task",
            json!({"session_id":id,"instruction":"seed-turn"}),
        )
        .await;
        assert_eq!(first["isError"], false, "{first}");
        let task = server
            .require_task(&json!({"task_id":first["structuredContent"]["task_id"]}))
            .await
            .unwrap();
        // Consume the seed turn's model request before racing continuations.
        let seed_request = tokio::time::timeout(std::time::Duration::from_secs(60), rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(
            seed_request["messages"].to_string().contains("seed-turn"),
            "{seed_request}"
        );

        const ROUNDS: usize = 12;
        for round in 0..ROUNDS {
            let instruction = format!("followup-{round}");
            // Wait for terminal, then continue IMMEDIATELY — the race this
            // test exists for.
            tokio::time::timeout(std::time::Duration::from_secs(60), async {
                while !task.phase().terminal() {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                }
            })
            .await
            .unwrap();
            assert_eq!(task.phase(), TaskPhase::Completed);
            let accepted = call(
                &server,
                "continue_task",
                json!({"task_id":task.id,"instruction":instruction}),
            )
            .await;
            assert_eq!(accepted["isError"], false, "{accepted}");
            // The accepted instruction must be consumed: exactly one more
            // model request carrying it arrives.
            let request = tokio::time::timeout(std::time::Duration::from_secs(60), rx.recv())
                .await
                .unwrap()
                .unwrap();
            let messages = request["messages"].to_string();
            assert!(
                messages.contains(&instruction),
                "round {round}: accepted instruction never consumed: {messages}"
            );
            // And it is not executed twice.
            let duplicate =
                tokio::time::timeout(std::time::Duration::from_millis(300), rx.recv()).await;
            assert!(
                duplicate.is_err(),
                "round {round}: instruction executed twice: {:?}",
                duplicate
            );
        }
        // Everything consumed: queue is empty and the runner is gone.
        assert!(task.pending_instructions.lock().unwrap().is_empty());
        task.abort_runner();
        tokio::time::timeout(std::time::Duration::from_secs(5), async {
            while !task.runner_state().eq("exited") {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap();
        http.abort();
        let _ = kkagent_core::session::store::SessionStore::open_default().delete(&id);
    }
}
