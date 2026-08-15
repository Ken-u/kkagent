//! Slash command registry — mirrors kimi-code `BUILTIN_SLASH_COMMANDS`
//! (excluding OAuth / Web / VSCode / plugin-market for v1).

#[derive(Debug, Clone, Copy)]
pub struct SlashCommand {
    pub name: &'static str,
    pub aliases: &'static [&'static str],
    pub description: &'static str,
    pub priority: i32,
    pub argument_hint: Option<&'static str>,
}

impl SlashCommand {
    pub fn matches(&self, query: &str) -> bool {
        let q = query.to_lowercase();
        if q.is_empty() {
            return true;
        }
        self.name.starts_with(&q)
            || self.name.contains(&q)
            || self
                .aliases
                .iter()
                .any(|a| a.starts_with(q.as_str()) || a.contains(q.as_str()))
    }

    pub fn accepts_name(&self, name: &str) -> bool {
        let n = name.to_lowercase();
        self.name == n || self.aliases.iter().any(|a| *a == n)
    }
}

/// Built-in slash commands (kimi-aligned, v1 CLI scope).
pub const BUILTIN_SLASH_COMMANDS: &[SlashCommand] = &[
    SlashCommand {
        name: "yolo",
        aliases: &["yes"],
        description: "Toggle YOLO mode: auto-approve tool actions, but the agent may still ask",
        priority: 101,
        argument_hint: None,
    },
    SlashCommand {
        name: "auto",
        aliases: &[],
        description: "Toggle Auto mode: fully autonomous, agent decides without asking",
        priority: 99,
        argument_hint: None,
    },
    SlashCommand {
        name: "permission",
        aliases: &[],
        description: "Choose permission mode (manual / yolo / auto)",
        priority: 100,
        argument_hint: None,
    },
    SlashCommand {
        name: "plan",
        aliases: &[],
        description: "Toggle plan mode (read-only planning)",
        priority: 100,
        argument_hint: Some("[clear]"),
    },
    SlashCommand {
        name: "model",
        aliases: &[],
        description: "Show or switch LLM model",
        priority: 100,
        argument_hint: Some("<model>"),
    },
    SlashCommand {
        name: "effort",
        aliases: &["thinking"],
        description: "Show or switch thinking effort",
        priority: 95,
        argument_hint: Some("[off|low|medium|high]"),
    },
    SlashCommand {
        name: "help",
        aliases: &["h", "?"],
        description: "Browse commands (opens picker)",
        priority: 80,
        argument_hint: None,
    },
    SlashCommand {
        name: "new",
        aliases: &["clear"],
        description: "Start a fresh session (keeps previous running; Tab to switch)",
        priority: 80,
        argument_hint: None,
    },
    SlashCommand {
        name: "sessions",
        aliases: &["resume"],
        description: "Browse and resume sessions",
        priority: 80,
        argument_hint: Some("[<session_id>]"),
    },
    SlashCommand {
        name: "compact",
        aliases: &[],
        description: "Compact the conversation context",
        priority: 80,
        argument_hint: Some("<instruction>"),
    },
    SlashCommand {
        name: "goal",
        aliases: &[],
        description: "Start or manage an autonomous goal",
        priority: 80,
        argument_hint: Some("[status|pause|resume|cancel] | <objective>"),
    },
    SlashCommand {
        name: "undo",
        aliases: &[],
        description: "Choose a prior prompt, fork before it, and edit",
        priority: 80,
        argument_hint: Some("[turn_count]"),
    },
    SlashCommand {
        name: "timeline",
        aliases: &["tl"],
        description: "Show compact turn timeline (step / tool / retries)",
        priority: 55,
        argument_hint: None,
    },
    SlashCommand {
        name: "edit",
        aliases: &["rerun"],
        description: "Load last user prompt into the composer to edit and re-run",
        priority: 75,
        argument_hint: Some("[fork]"),
    },
    SlashCommand {
        name: "init",
        aliases: &[],
        description: "Analyze the codebase and generate AGENTS.md",
        priority: 70,
        argument_hint: None,
    },
    SlashCommand {
        name: "title",
        aliases: &["rename"],
        description: "Set or show session title",
        priority: 60,
        argument_hint: Some("<title>"),
    },
    SlashCommand {
        name: "status",
        aliases: &[],
        description: "Session / runtime status panel",
        priority: 60,
        argument_hint: None,
    },
    SlashCommand {
        name: "usage",
        aliases: &[],
        description: "Context window usage panel",
        priority: 60,
        argument_hint: None,
    },
    SlashCommand {
        name: "mcp",
        aliases: &[],
        description: "Manage MCP servers (enable/disable, persisted)",
        priority: 60,
        argument_hint: None,
    },
    SlashCommand {
        name: "tasks",
        aliases: &["task"],
        description: "Browse background tasks",
        priority: 60,
        argument_hint: None,
    },
    SlashCommand {
        name: "agents",
        aliases: &["agent"],
        description: "Browse subagent status and recent activity",
        priority: 60,
        argument_hint: None,
    },
    SlashCommand {
        name: "config",
        aliases: &[],
        description: "Open configuration menu",
        priority: 50,
        argument_hint: None,
    },
    SlashCommand {
        name: "auth",
        aliases: &[],
        description: "Browse API key / auth status",
        priority: 50,
        argument_hint: None,
    },
    SlashCommand {
        name: "plugins",
        aliases: &["plugin"],
        description: "Browse, install, and manage plugins",
        priority: 50,
        argument_hint: Some("[marketplace|install|update|enable|disable|remove|info|reload]"),
    },
    SlashCommand {
        name: "skills",
        aliases: &[],
        description: "Manage skills (enable/disable) or /skills <name> to activate",
        priority: 50,
        argument_hint: Some("[name]"),
    },
    SlashCommand {
        name: "swarm",
        aliases: &[],
        description: "Swarm mode and background tasks",
        priority: 50,
        argument_hint: Some("[enter|exit]"),
    },
    SlashCommand {
        name: "provider",
        aliases: &["providers"],
        description: "Pick an LLM provider, then a model",
        priority: 50,
        argument_hint: None,
    },
    SlashCommand {
        name: "reload",
        aliases: &[],
        description: "Reload config.toml from disk",
        priority: 50,
        argument_hint: None,
    },
    SlashCommand {
        name: "web",
        aliases: &[],
        description: "Queue a web search prompt",
        priority: 40,
        argument_hint: Some("<query>"),
    },
    SlashCommand {
        name: "info",
        aliases: &[],
        description: "Session / system status panel",
        priority: 40,
        argument_hint: None,
    },
    SlashCommand {
        name: "add-dir",
        aliases: &["add_dir"],
        description: "Note an extra work directory for this session",
        priority: 40,
        argument_hint: Some("<path>"),
    },
    SlashCommand {
        name: "btw",
        aliases: &[],
        description: "Open BTW workspace and ask a side question",
        priority: 55,
        argument_hint: Some("<question>"),
    },
    SlashCommand {
        name: "fork",
        aliases: &[],
        description: "Fork current session (stay on original; switch via /sessions)",
        priority: 55,
        argument_hint: Some("[title]"),
    },
    SlashCommand {
        name: "search",
        aliases: &["find"],
        description: "Search messages in the transcript (Ctrl-F)",
        priority: 45,
        argument_hint: Some("<query>"),
    },
    SlashCommand {
        name: "prompts",
        aliases: &["prompt"],
        description: "Browse prompt templates",
        priority: 40,
        argument_hint: None,
    },
    SlashCommand {
        name: "experimental-flags",
        aliases: &["flags"],
        description: "Browse experimental feature flags",
        priority: 30,
        argument_hint: None,
    },
    SlashCommand {
        name: "copy",
        aliases: &[],
        description: "Copy the last assistant message to the clipboard",
        priority: 40,
        argument_hint: None,
    },
    SlashCommand {
        name: "export-md",
        aliases: &["export"],
        description: "Export current session as Markdown",
        priority: 40,
        argument_hint: None,
    },
    SlashCommand {
        name: "version",
        aliases: &[],
        description: "Show version information",
        priority: 20,
        argument_hint: None,
    },
    SlashCommand {
        name: "exit",
        aliases: &["quit", "q"],
        description: "Exit the application",
        priority: 20,
        argument_hint: None,
    },
];

#[derive(Debug, Clone)]
pub struct SlashSuggestion {
    pub name: String,
    pub description: String,
    pub argument_hint: Option<String>,
}

/// Filter + sort commands for the `/` autocomplete popup.
pub fn filter_slash_commands(query: &str) -> Vec<SlashSuggestion> {
    filter_slash_commands_with_extras(query, &[])
}

/// Like [`filter_slash_commands`], also matching dynamic skill slash entries.
pub fn filter_slash_commands_with_extras(
    query: &str,
    extras: &[SlashSuggestion],
) -> Vec<SlashSuggestion> {
    let q = query
        .trim_start_matches('/')
        .split_whitespace()
        .next()
        .unwrap_or("")
        .to_lowercase();

    let mut matched: Vec<SlashSuggestion> = BUILTIN_SLASH_COMMANDS
        .iter()
        .filter(|c| c.matches(&q))
        .map(|c| SlashSuggestion {
            name: c.name.to_string(),
            description: c.description.to_string(),
            argument_hint: c.argument_hint.map(String::from),
        })
        .collect();

    for extra in extras {
        let name_l = extra.name.to_lowercase();
        if q.is_empty()
            || name_l.starts_with(&q)
            || name_l.contains(&q)
            || extra.description.to_lowercase().contains(&q)
        {
            matched.push(extra.clone());
        }
    }

    matched.sort_by(|a, b| {
        let a_prefix = a.name.to_lowercase().starts_with(&q) as i32;
        let b_prefix = b.name.to_lowercase().starts_with(&q) as i32;
        let a_pri = find_slash_command(&a.name)
            .map(|c| c.priority)
            .unwrap_or(40);
        let b_pri = find_slash_command(&b.name)
            .map(|c| c.priority)
            .unwrap_or(40);
        b_prefix
            .cmp(&a_prefix)
            .then(b_pri.cmp(&a_pri))
            .then(a.name.cmp(&b.name))
    });

    matched
}

/// Build kimi-style skill slash commands (`skill:name`).
pub fn build_skill_slash_commands(
    skills: &[(String, String)],
) -> (
    Vec<SlashSuggestion>,
    std::collections::HashMap<String, String>,
) {
    let mut command_map = std::collections::HashMap::new();
    let mut commands = Vec::new();
    let mut sorted = skills.to_vec();
    sorted.sort_by(|a, b| a.0.cmp(&b.0));
    for (name, description) in sorted {
        let command_name = format!("skill:{name}");
        command_map.insert(command_name.clone(), name.clone());
        // Also allow bare `/name` when it doesn't collide with builtins.
        if find_slash_command(&name).is_none() {
            command_map.insert(name.clone(), name.clone());
            commands.push(SlashSuggestion {
                name: name.clone(),
                description: description.clone(),
                argument_hint: None,
            });
        }
        commands.push(SlashSuggestion {
            name: command_name,
            description,
            argument_hint: None,
        });
    }
    (commands, command_map)
}

pub fn find_slash_command(name: &str) -> Option<&'static SlashCommand> {
    BUILTIN_SLASH_COMMANDS.iter().find(|c| c.accepts_name(name))
}

/// Parse `/cmd args` → (name, args). Returns None if not a slash command.
pub fn parse_slash_input(text: &str) -> Option<(String, String)> {
    let trimmed = text.trim();
    if !trimmed.starts_with('/') {
        return None;
    }
    let rest = &trimmed[1..];
    let mut parts = rest.splitn(2, char::is_whitespace);
    let name = parts.next()?.trim().to_lowercase();
    if name.is_empty() {
        return Some((String::new(), String::new()));
    }
    let args = parts.next().unwrap_or("").trim().to_string();
    Some((name, args))
}

/// True when the input is still completing the command name (no args yet).
pub fn is_slash_name_completion(text: &str) -> bool {
    let trimmed = text.trim_start();
    if !trimmed.starts_with('/') {
        return false;
    }
    // `/` alone or `/partial` without trailing space after a full token
    let after = &trimmed[1..];
    !after.contains(char::is_whitespace)
}
