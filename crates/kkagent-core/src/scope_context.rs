//! Agent scope context — working dir, session, permission, plan.

use std::path::PathBuf;

use kkagent_protocol::PermissionMode;

#[derive(Debug, Clone)]
pub struct ScopeContext {
    pub session_id: String,
    pub working_dir: PathBuf,
    pub permission_mode: PermissionMode,
    pub plan_mode: bool,
    pub model_alias: String,
    pub agent_name: String,
}

impl ScopeContext {
    pub fn main(
        session_id: impl Into<String>,
        working_dir: PathBuf,
        permission_mode: PermissionMode,
        plan_mode: bool,
        model_alias: impl Into<String>,
    ) -> Self {
        Self {
            session_id: session_id.into(),
            working_dir,
            permission_mode,
            plan_mode,
            model_alias: model_alias.into(),
            agent_name: "main".into(),
        }
    }

    pub fn child(&self, name: impl Into<String>) -> Self {
        let mut c = self.clone();
        c.agent_name = name.into();
        c
    }

    pub fn display_cwd(&self) -> String {
        self.working_dir.to_string_lossy().into_owned()
    }
}
