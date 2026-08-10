//! Platform-aware modifier helpers for TUI keybindings.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Returns true when the key event is the platform "copy" shortcut:
/// - macOS: ⌘Command + C (`SUPER` modifier) — not Ctrl+C
/// - Linux / Windows: Ctrl + C
#[inline]
pub fn is_copy_shortcut(key: &KeyEvent) -> bool {
    matches!(key.code, KeyCode::Char('c') | KeyCode::Char('C'))
        && is_platform_copy_modifier(key.modifiers)
}

/// Human-readable copy chord for help / tips.
pub fn copy_shortcut_label() -> &'static str {
    #[cfg(target_os = "macos")]
    {
        "⌘C"
    }
    #[cfg(not(target_os = "macos"))]
    {
        "Ctrl-C"
    }
}

/// Returns true when the modifiers represent the platform copy modifier.
#[inline]
pub fn is_platform_copy_modifier(modifiers: KeyModifiers) -> bool {
    #[cfg(target_os = "macos")]
    {
        // crossterm reports Command as SUPER on macOS.
        // Require SUPER; do not treat Ctrl+C as copy (that stays interrupt/quit).
        modifiers.contains(KeyModifiers::SUPER) && !modifiers.contains(KeyModifiers::CONTROL)
    }
    #[cfg(not(target_os = "macos"))]
    {
        modifiers.contains(KeyModifiers::CONTROL) && !modifiers.contains(KeyModifiers::SUPER)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(code: KeyCode, modifiers: KeyModifiers) -> KeyEvent {
        KeyEvent::new(code, modifiers)
    }

    #[test]
    fn copy_shortcut_is_platform_specific() {
        let cmd_c = key(KeyCode::Char('c'), KeyModifiers::SUPER);
        let ctrl_c = key(KeyCode::Char('c'), KeyModifiers::CONTROL);

        #[cfg(target_os = "macos")]
        {
            assert!(is_copy_shortcut(&cmd_c));
            assert!(!is_copy_shortcut(&ctrl_c));
        }
        #[cfg(not(target_os = "macos"))]
        {
            assert!(!is_copy_shortcut(&cmd_c));
            assert!(is_copy_shortcut(&ctrl_c));
        }
    }
}
