//! toolPolicy — layered tool activation (workspace / profile / global / session).

use std::collections::HashSet;

#[derive(Debug, Clone, Default)]
pub struct ToolActivationPolicy {
    /// Allowlist; `None` = unconstrained.
    pub tools: Option<Vec<String>>,
    pub disallowed_tools: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default)]
pub struct GlobalToolsPolicy {
    pub enabled: Option<Vec<String>>,
    pub disabled: Option<Vec<String>>,
}

#[derive(Debug, Clone, Default)]
pub struct ToolPolicyLayers {
    pub workspace_disabled: Vec<String>,
    pub profile: ToolActivationPolicy,
    pub global: GlobalToolsPolicy,
    pub session_disabled: Vec<String>,
}

fn is_mcp_name(name: &str) -> bool {
    name.starts_with("mcp__")
}

fn pattern_match(pattern: &str, name: &str) -> bool {
    if pattern == name {
        return true;
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        return name.starts_with(prefix);
    }
    false
}

pub fn is_tool_active(policy: &ToolActivationPolicy, name: &str) -> bool {
    let mcp = is_mcp_name(name);
    if let Some(ref allowed) = policy.tools {
        let ok = if mcp {
            allowed
                .iter()
                .filter(|p| is_mcp_name(p))
                .any(|p| pattern_match(p, name))
        } else {
            allowed.iter().any(|p| p == name)
        };
        if !ok {
            return false;
        }
    }
    if let Some(ref denied) = policy.disallowed_tools {
        if mcp {
            if denied
                .iter()
                .filter(|p| is_mcp_name(p))
                .any(|p| pattern_match(p, name))
            {
                return false;
            }
        } else if denied.iter().any(|p| p == name) {
            return false;
        }
    }
    true
}

pub fn is_tool_active_composed(layers: &ToolPolicyLayers, name: &str) -> bool {
    let workspace = ToolActivationPolicy {
        tools: None,
        disallowed_tools: Some(layers.workspace_disabled.clone()),
    };
    let global = ToolActivationPolicy {
        tools: layers
            .global
            .enabled
            .as_ref()
            .filter(|v| !v.is_empty())
            .cloned(),
        disallowed_tools: layers.global.disabled.clone(),
    };
    let session = ToolActivationPolicy {
        tools: None,
        disallowed_tools: Some(layers.session_disabled.clone()),
    };
    is_tool_active(&workspace, name)
        && is_tool_active(&layers.profile, name)
        && is_tool_active(&global, name)
        && is_tool_active(&session, name)
}

#[derive(Debug, Clone, Default)]
pub struct ToolPolicyService {
    layers: ToolPolicyLayers,
}

impl ToolPolicyService {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn layers(&self) -> &ToolPolicyLayers {
        &self.layers
    }

    pub fn layers_mut(&mut self) -> &mut ToolPolicyLayers {
        &mut self.layers
    }

    pub fn set_session_disabled(&mut self, tools: impl IntoIterator<Item = String>) {
        self.layers.session_disabled = tools.into_iter().collect();
    }

    pub fn disable_session_tool(&mut self, name: &str) {
        if !self.layers.session_disabled.iter().any(|t| t == name) {
            self.layers.session_disabled.push(name.to_string());
        }
    }

    pub fn enable_session_tool(&mut self, name: &str) {
        self.layers.session_disabled.retain(|t| t != name);
    }

    pub fn is_active(&self, name: &str) -> bool {
        is_tool_active_composed(&self.layers, name)
    }

    /// Filter a tool name list to those currently active.
    pub fn filter_active<'a>(&self, names: impl IntoIterator<Item = &'a str>) -> HashSet<String> {
        names
            .into_iter()
            .filter(|n| self.is_active(n))
            .map(|n| n.to_string())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn composed_session_disable() {
        let mut svc = ToolPolicyService::new();
        assert!(svc.is_active("Bash"));
        svc.disable_session_tool("Bash");
        assert!(!svc.is_active("Bash"));
        assert!(svc.is_active("Read"));
    }

    #[test]
    fn mcp_glob() {
        let policy = ToolActivationPolicy {
            tools: Some(vec!["mcp__github__*".into()]),
            disallowed_tools: None,
        };
        assert!(is_tool_active(&policy, "mcp__github__create_issue"));
        assert!(!is_tool_active(&policy, "Bash"));
    }
}
