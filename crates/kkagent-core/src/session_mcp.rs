//! MCP servers supplied by the ACP client for one session.
//!
//! `session/new` (and `session/load` / `session/resume`) may carry the standard
//! `mcpServers` array. Those servers belong to that session only: they are
//! started for it, their tools show up as `mcp__<server>__<tool>` in its
//! registry (so the strict tool allowlist and the permission chain apply like
//! for any MCP tool), and they are stopped when it ends.

use std::collections::{HashMap, HashSet};
use std::path::Path;

use kkagent_config::McpServerConfig as AppMcpServerConfig;
use kkagent_mcp::{sanitize_mcp_name_part, McpServerConfig};
use kkagent_protocol::ToolAllowlist;
use serde_json::Value;

/// Runtime names of servers the process configured itself start with this for
/// plugins; session servers may not impersonate them.
const PLUGIN_NAME_PREFIX: &str = "plugin-";

/// Name/value pairs arrive either as the ACP array form
/// (`[{"name": "A", "value": "1"}]`) or as a plain object.
fn pairs(value: Option<&Value>, what: &str) -> Result<HashMap<String, String>, String> {
    let mut out = HashMap::new();
    match value {
        None | Some(Value::Null) => {}
        Some(Value::Array(items)) => {
            for item in items {
                let name = item
                    .get("name")
                    .and_then(Value::as_str)
                    .filter(|name| !name.is_empty())
                    .ok_or_else(|| format!("{what} entries need a `name`"))?;
                let value = item.get("value").and_then(Value::as_str).unwrap_or("");
                out.insert(name.to_string(), value.to_string());
            }
        }
        Some(Value::Object(map)) => {
            for (name, value) in map {
                let value = value
                    .as_str()
                    .ok_or_else(|| format!("{what} `{name}` must be a string"))?;
                out.insert(name.clone(), value.to_string());
            }
        }
        Some(_) => return Err(format!("{what} must be an array or an object")),
    }
    Ok(out)
}

/// Parse the `mcpServers` of a session request.
///
/// Stdio servers (no `type`) run in the session's working directory. Names must
/// be unique after sanitizing and may not look like plugin servers.
pub fn parse_acp_mcp_servers(
    servers: &[Value],
    working_dir: &Path,
) -> Result<Vec<McpServerConfig>, String> {
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for descriptor in servers {
        let object = descriptor
            .as_object()
            .ok_or_else(|| "mcpServers entries must be objects".to_string())?;
        let name = object
            .get("name")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|name| !name.is_empty())
            .ok_or_else(|| "every mcpServers entry needs a `name`".to_string())?;
        if name.starts_with(PLUGIN_NAME_PREFIX) {
            return Err(format!(
                "mcp server name `{name}` is reserved for plugin servers"
            ));
        }
        let sanitized = sanitize_mcp_name_part(name);
        if sanitized.is_empty() || !seen.insert(sanitized) {
            return Err(format!("mcp server name `{name}` is empty or duplicated"));
        }
        let transport = object.get("type").and_then(Value::as_str);
        let config = match transport {
            None | Some("stdio") => {
                let command = object
                    .get("command")
                    .and_then(Value::as_str)
                    .filter(|command| !command.trim().is_empty())
                    .ok_or_else(|| format!("stdio mcp server `{name}` needs a `command`"))?;
                let args = match object.get("args") {
                    None | Some(Value::Null) => Vec::new(),
                    Some(Value::Array(items)) => items
                        .iter()
                        .map(|item| {
                            item.as_str()
                                .map(str::to_string)
                                .ok_or_else(|| format!("mcp server `{name}`: args must be strings"))
                        })
                        .collect::<Result<_, _>>()?,
                    Some(_) => return Err(format!("mcp server `{name}`: args must be an array")),
                };
                AppMcpServerConfig {
                    transport_type: Some("stdio".into()),
                    command: Some(command.to_string()),
                    args,
                    env: pairs(object.get("env"), "env")?,
                    cwd: Some(working_dir.to_string_lossy().into_owned()),
                    ..Default::default()
                }
            }
            Some(kind @ ("http" | "sse")) => {
                let url = object
                    .get("url")
                    .and_then(Value::as_str)
                    .filter(|url| !url.trim().is_empty())
                    .ok_or_else(|| format!("{kind} mcp server `{name}` needs a `url`"))?;
                AppMcpServerConfig {
                    transport_type: Some(kind.to_string()),
                    url: Some(url.to_string()),
                    headers: pairs(object.get("headers"), "headers")?,
                    ..Default::default()
                }
            }
            Some(other) => {
                return Err(format!(
                    "mcp server `{name}`: unsupported transport `{other}`"
                ))
            }
        };
        out.push(McpServerConfig::from_app(name.to_string(), &config));
    }
    Ok(out)
}

/// Whether the allowlist could ever permit a tool of `server`. Servers it
/// cannot permit are not started at all (no side effects for tools that would
/// be filtered out anyway).
pub fn allowlist_may_use_server(allowlist: Option<&ToolAllowlist>, server: &str) -> bool {
    allowlist.is_none_or(|allowlist| {
        allowlist.may_allow_prefix(&format!("mcp__{}__", sanitize_mcp_name_part(server)))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use kkagent_mcp::McpTransportKind;
    use serde_json::json;

    fn parse(value: Value) -> Result<Vec<McpServerConfig>, String> {
        parse_acp_mcp_servers(value.as_array().unwrap(), Path::new("/work"))
    }

    #[test]
    fn stdio_http_and_sse_descriptors_are_converted() {
        let servers = parse(json!([
            {"name": "files", "command": "mcp-files", "args": ["--ro"],
             "env": [{"name": "TOKEN", "value": "abc"}]},
            {"type": "http", "name": "docs", "url": "https://example.test/mcp",
             "headers": [{"name": "Authorization", "value": "Bearer x"}]},
            {"type": "sse", "name": "legacy", "url": "https://example.test/sse"},
        ]))
        .unwrap();
        assert_eq!(servers.len(), 3);
        assert_eq!(servers[0].transport, McpTransportKind::Stdio);
        assert_eq!(servers[0].command.as_deref(), Some("mcp-files"));
        assert_eq!(servers[0].args, vec!["--ro"]);
        assert_eq!(servers[0].env.get("TOKEN").map(String::as_str), Some("abc"));
        assert_eq!(servers[0].cwd.as_deref(), Some("/work"));
        assert_eq!(servers[1].transport, McpTransportKind::Http);
        assert_eq!(
            servers[1].headers.get("Authorization").map(String::as_str),
            Some("Bearer x")
        );
        assert_eq!(servers[2].transport, McpTransportKind::Sse);
    }

    #[test]
    fn malformed_descriptors_are_rejected() {
        for bad in [
            json!([1]),
            json!([{"command": "x"}]),
            json!([{"name": "a"}]),
            json!([{"name": "a", "command": "x", "args": [1]}]),
            json!([{"name": "a", "type": "http"}]),
            json!([{"name": "a", "type": "carrier-pigeon", "url": "x"}]),
            json!([{"name": "plugin-evil:x", "command": "x"}]),
            json!([{"name": "a b", "command": "x"}, {"name": "a_b", "command": "y"}]),
            json!([{"name": "a", "command": "x", "env": [{"value": "1"}]}]),
        ] {
            assert!(parse(bad.clone()).is_err(), "{bad} should be rejected");
        }
    }

    #[test]
    fn allowlist_gates_which_servers_may_start() {
        assert!(allowlist_may_use_server(None, "files"));
        let allow = ToolAllowlist::new(["Read", "mcp__files__*", "mcp__docs__search"]);
        assert!(allowlist_may_use_server(Some(&allow), "files"));
        assert!(allowlist_may_use_server(Some(&allow), "docs"));
        assert!(!allowlist_may_use_server(Some(&allow), "other"));
        assert!(!allowlist_may_use_server(
            Some(&ToolAllowlist::deny_all()),
            "files"
        ));
    }
}
