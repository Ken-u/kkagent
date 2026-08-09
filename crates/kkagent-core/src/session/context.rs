//! Session context — seeded per-session facts (agent-core-v2 `sessionContext`).

use std::path::PathBuf;

/// Pure facts frozen at session creation — no store, no IO.
#[derive(Debug, Clone)]
pub struct SessionContext {
    pub session_id: String,
    pub workspace_id: String,
    pub session_dir: PathBuf,
    pub meta_scope: String,
    pub cwd: PathBuf,
    session_scope: String,
}

impl SessionContext {
    pub fn new(
        session_id: impl Into<String>,
        workspace_id: impl Into<String>,
        session_dir: PathBuf,
        cwd: PathBuf,
    ) -> Self {
        let session_id = session_id.into();
        let workspace_id = workspace_id.into();
        let session_scope = format!("sessions/{workspace_id}/{session_id}");
        Self {
            session_id,
            workspace_id,
            session_dir,
            meta_scope: session_scope.clone(),
            cwd,
            session_scope,
        }
    }

    /// Persistence scope, optionally with a child key (`agents/main/cron`).
    pub fn scope(&self, sub_key: Option<&str>) -> String {
        match sub_key {
            None | Some("") => self.session_scope.clone(),
            Some(k) => format!("{}/{}", self.session_scope, k.trim_start_matches('/')),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scope_child() {
        let ctx = SessionContext::new("s1", "wd_x", PathBuf::from("/tmp/s1"), PathBuf::from("/proj"));
        assert_eq!(ctx.scope(None), "sessions/wd_x/s1");
        assert_eq!(ctx.scope(Some("agents/main")), "sessions/wd_x/s1/agents/main");
    }
}
