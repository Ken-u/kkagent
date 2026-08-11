//! Keybinding map (emacs-like defaults, matching pi-tui).

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use std::collections::{HashMap, HashSet};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EditorAction {
    Insert(char),
    Backspace,
    Delete,
    Left,
    Right,
    WordLeft,
    WordRight,
    Home,
    End,
    UpHistory,
    DownHistory,
    KillLine,
    KillWord,
    Yank,
    Undo,
    Redo,
    Submit,
    Clear,
    Autocomplete,
    Escape,
    Interrupt,
    ToggleExpand,
    None,
}

pub fn map_key(key: KeyEvent) -> EditorAction {
    let m = key.modifiers;
    match key.code {
        KeyCode::Enter => EditorAction::Submit,
        KeyCode::Esc => EditorAction::Escape,
        KeyCode::Tab => EditorAction::Autocomplete,
        KeyCode::Backspace => {
            if m.contains(KeyModifiers::ALT) || m.contains(KeyModifiers::CONTROL) {
                EditorAction::KillWord
            } else {
                EditorAction::Backspace
            }
        }
        KeyCode::Delete => EditorAction::Delete,
        KeyCode::Left if m.contains(KeyModifiers::CONTROL) || m.contains(KeyModifiers::ALT) => {
            EditorAction::WordLeft
        }
        KeyCode::Right if m.contains(KeyModifiers::CONTROL) || m.contains(KeyModifiers::ALT) => {
            EditorAction::WordRight
        }
        KeyCode::Left => EditorAction::Left,
        KeyCode::Right => EditorAction::Right,
        KeyCode::Home => EditorAction::Home,
        KeyCode::End => EditorAction::End,
        KeyCode::Up => EditorAction::UpHistory,
        KeyCode::Down => EditorAction::DownHistory,
        KeyCode::Char('a') if m.contains(KeyModifiers::CONTROL) => EditorAction::Home,
        KeyCode::Char('e') if m.contains(KeyModifiers::CONTROL) => EditorAction::End,
        KeyCode::Char('b') if m.contains(KeyModifiers::CONTROL) => EditorAction::Left,
        KeyCode::Char('f') if m.contains(KeyModifiers::CONTROL) => EditorAction::Right,
        KeyCode::Char('d') if m.contains(KeyModifiers::CONTROL) => EditorAction::Delete,
        KeyCode::Char('k') if m.contains(KeyModifiers::CONTROL) => EditorAction::KillLine,
        KeyCode::Char('u') if m.contains(KeyModifiers::CONTROL) => EditorAction::Clear,
        KeyCode::Char('w') if m.contains(KeyModifiers::CONTROL) => EditorAction::KillWord,
        KeyCode::Char('y') if m.contains(KeyModifiers::CONTROL) => EditorAction::Yank,
        KeyCode::Char('_') if m.contains(KeyModifiers::CONTROL) => EditorAction::Undo,
        KeyCode::Char('z') if m.contains(KeyModifiers::CONTROL) => EditorAction::Undo,
        KeyCode::Char('Z') if m.contains(KeyModifiers::CONTROL) => EditorAction::Redo,
        KeyCode::Char('c') if m.contains(KeyModifiers::CONTROL) => EditorAction::Interrupt,
        KeyCode::Char('l') if m.contains(KeyModifiers::CONTROL) => EditorAction::ToggleExpand,
        KeyCode::Char(c)
            if !m.contains(KeyModifiers::CONTROL) && !m.contains(KeyModifiers::ALT) =>
        {
            EditorAction::Insert(c)
        }
        _ => EditorAction::None,
    }
}

/// Validate `[ui.keybindings]` overrides. Reserved chords for interrupt/submit cannot be unbound.
pub fn validate_overrides(map: &HashMap<String, String>) -> Result<(), String> {
    if map.is_empty() {
        return Ok(());
    }
    let mut seen = HashSet::new();
    let reserved = ["ctrl-c", "enter", "escape"];
    for (action, chord) in map {
        let c = chord.trim().to_lowercase();
        if c.is_empty() {
            return Err(format!("empty chord for action {action}"));
        }
        if !seen.insert(c.clone()) {
            return Err(format!("duplicate chord {c}"));
        }
        if reserved.contains(&c.as_str())
            && action != "interrupt"
            && action != "submit"
            && action != "escape"
        {
            return Err(format!("chord {c} is reserved for interrupt/submit/escape"));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_duplicate_chords() {
        let mut m = HashMap::new();
        m.insert("foo".into(), "ctrl-a".into());
        m.insert("bar".into(), "ctrl-a".into());
        assert!(validate_overrides(&m).is_err());
    }
}
