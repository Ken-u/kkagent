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
pub use tool_bridge::{qualify_mcp_tool_name, register_mcp_tools};
