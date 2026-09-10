//! Unified system-reminder injection helpers.

use std::path::Path;

pub fn interruption_reminder() -> String {
    "<system-reminder>\nThe previous turn was interrupted. Resume carefully without repeating completed work.\n</system-reminder>"
        .into()
}

pub fn plan_reminder(plan_path: &Path) -> String {
    format!(
        "<system-reminder>\nPlan mode is active. Only edit `{}`. Do not modify other files until ExitPlanMode.\n</system-reminder>",
        plan_path.display()
    )
}

pub fn todo_reminder() -> String {
    "<system-reminder>\nThe TodoList tool has not been updated recently. \
If you are working on multi-step tasks, consider updating TodoList. \
Do not mention this reminder to the user.\n</system-reminder>"
        .into()
}

pub fn agents_md_loaded(name: &str) -> String {
    format!(
        "<system-reminder>\nProject instructions from {name} are loaded into the system prompt.\n</system-reminder>"
    )
}

pub fn wrap(body: &str) -> String {
    format!("<system-reminder>\n{body}\n</system-reminder>")
}
