//! Bridge MCP remote tools into the local `ToolRegistry` as `mcp__server__tool`.

use std::sync::Arc;

use async_trait::async_trait;
use serde_json::Value;

use kkagent_tools::{Tool, ToolContext, ToolOutput, ToolRegistry};

use crate::client::{McpManager, McpToolInfo};

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
    let full = format!(
        "{}{}{}{}",
        MCP_NAME_PREFIX,
        sanitize_mcp_name_part(server_name),
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
pub async fn register_mcp_tools(registry: &mut ToolRegistry, manager: &Arc<McpManager>) {
    let tools = manager.list_tools().await;
    for info in tools {
        register_one(registry, manager.clone(), &info);
    }
}

fn register_one(registry: &mut ToolRegistry, manager: Arc<McpManager>, info: &McpToolInfo) {
    let qualified = qualify_mcp_tool_name(&info.server_name, &info.name);
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

    #[test]
    fn sanitize_special_chars() {
        assert_eq!(sanitize_mcp_name_part("my server!"), "my_server_");
    }
}
