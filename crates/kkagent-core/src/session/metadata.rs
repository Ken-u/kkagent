//! Typed session metadata — persisted as `state.json` (agent-core-v2 aligned).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

pub const SESSION_META_VERSION: u32 = 2;

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum AgentKind {
    #[default]
    Main,
    Sub,
    Independent,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Default)]
#[serde(rename_all = "camelCase")]
pub struct AgentMeta {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub homedir: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none", rename = "type")]
    pub kind: Option<AgentKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_from: Option<String>,
    #[serde(default, skip_serializing_if = "HashMap::is_empty")]
    pub labels: HashMap<String, String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub swarm_item: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum TurnReason {
    Completed,
    Cancelled,
    Failed,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SessionMeta {
    pub id: String,
    #[serde(default = "default_meta_version")]
    pub version: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default)]
    pub is_custom_title: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_prompt: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    #[serde(default)]
    pub archived: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub forked_from: Option<String>,
    #[serde(default)]
    pub agents: HashMap<String, AgentMeta>,
    #[serde(default)]
    pub custom: HashMap<String, serde_json::Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_turn_reason: Option<TurnReason>,
    /// kimi-compatible workDir field used by store summaries.
    #[serde(default, skip_serializing_if = "Option::is_none", alias = "workDir")]
    pub work_dir: Option<String>,
}

fn default_meta_version() -> u32 {
    SESSION_META_VERSION
}

impl SessionMeta {
    pub fn new(id: impl Into<String>, cwd: impl AsRef<Path>) -> Self {
        let now = chrono::Utc::now().timestamp_millis();
        let cwd_s = cwd.as_ref().to_string_lossy().into_owned();
        Self {
            id: id.into(),
            version: SESSION_META_VERSION,
            title: None,
            is_custom_title: false,
            last_prompt: None,
            created_at: now,
            updated_at: now,
            archived: false,
            cwd: Some(cwd_s.clone()),
            forked_from: None,
            agents: HashMap::new(),
            custom: HashMap::new(),
            last_turn_reason: None,
            work_dir: Some(cwd_s),
        }
    }

    pub fn normalize(mut self, session_id: &str) -> Self {
        self.id = session_id.to_string();
        if self.version == 0 {
            self.version = SESSION_META_VERSION;
        }
        self
    }
}

#[derive(Debug, Clone, Default)]
pub struct SessionMetaPatch {
    pub title: Option<Option<String>>,
    pub is_custom_title: Option<bool>,
    pub last_prompt: Option<Option<String>>,
    pub archived: Option<bool>,
    pub cwd: Option<Option<String>>,
    pub forked_from: Option<Option<String>>,
    pub agents: Option<HashMap<String, AgentMeta>>,
    pub custom: Option<HashMap<String, serde_json::Value>>,
    pub last_turn_reason: Option<Option<TurnReason>>,
    pub work_dir: Option<Option<String>>,
}

/// In-memory + atomic `state.json` document for one session.
#[derive(Debug)]
pub struct SessionMetadataService {
    path: PathBuf,
    data: SessionMeta,
}

impl SessionMetadataService {
    pub fn create_new(session_dir: &Path, id: &str, cwd: &Path) -> anyhow::Result<Self> {
        std::fs::create_dir_all(session_dir)?;
        let path = session_dir.join("state.json");
        let data = SessionMeta::new(id, cwd);
        let svc = Self { path, data };
        svc.persist()?;
        Ok(svc)
    }

    pub fn load_or_create(session_dir: &Path, id: &str, cwd: &Path) -> anyhow::Result<Self> {
        let path = session_dir.join("state.json");
        if path.exists() {
            let text = std::fs::read_to_string(&path)?;
            let data: SessionMeta = serde_json::from_str(&text)?;
            Ok(Self {
                path,
                data: data.normalize(id),
            })
        } else {
            Self::create_new(session_dir, id, cwd)
        }
    }

    pub fn read(&self) -> &SessionMeta {
        &self.data
    }

    pub fn update(
        &mut self,
        patch: SessionMetaPatch,
        touch_updated_at: bool,
    ) -> anyhow::Result<()> {
        if let Some(v) = patch.title {
            self.data.title = v;
        }
        if let Some(v) = patch.is_custom_title {
            self.data.is_custom_title = v;
        }
        if let Some(v) = patch.last_prompt {
            self.data.last_prompt = v;
        }
        if let Some(v) = patch.archived {
            self.data.archived = v;
        }
        if let Some(v) = patch.cwd {
            self.data.cwd = v.clone();
            if self.data.work_dir.is_none() {
                self.data.work_dir = v;
            }
        }
        if let Some(v) = patch.work_dir {
            self.data.work_dir = v;
        }
        if let Some(v) = patch.forked_from {
            self.data.forked_from = v;
        }
        if let Some(v) = patch.agents {
            self.data.agents = v;
        }
        if let Some(v) = patch.custom {
            self.data.custom = v;
        }
        if let Some(v) = patch.last_turn_reason {
            self.data.last_turn_reason = v;
        }
        if touch_updated_at {
            self.data.updated_at = chrono::Utc::now().timestamp_millis();
        }
        self.persist()
    }

    pub fn set_title(&mut self, title: impl Into<String>) -> anyhow::Result<()> {
        self.update(
            SessionMetaPatch {
                title: Some(Some(title.into())),
                is_custom_title: Some(true),
                ..Default::default()
            },
            true,
        )
    }

    pub fn set_archived(&mut self, archived: bool) -> anyhow::Result<()> {
        self.update(
            SessionMetaPatch {
                archived: Some(archived),
                ..Default::default()
            },
            true,
        )
    }

    pub fn register_agent(&mut self, agent_id: &str, meta: AgentMeta) -> anyhow::Result<bool> {
        if self.data.agents.get(agent_id) == Some(&meta) {
            return Ok(false);
        }
        self.data.agents.insert(agent_id.to_string(), meta);
        self.data.updated_at = chrono::Utc::now().timestamp_millis();
        self.persist()?;
        Ok(true)
    }

    pub fn set_last_prompt(&mut self, prompt: impl Into<String>) -> anyhow::Result<()> {
        let p = prompt.into();
        let short: String = p.chars().take(200).collect();
        self.update(
            SessionMetaPatch {
                last_prompt: Some(Some(short)),
                ..Default::default()
            },
            true,
        )
    }

    pub fn set_last_turn_reason(&mut self, reason: TurnReason) -> anyhow::Result<()> {
        self.update(
            SessionMetaPatch {
                last_turn_reason: Some(Some(reason)),
                ..Default::default()
            },
            true,
        )
    }

    fn persist(&self) -> anyhow::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let tmp = self.path.with_extension("json.tmp");
        let text = serde_json::to_string_pretty(&self.data)?;
        std::fs::write(&tmp, text)?;
        std::fs::rename(&tmp, &self.path)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_state_json() {
        let dir = std::env::temp_dir().join(format!("kkagent-meta-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut svc = SessionMetadataService::create_new(&dir, "abc", Path::new("/w")).unwrap();
        svc.set_title("hello").unwrap();
        let loaded = SessionMetadataService::load_or_create(&dir, "abc", Path::new("/w")).unwrap();
        assert_eq!(loaded.read().title.as_deref(), Some("hello"));
        assert!(loaded.read().is_custom_title);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
