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
            || self.aliases.iter().any(|a| a.starts_with(q.as_str()) || a.contains(q.as_str()))
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
        description: "Cycle permission mode (manual → yolo → auto)",
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
        description: "Show or set thinking effort",
        priority: 95,
        argument_hint: Some("[off|low|medium|high]"),
    },
    SlashCommand {
        name: "help",
        aliases: &["h", "?"],
        description: "Show available commands and shortcuts",
        priority: 80,
        argument_hint: None,
    },
    SlashCommand {
        name: "new",
        aliases: &["clear"],
        description: "Start a fresh session in the current workspace",
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
        description: "Withdraw the last prompt from the transcript",
        priority: 80,
        argument_hint: None,
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
        description: "Show current session and runtime status",
        priority: 60,
        argument_hint: None,
    },
    SlashCommand {
        name: "usage",
        aliases: &[],
        description: "Show session tokens and context window usage",
        priority: 60,
        argument_hint: None,
    },
    SlashCommand {
        name: "mcp",
        aliases: &[],
        description: "Show MCP server status",
        priority: 60,
        argument_hint: None,
    },
    SlashCommand {
        name: "tasks",
        aliases: &["task"],
        description: "Browse background tasks / subagents",
        priority: 60,
        argument_hint: None,
    },
    SlashCommand {
        name: "config",
        aliases: &[],
        description: "Show runtime configuration summary",
        priority: 50,
        argument_hint: None,
    },
    SlashCommand {
        name: "auth",
        aliases: &[],
        description: "Show auth / API key status (no secrets)",
        priority: 50,
        argument_hint: None,
    },
    SlashCommand {
        name: "plugins",
        aliases: &["plugin"],
        description: "List loaded plugins",
        priority: 50,
        argument_hint: None,
    },
    SlashCommand {
        name: "skills",
        aliases: &["skill"],
        description: "List available skills",
        priority: 50,
        argument_hint: Some("[name]"),
    },
    SlashCommand {
        name: "swarm",
        aliases: &[],
        description: "Show subagent swarm / task roster",
        priority: 50,
        argument_hint: None,
    },
    SlashCommand {
        name: "provider",
        aliases: &["providers"],
        description: "List LLM providers and models",
        priority: 50,
        argument_hint: None,
    },
    SlashCommand {
        name: "reload",
        aliases: &[],
        description: "Reload config from disk (next turn)",
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
        description: "System info (version/paths/model/tokens)",
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
        description: "Add a BTW note / reminder",
        priority: 40,
        argument_hint: Some("<note>"),
    },
    SlashCommand {
        name: "prompts",
        aliases: &["prompt"],
        description: "List prompt templates",
        priority: 40,
        argument_hint: None,
    },
    SlashCommand {
        name: "experimental-flags",
        aliases: &["flags"],
        description: "Show experimental feature flags",
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
    let q = query.trim_start_matches('/').split_whitespace().next().unwrap_or("").to_lowercase();

    let mut matched: Vec<&SlashCommand> = BUILTIN_SLASH_COMMANDS
        .iter()
        .filter(|c| c.matches(&q))
        .collect();

    matched.sort_by(|a, b| {
        // Prefer prefix matches, then priority, then name
        let a_prefix = a.name.starts_with(&q) as i32;
        let b_prefix = b.name.starts_with(&q) as i32;
        b_prefix
            .cmp(&a_prefix)
            .then(b.priority.cmp(&a.priority))
            .then(a.name.cmp(b.name))
    });

    matched
        .into_iter()
        .map(|c| SlashSuggestion {
            name: c.name.to_string(),
            description: c.description.to_string(),
            argument_hint: c.argument_hint.map(String::from),
        })
        .collect()
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
