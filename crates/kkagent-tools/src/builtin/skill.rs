use async_trait::async_trait;
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::{Tool, ToolContext, ToolOutput};

const MAX_SKILL_BYTES: u64 = 256 * 1024;
const MAX_RESOURCE_BYTES: u64 = 1024 * 1024;
const MAX_RESOURCES: usize = 128;

#[derive(Debug, Clone)]
pub struct SkillEntry {
    pub name: String,
    pub root: PathBuf,
    pub path: PathBuf,
    pub description: String,
    pub version: Option<String>,
    pub triggers: Vec<String>,
    pub resources: Vec<String>,
}

/// Per-workspace skill discovery. Catalog operations rescan on every call so a
/// long-running server sees skill edits without a restart.
pub struct SkillCatalog {
    default_working_dir: PathBuf,
    extra_dirs: Vec<PathBuf>,
    merge_all: bool,
}

impl SkillCatalog {
    pub fn new() -> Self {
        Self {
            default_working_dir: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
            extra_dirs: Vec::new(),
            merge_all: false,
        }
    }

    pub async fn discover(working_dir: &Path) -> Self {
        Self::configured(working_dir, &[], false).await
    }

    pub async fn configured(working_dir: &Path, extra_dirs: &[String], merge_all: bool) -> Self {
        if let Err(error) = ensure_builtin_skills().await {
            tracing::warn!("builtin skills: {error}");
        }
        let extra_dirs = extra_dirs
            .iter()
            .map(|value| {
                let path = PathBuf::from(value);
                if path.is_absolute() {
                    path
                } else {
                    working_dir.join(path)
                }
            })
            .collect();
        Self {
            default_working_dir: working_dir.to_path_buf(),
            extra_dirs,
            merge_all,
        }
    }

    pub async fn list(&self) -> Vec<SkillEntry> {
        self.list_for(&self.default_working_dir).await
    }

    pub async fn list_for(&self, working_dir: &Path) -> Vec<SkillEntry> {
        let mut entries = BTreeMap::<String, SkillEntry>::new();
        // Later roots override earlier roots: workspace > compatibility > extra > user.
        let mut roots = vec![kkagent_config::default_config_dir().join("skills")];
        roots.extend(self.extra_dirs.iter().cloned());
        roots.push(working_dir.join(".kimi").join("skills"));
        roots.push(working_dir.join(".kkagent").join("skills"));
        for root in roots {
            scan_dir(&root, &mut entries).await;
        }
        entries.into_values().collect()
    }

    pub async fn load_for(
        &self,
        working_dir: &Path,
        name: &str,
    ) -> anyhow::Result<(SkillEntry, String)> {
        let entry = self
            .list_for(working_dir)
            .await
            .into_iter()
            .find(|entry| entry.name == name)
            .ok_or_else(|| anyhow::anyhow!("Skill not found: {name}"))?;
        let content = read_bounded_utf8(&entry.path, MAX_SKILL_BYTES).await?;
        Ok((entry, content))
    }

    pub async fn read_resource_for(
        &self,
        working_dir: &Path,
        name: &str,
        resource: &str,
    ) -> anyhow::Result<String> {
        let (entry, _) = self.load_for(working_dir, name).await?;
        if resource.is_empty() || Path::new(resource).is_absolute() {
            anyhow::bail!("skill resource must be a non-empty relative path");
        }
        let root = tokio::fs::canonicalize(&entry.root).await?;
        let requested = tokio::fs::canonicalize(entry.root.join(resource)).await?;
        if !requested.starts_with(&root) || requested == entry.path {
            anyhow::bail!("skill resource escapes its skill directory");
        }
        read_bounded_utf8(&requested, MAX_RESOURCE_BYTES).await
    }

    pub async fn catalog_prompt_section(&self) -> String {
        self.catalog_prompt_section_for(&self.default_working_dir)
            .await
    }

    pub async fn catalog_prompt_section_for(&self, working_dir: &Path) -> String {
        let entries = self.list_for(working_dir).await;
        if entries.is_empty() {
            return String::new();
        }
        let mut output = String::from(
            "\n\n# Available Skills\n\nUse the Skill tool to load a skill by name when relevant.\n",
        );
        for entry in entries {
            output.push_str(&format!("- `{}`: {}\n", entry.name, entry.description));
            if self.merge_all {
                if let Ok(content) = read_bounded_utf8(&entry.path, MAX_SKILL_BYTES).await {
                    output.push_str(&format!(
                        "\n<skill name=\"{}\">\n{}\n</skill>\n",
                        entry.name, content
                    ));
                }
            }
        }
        output
    }
}

impl Default for SkillCatalog {
    fn default() -> Self {
        Self::new()
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
        let directory = root.join(name);
        let file = directory.join("SKILL.md");
        if !file.exists() {
            tokio::fs::create_dir_all(&directory).await?;
            tokio::fs::write(&file, body).await?;
        }
    }
    Ok(())
}

async fn scan_dir(root: &Path, output: &mut BTreeMap<String, SkillEntry>) {
    let Ok(canonical_root) = tokio::fs::canonicalize(root).await else {
        return;
    };
    let Ok(mut directory) = tokio::fs::read_dir(&canonical_root).await else {
        return;
    };
    while let Ok(Some(item)) = directory.next_entry().await {
        let path = item.path();
        let Ok(file_type) = item.file_type().await else {
            continue;
        };
        if !file_type.is_dir() || file_type.is_symlink() {
            continue;
        }
        let skill_file = path.join("SKILL.md");
        let Ok(canonical_skill) = tokio::fs::canonicalize(&skill_file).await else {
            continue;
        };
        if !canonical_skill.starts_with(&canonical_root) {
            continue;
        }
        let Ok(content) = read_bounded_utf8(&canonical_skill, MAX_SKILL_BYTES).await else {
            tracing::warn!("invalid skill ignored: {}", skill_file.display());
            continue;
        };
        let directory_name = path
            .file_name()
            .map(|name| name.to_string_lossy().to_string())
            .unwrap_or_default();
        let metadata = parse_frontmatter(&content);
        let name = metadata.name.unwrap_or(directory_name);
        if !valid_skill_name(&name) {
            tracing::warn!("invalid skill name ignored: {name}");
            continue;
        }
        let description = metadata.description.unwrap_or_else(|| {
            content
                .lines()
                .find(|line| !line.trim().is_empty() && !line.starts_with('#') && *line != "---")
                .unwrap_or("No description")
                .trim()
                .to_string()
        });
        let resources = list_resources(&path, &canonical_skill);
        output.insert(
            name.clone(),
            SkillEntry {
                name,
                root: path,
                path: canonical_skill,
                description,
                version: metadata.version,
                triggers: metadata.triggers,
                resources,
            },
        );
    }
}

fn list_resources(root: &Path, skill_file: &Path) -> Vec<String> {
    let mut resources = walkdir::WalkDir::new(root)
        .follow_links(false)
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file() && entry.path() != skill_file)
        .filter_map(|entry| {
            entry
                .path()
                .strip_prefix(root)
                .ok()
                .map(|path| path.to_string_lossy().replace('\\', "/"))
        })
        .take(MAX_RESOURCES)
        .collect::<Vec<_>>();
    resources.sort();
    resources
}

async fn read_bounded_utf8(path: &Path, limit: u64) -> anyhow::Result<String> {
    let metadata = tokio::fs::metadata(path).await?;
    if !metadata.is_file() || metadata.len() > limit {
        anyhow::bail!("file exceeds the {} byte skill limit", limit);
    }
    Ok(tokio::fs::read_to_string(path).await?)
}

#[derive(Default)]
struct SkillMetadata {
    name: Option<String>,
    description: Option<String>,
    version: Option<String>,
    triggers: Vec<String>,
}

fn parse_frontmatter(content: &str) -> SkillMetadata {
    let mut lines = content.lines();
    if lines.next().map(str::trim) != Some("---") {
        return SkillMetadata::default();
    }
    let mut metadata = SkillMetadata::default();
    for line in lines {
        let line = line.trim();
        if line == "---" {
            break;
        }
        let Some((key, value)) = line.split_once(':') else {
            continue;
        };
        let value = value.trim().trim_matches(['\'', '"']);
        match key.trim() {
            "name" => metadata.name = Some(value.to_string()),
            "description" => metadata.description = Some(value.to_string()),
            "version" => metadata.version = Some(value.to_string()),
            "triggers" => {
                metadata.triggers = value
                    .trim_matches(['[', ']'])
                    .split(',')
                    .map(|item| item.trim().trim_matches(['\'', '"']).to_string())
                    .filter(|item| !item.is_empty())
                    .collect();
            }
            _ => {}
        }
    }
    metadata
}

fn valid_skill_name(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 128
        && name
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
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
        "Load workspace-specific skill instructions or a text resource referenced by a skill."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "Skill name from the available skills list"},
                "args": {"type": "string", "description": "Optional invocation context"},
                "resource": {"type": "string", "description": "Optional relative text resource path within the skill directory"}
            },
            "required": ["name"]
        })
    }

    fn read_only(&self) -> bool {
        // Skill can inject arbitrary instructions / resources — ask like kimi.
        false
    }

    fn default_approve(&self) -> bool {
        false
    }

    async fn execute(&self, input: Value, context: &ToolContext) -> anyhow::Result<ToolOutput> {
        let name = input
            .get("name")
            .and_then(Value::as_str)
            .unwrap_or("")
            .trim();
        if name.is_empty() {
            let list = self.catalog.list_for(&context.working_dir).await;
            let lines = list
                .iter()
                .map(|entry| format!("- {}: {}", entry.name, entry.description))
                .collect::<Vec<_>>();
            return Ok(ToolOutput::success(if lines.is_empty() {
                "No skills discovered.".to_string()
            } else {
                format!("Available skills:\n{}", lines.join("\n"))
            }));
        }
        if let Some(resource) = input.get("resource").and_then(Value::as_str) {
            return Ok(
                match self
                    .catalog
                    .read_resource_for(&context.working_dir, name, resource)
                    .await
                {
                    Ok(content) => ToolOutput::success(format!(
                        "# Skill resource: {name}/{resource}\n\n{content}"
                    )),
                    Err(error) => ToolOutput::error(error.to_string()),
                },
            );
        }
        Ok(
            match self.catalog.load_for(&context.working_dir, name).await {
                Ok((entry, content)) => {
                    let args = input.get("args").and_then(Value::as_str).unwrap_or("");
                    let mut output = format!("# Skill: {}\n\n{}", entry.name, content);
                    if !entry.resources.is_empty() {
                        output.push_str(&format!(
                            "\n\n## Available resources\n\n{}",
                            entry
                                .resources
                                .iter()
                                .map(|path| format!("- `{path}`"))
                                .collect::<Vec<_>>()
                                .join("\n")
                        ));
                    }
                    if !args.is_empty() {
                        output.push_str(&format!("\n\n## Invoked with args\n\n{args}"));
                    }
                    ToolOutput::success(output)
                }
                Err(error) => ToolOutput::error(error.to_string()),
            },
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temporary_workspace() -> PathBuf {
        let path = std::env::temp_dir().join(format!("kkagent-skill-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[tokio::test]
    async fn discovers_workspace_skill_and_reloads_changes() {
        let workspace = temporary_workspace();
        let skill = workspace.join(".kkagent/skills/release");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: release\ndescription: First\nversion: 1\ntriggers: [ship, publish]\n---\nRun tests.",
        )
        .unwrap();
        let catalog = SkillCatalog::discover(&workspace).await;
        let first = catalog.list_for(&workspace).await;
        let release = first.iter().find(|entry| entry.name == "release").unwrap();
        assert_eq!(release.description, "First");
        assert_eq!(release.triggers, ["ship", "publish"]);

        std::fs::write(skill.join("SKILL.md"), "# release\n\nSecond").unwrap();
        let (_, content) = catalog.load_for(&workspace, "release").await.unwrap();
        assert!(content.contains("Second"));
        std::fs::remove_dir_all(workspace).unwrap();
    }

    #[tokio::test]
    async fn project_skill_overrides_global_extra_and_reads_bounded_resource() {
        let workspace = temporary_workspace();
        let extra = temporary_workspace();
        let extra_skill = extra.join("release");
        std::fs::create_dir_all(&extra_skill).unwrap();
        std::fs::write(extra_skill.join("SKILL.md"), "# release\n\nExtra").unwrap();
        let project_skill = workspace.join(".kkagent/skills/release");
        std::fs::create_dir_all(project_skill.join("references")).unwrap();
        std::fs::write(project_skill.join("SKILL.md"), "# release\n\nProject").unwrap();
        std::fs::write(project_skill.join("references/checklist.md"), "Checklist").unwrap();
        let catalog =
            SkillCatalog::configured(&workspace, &[extra.display().to_string()], false).await;
        let (_, content) = catalog.load_for(&workspace, "release").await.unwrap();
        assert!(content.contains("Project"));
        let resource = catalog
            .read_resource_for(&workspace, "release", "references/checklist.md")
            .await
            .unwrap();
        assert_eq!(resource, "Checklist");
        assert!(catalog
            .read_resource_for(&workspace, "release", "../SKILL.md")
            .await
            .is_err());
        std::fs::remove_dir_all(workspace).unwrap();
        std::fs::remove_dir_all(extra).unwrap();
    }
}
