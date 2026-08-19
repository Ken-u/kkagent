use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;

use crate::toolchain::{GrantAccess, GrantScope, ToolchainGrant, ToolchainGrantStore};
use crate::{Tool, ToolContext, ToolOutput};

/// Request a scoped read/read-write path grant for toolchain/sandbox use.
pub struct RequestToolchainAccessTool {
    grants: Arc<ToolchainGrantStore>,
    /// When false (process/disabled sandbox), the tool explains that grants are N/A.
    path_isolation_active: bool,
}

impl RequestToolchainAccessTool {
    pub fn new(grants: Arc<ToolchainGrantStore>, path_isolation_active: bool) -> Self {
        Self {
            grants,
            path_isolation_active,
        }
    }
}

#[async_trait]
impl Tool for RequestToolchainAccessTool {
    fn name(&self) -> &str {
        "RequestToolchainAccess"
    }

    fn description(&self) -> &str {
        "Request a scoped absolute-path grant for toolchain/sandbox access \
(read or read_write). Cannot modify sandbox policy itself. Use for precise \
runtime/cache paths that are not covered by built-in toolchain profiles."
    }

    fn disclosure(&self) -> crate::ToolDisclosure {
        crate::ToolDisclosure::Deferred
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "path": {
                    "type": "string",
                    "description": "Absolute path to grant (no ~, env vars, or globs)."
                },
                "access": {
                    "type": "string",
                    "enum": ["read", "read_write"],
                    "description": "Minimum access required."
                },
                "reason": {
                    "type": "string",
                    "description": "Why this path is needed."
                },
                "profile": {
                    "type": "string",
                    "description": "Optional toolchain profile name (rust/node/python/go/java)."
                },
                "scope": {
                    "type": "string",
                    "enum": ["once", "turn", "session", "workspace"],
                    "description": "How long the grant remains active. Default: session."
                }
            },
            "required": ["path", "access", "reason"],
            "additionalProperties": false
        })
    }

    fn read_only(&self) -> bool {
        false
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        if !self.path_isolation_active {
            return Ok(ToolOutput::error(
                "RequestToolchainAccess is only available when the effective sandbox \
mode provides filesystem path isolation (macOS/Linux workspace). \
Current mode does not enforce path mounts, so grants would be meaningless.",
            ));
        }

        let path = input
            .get("path")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        let access = input
            .get("access")
            .and_then(|v| v.as_str())
            .unwrap_or("read");
        let reason = input
            .get("reason")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        let profile = input
            .get("profile")
            .and_then(|v| v.as_str())
            .map(str::to_string);
        let scope = input
            .get("scope")
            .and_then(|v| v.as_str())
            .unwrap_or("session");

        if path.is_empty() || reason.is_empty() {
            return Ok(ToolOutput::error("path and reason are required"));
        }
        if let Some(err) = validate_grant_path(path) {
            return Ok(ToolOutput::error(err));
        }
        let access = match access {
            "read" => GrantAccess::Read,
            "read_write" => GrantAccess::ReadWrite,
            other => {
                return Ok(ToolOutput::error(format!(
                    "invalid access {other:?}; expected read or read_write"
                )))
            }
        };
        let scope = match scope {
            "once" => GrantScope::Once,
            "turn" => GrantScope::Turn,
            "session" => GrantScope::Session,
            "workspace" => GrantScope::Workspace,
            other => {
                return Ok(ToolOutput::error(format!(
                    "invalid scope {other:?}; expected once, turn, session, or workspace"
                )))
            }
        };

        let path_buf = PathBuf::from(path);
        self.grants.grant(ToolchainGrant {
            path: path_buf.clone(),
            access,
            reason: reason.to_string(),
            profile,
            scope,
            turn_id: if scope == GrantScope::Turn {
                Some(ctx.turn_id.clone())
            } else {
                None
            },
        });

        let scope_desc = match scope {
            GrantScope::Once => "the next Bash invocation only",
            GrantScope::Turn => "the current turn",
            GrantScope::Session => "this session",
            GrantScope::Workspace => "all sessions in this workspace",
        };

        Ok(ToolOutput::success(format!(
            "Granted {:?} access to `{}` for {}.\nReason: {reason}\n\
Subsequent Bash commands in workspace mode will mount this path.",
            access,
            path_buf.display(),
            scope_desc
        )))
    }
}

fn validate_grant_path(path: &str) -> Option<String> {
    if path.contains('~') || path.contains('$') || path.contains('*') || path.contains('?') {
        return Some("path must be a concrete absolute path without ~, env vars, or globs".into());
    }
    let p = PathBuf::from(path);
    if !p.is_absolute() {
        return Some("path must be absolute".into());
    }
    if path == "/" || path == "\\" {
        return Some("refusing to grant filesystem root".into());
    }
    if let Some(home) = dirs::home_dir() {
        if p == home {
            return Some("refusing to grant the entire user HOME".into());
        }
    }
    let lower = path.replace('\\', "/").to_ascii_lowercase();
    for sens in [
        "/.ssh",
        "/.gnupg",
        "/.aws",
        "/.gcp",
        "/.docker",
        "/.kube",
        "/.npmrc",
        "/.netrc",
        "/credentials",
    ] {
        if lower.contains(sens) {
            return Some(format!(
                "refusing sensitive path containing `{sens}`; use an explicit opt-in outside this tool"
            ));
        }
    }
    if path.split(['/', '\\']).any(|c| c == "..") {
        return Some("path must not contain `..` components".into());
    }
    None
}

/// Report toolchain profile health and cache usage.
pub struct ToolchainDoctorTool {
    config: kkagent_config::ToolchainConfig,
    grants: Arc<ToolchainGrantStore>,
}

impl ToolchainDoctorTool {
    pub fn new(config: kkagent_config::ToolchainConfig, grants: Arc<ToolchainGrantStore>) -> Self {
        Self { config, grants }
    }
}

#[async_trait]
impl Tool for ToolchainDoctorTool {
    fn name(&self) -> &str {
        "ToolchainDoctor"
    }

    fn description(&self) -> &str {
        "Diagnose toolchain sandbox profiles: availability, cache sizes, env injection, \
and current session grants. Does not print secrets."
    }

    fn disclosure(&self) -> crate::ToolDisclosure {
        crate::ToolDisclosure::Deferred
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {},
            "additionalProperties": false
        })
    }

    fn read_only(&self) -> bool {
        true
    }

    fn default_approve(&self) -> bool {
        true
    }

    async fn execute(&self, _input: Value, _ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        let report = crate::toolchain::doctor_report(&self.config, &self.grants.snapshot());
        Ok(ToolOutput::success(report))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_home_and_sensitive() {
        assert!(validate_grant_path("/").is_some());
        assert!(validate_grant_path("~/code").is_some());
        assert!(validate_grant_path("/tmp/../etc").is_some());
        if let Some(home) = dirs::home_dir() {
            assert!(validate_grant_path(home.to_str().unwrap()).is_some());
            assert!(validate_grant_path(home.join(".ssh/id_rsa").to_str().unwrap()).is_some());
        }
        assert!(validate_grant_path("/tmp/kkagent-cache").is_none());
    }
}
