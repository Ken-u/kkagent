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

/// Normalize terminal-specific aliases before dispatching a key event.
///
/// Some terminals, notably MobaXterm, report Backspace as the traditional
/// ASCII Ctrl-H chord instead of `KeyCode::Backspace`. Treat that chord as an
/// unmodified Backspace so every input surface gets the same behavior.
#[inline]
pub fn normalize_key_event(mut key: KeyEvent) -> KeyEvent {
    if matches!(key.code, KeyCode::Char('h') | KeyCode::Char('H'))
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && !key
            .modifiers
            .intersects(KeyModifiers::ALT | KeyModifiers::SUPER)
    {
        key.code = KeyCode::Backspace;
        key.modifiers = KeyModifiers::NONE;
    }
    key
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

    #[test]
    fn ctrl_h_is_normalized_to_unmodified_backspace() {
        let normalized = normalize_key_event(key(KeyCode::Char('h'), KeyModifiers::CONTROL));

        assert_eq!(normalized.code, KeyCode::Backspace);
        assert_eq!(normalized.modifiers, KeyModifiers::NONE);
    }

    #[test]
    fn modified_ctrl_h_chords_are_not_normalized() {
        let ctrl_alt_h = key(
            KeyCode::Char('h'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        );

        assert_eq!(normalize_key_event(ctrl_alt_h), ctrl_alt_h);
    }
}
