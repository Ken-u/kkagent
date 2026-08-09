//! Session process runner registry (background shell processes).

use std::collections::HashMap;
use std::sync::RwLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProcessStatus {
    Running,
    Exited,
    Killed,
}

#[derive(Debug, Clone)]
pub struct ProcessHandle {
    pub id: String,
    pub command: String,
    pub cwd: String,
    pub status: ProcessStatus,
    pub exit_code: Option<i32>,
}

#[derive(Default)]
pub struct SessionProcessRunner {
    procs: RwLock<HashMap<String, ProcessHandle>>,
}

impl SessionProcessRunner {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn register(&self, command: impl Into<String>, cwd: impl Into<String>) -> ProcessHandle {
        let h = ProcessHandle {
            id: uuid::Uuid::new_v4().to_string(),
            command: command.into(),
            cwd: cwd.into(),
            status: ProcessStatus::Running,
            exit_code: None,
        };
        self.procs
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(h.id.clone(), h.clone());
        h
    }

    pub fn mark_exited(&self, id: &str, code: i32) {
        if let Some(p) = self
            .procs
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(id)
        {
            p.status = ProcessStatus::Exited;
            p.exit_code = Some(code);
        }
    }

    pub fn kill(&self, id: &str) -> bool {
        let mut map = self.procs.write().unwrap_or_else(|e| e.into_inner());
        if let Some(p) = map.get_mut(id) {
            p.status = ProcessStatus::Killed;
            return true;
        }
        false
    }

    pub fn list(&self) -> Vec<ProcessHandle> {
        self.procs
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect()
    }
}
