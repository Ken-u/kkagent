//! Cross-platform plan filenames in `YYYY-MM-DD_<plan-name>.md` form.

fn safe_plan_name(value: &str) -> String {
    let mut result = String::new();
    let mut separator_pending = false;
    for ch in value.trim().chars() {
        if ch.is_alphanumeric() {
            if separator_pending && !result.is_empty() {
                result.push('_');
            }
            separator_pending = false;
            result.extend(ch.to_lowercase());
        } else {
            separator_pending = !result.is_empty();
        }
        if result.chars().count() >= 60 {
            break;
        }
    }
    let result = result.trim_matches('_');
    if result.is_empty() {
        "plan".into()
    } else {
        result.into()
    }
}

pub(crate) fn markdown_plan_title(content: &str) -> anyhow::Result<&str> {
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
        .ok_or_else(|| {
            anyhow::anyhow!(
                "Plan Markdown must start with a level-1 title (`# Plan title`). Add the title as the first line, then call ExitPlanMode again."
            )
        })
}

pub(crate) fn plan_id_base(name_hint: &str) -> String {
    let date = chrono::Local::now().format("%Y-%m-%d");
    format!("{date}_{}", safe_plan_name(name_hint))
}

pub(crate) fn plan_id_matches_base(plan_id: &str, base: &str) -> bool {
    plan_id == base
        || plan_id
            .strip_prefix(base)
            .and_then(|suffix| suffix.strip_prefix('_'))
            .is_some_and(|suffix| {
                !suffix.is_empty() && suffix.chars().all(|ch| ch.is_ascii_digit())
            })
}

pub(crate) fn generate_plan_id(plans_dir: &std::path::Path, name_hint: &str) -> String {
    let base = plan_id_base(name_hint);
    if !plans_dir.join(format!("{base}.md")).exists() {
        return base;
    }
    for suffix in 2..=9999 {
        let candidate = format!("{base}_{suffix}");
        if !plans_dir.join(format!("{candidate}.md")).exists() {
            return candidate;
        }
    }
    format!("{base}_{}", &uuid::Uuid::new_v4().simple().to_string()[..8])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_safe_readable_plan_names() {
        assert_eq!(
            safe_plan_name("修复 Plan/Resume: 内容丢失?"),
            "修复_plan_resume_内容丢失"
        );
        assert_eq!(safe_plan_name("  ***  "), "plan");
        assert!(plan_id_base("Session resume").ends_with("_session_resume"));
        assert!(plan_id_matches_base(
            "2026-08-11_session_resume_2",
            "2026-08-11_session_resume"
        ));
        assert!(!plan_id_matches_base(
            "2026-08-11_session_resume_old",
            "2026-08-11_session_resume"
        ));
    }

    #[test]
    fn reads_plan_name_from_first_markdown_h1() {
        assert_eq!(
            markdown_plan_title("# 修复 Session Resume\n\n## Steps\n").unwrap(),
            "修复 Session Resume"
        );
        assert!(markdown_plan_title("\n# Too late\n").is_err());
        assert!(markdown_plan_title("## Not an H1\n").is_err());
    }

    #[test]
    fn adds_suffix_when_name_exists() {
        let dir = std::env::temp_dir().join(format!("kkagent-plan-name-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        let first = generate_plan_id(&dir, "Resume plan");
        std::fs::write(dir.join(format!("{first}.md")), "plan").unwrap();
        let second = generate_plan_id(&dir, "Resume plan");
        assert_eq!(second, format!("{first}_2"));
        std::fs::remove_dir_all(dir).unwrap();
    }
}
