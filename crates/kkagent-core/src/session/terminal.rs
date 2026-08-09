//! Session terminal registry (ACP / HTTP terminal bridge).

use std::collections::HashMap;
use std::sync::RwLock;

#[derive(Debug, Clone)]
pub struct TerminalHandle {
    pub id: String,
    pub title: String,
    pub cwd: String,
    pub pid: Option<u32>,
    pub alive: bool,
}

#[derive(Default)]
pub struct SessionTerminalService {
    terminals: RwLock<HashMap<String, TerminalHandle>>,
}

impl SessionTerminalService {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn open(&self, title: impl Into<String>, cwd: impl Into<String>) -> TerminalHandle {
        let h = TerminalHandle {
            id: uuid::Uuid::new_v4().to_string(),
            title: title.into(),
            cwd: cwd.into(),
            pid: None,
            alive: true,
        };
        self.terminals
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(h.id.clone(), h.clone());
        h
    }

    pub fn get(&self, id: &str) -> Option<TerminalHandle> {
        self.terminals
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .get(id)
            .cloned()
    }

    pub fn list(&self) -> Vec<TerminalHandle> {
        self.terminals
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .values()
            .cloned()
            .collect()
    }

    pub fn close(&self, id: &str) -> bool {
        let mut map = self.terminals.write().unwrap_or_else(|e| e.into_inner());
        if let Some(t) = map.get_mut(id) {
            t.alive = false;
            return true;
        }
        false
    }
}
