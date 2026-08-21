//! Fuzzy autocomplete for slash commands and `@` file paths.

use super::fuzzy::fuzzy_match;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Debug, Clone)]
pub struct CompletionItem {
    pub label: String,
    pub insert: String,
    pub detail: Option<String>,
    pub is_directory: bool,
}

impl CompletionItem {
    pub fn file(
        label: impl Into<String>,
        insert: impl Into<String>,
        detail: Option<String>,
    ) -> Self {
        Self {
            label: label.into(),
            insert: insert.into(),
            detail,
            is_directory: false,
        }
    }

    pub fn dir(
        label: impl Into<String>,
        insert: impl Into<String>,
        detail: Option<String>,
    ) -> Self {
        Self {
            label: label.into(),
            insert: insert.into(),
            detail,
            is_directory: true,
        }
    }
}

#[derive(Debug, Default)]
pub struct Autocomplete {
    pub items: Vec<CompletionItem>,
    pub selected: usize,
    pub active: bool,
    pub query: String,
}

impl Autocomplete {
    pub fn clear(&mut self) {
        self.items.clear();
        self.selected = 0;
        self.active = false;
        self.query.clear();
    }

    pub fn set_items(&mut self, items: Vec<CompletionItem>, query: impl Into<String>) {
        self.items = items;
        self.query = query.into();
        self.selected = 0;
        self.active = !self.items.is_empty();
    }

    pub fn next(&mut self) {
        if self.items.is_empty() {
            return;
        }
        self.selected = (self.selected + 1) % self.items.len();
    }

    pub fn prev(&mut self) {
        if self.items.is_empty() {
            return;
        }
        self.selected = if self.selected == 0 {
            self.items.len() - 1
        } else {
            self.selected - 1
        };
    }

    pub fn current(&self) -> Option<&CompletionItem> {
        self.items.get(self.selected)
    }

    pub fn visible(&self, max: usize) -> &[CompletionItem] {
        let n = self.items.len().min(max);
        &self.items[..n]
    }
}

/// Rank slash commands by fuzzy score (lower is better).
pub fn complete_slash(commands: &[(&str, &str)], prefix: &str) -> Vec<CompletionItem> {
    let q = prefix.trim_start_matches('/');
    let mut scored: Vec<(f64, CompletionItem)> = commands
        .iter()
        .filter_map(|(name, desc)| {
            let m = fuzzy_match(q, name);
            if !m.matches {
                return None;
            }
            Some((
                m.score,
                CompletionItem::file(
                    format!("/{name}"),
                    format!("/{name}"),
                    Some((*desc).to_string()),
                ),
            ))
        })
        .collect();
    scored.sort_by(|a, b| {
        a.0.partial_cmp(&b.0)
            .unwrap_or(std::cmp::Ordering::Equal)
            .then_with(|| a.1.label.cmp(&b.1.label))
    });
    scored.into_iter().map(|(_, i)| i).collect()
}

pub fn complete_path(cwd: &Path, partial: &str) -> Vec<CompletionItem> {
    let path = PathBuf::from(partial);
    let (dir, file_prefix) = if partial.ends_with('/') || partial.ends_with('\\') {
        (cwd.join(&path), String::new())
    } else {
        let parent = path.parent().unwrap_or_else(|| Path::new(""));
        let file = path
            .file_name()
            .map(|s| s.to_string_lossy().into_owned())
            .unwrap_or_default();
        (cwd.join(parent), file)
    };
    let Ok(rd) = std::fs::read_dir(&dir) else {
        return vec![];
    };
    let mut items = Vec::new();
    for ent in rd.flatten() {
        let name = ent.file_name().to_string_lossy().into_owned();
        if name.starts_with('.') && file_prefix.is_empty() {
            continue;
        }
        if !file_prefix.is_empty() && !fuzzy_match(&file_prefix, &name).matches {
            continue;
        }
        let is_dir = ent.file_type().map(|t| t.is_dir()).unwrap_or(false);
        let insert = if !partial.starts_with('/') && !partial.starts_with('\\') {
            let rel = PathBuf::from(partial);
            let base = rel.parent().unwrap_or_else(|| Path::new(""));
            base.join(&name)
        } else {
            dir.join(&name)
        };
        let mut s = to_display_path(&insert.to_string_lossy());
        if is_dir && !s.ends_with('/') {
            s.push('/');
        }
        items.push(if is_dir {
            CompletionItem::dir(format!("{name}/"), s, None)
        } else {
            CompletionItem::file(name, s, None)
        });
        if items.len() >= 40 {
            break;
        }
    }
    items.sort_by(|a, b| {
        b.is_directory
            .cmp(&a.is_directory)
            .then_with(|| a.label.cmp(&b.label))
    });
    items
}

const PATH_DELIMITERS: &[char] = &[' ', '\t', '"', '\'', '=', '\n'];

/// Extract the active `@path` token before the cursor.
/// Returns `(byte_start_of_at, query_without_at)`.
pub fn extract_at_token(text: &str, cursor: usize) -> Option<(usize, String)> {
    let cursor = cursor.min(text.len());
    let mut c = cursor;
    while c > 0 && !text.is_char_boundary(c) {
        c -= 1;
    }
    let before = &text[..c];
    // Prefer unclosed `@"...` quoted form.
    if let Some(qstart) = find_unclosed_at_quote(before) {
        let query = before[qstart + 2..].to_string();
        return Some((qstart, query));
    }
    let at = before.rfind('@')?;
    if at > 0 {
        let prev = before[..at].chars().next_back()?;
        if !PATH_DELIMITERS.contains(&prev) {
            return None;
        }
    }
    let after_at = &before[at + 1..];
    if after_at.contains(' ') || after_at.contains('\t') || after_at.contains('\n') {
        return None;
    }
    // Don't steal email-like tokens mid-word (a@b) — already guarded by delimiter.
    Some((at, after_at.to_string()))
}

fn find_unclosed_at_quote(before: &str) -> Option<usize> {
    // Find last `@" ` that has no closing `"` after it.
    let bytes = before.as_bytes();
    let mut i = 0usize;
    let mut last = None;
    while i + 1 < bytes.len() {
        if bytes[i] == b'@' && bytes[i + 1] == b'"' {
            let token_ok = i == 0
                || before[..i]
                    .chars()
                    .next_back()
                    .map(|ch| PATH_DELIMITERS.contains(&ch))
                    .unwrap_or(true);
            if token_ok {
                last = Some(i);
            }
            i += 2;
            continue;
        }
        i += 1;
    }
    let start = last?;
    let rest = &before[start + 2..];
    if rest.contains('"') {
        return None;
    }
    Some(start)
}

/// `@` file/dir fuzzy completion (gitignore-aware walk, optional `fd`).
pub fn complete_at_files(cwd: &Path, query: &str, max: usize) -> Vec<CompletionItem> {
    let query = to_display_path(query);
    let mut items = if let Some(via_fd) = try_fd_complete(cwd, &query, max) {
        via_fd
    } else {
        walk_complete(cwd, &query, max)
    };
    // Also include shallow path listing when query looks like a directory prefix.
    if query.contains('/') || query.ends_with('/') {
        for extra in complete_path(cwd, query.trim_start_matches("./")) {
            if !items.iter().any(|i| i.insert == extra.insert) {
                items.push(with_at_prefix(extra));
            }
        }
    }
    // Ensure insert values are `@…`
    for item in &mut items {
        if !item.insert.starts_with('@') {
            *item = with_at_prefix(item.clone());
        }
    }
    rank_at_items(&mut items, &query);
    items.truncate(max);
    items
}

fn with_at_prefix(mut item: CompletionItem) -> CompletionItem {
    if !item.insert.starts_with('@') {
        item.insert = format!("@{}", item.insert);
    }
    if item.is_directory && !item.label.ends_with('/') {
        item.label.push('/');
    }
    item
}

fn rank_at_items(items: &mut [CompletionItem], query: &str) {
    let q = query.to_lowercase();
    items.sort_by(|a, b| {
        let score = |it: &CompletionItem| -> (i32, i32, String) {
            let path = it.insert.trim_start_matches('@').to_lowercase();
            let base = Path::new(&path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_lowercase();
            let exact = if base == q { 0 } else { 1 };
            let prefix = if base.starts_with(&q) { 0 } else { 1 };
            let dir_first = if it.is_directory { 0 } else { 1 };
            (exact + prefix, dir_first, path)
        };
        score(a).cmp(&score(b))
    });
}

fn try_fd_complete(cwd: &Path, query: &str, max: usize) -> Option<Vec<CompletionItem>> {
    let fd = which_fd()?;
    let mut args = vec![
        "--base-directory".into(),
        cwd.to_string_lossy().into_owned(),
        "--max-results".into(),
        max.to_string(),
        "--type".into(),
        "f".into(),
        "--type".into(),
        "d".into(),
        "--exclude".into(),
        ".git".into(),
    ];
    if query.contains('/') {
        args.push("--full-path".into());
    }
    if !query.is_empty() {
        args.push(fd_query(query));
    }
    let output = Command::new(fd)
        .args(&args)
        .output()
        .ok()
        .filter(|o| o.status.success())?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let mut items = Vec::new();
    for line in stdout.lines().filter(|l| !l.is_empty()) {
        let display = to_display_path(line);
        if display == ".git" || display.starts_with(".git/") || display.contains("/.git/") {
            continue;
        }
        let is_dir = display.ends_with('/');
        let path = display.trim_end_matches('/').to_string();
        let label = if is_dir {
            format!(
                "{}/",
                Path::new(&path)
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(&path)
            )
        } else {
            Path::new(&path)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(&path)
                .to_string()
        };
        let insert_path = if is_dir { format!("{path}/") } else { path };
        items.push(if is_dir {
            CompletionItem::dir(label, format!("@{insert_path}"), None)
        } else {
            CompletionItem::file(label, format!("@{insert_path}"), Some(insert_path))
        });
        if items.len() >= max {
            break;
        }
    }
    Some(items)
}

fn fd_query(query: &str) -> String {
    let normalized = to_display_path(query);
    if !normalized.contains('/') {
        return normalized;
    }
    let trailing = normalized.ends_with('/');
    let trimmed = normalized.trim_matches('/');
    if trimmed.is_empty() {
        return normalized;
    }
    let parts: Vec<String> = trimmed
        .split('/')
        .filter(|s| !s.is_empty())
        .map(regex_escape)
        .collect();
    let mut pattern = parts.join("[\\\\/]");
    if trailing {
        pattern.push_str("[\\\\/]");
    }
    pattern
}

fn regex_escape(s: &str) -> String {
    let mut out = String::new();
    for ch in s.chars() {
        if matches!(
            ch,
            '.' | '+' | '*' | '?' | '(' | ')' | '[' | ']' | '{' | '}' | '|' | '\\' | '^' | '$'
        ) {
            out.push('\\');
        }
        out.push(ch);
    }
    out
}

fn which_fd() -> Option<&'static str> {
    // Probing spawns a process; cache the result so `@` completion does not
    // pay for it on every keystroke.
    static FD_BIN: std::sync::OnceLock<Option<&'static str>> = std::sync::OnceLock::new();
    *FD_BIN.get_or_init(|| {
        ["fd", "fdfind"].into_iter().find(|name| {
            Command::new(name)
                .arg("--version")
                .output()
                .map(|o| o.status.success())
                .unwrap_or(false)
        })
    })
}

fn walk_complete(cwd: &Path, query: &str, max: usize) -> Vec<CompletionItem> {
    let mut items = Vec::new();
    // `hidden(false)` is required for `.github`-style entries, but it also
    // re-enables `.repo/`, which holds millions of entries in an AOSP
    // checkout and would eat the whole scan budget. Prune heavy/hidden
    // metadata dirs explicitly instead.
    let walker = ignore::WalkBuilder::new(cwd)
        .hidden(false)
        .git_ignore(true)
        .git_global(true)
        .git_exclude(true)
        .max_depth(Some(8))
        .filter_entry(|e| {
            if e.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                let name = e.file_name().to_string_lossy();
                !matches!(
                    name.as_ref(),
                    ".repo" | "out" | "target" | "node_modules" | ".git"
                )
            } else {
                true
            }
        })
        .build();
    let q_lower = query.to_lowercase();
    let mut scanned = 0usize;
    for entry in walker.flatten() {
        scanned += 1;
        if scanned > 8_000 {
            break;
        }
        let path = entry.path();
        if path == cwd {
            continue;
        }
        let Ok(rel) = path.strip_prefix(cwd) else {
            continue;
        };
        let mut display = to_display_path(&rel.to_string_lossy());
        if display.is_empty() || display.starts_with(".git/") || display == ".git" {
            continue;
        }
        let is_dir = entry.file_type().map(|t| t.is_dir()).unwrap_or(false);
        if is_dir && !display.ends_with('/') {
            display.push('/');
        }
        if !q_lower.is_empty() {
            let hay = display.to_lowercase();
            let base = Path::new(display.trim_end_matches('/'))
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or("")
                .to_lowercase();
            let ok = hay.contains(&q_lower)
                || fuzzy_match(&q_lower, &base).matches
                || fuzzy_match(&q_lower, &hay).matches;
            if !ok {
                continue;
            }
        }
        let label = if is_dir {
            format!(
                "{}/",
                Path::new(display.trim_end_matches('/'))
                    .file_name()
                    .and_then(|n| n.to_str())
                    .unwrap_or(display.trim_end_matches('/'))
            )
        } else {
            Path::new(&display)
                .file_name()
                .and_then(|n| n.to_str())
                .unwrap_or(&display)
                .to_string()
        };
        items.push(if is_dir {
            CompletionItem::dir(label, format!("@{display}"), None)
        } else {
            CompletionItem::file(label, format!("@{display}"), Some(display))
        });
        if items.len() >= max.saturating_mul(3) {
            // collect extra then rank/truncate
            break;
        }
    }
    // Empty query: prefer top-level entries
    if query.is_empty() {
        if let Ok(rd) = std::fs::read_dir(cwd) {
            let mut top = Vec::new();
            for ent in rd.flatten() {
                let name = ent.file_name().to_string_lossy().into_owned();
                if name.starts_with('.') {
                    continue;
                }
                let is_dir = ent.file_type().map(|t| t.is_dir()).unwrap_or(false);
                let display = if is_dir {
                    format!("{name}/")
                } else {
                    name.clone()
                };
                top.push(if is_dir {
                    CompletionItem::dir(format!("{name}/"), format!("@{display}"), None)
                } else {
                    CompletionItem::file(name, format!("@{display}"), Some(display))
                });
            }
            top.sort_by(|a, b| {
                b.is_directory
                    .cmp(&a.is_directory)
                    .then_with(|| a.label.cmp(&b.label))
            });
            return top.into_iter().take(max).collect();
        }
    }
    items
}

fn to_display_path(s: &str) -> String {
    s.replace('\\', "/")
}

/// Build the replacement string and whether to leave the menu open (directory).
pub fn format_at_completion(item: &CompletionItem, quoted: bool) -> (String, bool) {
    let path = item.insert.trim_start_matches('@');
    let needs_quotes = quoted || path.contains(' ');
    let value = if needs_quotes {
        format!("@\"{path}\"")
    } else {
        item.insert.clone()
    };
    let keep_open = item.is_directory;
    let value = if keep_open || value.ends_with(' ') {
        value
    } else {
        format!("{value} ")
    };
    (value, keep_open)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slash_ranks_exact() {
        let cmds = [("help", "show help"), ("hello", "greet")];
        let items = complete_slash(&cmds, "/help");
        assert!(!items.is_empty());
        assert_eq!(items[0].label, "/help");
    }

    #[test]
    fn extract_at_basic() {
        let (start, q) = extract_at_token("see @src/fo", 11).unwrap();
        assert_eq!(start, 4);
        assert_eq!(q, "src/fo");
    }

    #[test]
    fn extract_at_rejects_email() {
        assert!(extract_at_token("a@b.com", 7).is_none());
    }

    #[test]
    fn complete_at_top_level() {
        let dir = std::env::temp_dir().join(format!("kkagent-at-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(dir.join("src")).unwrap();
        std::fs::write(dir.join("README.md"), "x").unwrap();
        let items = complete_at_files(&dir, "", 20);
        assert!(items.iter().any(|i| i.insert.contains("README")));
        assert!(items
            .iter()
            .any(|i| i.is_directory && i.insert.contains("src")));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn complete_at_filters() {
        let dir = std::env::temp_dir().join(format!("kkagent-atf-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("alpha.rs"), "x").unwrap();
        std::fs::write(dir.join("beta.rs"), "x").unwrap();
        let items = complete_at_files(&dir, "alp", 20);
        assert!(items.iter().any(|i| i.insert.contains("alpha")));
        assert!(!items.iter().any(|i| i.insert.contains("beta")));
        let _ = std::fs::remove_dir_all(dir);
    }
}
