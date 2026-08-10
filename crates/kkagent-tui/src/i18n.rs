//! UI strings with a locale hook. Only English is wired today; add locales later.

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Locale {
    #[default]
    En,
}

/// Format a collapsed turn tool-history overview line.
pub fn tool_history_overview(
    locale: Locale,
    tool_count: u32,
    duration_ms: u64,
    tokens: u64,
) -> String {
    match locale {
        Locale::En => {
            let calls = if tool_count == 1 {
                "1 tool call".to_string()
            } else {
                format!("{tool_count} tool calls")
            };
            format!(
                "{calls}, took {}, used {} tokens",
                format_duration_en(duration_ms),
                format_tokens_en(tokens)
            )
        }
    }
}

pub fn tool_history_expand_hint(locale: Locale) -> &'static str {
    match locale {
        Locale::En => "ctrl+o to expand",
    }
}

pub fn tool_history_collapse_hint(locale: Locale) -> &'static str {
    match locale {
        Locale::En => "ctrl+o to collapse",
    }
}

fn format_duration_en(duration_ms: u64) -> String {
    let secs = duration_ms.saturating_add(500) / 1000; // round to nearest second
    if secs < 60 {
        format!("{secs}s")
    } else if secs < 3600 {
        let mins = secs / 60;
        let rem = secs % 60;
        if rem == 0 {
            format!("{mins} min")
        } else {
            format!("{mins}.{} min", (rem * 10 / 60).max(1).min(9))
        }
    } else {
        let hours = secs / 3600;
        let mins = (secs % 3600) / 60;
        if mins == 0 {
            format!("{hours} h")
        } else {
            format!("{hours} h {mins} min")
        }
    }
}

fn format_tokens_en(tokens: u64) -> String {
    if tokens < 1_000 {
        tokens.to_string()
    } else if tokens < 10_000 {
        format!("{:.1}k", tokens as f64 / 1_000.0)
    } else if tokens < 1_000_000 {
        format!("{}k", (tokens + 500) / 1_000)
    } else {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn overview_english() {
        let s = tool_history_overview(Locale::En, 5, 125_000, 12_300);
        assert!(s.contains("5 tool calls"));
        assert!(s.contains("took"));
        assert!(s.contains("tokens"));
    }

    #[test]
    fn duration_seconds_and_minutes() {
        assert_eq!(format_duration_en(1_200), "1s");
        assert_eq!(format_duration_en(90_000), "1.5 min");
        assert_eq!(format_duration_en(120_000), "2 min");
    }
}
