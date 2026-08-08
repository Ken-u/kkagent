pub mod client;
pub mod skills;
pub mod hooks;
pub mod tool_bridge;
pub mod oauth;
pub mod sse_client;

pub use client::{McpManager, McpServerConfig, McpToolInfo, McpTransportKind};
pub use skills::SkillsManager;
pub use hooks::{HookManager, HookEvent, HookOutcome, HookConfig as McpHookConfig};
pub use tool_bridge::{qualify_mcp_tool_name, register_mcp_tools};
