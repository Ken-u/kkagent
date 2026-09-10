//! Session-shared todo list.

use serde::{Deserialize, Serialize};
use std::{collections::HashSet, sync::RwLock};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Done,
    Cancelled,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TodoItem {
    #[serde(default = "new_id")]
    pub id: String,
    pub title: String,
    pub status: TodoStatus,
}

#[derive(Default)]
pub struct SessionTodoService {
    todos: RwLock<Vec<TodoItem>>,
}

impl SessionTodoService {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get_todos(&self) -> Vec<TodoItem> {
        self.todos.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn set_todos(&self, mut todos: Vec<TodoItem>) {
        let mut ids = HashSet::new();
        for item in &mut todos {
            if item.id.trim().is_empty() || !ids.insert(item.id.clone()) {
                item.id = new_id();
                ids.insert(item.id.clone());
            }
        }
        *self.todos.write().unwrap_or_else(|e| e.into_inner()) = todos;
    }

    pub fn clear(&self) {
        self.todos
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clear();
    }

    pub fn render(&self) -> String {
        render_todo_list(&self.get_todos(), "Current todo list:")
    }
}

fn new_id() -> String {
    // 12 hex chars: unguessable by models but cheaper than a full UUID;
    // matches kkagent-tools TodoList id format.
    uuid::Uuid::new_v4().simple().to_string()[..12].to_string()
}

pub fn render_todo_list(todos: &[TodoItem], title: &str) -> String {
    if todos.is_empty() {
        return "Todo list is empty.".into();
    }
    let mut lines = vec![title.to_string()];
    for t in todos {
        let marker = match t.status {
            TodoStatus::Pending => "[pending]",
            TodoStatus::InProgress => "[in_progress]",
            TodoStatus::Done => "[done]",
            TodoStatus::Cancelled => "[cancelled]",
        };
        lines.push(format!("  {marker} {} (id: {})", t.title, t.id));
    }
    lines.join("\n")
}

pub fn parse_todo_items(raw: &serde_json::Value) -> Vec<TodoItem> {
    let Some(arr) = raw.as_array() else {
        return Vec::new();
    };
    arr.iter()
        .filter_map(|v| {
            let title = v.get("title")?.as_str()?.to_string();
            let status = match v.get("status").and_then(|s| s.as_str()) {
                Some("in_progress") | Some("in-progress") => TodoStatus::InProgress,
                Some("done") | Some("completed") => TodoStatus::Done,
                Some("cancelled") | Some("canceled") => TodoStatus::Cancelled,
                _ => TodoStatus::Pending,
            };
            let id = v
                .get("id")
                .and_then(|v| v.as_str())
                .filter(|id| !id.trim().is_empty())
                .map(str::to_string)
                .unwrap_or_else(new_id);
            Some(TodoItem { id, title, status })
        })
        .collect()
}
