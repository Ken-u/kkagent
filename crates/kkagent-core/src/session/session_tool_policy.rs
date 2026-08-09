//! Session-wide client tool denylist (survives resume).

use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::path::Path;
use std::sync::RwLock;

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SessionToolPolicyDoc {
    #[serde(default)]
    pub disabled_tools: Vec<String>,
}

#[derive(Default)]
pub struct SessionToolPolicyService {
    disabled: RwLock<HashSet<String>>,
}

impl SessionToolPolicyService {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn load_from_dir(&self, session_dir: &Path) -> anyhow::Result<()> {
        let path = session_dir.join("tool-policy.json");
        if !path.exists() {
            return Ok(());
        }
        let text = std::fs::read_to_string(path)?;
        let doc: SessionToolPolicyDoc = serde_json::from_str(&text)?;
        self.set_disabled_tools(doc.disabled_tools);
        Ok(())
    }

    pub fn persist(&self, session_dir: &Path) -> anyhow::Result<()> {
        let doc = SessionToolPolicyDoc {
            disabled_tools: self.disabled_tools(),
        };
        std::fs::write(
            session_dir.join("tool-policy.json"),
            serde_json::to_string_pretty(&doc)?,
        )?;
        Ok(())
    }

    pub fn disabled_tools(&self) -> Vec<String> {
        let mut v: Vec<_> = self
            .disabled
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .collect();
        v.sort();
        v
    }

    pub fn set_disabled_tools(&self, names: impl IntoIterator<Item = String>) {
        *self.disabled.write().unwrap_or_else(|e| e.into_inner()) = names.into_iter().collect();
    }

    pub fn is_disabled(&self, name: &str) -> bool {
        self.disabled
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .contains(name)
    }
}

/// Gate that combines session denylist with profile enable-set.
#[derive(Default)]
pub struct SessionToolPolicyGate {
    pub session_policy: SessionToolPolicyService,
}

impl SessionToolPolicyGate {
    pub fn allows(&self, name: &str, enabled: Option<&HashSet<String>>) -> bool {
        if self.session_policy.is_disabled(name) {
            return false;
        }
        match enabled {
            None => true,
            Some(set) => set.contains(name),
        }
    }
}
