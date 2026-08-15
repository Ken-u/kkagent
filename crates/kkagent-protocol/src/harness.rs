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
    // Match `<tag>` or `<tag attrs…>` (kimi skill blocks carry attributes).
    let open_prefix = format!("<{tag}");
    let close = format!("</{tag}>");
    let mut out = text.to_string();
    let mut cursor = 0usize;
    while let Some(rel) = out[cursor..].find(&open_prefix) {
        let start = cursor + rel;
        let after = start + open_prefix.len();
        let boundary_ok = matches!(
            out[after..].chars().next(),
            Some('>') | Some(' ') | Some('\t') | Some('\n') | Some('\r')
        );
        if !boundary_ok {
            cursor = after;
            continue;
        }
        let Some(rel_end) = out[start..].find(&close) else {
            out.truncate(start);
            break;
        };
        let end = start + rel_end + close.len();
        out.replace_range(start..end, "");
        cursor = start;
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

/// First non-harness user text from a sequence of raw user message bodies.
///
/// Skips empty / harness-only injections (`<system-reminder>`, `<cron-fire>`, …)
/// and returns [`visible_user_text`] of the first usable message.
pub fn first_real_user_text<'a>(texts: impl IntoIterator<Item = &'a str>) -> Option<String> {
    for text in texts {
        if is_harness_only_user_text(text) {
            continue;
        }
        let visible = visible_user_text(text);
        if !visible.is_empty() {
            return Some(visible);
        }
    }
    None
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
    fn strips_skill_loaded_with_attributes() {
        let raw = concat!(
            "<kimi-skill-loaded name=\"demo\" trigger=\"model-tool\">\n",
            "body here\n",
            "</kimi-skill-loaded>\n",
            "hello"
        );
        assert_eq!(visible_user_text(raw), "hello");
        assert!(is_harness_only_user_text(
            "<kimi-skill-loaded name=\"x\">\nonly\n</kimi-skill-loaded>"
        ));
    }

    #[test]
    fn first_real_user_text_skips_harness_only() {
        let rem = "<system-reminder>\nToday\n</system-reminder>";
        assert_eq!(
            first_real_user_text([rem, "real question"]),
            Some("real question".into())
        );
        assert_eq!(first_real_user_text([rem, rem]), None);
        assert_eq!(
            first_real_user_text(["hello", "later"]),
            Some("hello".into())
        );
    }
}
