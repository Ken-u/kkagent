//! Agent roles: system prompt + strict tool allowlist + extension toggles.
//!
//! Roles come only from trusted sources: the user-level config
//! (`[agent_roles.NAME]`), the process command line (`kkagent acp --role`) and
//! the connected host (ACP session parameters). Project files cannot define or
//! select roles.
//!
//! Resolution is *narrowing only*. The process-level policy is a ceiling:
//! session parameters may intersect the tool allowlist and switch extensions
//! off, but can never widen what the process allows.

use kkagent_config::{
    validate_tool_pattern, AgentRoleConfig, AppConfig, ExtensionsConfig, StorageBackend,
    SystemPromptMode,
};
use kkagent_protocol::host::{ExtensionOverrides, HostSessionMeta, HostToolSpec};
use kkagent_protocol::{PermissionMode, ToolAllowlist};

use crate::session::Session;

/// A system prompt contribution and how it combines with the built-in prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SystemPromptSpec {
    pub mode: SystemPromptMode,
    pub text: String,
}

/// The fully resolved role of one session.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SessionRole {
    /// Role name, when the role (or process default) is a named one.
    pub name: Option<String>,
    pub system_prompt: Option<SystemPromptSpec>,
    /// `None` = unrestricted (default behavior); `Some` = strict ceiling.
    pub tool_allowlist: Option<ToolAllowlist>,
    /// Initial permission mode for the session.
    pub permission_mode: Option<PermissionMode>,
    pub extensions: ExtensionsConfig,
    /// Tools the host injected for this session. They are only registered
    /// when the tool allowlist permits their names.
    pub host_tools: Vec<HostToolSpec>,
}

impl SessionRole {
    /// True when a strict tool allowlist is in force.
    pub fn is_restricted(&self) -> bool {
        self.tool_allowlist.is_some()
    }

    /// Whether the role lets the model use `tool` (always true when
    /// unrestricted).
    pub fn permits_tool(&self, tool: &str) -> bool {
        self.tool_allowlist
            .as_ref()
            .is_none_or(|allowlist| allowlist.allows(tool))
    }

    /// First phase of applying a role to a fresh session, before workspace
    /// context is added: carries the extension toggles and, for `replace`
    /// mode, swaps the built-in prompt for the role prompt.
    pub fn apply_before_context(&self, session: &mut Session) {
        session.load_project_instructions = self.extensions.load_project_instructions;
        if !self.extensions.load_plugins {
            // A plugin may have replaced the base prompt process-wide; a
            // session with plugins off starts from the built-in prompt.
            session.system_prompt = crate::session::runtime::default_system_prompt();
        }
        if let Some(spec) = &self.system_prompt {
            if spec.mode == SystemPromptMode::Replace {
                session.system_prompt = spec.text.clone();
            }
        }
    }

    /// Second phase, after workspace context and prompt sections: `append`
    /// mode adds the role prompt last so it keeps system-level precedence.
    pub fn apply_after_context(&self, session: &mut Session) {
        if let Some(spec) = &self.system_prompt {
            if spec.mode == SystemPromptMode::Append {
                session.system_prompt.push_str("\n\n");
                session.system_prompt.push_str(&spec.text);
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RoleError {
    #[error("unknown agent role `{0}` (define it under [agent_roles.{0}] in the user config)")]
    UnknownRole(String),
    #[error("invalid permission mode `{0}` (expected manual, yolo or auto)")]
    InvalidPermissionMode(String),
    #[error("invalid tool allowlist: {0}")]
    InvalidToolPattern(String),
}

/// Process-wide policy: the ceiling every session of this process lives under.
#[derive(Debug, Clone, Default)]
pub struct ProcessPolicy {
    /// Role selected for the whole process (`kkagent acp --role`).
    pub role: Option<SessionRole>,
    /// `[extensions]` narrowed by command-line switches.
    pub extensions: ExtensionsConfig,
    /// Where sessions, messages and events live (`[storage]`, optionally
    /// overridden on the command line).
    pub storage: StorageBackend,
}

impl ProcessPolicy {
    /// Build the process policy from the trusted config plus command-line
    /// choices. `cli_extensions` can only switch extensions off.
    pub fn from_config(
        config: &AppConfig,
        role_name: Option<&str>,
        cli_extensions: ExtensionsConfig,
    ) -> Result<Self, RoleError> {
        let extensions = config.extensions.narrow(cli_extensions);
        let storage = config.storage.backend;
        let role = match role_name {
            Some(name) => {
                let role_config = lookup_role(config, name)?;
                Some(role_from_config(name, role_config, extensions)?)
            }
            None => None,
        };
        Ok(Self {
            role,
            extensions,
            storage,
        })
    }

    /// The tool ceiling of this process, if any.
    pub fn tool_ceiling(&self) -> Option<&ToolAllowlist> {
        self.role
            .as_ref()
            .and_then(|role| role.tool_allowlist.as_ref())
    }

    /// The role of a session that supplied no parameters of its own.
    pub fn default_role(&self) -> SessionRole {
        match &self.role {
            Some(role) => role.clone(),
            None => SessionRole {
                extensions: self.extensions,
                ..Default::default()
            },
        }
    }
}

fn lookup_role<'a>(config: &'a AppConfig, name: &str) -> Result<&'a AgentRoleConfig, RoleError> {
    config
        .agent_roles
        .get(name)
        .ok_or_else(|| RoleError::UnknownRole(name.to_string()))
}

fn parse_permission_mode(value: &str) -> Result<PermissionMode, RoleError> {
    value
        .parse::<PermissionMode>()
        .map_err(|_| RoleError::InvalidPermissionMode(value.to_string()))
}

fn checked_allowlist(patterns: &[String]) -> Result<ToolAllowlist, RoleError> {
    for pattern in patterns {
        validate_tool_pattern(pattern).map_err(RoleError::InvalidToolPattern)?;
    }
    Ok(ToolAllowlist::new(patterns.iter().cloned()))
}

fn role_from_config(
    name: &str,
    config: &AgentRoleConfig,
    extensions: ExtensionsConfig,
) -> Result<SessionRole, RoleError> {
    Ok(SessionRole {
        name: Some(name.to_string()),
        system_prompt: config
            .system_prompt
            .as_deref()
            .filter(|text| !text.trim().is_empty())
            .map(|text| SystemPromptSpec {
                mode: config.prompt_mode,
                text: text.to_string(),
            }),
        tool_allowlist: config.tools.as_deref().map(checked_allowlist).transpose()?,
        permission_mode: config
            .permission_mode
            .as_deref()
            .map(parse_permission_mode)
            .transpose()?,
        extensions,
        host_tools: Vec::new(),
    })
}

fn apply_extension_overrides(
    base: ExtensionsConfig,
    overrides: Option<&ExtensionOverrides>,
) -> ExtensionsConfig {
    let Some(overrides) = overrides else {
        return base;
    };
    // `Some(true)` is ignored on purpose: an override can only switch off.
    base.narrow(ExtensionsConfig {
        load_project_instructions: overrides.load_project_instructions.unwrap_or(true),
        load_hooks: overrides.load_hooks.unwrap_or(true),
        load_plugins: overrides.load_plugins.unwrap_or(true),
    })
}

/// Resolve the role of one session from the process ceiling, an optional named
/// role, the host's `_meta` and the standard ACP `systemPrompt`.
///
/// - Tools: `process ∩ named role ∩ meta.toolAllowlist` (each present layer).
/// - Prompt: ACP `systemPrompt` > named role > process role.
/// - Permission mode: `meta.permissionMode` > named role > process role.
/// - Extensions: process setting, narrowed by the session's overrides.
pub fn resolve_session_role(
    config: &AppConfig,
    process: &ProcessPolicy,
    meta: &HostSessionMeta,
    system_prompt: Option<SystemPromptSpec>,
) -> Result<SessionRole, RoleError> {
    let named = match meta.role.as_deref() {
        Some(name) => Some((name, lookup_role(config, name)?)),
        None => None,
    };
    let named_role = named
        .map(|(name, role_config)| role_from_config(name, role_config, process.extensions))
        .transpose()?;
    let meta_allowlist = meta
        .tool_allowlist
        .as_deref()
        .map(checked_allowlist)
        .transpose()?;

    let tool_allowlist = [
        process.tool_ceiling(),
        named_role
            .as_ref()
            .and_then(|role| role.tool_allowlist.as_ref()),
        meta_allowlist.as_ref(),
    ]
    .into_iter()
    .fold(None, |acc: Option<ToolAllowlist>, layer| {
        ToolAllowlist::narrow(acc.as_ref(), layer)
    });

    let process_role = process.role.as_ref();
    let permission_mode = match meta.permission_mode.as_deref() {
        Some(value) => Some(parse_permission_mode(value)?),
        None => named_role
            .as_ref()
            .and_then(|role| role.permission_mode)
            .or_else(|| process_role.and_then(|role| role.permission_mode)),
    };
    let system_prompt = system_prompt
        .or_else(|| {
            named_role
                .as_ref()
                .and_then(|role| role.system_prompt.clone())
        })
        .or_else(|| process_role.and_then(|role| role.system_prompt.clone()));
    let name = named_role
        .as_ref()
        .and_then(|role| role.name.clone())
        .or_else(|| process_role.and_then(|role| role.name.clone()));

    Ok(SessionRole {
        name,
        system_prompt,
        tool_allowlist,
        permission_mode,
        extensions: apply_extension_overrides(process.extensions, meta.extensions.as_ref()),
        host_tools: meta.host_tools.clone(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_with_roles() -> AppConfig {
        let mut config: AppConfig = serde_json::from_value(serde_json::json!({
            "agent_roles": {
                "supervisor": {
                    "system_prompt": "You coordinate.",
                    "prompt_mode": "replace",
                    "tools": ["Read", "Grep", "host_*"],
                    "permission_mode": "manual"
                },
                "narrow": {"tools": ["Read"]},
                "wide": {"tools": ["*"]},
                "none": {"tools": []}
            }
        }))
        .unwrap();
        config.default_model = Some("x".into());
        config
    }

    fn meta(json: serde_json::Value) -> HostSessionMeta {
        serde_json::from_value(json).unwrap()
    }

    #[test]
    fn without_any_role_nothing_is_restricted() {
        let config = AppConfig::default();
        let process =
            ProcessPolicy::from_config(&config, None, ExtensionsConfig::default()).unwrap();
        let role =
            resolve_session_role(&config, &process, &HostSessionMeta::default(), None).unwrap();
        assert!(!role.is_restricted());
        assert!(role.system_prompt.is_none());
        assert_eq!(role.extensions, ExtensionsConfig::default());
        assert_eq!(process.default_role(), role);
    }

    #[test]
    fn process_role_is_the_default_for_every_session() {
        let config = config_with_roles();
        let process =
            ProcessPolicy::from_config(&config, Some("supervisor"), ExtensionsConfig::default())
                .unwrap();
        let role =
            resolve_session_role(&config, &process, &HostSessionMeta::default(), None).unwrap();
        assert_eq!(role.name.as_deref(), Some("supervisor"));
        assert!(role.is_restricted());
        let allow = role.tool_allowlist.unwrap();
        assert!(allow.allows("Read") && allow.allows("host_task") && !allow.allows("Bash"));
        assert_eq!(role.system_prompt.unwrap().mode, SystemPromptMode::Replace);
        assert_eq!(role.permission_mode, Some(PermissionMode::Manual));
    }

    #[test]
    fn session_parameters_can_only_narrow_the_process_ceiling() {
        let config = config_with_roles();
        let process =
            ProcessPolicy::from_config(&config, Some("supervisor"), ExtensionsConfig::default())
                .unwrap();

        // Asking for a wide role or extra tools does not widen the ceiling.
        for params in [
            meta(serde_json::json!({"role": "wide"})),
            meta(serde_json::json!({"toolAllowlist": ["*"]})),
            meta(serde_json::json!({"toolAllowlist": ["Bash", "Read"]})),
        ] {
            let role = resolve_session_role(&config, &process, &params, None).unwrap();
            let allow = role.tool_allowlist.unwrap();
            assert!(!allow.allows("Bash"), "{params:?}");
            assert!(!allow.allows("Write"), "{params:?}");
        }

        // Narrowing works.
        let role = resolve_session_role(
            &config,
            &process,
            &meta(serde_json::json!({"role": "narrow"})),
            None,
        )
        .unwrap();
        let allow = role.tool_allowlist.unwrap();
        assert!(allow.allows("Read"));
        assert!(!allow.allows("Grep"));
        assert!(!allow.allows("host_task"));

        let role = resolve_session_role(
            &config,
            &process,
            &meta(serde_json::json!({"role": "none"})),
            None,
        )
        .unwrap();
        assert!(role.tool_allowlist.unwrap().is_empty());
    }

    #[test]
    fn unrestricted_process_lets_the_session_pick_its_own_ceiling() {
        let config = config_with_roles();
        let process =
            ProcessPolicy::from_config(&config, None, ExtensionsConfig::default()).unwrap();
        let role = resolve_session_role(
            &config,
            &process,
            &meta(serde_json::json!({"role": "narrow", "toolAllowlist": ["Read", "Bash"]})),
            None,
        )
        .unwrap();
        let allow = role.tool_allowlist.unwrap();
        assert!(allow.allows("Read"));
        assert!(!allow.allows("Bash"));
    }

    #[test]
    fn prompt_and_permission_precedence() {
        let config = config_with_roles();
        let process =
            ProcessPolicy::from_config(&config, Some("supervisor"), ExtensionsConfig::default())
                .unwrap();
        let acp_prompt = SystemPromptSpec {
            mode: SystemPromptMode::Append,
            text: "from acp".into(),
        };
        let role = resolve_session_role(
            &config,
            &process,
            &meta(serde_json::json!({"permissionMode": "yolo"})),
            Some(acp_prompt.clone()),
        )
        .unwrap();
        assert_eq!(role.system_prompt, Some(acp_prompt));
        assert_eq!(role.permission_mode, Some(PermissionMode::Yolo));
    }

    #[test]
    fn extensions_only_narrow() {
        let mut config = AppConfig::default();
        config.extensions.load_hooks = false;
        let process = ProcessPolicy::from_config(
            &config,
            None,
            ExtensionsConfig {
                load_plugins: false,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(!process.extensions.load_hooks);
        assert!(!process.extensions.load_plugins);
        assert!(process.extensions.load_project_instructions);

        let role = resolve_session_role(
            &config,
            &process,
            &meta(serde_json::json!({"extensions": {
                "loadHooks": true,
                "loadPlugins": true,
                "loadProjectInstructions": false
            }})),
            None,
        )
        .unwrap();
        assert!(!role.extensions.load_hooks, "cannot re-enable hooks");
        assert!(!role.extensions.load_plugins, "cannot re-enable plugins");
        assert!(!role.extensions.load_project_instructions);
    }

    #[test]
    fn role_prompt_phases_replace_and_append() {
        let new_session = || {
            Session::for_subagent(
                format!("role-apply-{}", uuid::Uuid::new_v4()),
                std::env::temp_dir(),
                PermissionMode::Auto,
                "m".into(),
            )
        };

        let replace = SessionRole {
            system_prompt: Some(SystemPromptSpec {
                mode: SystemPromptMode::Replace,
                text: "ROLE".into(),
            }),
            extensions: ExtensionsConfig::none(),
            ..Default::default()
        };
        let mut session = new_session();
        assert!(session.load_project_instructions);
        replace.apply_before_context(&mut session);
        assert_eq!(session.system_prompt, "ROLE");
        assert!(!session.load_project_instructions);
        session.system_prompt.push_str("\nCTX");
        replace.apply_after_context(&mut session);
        assert_eq!(session.system_prompt, "ROLE\nCTX");
        let _ = std::fs::remove_dir_all(session.session_dir());

        let append = SessionRole {
            system_prompt: Some(SystemPromptSpec {
                mode: SystemPromptMode::Append,
                text: "EXTRA".into(),
            }),
            ..Default::default()
        };
        let mut session = new_session();
        let builtin = session.system_prompt.clone();
        append.apply_before_context(&mut session);
        assert_eq!(session.system_prompt, builtin);
        session.system_prompt.push_str("\nCTX");
        append.apply_after_context(&mut session);
        assert_eq!(session.system_prompt, format!("{builtin}\nCTX\n\nEXTRA"));
        let _ = std::fs::remove_dir_all(session.session_dir());
    }

    #[test]
    fn permits_tool_follows_the_allowlist() {
        let open = SessionRole::default();
        assert!(open.permits_tool("Bash"));
        let strict = SessionRole {
            tool_allowlist: Some(ToolAllowlist::new(["Read"])),
            ..Default::default()
        };
        assert!(strict.permits_tool("Read"));
        assert!(!strict.permits_tool("Bash"));
    }

    #[test]
    fn unknown_roles_and_bad_values_are_errors() {
        let config = config_with_roles();
        assert_eq!(
            ProcessPolicy::from_config(&config, Some("ghost"), ExtensionsConfig::default())
                .unwrap_err(),
            RoleError::UnknownRole("ghost".into())
        );
        let process =
            ProcessPolicy::from_config(&config, None, ExtensionsConfig::default()).unwrap();
        assert_eq!(
            resolve_session_role(
                &config,
                &process,
                &meta(serde_json::json!({"role": "ghost"})),
                None
            )
            .unwrap_err(),
            RoleError::UnknownRole("ghost".into())
        );
        assert!(matches!(
            resolve_session_role(
                &config,
                &process,
                &meta(serde_json::json!({"permissionMode": "root"})),
                None
            ),
            Err(RoleError::InvalidPermissionMode(_))
        ));
        assert!(matches!(
            resolve_session_role(
                &config,
                &process,
                &meta(serde_json::json!({"toolAllowlist": ["Re*ad"]})),
                None
            ),
            Err(RoleError::InvalidToolPattern(_))
        ));
    }
}
