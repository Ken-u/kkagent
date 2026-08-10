//! Parse common test runner output into a one-line summary.

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TestSummary {
    pub passed: u32,
    pub failed: u32,
    pub ignored: u32,
    pub failures: Vec<String>,
}

impl TestSummary {
    pub fn one_line(&self) -> String {
        let mut parts = Vec::new();
        if self.passed > 0 {
            parts.push(format!("{} passed", self.passed));
        }
        if self.failed > 0 {
            parts.push(format!("{} failed", self.failed));
        }
        if self.ignored > 0 {
            parts.push(format!("{} ignored", self.ignored));
        }
        if parts.is_empty() {
            "no tests parsed".into()
        } else {
            parts.join(" · ")
        }
    }
}

/// Best-effort parse of cargo / libtest / pytest-ish summaries.
pub fn parse_test_output(text: &str) -> Option<TestSummary> {
    let mut summary = TestSummary::default();
    let mut found = false;
    for line in text.lines() {
        let l = line.trim();
        // cargo: `test result: ok. 7 passed; 0 failed; 0 ignored; ...`
        if let Some(rest) = l.strip_prefix("test result:") {
            found = true;
            for part in rest.split(';') {
                let p = part.trim();
                if let Some(n) = p.strip_suffix(" passed").and_then(|s| {
                    s.split_whitespace().last().and_then(|x| x.parse().ok())
                }) {
                    summary.passed = n;
                } else if let Some(n) = p
                    .strip_suffix(" failed")
                    .and_then(|s| s.split_whitespace().last().and_then(|x| x.parse().ok()))
                {
                    summary.failed = n;
                } else if let Some(n) = p
                    .strip_suffix(" ignored")
                    .and_then(|s| s.split_whitespace().last().and_then(|x| x.parse().ok()))
                {
                    summary.ignored = n;
                }
            }
        }
        if l.starts_with("FAILED ") || l.contains(" FAILED") {
            let name = l
                .trim_start_matches("FAILED ")
                .split_whitespace()
                .next()
                .unwrap_or(l);
            if summary.failures.len() < 8 {
                summary.failures.push(name.to_string());
            }
        }
    }
    found.then_some(summary)
}

/// OSC 8 hyperlink when the terminal likely supports it.
pub fn osc8_link(url: &str, label: &str) -> String {
    // ESC ] 8 ; ; URL ST  label ESC ] 8 ; ; ST
    format!("\x1b]8;;{url}\x1b\\{label}\x1b]8;;\x1b\\")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_cargo_summary() {
        let out = "test result: ok. 7 passed; 0 failed; 0 ignored; 0 measured; 45 filtered out";
        let s = parse_test_output(out).unwrap();
        assert_eq!(s.passed, 7);
        assert_eq!(s.failed, 0);
        assert!(s.one_line().contains("7 passed"));
    }
}
