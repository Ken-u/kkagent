//! Platform-aware modifier helpers for TUI keybindings.

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

/// Returns true when the key event is the platform "copy" shortcut:
/// - macOS: ⌘Command + C (`SUPER`) — and Ctrl + C as a fallback, because the
///   stock Terminal.app does not forward ⌘C into the TUI.
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
        "⌘C / Ctrl-C"
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
        // crossterm reports Command as SUPER on macOS. Terminals that forward
        // ⌘C (iTerm2 / Kitty / WezTerm) hit the SUPER path; the stock
        // Terminal.app swallows ⌘C, so accept Ctrl+C as well so copy keeps
        // working there. Either modifier alone is fine — we only reject when
        // neither is present.
        modifiers.contains(KeyModifiers::SUPER) || modifiers.contains(KeyModifiers::CONTROL)
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
            assert!(is_copy_shortcut(&ctrl_c));
        }
        #[cfg(not(target_os = "macos"))]
        {
            assert!(!is_copy_shortcut(&cmd_c));
            assert!(is_copy_shortcut(&ctrl_c));
        }
    }
}
