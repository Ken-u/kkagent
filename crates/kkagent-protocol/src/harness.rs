//! Harness-injected user/tool text (`<system-reminder>`, cron fires, …).
//! These are for the model only and must not surface as normal chat UI / titles.

/// Remove known harness XML blocks from text (best-effort, non-nested).
pub fn strip_harness_blocks(text: &str) -> String {
    let mut out = text.to_string();
    for tag in ["system-reminder", "cron-fire", "kimi-skill-loaded"] {
        out = strip_tagged_blocks(&out, tag);
    }
    out
}

fn strip_tagged_blocks(text: &str, tag: &str) -> String {
    let open = format!("<{tag}>");
    let close = format!("</{tag}>");
    let mut out = text.to_string();
    while let Some(start) = out.find(&open) {
        let Some(rel_end) = out[start..].find(&close) else {
            // Unclosed tag — drop from open to end (truncated display junk).
            out.truncate(start);
            break;
        };
        let end = start + rel_end + close.len();
        out.replace_range(start..end, "");
    }
    out
}

/// True when the text is empty after stripping harness blocks (model-only injection).
pub fn is_harness_only_text(text: &str) -> bool {
    strip_harness_blocks(text).trim().is_empty() && !text.trim().is_empty()
        || text.trim().is_empty()
}

/// Like [`is_harness_only_text`], but empty string is not "harness-only".
pub fn is_harness_only_user_text(text: &str) -> bool {
    let trimmed = text.trim();
    if trimmed.is_empty() {
        return false;
    }
    strip_harness_blocks(trimmed).trim().is_empty()
}

/// Visible user-facing text: harness blocks removed, whitespace normalized lightly.
pub fn visible_user_text(text: &str) -> String {
    strip_harness_blocks(text)
        .lines()
        .map(str::trim_end)
        .collect::<Vec<_>>()
        .join("\n")
        .trim()
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_system_reminder() {
        let raw = "<system-reminder>\nPlan mode\n</system-reminder>\nhello";
        assert_eq!(visible_user_text(raw), "hello");
        assert!(is_harness_only_user_text(
            "<system-reminder>\nPlan mode\n</system-reminder>"
        ));
        assert!(!is_harness_only_user_text("hello"));
    }

    #[test]
    fn unclosed_reminder_is_harness_only() {
        let raw = "<system-reminder>\nToday's date";
        assert!(is_harness_only_user_text(raw));
        assert!(visible_user_text(raw).is_empty());
    }
}
