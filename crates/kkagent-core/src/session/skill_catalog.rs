//! Session skill catalog snapshot (names + descriptions for prompts).

use serde::{Deserialize, Serialize};
use std::sync::RwLock;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SkillCatalogEntry {
    pub name: String,
    pub description: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
}

#[derive(Default)]
pub struct SessionSkillCatalog {
    entries: RwLock<Vec<SkillCatalogEntry>>,
}

impl SessionSkillCatalog {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set(&self, entries: Vec<SkillCatalogEntry>) {
        *self.entries.write().unwrap_or_else(|e| e.into_inner()) = entries;
    }

    pub fn list(&self) -> Vec<SkillCatalogEntry> {
        self.entries.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn prompt_section(&self) -> String {
        let entries = self.list();
        if entries.is_empty() {
            return String::new();
        }
        let mut out = String::from("\n\n# Available skills\n\n");
        for e in entries {
            out.push_str(&format!("- `{}`: {}\n", e.name, e.description));
        }
        out
    }
}
