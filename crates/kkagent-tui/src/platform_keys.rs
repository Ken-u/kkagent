//! Platform-aware modifier helpers for TUI keybindings.

use crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers};

/// Press and repeat events perform actions; release events must never do so.
/// Crossterm reports releases on Windows and when enhanced keyboard reporting
/// is enabled, while most Unix terminals only report presses.
#[inline]
pub fn is_actionable_key_event(key: &KeyEvent) -> bool {
    !matches!(key.kind, KeyEventKind::Release)
}

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
pub fn normalize_key_event(key: KeyEvent) -> KeyEvent {
    normalize_key_event_for_platform(key, cfg!(target_os = "windows"))
}

fn normalize_key_event_for_platform(mut key: KeyEvent, is_windows: bool) -> KeyEvent {
    if matches!(key.code, KeyCode::Char('h') | KeyCode::Char('H'))
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && !key
            .modifiers
            .intersects(KeyModifiers::ALT | KeyModifiers::SUPER)
    {
        key.code = KeyCode::Backspace;
        key.modifiers = KeyModifiers::NONE;
        return key;
    }

    // Windows exposes AltGr as Ctrl+Alt. When that chord has already produced
    // a symbol or a non-ASCII character, it is text input rather than an app
    // shortcut. ASCII letters/digits stay modified so real Ctrl-Alt shortcuts
    // are not silently converted into text.
    if is_windows
        && matches!(key.code, KeyCode::Char(c) if !c.is_ascii_alphanumeric())
        && key
            .modifiers
            .contains(KeyModifiers::CONTROL | KeyModifiers::ALT)
        && !key.modifiers.contains(KeyModifiers::SUPER)
    {
        key.modifiers
            .remove(KeyModifiers::CONTROL | KeyModifiers::ALT);
    }

    // Caps Lock can make Windows report Ctrl-C and similar shortcuts with an
    // uppercase character but without Shift. Normalize that representation;
    // explicit Ctrl-Shift shortcuts remain uppercase and distinct.
    if is_windows
        && key.modifiers.contains(KeyModifiers::CONTROL)
        && !key
            .modifiers
            .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT | KeyModifiers::SUPER)
    {
        if let KeyCode::Char(c) = key.code {
            if c.is_ascii_uppercase() {
                key.code = KeyCode::Char(c.to_ascii_lowercase());
            }
        }
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

    #[test]
    fn key_release_is_not_actionable_but_repeat_is() {
        let release = KeyEvent::new_with_kind(
            KeyCode::Char('a'),
            KeyModifiers::NONE,
            KeyEventKind::Release,
        );
        let repeat =
            KeyEvent::new_with_kind(KeyCode::Char('a'), KeyModifiers::NONE, KeyEventKind::Repeat);

        assert!(!is_actionable_key_event(&release));
        assert!(is_actionable_key_event(&repeat));
    }

    #[test]
    fn altgr_symbol_is_normalized_to_text_input() {
        let altgr_at = key(
            KeyCode::Char('@'),
            KeyModifiers::CONTROL | KeyModifiers::ALT,
        );

        assert_eq!(
            normalize_key_event_for_platform(altgr_at, true),
            key(KeyCode::Char('@'), KeyModifiers::NONE)
        );
        assert_eq!(normalize_key_event_for_platform(altgr_at, false), altgr_at);
    }

    #[test]
    fn caps_lock_uppercase_control_chord_is_normalized() {
        let ctrl_upper_c = key(KeyCode::Char('C'), KeyModifiers::CONTROL);
        let ctrl_shift_c = key(
            KeyCode::Char('C'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        );

        assert_eq!(
            normalize_key_event_for_platform(ctrl_upper_c, true),
            key(KeyCode::Char('c'), KeyModifiers::CONTROL)
        );
        assert_eq!(
            normalize_key_event_for_platform(ctrl_upper_c, false),
            ctrl_upper_c
        );
        assert_eq!(
            normalize_key_event_for_platform(ctrl_shift_c, true),
            ctrl_shift_c
        );
    }
}
