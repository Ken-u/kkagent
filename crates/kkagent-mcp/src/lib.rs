pub mod client;
pub mod hooks;
pub mod oauth;
pub mod skills;
pub mod sse_client;
pub mod tool_bridge;

pub use client::{
    McpManager, McpServerConfig, McpServerStatus, McpStatusSnapshot, McpToolInfo, McpTransportKind,
};
pub use hooks::{HookConfig as McpHookConfig, HookEvent, HookManager, HookOutcome};
pub use skills::SkillsManager;
pub use tool_bridge::{
    is_plugin_mcp_server, qualified_tool_name as qualify_namespaced_tool_name,
    qualify_mcp_tool_name, register_mcp_tools, register_mcp_tools_where, sanitize_mcp_name_part,
};

// Keep unit tests out of the real ~/.kkagent home.
// tools/mcp don't depend on kkagent-core; install via config.
kkagent_config::install_test_home!();
