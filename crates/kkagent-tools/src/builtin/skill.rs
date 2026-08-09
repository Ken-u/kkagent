use async_trait::async_trait;
use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;

use crate::{Tool, ToolContext, ToolOutput};

#[derive(Debug, Clone)]
pub struct SkillEntry {
    pub name: String,
    pub path: PathBuf,
    pub description: String,
}

pub struct SkillCatalog {
    entries: Mutex<Vec<SkillEntry>>,
}

impl SkillCatalog {
    pub fn new() -> Self {
        Self {
            entries: Mutex::new(Vec::new()),
        }
    }

    pub async fn discover(working_dir: &std::path::Path) -> Self {
        let cat = Self::new();
        cat.rescan(working_dir).await;
        cat
    }

    pub async fn rescan(&self, working_dir: &std::path::Path) {
        let mut entries = Vec::new();
        // Ensure builtin sub-skills exist under ~/.kkagent/skills/
        if let Err(e) = ensure_builtin_skills().await {
            tracing::warn!("builtin skills: {e}");
        }
        let global = kkagent_config::default_config_dir().join("skills");
        scan_dir(&global, &mut entries).await;
        scan_dir(&working_dir.join(".kkagent").join("skills"), &mut entries).await;
        scan_dir(&working_dir.join(".kimi").join("skills"), &mut entries).await;
        *self.entries.lock().await = entries;
    }

    pub async fn list(&self) -> Vec<SkillEntry> {
        self.entries.lock().await.clone()
    }

    pub async fn load(&self, name: &str) -> anyhow::Result<(SkillEntry, String)> {
        let entries = self.entries.lock().await;
        let entry = entries
            .iter()
            .find(|e| e.name == name)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("Skill not found: {}", name))?;
        let content = tokio::fs::read_to_string(&entry.path).await?;
        Ok((entry, content))
    }

    pub async fn catalog_prompt_section(&self) -> String {
        let entries = self.list().await;
        if entries.is_empty() {
            return String::new();
        }
        let mut out = String::from(
            "\n\n# Available Skills\n\nUse the Skill tool to load a skill by name when relevant.\n",
        );
        for e in entries {
            out.push_str(&format!("- `{}`: {}\n", e.name, e.description));
        }
        out
    }
}

async fn ensure_builtin_skills() -> anyhow::Result<()> {
    let root = kkagent_config::default_config_dir().join("skills");
    let builtins = [
        (
            "consolidate",
            "# consolidate\n\nSummarize long threads into decisions, open questions, and next actions. Prefer bullets.\n",
        ),
        (
            "review",
            "# review\n\nReview code changes for correctness, security, and missing tests. Be specific about file:line.\n",
        ),
        (
            "write-goal",
            "# write-goal\n\nHelp craft a clear CreateGoal description with measurable done criteria and budgets.\n",
        ),
    ];
    for (name, body) in builtins {
        let dir = root.join(name);
        let file = dir.join("SKILL.md");
        if !file.exists() {
            tokio::fs::create_dir_all(&dir).await?;
            tokio::fs::write(&file, body).await?;
        }
    }
    Ok(())
}

async fn scan_dir(dir: &std::path::Path, out: &mut Vec<SkillEntry>) {
    if !dir.exists() {
        return;
    }
    let Ok(mut rd) = tokio::fs::read_dir(dir).await else {
        return;
    };
    while let Ok(Some(entry)) = rd.next_entry().await {
        let path = entry.path();
        if path.is_dir() {
            let skill_file = path.join("SKILL.md");
            if skill_file.exists() {
                let content = tokio::fs::read_to_string(&skill_file)
                    .await
                    .unwrap_or_default();
                let description = content
                    .lines()
                    .find(|l| !l.trim().is_empty() && !l.starts_with('#'))
                    .unwrap_or("No description")
                    .trim()
                    .to_string();
                let name = path
                    .file_name()
                    .map(|n| n.to_string_lossy().to_string())
                    .unwrap_or_default();
                out.push(SkillEntry {
                    name,
                    path: skill_file,
                    description,
                });
            }
        }
    }
}

pub struct SkillTool {
    catalog: Arc<SkillCatalog>,
}

impl SkillTool {
    pub fn new(catalog: Arc<SkillCatalog>) -> Self {
        Self { catalog }
    }
}

#[async_trait]
impl Tool for SkillTool {
    fn name(&self) -> &str {
        "Skill"
    }

    fn description(&self) -> &str {
        "Load a skill by name and return its instructions. Use when a listed skill matches the task."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "Skill name from the available skills list"},
                "args": {"type": "string", "description": "Optional arguments / context for the skill"}
            },
            "required": ["name"]
        })
    }

    fn read_only(&self) -> bool {
        true
    }

    async fn execute(&self, input: Value, _ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        let name = input
            .get("name")
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .trim();
        if name.is_empty() {
            let list = self.catalog.list().await;
            if list.is_empty() {
                return Ok(ToolOutput::success("No skills discovered."));
            }
            let lines: Vec<String> = list
                .iter()
                .map(|e| format!("- {}: {}", e.name, e.description))
                .collect();
            return Ok(ToolOutput::success(format!(
                "Available skills:\n{}",
                lines.join("\n")
            )));
        }
        match self.catalog.load(name).await {
            Ok((entry, content)) => {
                let args = input.get("args").and_then(|v| v.as_str()).unwrap_or("");
                let mut out = format!("# Skill: {}\n\n{}", entry.name, content);
                if !args.is_empty() {
                    out.push_str(&format!("\n\n## Invoked with args\n\n{}", args));
                }
                Ok(ToolOutput::success(out))
            }
            Err(e) => Ok(ToolOutput::error(e.to_string())),
        }
    }
}
