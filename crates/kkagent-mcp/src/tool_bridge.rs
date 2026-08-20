//! Bridge MCP remote tools into the local `ToolRegistry` as `mcp__server__tool`.
//!
//! Server runtime names (used for `/mcp` toggles, `disabled.toml`, and the
//! OAuth store) stay untouched; only the exposed tool names are shortened via
//! per-server tool namespaces (see `build_namespace_map`).

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use kkagent_tools::{Tool, ToolContext, ToolOutput, ToolRegistry};

use crate::client::{McpManager, McpServerConfig, McpToolInfo};

const MCP_NAME_PREFIX: &str = "mcp__";
const MCP_NAME_SEPARATOR: &str = "__";
const MAX_QUALIFIED_LENGTH: usize = 64;

/// Sanitize one segment of an MCP qualified tool name (kimi-code `tool-naming.ts`).
pub fn sanitize_mcp_name_part(part: &str) -> String {
    let mut out = String::with_capacity(part.len());
    let mut prev_underscore = false;
    for ch in part.chars() {
        let ok = ch.is_ascii_alphanumeric() || ch == '_' || ch == '-';
        if ok {
            out.push(ch);
            prev_underscore = ch == '_';
        } else if !prev_underscore {
            out.push('_');
            prev_underscore = true;
        }
    }
    out
}

/// Qualify as `mcp__<server>__<tool>`, truncating with a stable hash if too long.
pub fn qualify_mcp_tool_name(server_name: &str, tool_name: &str) -> String {
    qualified_tool_name(server_name, tool_name)
}

/// Same as [`qualify_mcp_tool_name`] but with an explicit (possibly shortened)
/// tool namespace instead of the raw server name.
fn qualified_tool_name(namespace: &str, tool_name: &str) -> String {
    let full = format!(
        "{}{}{}{}",
        MCP_NAME_PREFIX,
        sanitize_mcp_name_part(namespace),
        MCP_NAME_SEPARATOR,
        sanitize_mcp_name_part(tool_name)
    );
    if full.len() <= MAX_QUALIFIED_LENGTH {
        return full;
    }
    let hash = stable_hash8(&full);
    let keep = MAX_QUALIFIED_LENGTH.saturating_sub(hash.len() + 1);
    format!("{}_{}", &full[..keep], hash)
}

/// Maximum length for a tool namespace segment before truncation kicks in.
const MAX_NAMESPACE_LENGTH: usize = 32;
/// Namespace used by plugin servers: `plugin-<plugin-id>:<server-name>`.
const PLUGIN_SERVER_PREFIX: &str = "plugin-";

/// Derive the short tool namespace for one server.
///
/// - Explicit `tool_namespace` wins (plugin manager supplies plugin-id based
///   short names).
/// - `plugin-<id>:<server>` runtime names are compressed to `<id>` /
///   `<id>_<server>` as a fallback so any unadapted producer still gets short
///   tool names.
/// - Everything else (user-configured `config.toml` servers) keeps its name.
fn tool_namespace_for(config: &McpServerConfig) -> String {
    if let Some(ns) = config.tool_namespace.as_deref() {
        let ns = sanitize_mcp_name_part(ns);
        if !ns.is_empty() {
            return truncate_namespace(&ns);
        }
    }
    match compressed_plugin_namespace(&config.name) {
        Some(ns) => ns,
        None => sanitize_mcp_name_part(&config.name),
    }
}

/// `plugin-<id>:<server>` -> `<id>` (single-server plugins conventionally
/// embed the server name into the id, e.g. `rk-codesearch_search`).
fn compressed_plugin_namespace(server_name: &str) -> Option<String> {
    let rest = server_name.strip_prefix(PLUGIN_SERVER_PREFIX)?;
    let (plugin_id, server) = match rest.split_once(':') {
        Some((id, server)) => (id, Some(server)),
        None => (rest, None),
    };
    if plugin_id.is_empty() {
        return None;
    }
    let candidate = match server {
        Some(server) if !server.is_empty() => format!("{plugin_id}_{server}"),
        _ => plugin_id.to_string(),
    };
    Some(truncate_namespace(&sanitize_mcp_name_part(&candidate)))
}

fn truncate_namespace(ns: &str) -> String {
    if ns.len() <= MAX_NAMESPACE_LENGTH {
        return ns.to_string();
    }
    let hash = stable_hash8(ns);
    let keep = MAX_NAMESPACE_LENGTH.saturating_sub(hash.len() + 1);
    // Keep the cut on a char boundary; sanitize guarantees ASCII, but be safe.
    let mut end = keep.min(ns.len());
    while end > 0 && !ns.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}_{}", &ns[..end], hash)
}

/// Build the server-name -> tool-namespace map, resolving collisions by
/// appending a stable hash suffix to later (deterministically ordered) names.
fn build_namespace_map(configs: &[McpServerConfig]) -> HashMap<String, String> {
    let mut map: HashMap<String, String> = HashMap::new();
    let mut seen: HashSet<String> = HashSet::new();
    for config in configs {
        let mut ns = tool_namespace_for(config);
        if seen.contains(&ns) {
            let hash = stable_hash8(&format!("{}|{}", config.name, ns));
            let keep = MAX_NAMESPACE_LENGTH.saturating_sub(hash.len() + 1);
            // sanitize keeps namespaces ASCII, so byte slicing is safe here.
            let keep = keep.min(ns.len());
            let disambiguated = format!("{}_{}", &ns[..keep], hash);
            tracing::warn!(
                server = %config.name,
                namespace = %ns,
                disambiguated = %disambiguated,
                "tool namespace collision resolved with hash suffix"
            );
            ns = disambiguated;
        }
        seen.insert(ns.clone());
        map.insert(config.name.clone(), ns);
    }
    map
}

fn stable_hash8(input: &str) -> String {
    let mut hash: u32 = 0x811c_9dc5;
    for ch in input.chars() {
        hash ^= ch as u32;
        hash = hash.wrapping_mul(0x0100_0193);
    }
    format!("{:08x}", hash)
}

struct McpProxyTool {
    manager: Arc<McpManager>,
    server_name: String,
    remote_name: String,
    qualified_name: String,
    description: String,
    input_schema: Value,
}

#[async_trait]
impl Tool for McpProxyTool {
    fn name(&self) -> &str {
        &self.qualified_name
    }

    fn description(&self) -> &str {
        &self.description
    }

    fn parameters_schema(&self) -> Value {
        self.input_schema.clone()
    }

    fn disclosure(&self) -> kkagent_tools::ToolDisclosure {
        kkagent_tools::ToolDisclosure::Deferred
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        match self
            .manager
            .call_tool(&self.server_name, &self.remote_name, input)
            .await
        {
            Ok(result) => {
                let content = if result.text.is_empty() {
                    if result.images.is_empty() {
                        "(empty MCP result)".into()
                    } else {
                        format!("MCP returned {} image attachment(s).", result.images.len())
                    }
                } else {
                    result.text
                };
                let mut output = if result.is_error {
                    ToolOutput::error(content)
                } else {
                    ToolOutput::success(content)
                };
                for image in result.images {
                    match kkagent_tools::builtin::media::normalize_external_image(
                        &image.data,
                        &ctx.image,
                    ) {
                        Ok(image) => output.images.push(image),
                        Err(error) => output
                            .content
                            .push_str(&format!("\n[MCP image omitted: {error}]")),
                    }
                }
                Ok(output)
            }
            Err(e) => Ok(ToolOutput::error(format!(
                "MCP tool {}.{} failed: {}",
                self.server_name, self.remote_name, e
            ))),
        }
    }
}

/// Register every discovered MCP tool onto `registry` under kimi-style names.
///
/// Tool names use the shortened namespace (`mcp__<namespace>__<tool>`) while
/// tool execution keeps addressing the runtime server name.
pub async fn register_mcp_tools(registry: &mut ToolRegistry, manager: &Arc<McpManager>) {
    let tools = manager.list_tools().await;
    if tools.is_empty() {
        return;
    }
    let namespaces = build_namespace_map(&manager.configs_snapshot());
    for info in tools {
        let namespace = namespaces
            .get(&info.server_name)
            .cloned()
            .unwrap_or_else(|| sanitize_mcp_name_part(&info.server_name));
        register_one(registry, manager.clone(), &info, namespace);
    }
}

fn register_one(
    registry: &mut ToolRegistry,
    manager: Arc<McpManager>,
    info: &McpToolInfo,
    namespace: String,
) {
    let qualified = qualified_tool_name(&namespace, &info.name);
    let description = if info.description.is_empty() {
        format!(
            "MCP tool `{}` from server `{}`.",
            info.name, info.server_name
        )
    } else {
        format!("[MCP:{}] {}", info.server_name, info.description)
    };

    let schema = if info.input_schema.is_null()
        || info
            .input_schema
            .as_object()
            .map(|o| o.is_empty())
            .unwrap_or(false)
    {
        serde_json::json!({
            "type": "object",
            "properties": {}
        })
    } else {
        info.input_schema.clone()
    };

    registry.register(Arc::new(McpProxyTool {
        manager,
        server_name: info.server_name.clone(),
        remote_name: info.name.clone(),
        qualified_name: qualified,
        description,
        input_schema: schema,
    }));
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn qualify_basic() {
        assert_eq!(
            qualify_mcp_tool_name("github", "search_issues"),
            "mcp__github__search_issues"
        );
    }

    fn config(name: &str, tool_namespace: Option<&str>) -> McpServerConfig {
        McpServerConfig {
            name: name.to_string(),
            enabled: true,
            transport: crate::client::McpTransportKind::Stdio,
            command: None,
            args: Vec::new(),
            env: Default::default(),
            cwd: None,
            url: None,
            headers: Default::default(),
            oauth: None,
            timeout_ms: None,
            tool_namespace: tool_namespace.map(str::to_string),
        }
    }

    #[test]
    fn plugin_runtime_names_compress_to_short_namespaces() {
        // Fallback compression of plugin runtime names (used when a producer
        // did not set `tool_namespace` explicitly).
        assert_eq!(
            tool_namespace_for(&config("plugin-rk-codesearch_search:search", None)),
            "rk-codesearch_search_search"
        );
        // Multi-server plugin: id + server suffix.
        assert_eq!(
            tool_namespace_for(&config("plugin-myplugin:search", None)),
            "myplugin_search"
        );
        // Malformed plugin runtime name still compresses to the raw rest.
        assert_eq!(
            tool_namespace_for(&config("plugin-myplugin", None)),
            "myplugin"
        );
        // User-configured servers keep their full name.
        assert_eq!(tool_namespace_for(&config("github", None)), "github");
        // Explicit namespace wins, including over the plugin pattern.
        assert_eq!(
            tool_namespace_for(&config(
                "plugin-rk-codesearch_search:search",
                Some("rk-codesearch")
            )),
            "rk-codesearch"
        );
    }

    #[test]
    fn long_namespaces_are_truncated_with_stable_hash() {
        let long = "a".repeat(64);
        let ns = tool_namespace_for(&config("plugin-x:y", Some(&long)));
        assert!(ns.len() <= MAX_NAMESPACE_LENGTH);
        assert_eq!(ns, tool_namespace_for(&config("plugin-x:y", Some(&long))));
    }

    #[test]
    fn namespace_collisions_get_hash_suffixes() {
        let configs = vec![
            config("plugin-alpha:search", None),
            // Compresses to the same `alpha_search` namespace.
            config("plugin-alpha_search:search", None),
        ];
        let map = build_namespace_map(&configs);
        let ns1 = &map["plugin-alpha:search"];
        let ns2 = &map["plugin-alpha_search:search"];
        assert_eq!(ns1, "alpha_search");
        assert_ne!(ns1, ns2);
        assert!(ns2.starts_with("alpha_search"));
    }

    #[test]
    fn qualified_names_stay_within_the_length_cap() {
        let ns = "n".repeat(MAX_NAMESPACE_LENGTH);
        let name = qualified_tool_name(&ns, &"t".repeat(80));
        assert!(name.len() <= MAX_QUALIFIED_LENGTH);
        assert!(name.starts_with(MCP_NAME_PREFIX));
    }

    #[test]
    fn sanitize_special_chars() {
        assert_eq!(sanitize_mcp_name_part("my server!"), "my_server_");
    }

    #[test]
    fn disclosure_defaults_to_inline_for_generic_tools() {
        // A trivial inline tool should report Inline disclosure.
        struct DummyTool;
        #[async_trait]
        impl Tool for DummyTool {
            fn name(&self) -> &str {
                "Dummy"
            }
            fn description(&self) -> &str {
                "test"
            }
            fn parameters_schema(&self) -> Value {
                serde_json::json!({})
            }
            async fn execute(
                &self,
                _input: Value,
                _ctx: &ToolContext,
            ) -> anyhow::Result<ToolOutput> {
                Ok(ToolOutput::success("ok"))
            }
        }
        let t = DummyTool;
        assert_eq!(t.disclosure(), kkagent_tools::ToolDisclosure::Inline);
    }
}
