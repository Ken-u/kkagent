//! Session log ring buffer (in-memory; optional file sink).

use std::path::Path;
use std::sync::RwLock;

#[derive(Debug, Clone)]
pub struct LogLine {
    pub at: i64,
    pub level: String,
    pub message: String,
}

#[derive(Default)]
pub struct SessionLogService {
    lines: RwLock<Vec<LogLine>>,
    max: usize,
}

impl SessionLogService {
    pub fn new() -> Self {
        Self {
            lines: RwLock::new(Vec::new()),
            max: 2000,
        }
    }

    pub fn info(&self, message: impl Into<String>) {
        self.push("info", message);
    }

    pub fn warn(&self, message: impl Into<String>) {
        self.push("warn", message);
    }

    pub fn error(&self, message: impl Into<String>) {
        self.push("error", message);
    }

    pub fn push(&self, level: &str, message: impl Into<String>) {
        let mut lines = self.lines.write().unwrap_or_else(|e| e.into_inner());
        lines.push(LogLine {
            at: chrono::Utc::now().timestamp_millis(),
            level: level.into(),
            message: message.into(),
        });
        if lines.len() > self.max {
            let drop_n = lines.len() - self.max;
            lines.drain(0..drop_n);
        }
    }

    pub fn recent(&self, n: usize) -> Vec<LogLine> {
        let lines = self.lines.read().unwrap_or_else(|e| e.into_inner());
        let start = lines.len().saturating_sub(n);
        lines[start..].to_vec()
    }

    pub fn flush_to_file(&self, session_dir: &Path) -> anyhow::Result<()> {
        let path = session_dir.join("logs").join("kkagent.log");
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        let mut out = String::new();
        for l in self.recent(self.max) {
            out.push_str(&format!("{} [{}] {}\n", l.at, l.level, l.message));
        }
        std::fs::write(path, out)?;
        Ok(())
    }
}
