//! Fuzzy autocomplete for slash commands and file paths.

use super::fuzzy::fuzzy_match;
use std::path::{Path, PathBuf};

#[derive(Debug, Clone)]
pub struct CompletionItem {
    pub label: String,
    pub insert: String,
    pub detail: Option<String>,
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
                CompletionItem {
                    label: format!("/{name}"),
                    insert: format!("/{name}"),
                    detail: Some((*desc).to_string()),
                },
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
        let mut s = insert.to_string_lossy().into_owned();
        if is_dir && !s.ends_with('/') {
            s.push('/');
        }
        items.push(CompletionItem {
            label: if is_dir { format!("{name}/") } else { name },
            insert: s,
            detail: None,
        });
        if items.len() >= 40 {
            break;
        }
    }
    items.sort_by(|a, b| a.label.cmp(&b.label));
    items
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
}
