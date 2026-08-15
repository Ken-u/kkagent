use crate::{Tool, ToolContext, ToolOutput};
use async_trait::async_trait;
use serde_json::{json, Value};

/// Replace the current session plan without exposing its host-managed path to
/// the model. The path is supplied only through `ToolContext` by the runtime.
pub struct WritePlanTool;

#[async_trait]
impl Tool for WritePlanTool {
    fn name(&self) -> &str {
        "WritePlan"
    }

    fn description(&self) -> &str {
        "Write or replace the complete plan document while plan mode is active. \
The host chooses the session-scoped path; do not pass a path. The content must start with \
`# <plan name>`, followed by concrete implementation steps and validation. Call this tool again \
with the full revised document after feedback, then call ExitPlanMode for user approval."
    }

    fn parameters_schema(&self) -> Value {
        json!({
            "type": "object",
            "properties": {
                "content": {
                    "type": "string",
                    "minLength": 1,
                    "description": "Complete Markdown plan. The first line must be a level-1 title: `# <plan name>`."
                }
            },
            "required": ["content"],
            "additionalProperties": false
        })
    }

    async fn execute(&self, input: Value, ctx: &ToolContext) -> anyhow::Result<ToolOutput> {
        let content = input
            .get("content")
            .and_then(Value::as_str)
            .ok_or_else(|| anyhow::anyhow!("Missing 'content'"))?;
        if plan_title(content).is_none() {
            return Ok(ToolOutput::error(
                "Plan Markdown must start with a level-1 title (`# Plan title`) on the first line.",
            ));
        }
        let Some(path) = ctx.plan_file_path.as_ref() else {
            return Ok(ToolOutput::error(
                "No session plan destination is available in this host.",
            ));
        };
        if std::fs::symlink_metadata(path).is_ok_and(|metadata| metadata.file_type().is_symlink()) {
            return Ok(ToolOutput::error(
                "Refusing to replace a symlinked session plan file.",
            ));
        }
        let Some(parent) = path.parent() else {
            return Ok(ToolOutput::error(
                "The host provided an invalid session plan destination.",
            ));
        };
        tokio::fs::create_dir_all(parent).await?;
        tokio::fs::write(path, content.as_bytes()).await?;

        let line_count = content.lines().count();
        Ok(ToolOutput::success_with_data(
            format!(
                "Plan saved ({} lines, {} bytes). Call ExitPlanMode when it is ready for review.",
                line_count,
                content.len()
            ),
            json!({
                "kind": "plan_write",
                "path": path.display().to_string(),
                "content": content,
                "bytesWritten": content.len(),
                "lineCount": line_count,
            }),
        ))
    }
}

fn plan_title(content: &str) -> Option<&str> {
    let first_line = content
        .trim_start_matches('\u{feff}')
        .lines()
        .next()
        .unwrap_or_default()
        .trim();
    first_line
        .strip_prefix("# ")
        .map(str::trim)
        .filter(|title| !title.is_empty() && !title.starts_with('#'))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn context(path: Option<std::path::PathBuf>, working_dir: &std::path::Path) -> ToolContext {
        ToolContext {
            working_dir: working_dir.to_path_buf(),
            session_id: "write-plan-test".into(),
            plan_file_path: path,
            image: kkagent_config::ImageConfig::default(),
            tool_call_id: None,
            interrupted: None,
            tools_config: kkagent_config::ToolsConfig::default(),
        }
    }

    #[tokio::test]
    async fn writes_only_the_host_managed_plan_path() {
        let root =
            std::env::temp_dir().join(format!("kkagent-write-plan-{}", uuid::Uuid::new_v4()));
        let workspace = root.join("workspace");
        let plan = root.join("session/agents/main/plans/plan.md");
        let outside = root.join("outside.md");
        std::fs::create_dir_all(&workspace).unwrap();

        let output = WritePlanTool
            .execute(
                json!({
                    "content": "# Safe plan\n\n1. Implement it.\n",
                    "path": outside,
                }),
                &context(Some(plan.clone()), &workspace),
            )
            .await
            .unwrap();

        assert!(!output.is_error);
        assert_eq!(
            std::fs::read_to_string(plan).unwrap(),
            "# Safe plan\n\n1. Implement it.\n"
        );
        assert!(!outside.exists());
        std::fs::remove_dir_all(root).unwrap();
    }

    #[tokio::test]
    async fn invalid_title_does_not_replace_existing_plan() {
        let root =
            std::env::temp_dir().join(format!("kkagent-write-plan-{}", uuid::Uuid::new_v4()));
        let plan = root.join("plan.md");
        std::fs::create_dir_all(&root).unwrap();
        std::fs::write(&plan, "# Existing\n").unwrap();

        let output = WritePlanTool
            .execute(
                json!({"content": "## Missing H1\n"}),
                &context(Some(plan.clone()), &root),
            )
            .await
            .unwrap();

        assert!(output.is_error);
        assert_eq!(std::fs::read_to_string(plan).unwrap(), "# Existing\n");
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn schema_does_not_allow_model_supplied_paths() {
        let error = crate::args_validator::validate_against_schema(
            &WritePlanTool.parameters_schema(),
            &json!({
                "content": "# Safe plan\n",
                "path": "/tmp/model-controlled.md",
            }),
        )
        .unwrap_err();
        assert!(error.message.contains("Additional properties"));
    }
}
