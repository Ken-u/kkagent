use serde::{Deserialize, Serialize};

/// How a tool's definition is disclosed to the model.
///
/// `Inline` tools always have their full JSON schema sent to the model.
/// `Deferred` tools are omitted from the schema list until loaded via
/// `SelectTools`; only their name and description appear in a system-prompt
/// announcement so the model knows they exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, Default)]
pub enum ToolDisclosure {
    #[default]
    Inline,
    Deferred,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDefinition {
    pub name: String,
    pub description: String,
    pub parameters: serde_json::Value,
    #[serde(default)]
    pub read_only: bool,
    #[serde(default)]
    pub disclosure: ToolDisclosure,
}
