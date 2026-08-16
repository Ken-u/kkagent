//! Append-only audit trail for security-relevant decisions.
//!
//! Records permission verdicts (approve/ask/deny and their source), user
//! approval responses, and sandbox fallbacks to `~/.kkagent/audit.jsonl` so
//! post-hoc questions — "why did the agent touch this file?" — can be
//! answered without guessing. One JSON object per line; readers tolerate
//! trailing partial lines (crash mid-write).

use serde::Serialize;
use std::io::Write;
use std::path::PathBuf;

#[derive(Debug, Clone, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AuditEvent<'a> {
    /// A permission-chain verdict for a tool call.
    PermissionVerdict {
        at: &'a str,
        session_id: &'a str,
        tool: &'a str,
        verdict: &'a str,
        /// Which chain step produced the verdict (rule index, mode, …).
        source: &'a str,
        detail: &'a str,
        permission_mode: &'a str,
    },
    /// The user's response to an approval request.
    ApprovalResponse {
        at: &'a str,
        session_id: &'a str,
        tool: &'a str,
        response: &'a str,
        scope: &'a str,
    },
    /// Sandbox could not reach its configured mode.
    SandboxFallback {
        at: &'a str,
        configured: &'a str,
        effective: &'a str,
        reason: &'a str,
    },
}

pub fn audit_path() -> PathBuf {
    kkagent_config::default_config_dir().join("audit.jsonl")
}

/// Append one event; failures are logged and swallowed — auditing must never
/// break the operation it observes.
///
/// Test builds never write: `cargo test` would otherwise litter the real
/// `~/.kkagent/audit.jsonl` with events from unit tests.
pub fn record(event: &AuditEvent<'_>) {
    if cfg!(test) {
        return;
    }
    if let Err(error) = record_impl(event) {
        tracing::warn!("audit write failed: {error:#}");
    }
}

fn record_impl(event: &AuditEvent<'_>) -> anyhow::Result<()> {
    let path = audit_path();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let line = serde_json::to_string(event)?;
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Best-effort tighten: only applies to freshly created files.
        let _ = std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o600));
    }
    writeln!(file, "{line}")?;
    Ok(())
}

pub fn now_rfc3339() -> String {
    chrono::Utc::now().to_rfc3339()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audit_line_roundtrips() {
        let event = AuditEvent::SandboxFallback {
            at: "2026-01-01T00:00:00Z",
            configured: "workspace",
            effective: "process",
            reason: "bwrap missing",
        };
        let line = serde_json::to_string(&event).unwrap();
        let value: serde_json::Value = serde_json::from_str(&line).unwrap();
        assert_eq!(value["kind"], "sandbox_fallback");
        assert_eq!(value["configured"], "workspace");
        assert_eq!(value["effective"], "process");
    }
}
