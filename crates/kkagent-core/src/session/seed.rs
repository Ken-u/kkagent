//! Session seed adapters — wire catalog / prompt sections into a new session.

use crate::session::profile_catalog::SessionAgentProfileCatalog;
use crate::session::skill_catalog::{SessionSkillCatalog, SkillCatalogEntry};

pub struct SessionSeed;

impl SessionSeed {
    pub fn seed_skills(catalog: &SessionSkillCatalog, entries: Vec<SkillCatalogEntry>) {
        catalog.set(entries);
    }

    pub fn seed_default_profiles(catalog: &SessionAgentProfileCatalog) {
        // SessionAgentProfileCatalog::new already seeds coder/explorer.
        let _ = catalog.list();
    }

    pub fn combine_prompt_sections(parts: &[String]) -> String {
        parts
            .iter()
            .filter(|p| !p.trim().is_empty())
            .cloned()
            .collect::<Vec<_>>()
            .join("")
    }
}
