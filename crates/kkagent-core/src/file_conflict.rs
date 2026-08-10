//! Cross-session file touch tracking for write conflict warnings.

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

#[derive(Debug, Default)]
pub struct FileConflictTracker {
    /// canonical path → set of session ids that touched it
    inner: Mutex<HashMap<PathBuf, HashSet<String>>>,
}

impl FileConflictTracker {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn normalize(path: &Path, cwd: &Path) -> PathBuf {
        let p = if path.is_absolute() {
            path.to_path_buf()
        } else {
            cwd.join(path)
        };
        p.canonicalize().unwrap_or(p)
    }

    pub fn record_read(&self, session_id: &str, path: &Path, cwd: &Path) {
        self.record(session_id, path, cwd);
    }

    pub fn record_write(&self, session_id: &str, path: &Path, cwd: &Path) {
        self.record(session_id, path, cwd);
    }

    fn record(&self, session_id: &str, path: &Path, cwd: &Path) {
        let key = Self::normalize(path, cwd);
        if let Ok(mut map) = self.inner.lock() {
            map.entry(key).or_default().insert(session_id.to_string());
        }
    }

    pub fn clear_session(&self, session_id: &str) {
        if let Ok(mut map) = self.inner.lock() {
            for set in map.values_mut() {
                set.remove(session_id);
            }
            map.retain(|_, set| !set.is_empty());
        }
    }

    /// Other sessions that have already touched this path (excluding `session_id`).
    pub fn conflicts_for(
        &self,
        session_id: &str,
        path: &Path,
        cwd: &Path,
    ) -> Vec<String> {
        let key = Self::normalize(path, cwd);
        let Ok(map) = self.inner.lock() else {
            return Vec::new();
        };
        map.get(&key)
            .map(|set| {
                set.iter()
                    .filter(|id| id.as_str() != session_id)
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn detects_cross_session_touch() {
        let t = FileConflictTracker::new();
        let cwd = Path::new("/tmp");
        t.record_write("a", Path::new("foo.rs"), cwd);
        let c = t.conflicts_for("b", Path::new("foo.rs"), cwd);
        assert_eq!(c, vec!["a".to_string()]);
        assert!(t.conflicts_for("a", Path::new("foo.rs"), cwd).is_empty());
    }
}
