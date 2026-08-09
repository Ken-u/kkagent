//! `/init` — generate AGENTS.md scaffold for the workspace.

use std::path::Path;

const AGENTS_MD_TEMPLATE: &str = r#"# AGENTS.md

## Project overview

Describe the project purpose, primary languages, and how to build/test.

## Conventions

- Prefer minimal, targeted changes
- Match existing style
- Do not commit secrets

## Important paths

- …

## Commands

```bash
# build / test / lint
```
"#;

pub struct SessionInitService {
    cancelled: std::sync::atomic::AtomicBool,
}

impl Default for SessionInitService {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionInitService {
    pub fn new() -> Self {
        Self {
            cancelled: std::sync::atomic::AtomicBool::new(false),
        }
    }

    pub fn cancel_init(&self) {
        self.cancelled
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    /// Write AGENTS.md if missing. Returns path + whether created.
    pub async fn generate_agents_md(&self, cwd: &Path) -> anyhow::Result<(std::path::PathBuf, bool)> {
        if self.cancelled.load(std::sync::atomic::Ordering::SeqCst) {
            anyhow::bail!("init cancelled");
        }
        let path = cwd.join("AGENTS.md");
        if path.exists() {
            return Ok((path, false));
        }
        tokio::fs::write(&path, AGENTS_MD_TEMPLATE).await?;
        Ok((path, true))
    }

    pub fn reminder_after_init(content: &str) -> String {
        format!(
            "<system-reminder>\nAGENTS.md was generated/updated. Treat the following as project instructions:\n\n{content}\n</system-reminder>"
        )
    }
}
