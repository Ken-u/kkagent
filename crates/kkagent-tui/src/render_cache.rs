//! Incremental transcript render cache.
//!
//! Completed assistant markdown is parsed/wrapped once per (content, width)
//! and reused until the terminal width changes or the message content updates.

use std::collections::HashMap;
use std::hash::{Hash, Hasher};

use ratatui::text::Line;

use crate::theme::Theme;

const DEFAULT_CAP: usize = 256;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
struct CacheKey {
    content_hash: u64,
    width: u16,
}

#[derive(Debug, Default)]
pub struct RenderCache {
    entries: HashMap<CacheKey, Vec<Line<'static>>>,
    order: Vec<CacheKey>,
    cap: usize,
    last_width: u16,
}

/// Cached, laid-out stable transcript. Rendering a frame only needs to clone
/// visible lines; while streaming, the active assistant tail stays separate.
#[derive(Debug, Default)]
pub struct TranscriptLayoutCache {
    fingerprint: Option<u64>,
    lines: Vec<Line<'static>>,
}

impl TranscriptLayoutCache {
    pub fn matches(&self, fingerprint: u64) -> bool {
        self.fingerprint == Some(fingerprint)
    }

    pub fn replace(&mut self, fingerprint: u64, lines: Vec<Line<'static>>) {
        self.fingerprint = Some(fingerprint);
        self.lines = lines;
    }

    pub fn lines(&self) -> &[Line<'static>] {
        &self.lines
    }

    pub fn invalidate(&mut self) {
        self.fingerprint = None;
        self.lines.clear();
    }
}

impl RenderCache {
    pub fn new() -> Self {
        Self {
            entries: HashMap::new(),
            order: Vec::new(),
            cap: DEFAULT_CAP,
            last_width: 0,
        }
    }

    pub fn clear_if_width_changed(&mut self, width: u16) {
        if self.last_width != 0 && self.last_width != width {
            self.entries.clear();
            self.order.clear();
        }
        self.last_width = width;
    }

    pub fn get_or_insert_markdown(
        &mut self,
        text: &str,
        width: u16,
        theme: &Theme,
    ) -> Vec<Line<'static>> {
        self.clear_if_width_changed(width);
        let key = CacheKey {
            content_hash: hash_text(text),
            width,
        };
        if let Some(lines) = self.entries.get(&key) {
            return lines.clone();
        }
        let avail = (width as usize).saturating_sub(2).max(1);
        let rendered = crate::markdown::render(text, avail, theme);
        self.insert(key, rendered.clone());
        rendered
    }

    fn insert(&mut self, key: CacheKey, lines: Vec<Line<'static>>) {
        if self.entries.len() >= self.cap {
            if let Some(old) = self.order.first().copied() {
                self.order.remove(0);
                self.entries.remove(&old);
            }
        }
        if !self.entries.contains_key(&key) {
            self.order.push(key);
        }
        self.entries.insert(key, lines);
    }

    pub fn invalidate_all(&mut self) {
        self.entries.clear();
        self.order.clear();
    }
}

fn hash_text(text: &str) -> u64 {
    let mut hasher = std::collections::hash_map::DefaultHasher::new();
    text.hash(&mut hasher);
    text.len().hash(&mut hasher);
    hasher.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn width_change_clears_cache() {
        let mut cache = RenderCache::new();
        let theme = Theme::default();
        let _ = cache.get_or_insert_markdown("hello **world**", 80, &theme);
        assert_eq!(cache.entries.len(), 1);
        cache.clear_if_width_changed(100);
        assert!(cache.entries.is_empty());
    }

    #[test]
    fn same_key_reuses_entry() {
        let mut cache = RenderCache::new();
        let theme = Theme::default();
        let a = cache.get_or_insert_markdown("same", 60, &theme);
        let b = cache.get_or_insert_markdown("same", 60, &theme);
        assert_eq!(a.len(), b.len());
        assert_eq!(cache.entries.len(), 1);
    }

    #[test]
    fn transcript_layout_reuses_only_matching_fingerprint() {
        let mut cache = TranscriptLayoutCache::default();
        cache.replace(7, vec![Line::from("one"), Line::from("two")]);
        assert!(cache.matches(7));
        assert!(!cache.matches(8));
        assert_eq!(cache.lines().len(), 2);

        cache.invalidate();
        assert!(!cache.matches(7));
        assert!(cache.lines().is_empty());
    }
}
