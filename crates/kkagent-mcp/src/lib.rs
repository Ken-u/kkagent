pub mod client;
pub mod skills;
pub mod hooks;
pub mod tool_bridge;

pub use client::{McpManager, McpServerConfig, McpToolInfo};
pub use skills::SkillsManager;
pub use hooks::HookManager;
pub use tool_bridge::{qualify_mcp_tool_name, register_mcp_tools};
