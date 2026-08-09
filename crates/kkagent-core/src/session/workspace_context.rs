//! Session workspace context — extra roots + trust + info.

use std::path::PathBuf;
use std::sync::RwLock;

#[derive(Debug, Clone, Default)]
pub struct WorkspaceInfo {
    pub root: PathBuf,
    pub trusted: bool,
    pub git_branch: Option<String>,
    pub extra_dirs: Vec<PathBuf>,
}

#[derive(Default)]
pub struct SessionWorkspaceContext {
    info: RwLock<WorkspaceInfo>,
}

impl SessionWorkspaceContext {
    pub fn new(root: PathBuf, trusted: bool) -> Self {
        Self {
            info: RwLock::new(WorkspaceInfo {
                root,
                trusted,
                git_branch: None,
                extra_dirs: Vec::new(),
            }),
        }
    }

    pub fn info(&self) -> WorkspaceInfo {
        self.info.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn add_dir(&self, path: PathBuf) {
        self.info
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .extra_dirs
            .push(path);
    }

    pub fn set_git_branch(&self, branch: Option<String>) {
        self.info
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .git_branch = branch;
    }

    pub fn set_trusted(&self, trusted: bool) {
        self.info.write().unwrap_or_else(|e| e.into_inner()).trusted = trusted;
    }
}
