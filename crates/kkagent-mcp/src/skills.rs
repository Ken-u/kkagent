use anyhow::Result;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct SkillInfo {
    pub name: String,
    pub path: PathBuf,
    pub description: String,
}

pub struct SkillsManager {
    skills: Vec<SkillInfo>,
}

impl SkillsManager {
    pub fn new() -> Self {
        Self { skills: Vec::new() }
    }

    /// Discover skills from the standard locations:
    /// 1. ~/.kkagent/skills/
    /// 2. .kkagent/skills/ (project-local)
    /// 3. AGENTS.md in project root
    pub async fn discover(&mut self, working_dir: &Path) -> Result<()> {
        self.skills.clear();

        // Global skills
        let global_dir = kkagent_config::default_config_dir().join("skills");
        if global_dir.exists() {
            self.scan_skills_dir(&global_dir).await?;
        }

        // Project-local skills
        let local_dir = working_dir.join(".kkagent").join("skills");
        if local_dir.exists() {
            self.scan_skills_dir(&local_dir).await?;
        }

        // AGENTS.md
        let agents_md = working_dir.join("AGENTS.md");
        if agents_md.exists() {
            if let Ok(content) = tokio::fs::read_to_string(&agents_md).await {
                self.skills.push(SkillInfo {
                    name: "AGENTS.md".to_string(),
                    path: agents_md,
                    description: content
                        .lines()
                        .next()
                        .unwrap_or("Project agents file")
                        .to_string(),
                });
            }
        }

        tracing::info!("Discovered {} skills", self.skills.len());
        Ok(())
    }

    async fn scan_skills_dir(&mut self, dir: &Path) -> Result<()> {
        let mut entries = tokio::fs::read_dir(dir).await?;
        while let Some(entry) = entries.next_entry().await? {
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
                        .to_string();
                    let name = path
                        .file_name()
                        .map(|n| n.to_string_lossy().to_string())
                        .unwrap_or_default();
                    self.skills.push(SkillInfo {
                        name,
                        path: skill_file,
                        description,
                    });
                }
            }
        }
        Ok(())
    }

    pub fn list(&self) -> &[SkillInfo] {
        &self.skills
    }

    pub async fn load_skill(&self, name: &str) -> Result<String> {
        let skill = self
            .skills
            .iter()
            .find(|s| s.name == name)
            .ok_or_else(|| anyhow::anyhow!("Skill not found: {}", name))?;
        let content = tokio::fs::read_to_string(&skill.path).await?;
        Ok(content)
    }
}
