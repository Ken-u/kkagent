//! Tool display schemas for TUI chip/summary rendering.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::HashMap;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolDisplaySchema {
    pub chip_template: String,
    pub summary_mode: SummaryMode,
    #[serde(default)]
    pub highlight: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SummaryMode {
    Text,
    Diff,
    Bash,
    Grep,
    Json,
    Media,
    Goal,
}

pub fn builtin_display_schemas() -> HashMap<String, ToolDisplaySchema> {
    let mut m = HashMap::new();
    let add =
        |m: &mut HashMap<String, ToolDisplaySchema>, name: &str, chip: &str, mode: SummaryMode| {
            m.insert(
                name.into(),
                ToolDisplaySchema {
                    chip_template: chip.into(),
                    summary_mode: mode,
                    highlight: None,
                },
            );
        };
    add(&mut m, "Bash", "$ {command}", SummaryMode::Bash);
    add(&mut m, "Read", "Read {path}", SummaryMode::Text);
    add(&mut m, "Write", "Write {path}", SummaryMode::Diff);
    add(&mut m, "Edit", "Edit {path}", SummaryMode::Diff);
    add(&mut m, "Grep", "grep {pattern}", SummaryMode::Grep);
    add(&mut m, "Glob", "glob {pattern}", SummaryMode::Text);
    add(&mut m, "ReadMediaFile", "media {path}", SummaryMode::Media);
    add(&mut m, "CreateGoal", "goal+", SummaryMode::Goal);
    add(&mut m, "GetGoal", "goal?", SummaryMode::Goal);
    add(&mut m, "UpdateGoal", "goal!", SummaryMode::Goal);
    add(&mut m, "SetGoalBudget", "goal$", SummaryMode::Goal);
    add(&mut m, "WebSearch", "web {query}", SummaryMode::Text);
    add(&mut m, "FetchURL", "fetch {url}", SummaryMode::Text);
    m
}

pub fn render_chip(tool: &str, input: &Value) -> String {
    let schemas = builtin_display_schemas();
    if let Some(schema) = schemas.get(tool) {
        let mut s = schema.chip_template.clone();
        if let Some(obj) = input.as_object() {
            for (k, v) in obj {
                let val = v
                    .as_str()
                    .map(|x| x.to_string())
                    .unwrap_or_else(|| v.to_string());
                let short: String = val.chars().take(48).collect();
                s = s.replace(&format!("{{{k}}}"), &short);
            }
        }
        // Drop unresolved placeholders.
        while let Some(start) = s.find('{') {
            if let Some(end) = s[start..].find('}') {
                s.replace_range(start..start + end + 1, "");
            } else {
                break;
            }
        }
        return s.trim().to_string();
    }
    format!("{tool}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn chip_bash() {
        let c = render_chip("Bash", &json!({"command": "ls -la"}));
        assert!(c.contains("ls -la"));
    }
}
