//! Platform-aware modifier helpers for TUI keybindings.

use crossterm::event::{KeyEvent, KeyModifiers};

/// Returns true when the key event is the platform "copy" shortcut:
/// - macOS: Command + C (`SUPER` modifier)
/// - Linux / Windows: Ctrl + C
#[inline]
pub fn is_copy_shortcut(key: &KeyEvent) -> bool {
    matches!(key.code, crossterm::event::KeyCode::Char('c'))
        && is_platform_copy_modifier(key.modifiers)
}

/// Returns true when the modifiers represent the platform copy modifier.
#[inline]
#[allow(unreachable_patterns)]
pub fn is_platform_copy_modifier(modifiers: KeyModifiers) -> bool {
    #[cfg(target_os = "macos")]
    {
        // crossterm reports Command as SUPER on macOS.
        modifiers.contains(KeyModifiers::SUPER)
    }
    #[cfg(not(target_os = "macos"))]
    {
        modifiers.contains(KeyModifiers::CONTROL)
    }
}
