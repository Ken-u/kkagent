//! Session instructions provider — AGENTS.md / CLAUDE.md discovery.

use std::path::{Path, PathBuf};

const MAX_CHARS: usize = 80_000;

#[derive(Debug, Clone)]
pub struct InstructionFile {
    pub path: PathBuf,
    pub name: String,
    pub content: String,
    pub truncated: bool,
}

pub struct SessionInstructionsProvider;

impl SessionInstructionsProvider {
    pub async fn load(cwd: &Path) -> Option<InstructionFile> {
        let candidates = [
            cwd.join("AGENTS.md"),
            cwd.join(".kkagent").join("AGENTS.md"),
            cwd.join("CLAUDE.md"),
        ];
        for path in &candidates {
            let Ok(content) = tokio::fs::read_to_string(path).await else {
                continue;
            };
            let content = content.trim();
            if content.is_empty() {
                continue;
            }
            let truncated = content.chars().count() > MAX_CHARS;
            let body: String = content.chars().take(MAX_CHARS).collect();
            let name = path
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("instructions")
                .to_string();
            return Some(InstructionFile {
                path: path.clone(),
                name,
                content: body,
                truncated,
            });
        }
        None
    }

    pub fn format_for_system_prompt(file: &InstructionFile) -> String {
        let mut s = format!(
            "\n\n# Project instructions ({})\n\n{}",
            file.name, file.content
        );
        if file.truncated {
            s.push_str("\n\n… (truncated; open the file for the full text)");
        }
        s
    }
}
