//! Session-shared todo list.

use serde::{Deserialize, Serialize};
use std::sync::RwLock;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum TodoStatus {
    Pending,
    InProgress,
    Done,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct TodoItem {
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

    pub fn set_todos(&self, todos: Vec<TodoItem>) {
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
        };
        lines.push(format!("  {marker} {}", t.title));
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
                _ => TodoStatus::Pending,
            };
            Some(TodoItem { title, status })
        })
        .collect()
}
