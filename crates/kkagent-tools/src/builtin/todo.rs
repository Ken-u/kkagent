use crate::{Tool, ToolContext, ToolOutput};
use async_trait::async_trait;
use serde::Deserialize;
use serde_json::{json, Value};
use std::{collections::HashSet, sync::Mutex};

pub struct TodoListTool {
    todos: Mutex<Vec<TodoItem>>,
}

#[derive(Debug, Clone)]
struct TodoItem {
    id: String,
    title: String,
    status: String,
}

#[derive(Deserialize)]
struct TodoWrite {
    id: Option<String>,
    #[serde(alias = "content")]
    title: String,
    #[serde(default = "pending_status")]
    status: String,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct TodoUpdate {
    id: Option<String>,
    title: Option<String>,
    status: Option<String>,
}

fn pending_status() -> String {
    "pending".into()
}

/// Canonical status for a model-written spelling (`completed`, `in-progress`,
/// `todo`, …), or `None` when it is not recognizable.
fn canonical_status(raw: &str) -> Option<&'static str> {
    let key = raw.trim().to_ascii_lowercase().replace(['-', ' '], "_");
    match key.as_str() {
        "pending" | "todo" | "to_do" | "open" | "not_started" | "queued" | "waiting" | "new"
        | "planned" => Some("pending"),
        "in_progress" | "inprogress" | "active" | "started" | "doing" | "running" | "working"
        | "current" | "wip" | "ongoing" => Some("in_progress"),
        "done" | "completed" | "complete" | "finished" | "success" | "succeeded" | "closed"
        | "resolved" => Some("done"),
        "cancelled" | "canceled" | "skipped" | "dropped" | "abandoned" | "obsolete" => {
            Some("cancelled")
        }
        // `completed_b48c39`: a status with a stray suffix still says "completed".
        k if k.starts_with("completed") || k.starts_with("done") => Some("done"),
        _ => None,
    }
}

/// Comparison key for titles: case, spacing and punctuation never matter.
fn title_key(title: &str) -> String {
    title
        .chars()
        .filter(|c| c.is_alphanumeric())
        .flat_map(char::to_lowercase)
        .collect()
}

/// `3`, `t3`, `#3`, `task-3` → 3. Models guess ordinal ids because ids are
/// opaque tokens that write results do not echo.
fn ordinal_id(id: &str) -> Option<usize> {
    let digits = id.trim_start_matches(|c: char| {
        c.is_ascii_alphabetic() || matches!(c, '#' | '-' | '_' | ' ' | ':')
    });
    if digits.is_empty() || !digits.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok().filter(|n| *n >= 1)
}

fn unfinished(item: &TodoItem) -> bool {
    !matches!(item.status.as_str(), "done" | "cancelled")
}

/// The task a model-written `title` refers to: exact text first, then the same
/// text modulo case/spacing/punctuation, then a unique containment (paraphrased
/// or truncated titles). Ties prefer unfinished tasks. `skip` excludes tasks
/// that are already spoken for.
fn find_by_title<'a>(
    todos: &'a [TodoItem],
    title: &str,
    skip: &dyn Fn(&TodoItem) -> bool,
) -> Option<&'a TodoItem> {
    let candidates: Vec<&TodoItem> = todos.iter().filter(|t| !skip(t)).collect();
    let first = |matches: &dyn Fn(&TodoItem) -> bool| {
        let found: Vec<&TodoItem> = candidates.iter().copied().filter(|t| matches(t)).collect();
        found
            .iter()
            .copied()
            .find(|t| unfinished(t))
            .or_else(|| found.first().copied())
    };
    if let Some(hit) = first(&|t| t.title == title) {
        return Some(hit);
    }
    let key = title_key(title);
    if key.is_empty() {
        return None;
    }
    if let Some(hit) = first(&|t| title_key(&t.title) == key) {
        return Some(hit);
    }
    let mut loose = candidates.iter().copied().filter(|t| {
        let other = title_key(&t.title);
        other.chars().count().min(key.chars().count()) >= 8
            && (other.contains(&key) || key.contains(&other))
    });
    match (loose.next(), loose.next()) {
        (Some(hit), None) => Some(hit),
        _ => None,
    }
}

/// A call's resulting list plus short notes for the model about anything the
/// tool adjusted on its behalf.
struct Applied {
    items: Vec<TodoItem>,
    notes: Vec<String>,
}

fn clip(title: &str) -> String {
    let mut out: String = title.chars().take(60).collect();
    if title.chars().count() > 60 {
        out.push('…');
    }
    out
}

/// At most one task is in_progress. `preferred` (the one this call just
/// started) wins; otherwise the last in list order does.
fn enforce_single_in_progress(
    items: &mut [TodoItem],
    preferred: Option<&str>,
    notes: &mut Vec<String>,
) {
    let active: Vec<usize> = (0..items.len())
        .filter(|i| items[*i].status == "in_progress")
        .collect();
    if active.len() < 2 {
        return;
    }
    let keep = preferred
        .and_then(|id| active.iter().copied().find(|i| items[*i].id == id))
        .unwrap_or(*active.last().unwrap());
    let mut demoted = 0;
    for i in active {
        if i != keep {
            items[i].status = "pending".into();
            demoted += 1;
        }
    }
    notes.push(format!(
        "Only one task can be in_progress: kept \"{}\", moved {demoted} other(s) back to pending.",
        clip(&items[keep].title)
    ));
}

/// 12 hex chars: unguessable (a model cannot fabricate an id that hits a real
/// task, so mistakes fail loudly instead of editing the wrong entry), but far
/// cheaper in tokens than a full UUID.
fn short_id() -> String {
    uuid::Uuid::new_v4().simple().to_string()[..12].to_string()
}

/// First non-colliding short id; the caller inserts the result into `taken`.
fn fresh_id(taken: &HashSet<String>) -> String {
    loop {
        let id = short_id();
        if !taken.contains(&id) {
            return id;
        }
    }
}

impl TodoListTool {
    pub fn new() -> Self {
        Self {
            todos: Mutex::new(Vec::new()),
        }
    }

    pub fn with_items(items: Vec<kkagent_protocol::TodoItemEvent>) -> Self {
        let mut taken = HashSet::new();
        let todos = items
            .into_iter()
            .filter_map(|item| {
                let title = item.content.trim().to_string();
                let keep = !item.id.trim().is_empty() && !taken.contains(&item.id);
                let id = if keep {
                    item.id.clone()
                } else {
                    fresh_id(&taken)
                };
                taken.insert(id.clone());
                (!title.is_empty()).then(|| TodoItem {
                    id,
                    title,
                    status: Self::normalize_status(&item.status),
                })
            })
            .collect();
        Self {
            todos: Mutex::new(todos),
        }
    }

    fn normalize_status(raw: &str) -> String {
        Self::parse_status(raw).unwrap_or("pending").into()
    }

    fn parse_status(raw: &str) -> Result<&'static str, String> {
        canonical_status(raw).ok_or_else(|| {
            format!("Unknown todo status: {raw}. Use pending, in_progress, done or cancelled.")
        })
    }

    fn display_status(status: &str) -> &'static str {
        match status {
            "done" => "completed",
            "in_progress" => "in_progress",
            "cancelled" => "cancelled",
            _ => "pending",
        }
    }

    fn items_event_json(todos: &[TodoItem]) -> Value {
        Value::Array(
            todos
                .iter()
                .map(|item| {
                    json!({
                        "id": item.id,
                        "content": item.title,
                        "status": Self::display_status(&item.status),
                    })
                })
                .collect(),
        )
    }

    fn render_list(todos: &[TodoItem]) -> String {
        if todos.is_empty() {
            return "Todo list is empty.".into();
        }
        let lines: Vec<String> = todos
            .iter()
            .map(|t| {
                let marker = match t.status.as_str() {
                    "done" => "[done]",
                    "in_progress" => "[in_progress]",
                    "cancelled" => "[cancelled]",
                    _ => "[pending]",
                };
                format!("  {} {} (id: {})", marker, t.title, t.id)
            })
            .collect();
        format!("Current todo list:\n{}", lines.join("\n"))
    }

    fn render_update(todos: &[TodoItem], notes: &[String]) -> String {
        if todos.is_empty() {
            return "Todo list cleared.".into();
        }
        let completed = todos.iter().filter(|t| t.status == "done").count();
        let pending = todos.iter().filter(|t| t.status == "pending").count();
        let cancelled = todos.iter().filter(|t| t.status == "cancelled").count();
        let mut active = todos.iter().filter(|t| t.status == "in_progress");
        let current = active.next();
        let active_count = usize::from(current.is_some()) + active.count();
        let focus = match (active_count, current) {
            (1, Some(item)) => format!(
                "Current task: {}\nFocus on this task and its necessary dependencies. \
Take the next concrete action once you have enough information; defer details of pending tasks. \
After completing and verifying this task, or identifying a blocker, update progress and choose the next unblocked task. \
Updates without id match an exact unique title first, otherwise act on this task.",
                item.title
            ),
            (0, _) if pending > 0 => {
                let candidate = todos.iter().find(|t| t.status == "pending").unwrap();
                let title = candidate.title.as_str();
                format!("No task is in_progress. Next pending candidate: {title}. \
Read TodoList for IDs before acting on a specific task; defer details of the other pending tasks.")
            }
            (0, _) => "No unfinished todo items remain. Check the result against the user's request before concluding.".into(),
            _ => "Multiple tasks are in_progress. Choose one current task and return the others to pending; only expand the current task and its necessary dependencies.".into(),
        };
        let notes: String = notes.iter().map(|n| format!("{n}\n")).collect();
        format!(
            "Todo list updated: {} total, {completed} done, {active_count} in_progress, {pending} pending, {cancelled} cancelled.\n{notes}{focus}\nCall TodoList with no arguments to read the full list when needed.",
            todos.len()
        )
    }

    /// Repair one `todos` / `updates` / `add` entry in place (see
    /// [`Tool::normalize_input`]). `seen` tracks ids already used by `todos`.
    fn normalize_item(
        kind: &str,
        item: &mut Value,
        todos: &[TodoItem],
        seen: &mut HashSet<String>,
    ) {
        if let Value::String(title) = item {
            if kind != "updates" {
                *item = json!({ "title": title.clone() });
            }
        }
        let Some(obj) = item.as_object_mut() else {
            return;
        };
        for alias in ["content", "text", "task", "name"] {
            if let Some(value) = obj.remove(alias) {
                obj.entry("title").or_insert(value);
            }
        }
        if obj.get("title").is_some_and(Value::is_null) {
            obj.remove("title");
        }
        let id = match obj.remove("id") {
            Some(Value::String(id)) => id.trim().to_string(),
            Some(Value::Number(id)) => id.to_string(),
            _ => String::new(),
        };
        if !id.is_empty() && kind != "add" {
            let known = todos.iter().any(|t| t.id == id);
            if known {
                // A full replacement names each task once.
                if kind != "todos" || seen.insert(id.clone()) {
                    obj.insert("id".into(), Value::String(id));
                }
            } else if kind == "updates" {
                let ordinal = ordinal_id(&id).filter(|n| *n <= todos.len());
                if let Some(n) = ordinal {
                    // `{"id": "3"}` means the third task; its title is the
                    // selector, not a rename.
                    obj.insert("id".into(), Value::String(todos[n - 1].id.clone()));
                    obj.remove("title");
                } else if !obj.contains_key("title") {
                    // Nothing else identifies the task: let the caller report it.
                    obj.insert("id".into(), Value::String(id));
                }
            }
        }
        match obj.get("status") {
            Some(Value::String(status)) => {
                if let Some(canonical) = canonical_status(status) {
                    obj.insert("status".into(), Value::String(canonical.into()));
                }
            }
            Some(_) => {
                obj.remove("status");
            }
            None => {}
        }
        if kind == "todos" && !obj.contains_key("status") {
            obj.insert("status".into(), Value::String("pending".into()));
        }
        obj.retain(|key, _| matches!(key.as_str(), "id" | "title" | "status"));
    }

    fn checked_title(title: &str) -> Result<String, String> {
        let title = title.trim();
        if title.is_empty() {
            return Err("Todo title must not be empty".into());
        }
        Ok(title.into())
    }

    /// `todos`: create or replace the whole list. Existing tasks keep their ids
    /// (named explicitly, else matched by title); unknown ids are ignored.
    fn replace(todos: &[TodoItem], value: &Value) -> Result<Applied, String> {
        let items: Vec<TodoWrite> =
            serde_json::from_value(value.clone()).map_err(|e| format!("Invalid todos: {e}"))?;
        let mut claimed: HashSet<String> = HashSet::new();
        let mut ids: Vec<Option<String>> = items
            .iter()
            .map(|item| {
                item.id
                    .clone()
                    .filter(|id| todos.iter().any(|t| &t.id == id) && claimed.insert(id.clone()))
            })
            .collect();
        let mut titles = Vec::with_capacity(items.len());
        for (item, id) in items.iter().zip(ids.iter_mut()) {
            let title = Self::checked_title(&item.title)?;
            if id.is_none() {
                if let Some(found) = find_by_title(todos, &title, &|t| claimed.contains(&t.id)) {
                    claimed.insert(found.id.clone());
                    *id = Some(found.id.clone());
                }
            }
            titles.push(title);
        }
        let mut taken: HashSet<String> = todos.iter().map(|t| t.id.clone()).collect();
        let mut out = Vec::with_capacity(items.len());
        for ((item, id), title) in items.into_iter().zip(ids).zip(titles) {
            let id = id.unwrap_or_else(|| {
                let id = fresh_id(&taken);
                taken.insert(id.clone());
                id
            });
            out.push(TodoItem {
                id,
                title,
                status: Self::parse_status(&item.status)?.into(),
            });
        }
        let mut notes = Vec::new();
        enforce_single_in_progress(&mut out, None, &mut notes);
        Ok(Applied { items: out, notes })
    }

    /// The task an update without id or title acts on: the (last) in_progress one.
    fn current_task_id(todos: &[TodoItem]) -> Result<String, String> {
        todos
            .iter()
            .rev()
            .find(|t| t.status == "in_progress")
            .map(|t| t.id.clone())
            .ok_or_else(|| {
                "No task is in_progress; an update without id or title targets the current task. Name the task by its exact title, or add it first."
                    .into()
            })
    }

    /// `updates` / `add`: incremental changes. Every target is resolved against
    /// the list as it arrived; the staged result replaces the list only if the
    /// whole call is valid.
    fn patch(todos: &[TodoItem], input: &Value) -> Result<Applied, String> {
        /// Where an update lands.
        enum Target {
            /// An existing task; `rename` when the update's title is the new title.
            Task { id: String, rename: bool },
            /// A title that matches nothing yet.
            Unmatched,
        }

        let mut next = todos.to_vec();
        let mut notes = Vec::new();
        let mut started: Option<String> = None;
        let mut taken: HashSet<String> = next.iter().map(|t| t.id.clone()).collect();
        if let Some(value) = input.get("updates") {
            let updates: Vec<TodoUpdate> = serde_json::from_value(value.clone())
                .map_err(|e| format!("Invalid updates: {e}"))?;
            // Pass 1: explicit ids and title matches.
            let mut targets = Vec::with_capacity(updates.len());
            for update in &updates {
                let id = update
                    .id
                    .as_deref()
                    .map(str::trim)
                    .filter(|s| !s.is_empty());
                let title = update
                    .title
                    .as_deref()
                    .map(Self::checked_title)
                    .transpose()?;
                if title.is_none() && update.status.is_none() {
                    return Err("Each update needs a title or status".into());
                }
                let target = match (id, &title) {
                    (Some(id), _) if todos.iter().any(|t| t.id == id) => {
                        Target::Task { id: id.into(), rename: title.is_some() }
                    }
                    (Some(id), None) => {
                        return Err(format!(
                            "Unknown todo id: {id}. Name the task by its exact title, or read TodoList for current IDs."
                        ))
                    }
                    (_, Some(title)) => match find_by_title(todos, title, &|_| false) {
                        Some(found) => Target::Task { id: found.id.clone(), rename: false },
                        None => Target::Unmatched,
                    },
                    (None, None) => {
                        Target::Task { id: Self::current_task_id(todos)?, rename: false }
                    }
                };
                targets.push(target);
            }
            // Pass 2: an unmatched title that is finishing something (done,
            // cancelled, or a bare rename) is taken to mean the current task,
            // unless another update already addresses it; everything else is
            // a new task.
            let targeted: HashSet<&str> = targets
                .iter()
                .filter_map(|t| match t {
                    Target::Task { id, .. } => Some(id.as_str()),
                    Target::Unmatched => None,
                })
                .collect();
            let mut current = todos
                .iter()
                .rev()
                .find(|t| t.status == "in_progress" && !targeted.contains(t.id.as_str()))
                .map(|t| t.id.clone());
            let mut resolved = Vec::with_capacity(targets.len());
            for (update, target) in updates.iter().zip(targets) {
                let finishing = match update.status.as_deref().map(Self::parse_status) {
                    None => true,
                    Some(status) => matches!(status?, "done" | "cancelled"),
                };
                resolved.push(match target {
                    Target::Unmatched if finishing && current.is_some() => Target::Task {
                        id: current.take().unwrap(),
                        rename: true,
                    },
                    other => other,
                });
            }
            for (update, target) in updates.into_iter().zip(resolved) {
                let status = update
                    .status
                    .as_deref()
                    .map(Self::parse_status)
                    .transpose()?;
                let id = match target {
                    Target::Task { id, rename } => {
                        let item = next
                            .iter_mut()
                            .find(|t| t.id == id)
                            .ok_or_else(|| format!("Unknown todo id: {id}."))?;
                        if let (true, Some(title)) = (rename, &update.title) {
                            item.title = Self::checked_title(title)?;
                        }
                        if let Some(status) = status {
                            item.status = status.into();
                        }
                        id
                    }
                    Target::Unmatched => {
                        // `checked_title` ran in pass 1; an unmatched target has a title.
                        let title = Self::checked_title(update.title.as_deref().unwrap_or(""))?;
                        let id = fresh_id(&taken);
                        taken.insert(id.clone());
                        notes.push(format!(
                            "Added new task \"{}\": no existing task has that title.",
                            clip(&title)
                        ));
                        next.push(TodoItem {
                            id: id.clone(),
                            title,
                            status: status.unwrap_or("pending").into(),
                        });
                        id
                    }
                };
                if status == Some("in_progress") {
                    started = Some(id);
                }
            }
        }
        if let Some(value) = input.get("add") {
            let items: Vec<TodoWrite> =
                serde_json::from_value(value.clone()).map_err(|e| format!("Invalid add: {e}"))?;
            for item in items {
                let id = fresh_id(&taken);
                taken.insert(id.clone());
                let status: &str = Self::parse_status(&item.status)?;
                if status == "in_progress" {
                    started = Some(id.clone());
                }
                next.push(TodoItem {
                    id,
                    title: Self::checked_title(&item.title)?,
                    status: status.into(),
                });
            }
        }
        enforce_single_in_progress(&mut next, started.as_deref(), &mut notes);
        Ok(Applied { items: next, notes })
    }
}

impl Default for TodoListTool {
    fn default() -> Self {
        Self::new()
    }
}

#[async_trait]
impl Tool for TodoListTool {
    fn name(&self) -> &str {
        "TodoList"
    }
    fn description(&self) -> &str {
        "Track progress on multi-step work. Create or revise the list by sending it whole with \
todos:[{title,status}] (status: pending|in_progress|done|cancelled; unchanged titles keep \
their identity; at most one in_progress; todos:[] clears). For a small change use \
updates:[{title,status}] naming an existing task by its exact title (no id needed; an \
unknown title adds a new task) or add:[{title}] to append. No arguments reads the list \
with ids. Do not mix todos with updates/add."
    }
    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "todos": {
                    "type": "array",
                    "description": "The complete list (create or revise). Unchanged titles keep their identity. Empty array clears.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": {"type": "string", "description": "Optional existing id from a read."},
                            "title": {"type": "string", "description": "Short actionable title"},
                            "status": {
                                "type": "string",
                                "enum": ["pending", "in_progress", "done", "cancelled"],
                                "description": "At most one in_progress; done after the task and its verification."
                            }
                        },
                        "required": ["title", "status"]
                    }
                },
                "updates": {
                    "type": "array",
                    "description": "Change existing tasks. Name a task by its exact title (or an id from a read); with neither, the current in_progress task is changed.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "id": {"type": "string", "description": "Optional id from a read."},
                            "title": {"type": "string", "description": "Exact existing title selects the task; an unknown title adds a new task (with an id: the new title)."},
                            "status": {"type": "string", "enum": ["pending", "in_progress", "done", "cancelled"]}
                        },
                        "additionalProperties": false
                    }
                },
                "add": {
                    "type": "array",
                    "description": "Append new tasks; status defaults to pending.",
                    "items": {
                        "type": "object",
                        "properties": {
                            "title": {"type": "string"},
                            "status": {"type": "string", "enum": ["pending", "in_progress", "done", "cancelled"]}
                        },
                        "required": ["title"]
                    }
                }
            }
        })
    }
    fn read_only(&self) -> bool {
        true
    }

    fn normalize_input(&self, input: Value) -> Value {
        let mut input = input;
        let Some(obj) = input.as_object_mut() else {
            return input;
        };
        let todos = self.todos.lock().unwrap();
        for key in ["todos", "updates", "add"] {
            let Some(slot) = obj.get_mut(key) else {
                continue;
            };
            // Some models send the array as a JSON string.
            if let Value::String(text) = slot {
                if let Ok(array @ Value::Array(_)) = serde_json::from_str::<Value>(text) {
                    *slot = array;
                }
            }
            let Value::Array(items) = slot else {
                continue;
            };
            let mut seen = HashSet::new();
            for item in items.iter_mut() {
                Self::normalize_item(key, item, &todos, &mut seen);
            }
        }
        input
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        let input = self.normalize_input(input);
        if !input.is_object() {
            return Ok(ToolOutput::error("TodoList arguments must be an object"));
        }
        let replace = input.get("todos");
        let patch = input.get("updates").is_some() || input.get("add").is_some();
        if (replace.is_some() && patch)
            || (input.get("action").is_some() && (replace.is_some() || patch))
        {
            return Ok(ToolOutput::error(
                "Use either todos, updates/add, or legacy action; do not mix these forms",
            ));
        }
        // Backward compatible: accept legacy `items`/`action` shape.
        if input.get("action").is_some() {
            return self.execute_legacy(input).await;
        }
        let mut todos = self.todos.lock().unwrap();
        let next = if let Some(value) = replace {
            Self::replace(&todos, value)
        } else if patch {
            Self::patch(&todos, &input)
        } else {
            return Ok(ToolOutput::success_with_data(
                Self::render_list(&todos),
                json!({ "items": Self::items_event_json(&todos) }),
            ));
        };
        let notes = match next {
            Ok(applied) => {
                *todos = applied.items;
                applied.notes
            }
            Err(error) => return Ok(ToolOutput::error(error)),
        };
        Ok(ToolOutput::success_with_data(
            Self::render_update(&todos, &notes),
            json!({ "items": Self::items_event_json(&todos) }),
        ))
    }
}

impl TodoListTool {
    async fn execute_legacy(&self, input: Value) -> anyhow::Result<ToolOutput> {
        let action = input
            .get("action")
            .and_then(|v| v.as_str())
            .unwrap_or("list");
        match action {
            "list" => {
                let todos = self.todos.lock().unwrap();
                Ok(ToolOutput::success_with_data(
                    Self::render_list(&todos),
                    json!({ "items": Self::items_event_json(&todos) }),
                ))
            }
            "set" => {
                let merge = input.get("merge").and_then(|v| v.as_bool()).unwrap_or(true);
                let new_items = input
                    .get("items")
                    .and_then(|v| v.as_array())
                    .cloned()
                    .unwrap_or_default();
                let mut todos = self.todos.lock().unwrap();
                let previous = if merge {
                    Vec::new()
                } else {
                    std::mem::take(&mut *todos)
                };
                for item_val in &new_items {
                    let title = item_val
                        .get("content")
                        .or_else(|| item_val.get("title"))
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let status = Self::normalize_status(
                        item_val
                            .get("status")
                            .and_then(|v| v.as_str())
                            .unwrap_or("pending"),
                    );
                    if title.is_empty() {
                        continue;
                    }
                    if let Some(existing) = todos.iter_mut().find(|t| t.title == title) {
                        existing.status = status;
                    } else {
                        let taken: HashSet<String> = todos.iter().map(|t| t.id.clone()).collect();
                        todos.push(TodoItem {
                            id: previous
                                .iter()
                                .find(|t| t.title == title)
                                .map(|t| t.id.clone())
                                .unwrap_or_else(|| fresh_id(&taken)),
                            title,
                            status,
                        });
                    }
                }
                Ok(ToolOutput::success_with_data(
                    Self::render_update(&todos, &[]),
                    json!({ "items": Self::items_event_json(&todos) }),
                ))
            }
            _ => Ok(ToolOutput::error(format!("Unknown action: {}", action))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn run(tool: &TodoListTool, input: Value) -> ToolOutput {
        let ctx = ToolContext {
            working_dir: std::env::temp_dir(),
            session_id: "todo-test".into(),
            turn_id: "todo-test:1".into(),
            plan_file_path: None,
            image: Default::default(),
            tool_call_id: None,
            interrupted: None,
            tools_config: Default::default(),
            model_alias: None,
        };
        tool.execute(input, &ctx).await.unwrap()
    }

    fn items(output: &ToolOutput) -> Vec<kkagent_protocol::TodoItemEvent> {
        serde_json::from_value(output.data.as_ref().unwrap()["items"].clone()).unwrap()
    }

    #[tokio::test]
    async fn incremental_updates_preserve_other_tasks_and_full_snapshot() {
        let tool = TodoListTool::new();
        let created = run(
            &tool,
            json!({"todos": [
                {"title": "Current work", "status": "in_progress"},
                {"title": "Future work", "status": "pending"},
                {"title": "Later work", "status": "pending"}
            ]}),
        )
        .await;
        let original = items(&created);
        assert!(created.content.contains("Current work"));
        // Write summaries no longer carry IDs; they appear only in reads.
        assert!(!created.content.contains(&original[0].id));
        assert!(!created.content.contains("Future work"));
        assert!(!created.content.contains("Later work"));

        let updated = run(
            &tool,
            json!({
                "updates": [
                    {"id": original[0].id, "status": "done"},
                    {"id": original[1].id, "status": "in_progress", "title": "Renamed work"}
                ],
                "add": [{"title": "New work"}]
            }),
        )
        .await;
        assert!(!updated.is_error);
        let after = items(&updated);
        assert_eq!(after.len(), 4);
        assert_eq!(after[0].status, "completed");
        assert_eq!(after[1].content, "Renamed work");
        assert_eq!(after[1].status, "in_progress");
        assert_eq!(after[2].content, "Later work");
        assert_eq!(after[2].status, "pending");
        for (before, after) in original.iter().zip(&after) {
            assert_eq!(before.id, after.id);
        }
        assert_eq!(after[3].status, "pending");
        assert!(after.iter().take(3).all(|t| t.id != after[3].id));
        assert!(updated.content.contains("Renamed work"));
        assert!(!updated.content.contains("Later work"));
        assert!(!updated.content.contains("New work"));

        let seeded = TodoListTool::with_items(after);
        let read = run(&seeded, json!({})).await;
        assert!(read.content.contains("Later work"));
        assert!(read.content.contains("New work"));
        let result = run(
            &seeded,
            json!({"updates": [{"id": original[2].id, "status": "cancelled"}]}),
        )
        .await;
        assert_eq!(items(&result)[2].status, "cancelled");
    }

    #[tokio::test]
    async fn idless_updates_act_on_the_current_task() {
        let tool = TodoListTool::new();
        let created = run(
            &tool,
            json!({"todos": [
                {"title": "First", "status": "in_progress"},
                {"title": "Second", "status": "pending"}
            ]}),
        )
        .await;
        let original = items(&created);
        // Current task's id never needs to be known: complete it and start
        // the next task in one call.
        let advanced = run(
            &tool,
            json!({"updates": [
                {"status": "done"},
                {"id": original[1].id, "status": "in_progress"}
            ]}),
        )
        .await;
        assert!(!advanced.is_error, "{}", advanced.content);
        let after = items(&advanced);
        assert_eq!(after[0].id, original[0].id);
        assert_eq!(after[0].status, "completed");
        assert_eq!(after[1].id, original[1].id);
        assert_eq!(after[1].status, "in_progress");
        assert!(advanced.content.contains("Second"));
        assert!(advanced
            .content
            .contains("Updates without id match an exact unique title first"));

        // A second title that matches nothing adds a task instead of
        // fighting over the current one.
        let two = run(
            &tool,
            json!({"updates": [{"status": "done"}, {"title": "Renamed"}]}),
        )
        .await;
        assert!(!two.is_error, "{}", two.content);
        assert!(two.content.contains("Added new task \"Renamed\""));
        let after = items(&two);
        assert_eq!(after.len(), 3);
        assert_eq!(after[1].status, "completed");
        assert_eq!(after[2].content, "Renamed");
        assert_eq!(after[2].status, "pending");
        // Nothing is in_progress now, so an idless update has no target.
        let no_current = run(&tool, json!({"updates": [{"status": "done"}]})).await;
        assert!(no_current.is_error);
        // Starting several tasks keeps the latest one in_progress; an idless
        // update then acts on it.
        let started = run(
            &tool,
            json!({"add": [
                {"title": "P1", "status": "in_progress"},
                {"title": "P2", "status": "in_progress"}
            ]}),
        )
        .await;
        let started = items(&started);
        assert_eq!(started[3].status, "pending");
        assert_eq!(started[4].status, "in_progress");
        let finished = run(&tool, json!({"updates": [{"status": "done"}]})).await;
        assert!(!finished.is_error, "{}", finished.content);
        assert_eq!(items(&finished)[4].status, "completed");
        assert_eq!(items(&run(&tool, json!({})).await).len(), 5);
    }

    #[tokio::test]
    async fn title_batches_advance_tasks_and_preserve_ids() {
        let tool = TodoListTool::new();
        let created = run(
            &tool,
            json!({"todos": [
                {"title": "审核 5 个 SSH 提交的完整 diff", "status": "in_progress"},
                {"title": "修复 SSH 并验证", "status": "pending"},
                {"title": "Later", "status": "pending"}
            ]}),
        )
        .await;
        let updated = run(
            &tool,
            json!({"updates": [
                {"title": "审核 5 个 SSH 提交的完整 diff", "status": "done"},
                {"title": "  修复 SSH 并验证  ", "status": "in_progress"}
            ]}),
        )
        .await;
        assert!(!updated.is_error, "{}", updated.content);
        let before = items(&created);
        let after = items(&updated);
        assert_eq!(after[0].status, "completed");
        assert_eq!(after[1].status, "in_progress");
        assert_eq!(after[2].content, before[2].content);
        assert_eq!(after[2].status, before[2].status);
        for (old, new) in before.iter().zip(&after) {
            assert_eq!(old.id, new.id);
        }
        // Targets are resolved before writes, even when the current task is renamed.
        let renamed = run(
            &tool,
            json!({"updates": [
                {"title": "修复中: run_ssh 收尾", "status": "done"},
                {"title": "Later", "status": "in_progress"}
            ]}),
        )
        .await;
        assert!(!renamed.is_error, "{}", renamed.content);
        assert_eq!(items(&renamed)[1].id, before[1].id);
        assert_eq!(items(&renamed)[1].content, "修复中: run_ssh 收尾");
        assert_eq!(items(&renamed)[2].status, "in_progress");
        // Exact titles also work with no current task.
        assert!(
            !run(
                &tool,
                json!({"updates": [{"title": "Later", "status": "done"}]})
            )
            .await
            .is_error
        );
        assert!(
            !run(
                &tool,
                json!({"updates": [{"title": "Later", "status": "pending"}]})
            )
            .await
            .is_error
        );
    }

    #[tokio::test]
    async fn duplicate_titles_pick_the_first_unfinished_and_ids_disambiguate() {
        let tool = TodoListTool::new();
        let created = run(
            &tool,
            json!({"todos": [
                {"title": "Current", "status": "in_progress"},
                {"title": "Same", "status": "pending"},
                {"title": "Same", "status": "pending"}
            ]}),
        )
        .await;
        let original = items(&created);
        // A bogus status still rejects the whole batch.
        let bogus = run(
            &tool,
            json!({"updates": [{"title": "Current", "status": "bogus"}]}),
        )
        .await;
        assert!(bogus.is_error);
        assert_eq!(run(&tool, json!({})).await.data, created.data);
        // By title, the first unfinished duplicate is the target.
        let by_title = run(
            &tool,
            json!({"updates": [
                {"title": "Current", "status": "done"},
                {"title": "Same", "status": "in_progress"}
            ]}),
        )
        .await;
        assert!(!by_title.is_error, "{}", by_title.content);
        assert_eq!(items(&by_title)[1].status, "in_progress");
        assert_eq!(items(&by_title)[2].status, "pending");
        // An id picks the other duplicate; only one task stays in_progress.
        let explicit = run(
            &tool,
            json!({"updates": [{"id": original[2].id, "title": "Same", "status": "in_progress"}]}),
        )
        .await;
        assert!(!explicit.is_error, "{}", explicit.content);
        assert_eq!(items(&explicit)[1].status, "pending");
        assert_eq!(items(&explicit)[2].status, "in_progress");
        assert!(explicit
            .content
            .contains("Only one task can be in_progress"));
    }

    #[tokio::test]
    async fn fresh_ids_are_short_random_hex_and_legacy_ids_survive() {
        let tool = TodoListTool::new();
        let added = run(
            &tool,
            json!({"add": [{"title": "A"}, {"title": "B"}, {"title": "C"}]}),
        )
        .await;
        let fresh = items(&added);
        assert_eq!(fresh.len(), 3);
        for item in &fresh {
            assert_eq!(item.id.len(), 12, "{}", item.id);
            assert!(item
                .id
                .chars()
                .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase()));
        }
        // Legacy persisted ids are kept as-is on seed.
        let seeded = TodoListTool::with_items(vec![kkagent_protocol::TodoItemEvent {
            id: "42".into(),
            content: "Legacy".into(),
            status: "in_progress".into(),
        }]);
        assert_eq!(seeded.todos.lock().unwrap()[0].id, "42");
        // A pre-seeded id colliding with a fresh short id would be lengthened;
        // dedup keeps ids unique regardless.
        let dup = TodoListTool::with_items(vec![
            kkagent_protocol::TodoItemEvent {
                id: "abc123def456".into(),
                content: "One".into(),
                status: "pending".into(),
            },
            kkagent_protocol::TodoItemEvent {
                id: "abc123def456".into(),
                content: "Two".into(),
                status: "pending".into(),
            },
        ]);
        let ids = dup.todos.lock().unwrap();
        assert_eq!(ids[0].id, "abc123def456");
        assert_ne!(ids[1].id, ids[0].id);
    }

    #[tokio::test]
    async fn invalid_batches_leave_the_entire_list_unchanged() {
        let tool = TodoListTool::new();
        let created = run(&tool, json!({"add": [{"title": "Keep"}]})).await;
        let id = &items(&created)[0].id;
        for invalid in [
            json!({"updates": [{"id": id, "status": "done"}, {"id": "missing", "status": "done"}]}),
            json!({"updates": [{"id": id, "status": "done"}], "add": [{"title": ""}]}),
            json!({"updates": [{"id": id, "status": "bogus"}]}),
            json!({"updates": [{"id": id, "title": " "}]}),
            json!({"updates": [{"id": id}]}),
            json!({"updates": null}),
            json!({"todos": [], "updates": []}),
            json!({"action": "set", "add": []}),
            json!({"todos": [{"title": "Bad", "status": "bogus"}]}),
            json!({"updates": [{"status": "done"}]}),
            json!({"updates": [{"status": "done"}, {"status": "pending"}]}),
            json!({"updates": [{"id": ""}, {"status": "pending"}]}),
        ] {
            let output = run(&tool, invalid.clone()).await;
            assert!(output.is_error, "accepted invalid input: {invalid}");
            assert_eq!(run(&tool, json!({})).await.data, created.data);
        }
    }

    #[tokio::test]
    async fn full_replacement_preserves_ids_across_reordering_and_renaming() {
        let tool = TodoListTool::new();
        let original = items(
            &run(
                &tool,
                json!({"todos": [
                    {"title": "First", "status": "pending"},
                    {"title": "Second", "status": "pending"}
                ]}),
            )
            .await,
        );
        let replaced = run(
            &tool,
            json!({"todos": [
                {"id": original[1].id, "title": "Renamed second", "status": "in_progress"},
                {"title": "First", "status": "pending"}
            ]}),
        )
        .await;
        assert_eq!(items(&replaced)[0].id, original[1].id);
        assert_eq!(items(&replaced)[1].id, original[0].id);
        let cleared = run(&tool, json!({"todos": []})).await;
        assert_eq!(cleared.content, "Todo list cleared.");
        assert!(items(&cleared).is_empty());
        assert!(
            run(
                &tool,
                json!({"updates": [{"id": original[0].id, "status": "done"}]})
            )
            .await
            .is_error
        );
    }

    #[tokio::test]
    async fn summaries_guide_selection_without_automatically_changing_status() {
        let tool = TodoListTool::new();
        let pending = run(
            &tool,
            json!({"add": [{"title": "Next"}, {"title": "Later"}]}),
        )
        .await;
        let initial = items(&pending);
        assert!(pending.content.contains("Next pending candidate: Next"));
        assert!(!pending.content.contains("Later"));
        assert!(initial.iter().all(|t| t.status == "pending"));
        let multiple = run(&tool, json!({"updates": initial.iter().map(|t| json!({"id": t.id, "status": "in_progress"})).collect::<Vec<_>>()})).await;
        // Only the task started last stays in_progress.
        assert!(multiple
            .content
            .contains("Only one task can be in_progress"));
        let started = items(&multiple);
        assert_eq!(started[0].status, "pending");
        assert_eq!(started[1].status, "in_progress");
        let finished = run(
            &tool,
            json!({"updates": [
                {"id": initial[0].id, "status": "done"},
                {"id": initial[1].id, "status": "cancelled"}
            ]}),
        )
        .await;
        assert!(finished
            .content
            .contains("1 done, 0 in_progress, 0 pending, 1 cancelled"));
        assert!(finished.content.contains("No unfinished todo items remain"));
    }

    #[tokio::test]
    async fn legacy_calls_still_merge_and_return_full_data_with_short_content() {
        let tool = TodoListTool::new();
        let first = run(
            &tool,
            json!({"action": "set", "items": [
                {"content": "Current", "status": "in_progress"},
                {"content": "Later", "status": "pending"}
            ]}),
        )
        .await;
        let second = run(
            &tool,
            json!({"action": "set", "items": [{"content": "Current", "status": "done"}]}),
        )
        .await;
        assert_eq!(items(&first)[0].id, items(&second)[0].id);
        assert_eq!(items(&second).len(), 2);
        assert_eq!(items(&second)[0].status, "completed");
        assert!(!first.content.contains("Later"));
        assert!(run(&tool, json!({"action": "list"}))
            .await
            .content
            .contains("Later"));
        let replaced = run(
            &tool,
            json!({"action": "set", "merge": false, "items": [
                {"content": "Later", "status": "in_progress"},
                {"content": "Current", "status": "done"}
            ]}),
        )
        .await;
        assert_eq!(items(&replaced)[0].id, items(&first)[1].id);
        assert_eq!(items(&replaced)[1].id, items(&first)[0].id);
    }

    #[tokio::test]
    async fn same_title_tasks_keep_distinct_ids_during_replacement() {
        let tool = TodoListTool::new();
        let initial = items(
            &run(
                &tool,
                json!({"add": [{"title": "Same"}, {"title": "Same"}]}),
            )
            .await,
        );
        assert_ne!(initial[0].id, initial[1].id);
        let replaced = run(
            &tool,
            json!({"todos": [
                {"title": "Same", "status": "pending"},
                {"id": initial[0].id, "title": "Renamed", "status": "in_progress"}
            ]}),
        )
        .await;
        assert!(!replaced.is_error);
        assert_eq!(items(&replaced)[0].id, initial[1].id);
        assert_eq!(items(&replaced)[1].id, initial[0].id);
    }

    /// What the agent loop does before running a call: repair, then validate.
    fn repaired(tool: &TodoListTool, input: Value) -> Value {
        let input = tool.normalize_input(input);
        crate::args_validator::validate_against_schema(&tool.parameters_schema(), &input)
            .unwrap_or_else(|e| panic!("{}\n{input}", e.message));
        input
    }

    #[test]
    fn model_slips_are_repaired_before_schema_validation() {
        let tool = TodoListTool::new();
        // Every shape below failed validation in real sessions.
        for input in [
            json!({"todos": [{"status": "completed", "title": "a"}, {"status": "in-progress", "title": "b"}]}),
            json!({"updates": [{"id": null, "status": "done", "title": "a"}, {"status": "in_progress", "title": "b"}]}),
            json!({"updates": [{"id": 1, "status": "done"}, {"id": 2, "status": "completed"}]}),
            json!({"updates": [{"status": "done", "title": "a", "title2": ""}]}),
            json!({"updates": [{"status": "completed_b48c39", "title": "a"}]}),
            json!({"updates": "[{\"status\": \"done\", \"title\": \"a\"}]"}),
            json!({"todos": [{"content": "a"}, {"title": "b"}]}),
            json!({"todos": ["a", "b"]}),
            json!({"add": [{"title": "a", "id": "invented", "status": "todo"}]}),
        ] {
            repaired(&tool, input);
        }
        // Repairs are idempotent.
        let once = tool.normalize_input(json!({"updates": [{"id": 3, "status": "completed"}]}));
        assert_eq!(tool.normalize_input(once.clone()), once);
    }

    #[test]
    fn status_spellings_map_to_the_four_states() {
        for (raw, want) in [
            ("completed", "done"),
            ("Complete", "done"),
            ("finished", "done"),
            ("in-progress", "in_progress"),
            ("In Progress", "in_progress"),
            ("doing", "in_progress"),
            ("todo", "pending"),
            ("not_started", "pending"),
            ("canceled", "cancelled"),
            ("skipped", "cancelled"),
        ] {
            assert_eq!(canonical_status(raw), Some(want), "{raw}");
        }
        assert_eq!(canonical_status("bogus"), None);
        assert_eq!(canonical_status(""), None);
    }

    #[tokio::test]
    async fn ordinal_and_label_ids_mean_list_positions() {
        let tool = TodoListTool::new();
        let created = items(
            &run(
                &tool,
                json!({"todos": [
                    {"title": "One", "status": "in_progress"},
                    {"title": "Two", "status": "pending"},
                    {"title": "Three", "status": "pending"}
                ]}),
            )
            .await,
        );
        // `1`, `t2` and `#3` name the first, second and third task; a title
        // sent next to an ordinal is not a rename.
        let advanced = run(
            &tool,
            json!({"updates": [
                {"id": "1", "status": "done"},
                {"id": "t2", "status": "in_progress", "title": "Two (renamed by mistake)"},
                {"id": 3, "status": "cancelled"}
            ]}),
        )
        .await;
        assert!(!advanced.is_error, "{}", advanced.content);
        let after = items(&advanced);
        assert_eq!(
            after.iter().map(|t| t.status.as_str()).collect::<Vec<_>>(),
            ["completed", "in_progress", "cancelled"]
        );
        assert_eq!(after[1].content, "Two");
        for (old, new) in created.iter().zip(&after) {
            assert_eq!(old.id, new.id);
        }
        // Out of range or without a way to resolve it: still an error.
        for bad in [
            json!({"updates": [{"id": "9", "status": "done"}]}),
            json!({"updates": [{"id": "verify-baseline", "status": "done"}]}),
        ] {
            let result = run(&tool, bad).await;
            assert!(result.is_error);
            assert!(
                result.content.contains("Unknown todo id"),
                "{}",
                result.content
            );
        }
        // An unknown id next to a title falls back to the title.
        let by_title = run(
            &tool,
            json!({"updates": [{"id": "13", "title": "Three", "status": "pending"}]}),
        )
        .await;
        assert!(!by_title.is_error, "{}", by_title.content);
        assert_eq!(items(&by_title)[2].id, created[2].id);
        assert_eq!(items(&by_title)[2].status, "pending");
    }

    #[tokio::test]
    async fn replacement_labels_are_not_ids() {
        let tool = TodoListTool::new();
        // First list written with invented labels `t1`..`t3`.
        let created = run(
            &tool,
            json!({"todos": [
                {"id": "t1", "status": "done", "title": "Alpha"},
                {"id": "t2", "status": "in_progress", "title": "Beta"},
                {"id": "t2", "status": "pending", "title": "Gamma"}
            ]}),
        )
        .await;
        assert!(!created.is_error, "{}", created.content);
        let created = items(&created);
        assert_eq!(created.len(), 3);
        assert!(created.iter().all(|t| t.id.len() == 12));
        // Rewriting the list with the same labels keeps the ids by title.
        let rewritten = run(
            &tool,
            json!({"todos": [
                {"id": "t1", "status": "done", "title": "Alpha"},
                {"id": "t2", "status": "done", "title": "Beta"},
                {"id": "t3", "status": "in_progress", "title": "Gamma"}
            ]}),
        )
        .await;
        let rewritten = items(&rewritten);
        for (old, new) in created.iter().zip(&rewritten) {
            assert_eq!(old.id, new.id);
        }
        assert_eq!(rewritten[2].status, "in_progress");
    }

    #[tokio::test]
    async fn duplicate_updates_for_one_task_merge_with_the_last_winning() {
        let tool = TodoListTool::new();
        let id = items(
            &run(
                &tool,
                json!({"add": [{"title": "Only", "status": "in_progress"}]}),
            )
            .await,
        )[0]
        .id
        .clone();
        let merged = run(
            &tool,
            json!({"updates": [
                {"id": id, "status": "done"},
                {"id": id, "status": "pending", "title": "Only (reopened)"},
                {"status": "in_progress"}
            ]}),
        )
        .await;
        assert!(!merged.is_error, "{}", merged.content);
        let after = items(&merged);
        assert_eq!(after.len(), 1);
        assert_eq!(after[0].content, "Only (reopened)");
        assert_eq!(after[0].status, "in_progress");
    }

    #[tokio::test]
    async fn mirrored_title_updates_follow_the_list_instead_of_failing() {
        let tool = TodoListTool::new();
        let created = items(
            &run(
                &tool,
                json!({"todos": [
                    {"title": "Read spec §4: acceptance criteria", "status": "in_progress"},
                    {"title": "Run focused tests", "status": "pending"},
                    {"title": "Report to the user", "status": "pending"}
                ]}),
            )
            .await,
        );
        // The models' habit: restate the list with each task's new status.
        // Case/punctuation differences and extended titles still match; the
        // finished task keeps its identity even when it is worded differently.
        let updated = run(
            &tool,
            json!({"updates": [
                {"status": "done", "title": "Read spec §4 and the acceptance list"},
                {"status": "in_progress", "title": "RUN focused tests!"},
                {"status": "pending", "title": "Report to the user: findings, risks and next steps"}
            ]}),
        )
        .await;
        assert!(!updated.is_error, "{}", updated.content);
        let after = items(&updated);
        assert_eq!(after.len(), 3, "no duplicates: {after:?}");
        assert_eq!(
            after.iter().map(|t| t.status.as_str()).collect::<Vec<_>>(),
            ["completed", "in_progress", "pending"]
        );
        for (old, new) in created.iter().zip(&after) {
            assert_eq!(old.id, new.id);
        }
        // Matching by title leaves the stored title alone; claiming the
        // current task for an unmatched finishing title renames it.
        assert_eq!(after[1].content, "Run focused tests");
        assert_eq!(after[0].content, "Read spec §4 and the acceptance list");
    }

    #[tokio::test]
    async fn unmatched_titles_add_tasks_and_a_new_task_takes_over_in_progress() {
        let tool = TodoListTool::new();
        let created = items(
            &run(
                &tool,
                json!({"todos": [
                    {"title": "Alpha", "status": "in_progress"},
                    {"title": "Beta", "status": "pending"}
                ]}),
            )
            .await,
        );
        // A pending title that matches nothing never hijacks the current task.
        let pending = run(
            &tool,
            json!({"updates": [{"status": "pending", "title": "Gamma"}]}),
        )
        .await;
        assert!(!pending.is_error, "{}", pending.content);
        assert!(pending.content.contains("Added new task \"Gamma\""));
        let after = items(&pending);
        assert_eq!(after.len(), 3);
        assert_eq!(after[0].content, "Alpha");
        assert_eq!(after[0].status, "in_progress");
        // Starting a new task moves the old current task back to pending.
        let started = run(
            &tool,
            json!({"updates": [{"status": "in_progress", "title": "Delta"}]}),
        )
        .await;
        let after = items(&started);
        assert_eq!(after.len(), 4);
        assert_eq!(after[0].status, "pending");
        assert_eq!(after[3].content, "Delta");
        assert_eq!(after[3].status, "in_progress");
        assert!(started.content.contains("Only one task can be in_progress"));
        assert_eq!(after[0].id, created[0].id);
    }

    #[tokio::test]
    async fn replacement_with_several_in_progress_keeps_the_last() {
        let tool = TodoListTool::new();
        let result = run(
            &tool,
            json!({"todos": [
                {"title": "A", "status": "in_progress"},
                {"title": "B", "status": "in_progress"},
                {"title": "C", "status": "pending"}
            ]}),
        )
        .await;
        let after = items(&result);
        assert_eq!(after[0].status, "pending");
        assert_eq!(after[1].status, "in_progress");
        assert!(result.content.contains("Only one task can be in_progress"));
    }

    #[test]
    fn seeded_items_preserve_progress_for_the_next_turn() {
        let tool = TodoListTool::with_items(vec![
            kkagent_protocol::TodoItemEvent {
                id: "todo-a".into(),
                content: "Finished".into(),
                status: "completed".into(),
            },
            kkagent_protocol::TodoItemEvent {
                id: "todo-b".into(),
                content: "Running".into(),
                status: "in_progress".into(),
            },
        ]);
        let todos = tool.todos.lock().unwrap();
        assert_eq!(todos.len(), 2);
        assert_eq!(todos[0].status, "done");
        assert_eq!(todos[0].id, "todo-a");
        assert_eq!(todos[1].id, "todo-b");
        assert_eq!(todos[1].status, "in_progress");
        assert!(TodoListTool::render_list(&todos).contains("[in_progress] Running"));
    }
}
