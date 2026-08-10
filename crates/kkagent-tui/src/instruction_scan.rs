//! Detect duplicate / conflicting project instruction files for `/context`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct InstructionSource {
    pub path: PathBuf,
    pub kind: String,
    pub readable: bool,
    pub effective: bool,
    pub note: Option<String>,
}

/// Scan common instruction filenames under `cwd` and mark conflicts.
pub fn scan_project_instructions(cwd: &Path) -> Vec<InstructionSource> {
    let candidates = [
        ("AGENTS.md", "agents"),
        ("AGENT.md", "agents"),
        ("CLAUDE.md", "agents"),
        (".kkagent/AGENTS.md", "agents"),
        ("README.agents.md", "agents"),
    ];
    let mut by_kind: HashMap<&str, Vec<PathBuf>> = HashMap::new();
    let mut out = Vec::new();
    for (rel, kind) in candidates {
        let path = cwd.join(rel);
        if !path.exists() {
            continue;
        }
        let readable = std::fs::read_to_string(&path).is_ok();
        by_kind.entry(kind).or_default().push(path.clone());
        out.push(InstructionSource {
            path,
            kind: kind.to_string(),
            readable,
            effective: false,
            note: if readable {
                None
            } else {
                Some("unreadable".into())
            },
        });
    }
    // First readable file per kind wins (cwd walk order above).
    for src in &mut out {
        if !src.readable {
            continue;
        }
        let first = by_kind
            .get(src.kind.as_str())
            .and_then(|v| v.first())
            .map(|p| p.as_path());
        if first == Some(src.path.as_path()) {
            src.effective = true;
        } else {
            src.note = Some("shadowed by earlier instruction file".into());
        }
    }
    // Mark duplicates of same basename content hash lightly.
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn marks_first_effective() {
        let dir = tempfile_dir();
        fs::write(dir.join("AGENTS.md"), "a").unwrap();
        fs::write(dir.join("CLAUDE.md"), "b").unwrap();
        let items = scan_project_instructions(&dir);
        assert!(items.iter().any(|i| i.effective));
        assert!(items.iter().any(|i| i.note.is_some()));
    }

    fn tempfile_dir() -> PathBuf {
        let p = std::env::temp_dir().join(format!("kkagent-instr-{}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        fs::create_dir_all(&p).unwrap();
        p
    }
}
