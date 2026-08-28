//! Mouse capture for in-app wheel scroll and text selection.
//!
//! Full SGR mouse capture keeps the wheel inside the TUI (scrolling the
//! transcript) and lets the app own left-drag text selection. Capture stays
//! enabled for the whole session so Mouse Up / wheel events are never lost.
//!
//! Set `KKAGENT_MOUSE_MODE=off` to disable mouse reporting entirely
//! (PgUp/PgDn only). Hold Shift while dragging if your terminal still offers
//! native selection as a fallback — kkagent does not disable that path.

use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use std::io::{self, Write};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseMode {
    /// Wheel + in-app drag selection via `Event::Mouse`.
    Capture,
    /// No mouse reporting (PgUp/PgDn only).
    Off,
}

impl MouseMode {
    pub fn parse(raw: &str) -> Self {
        match raw.trim().to_ascii_lowercase().as_str() {
            "off" | "none" | "0" => Self::Off,
            // legacy aliases from earlier builds
            "alternate-scroll" | "alt-scroll" | "1007" => Self::Off,
            _ => Self::Capture,
        }
    }

    pub fn from_env() -> Self {
        Self::parse(&std::env::var("KKAGENT_MOUSE_MODE").unwrap_or_default())
    }

    /// Resolve the effective mode: `KKAGENT_MOUSE_MODE` wins over the
    /// experimental `[mouse_mode]` config value (empty env = use config).
    pub fn resolve(config_value: Option<&str>) -> Self {
        if let Ok(env) = std::env::var("KKAGENT_MOUSE_MODE") {
            if !env.trim().is_empty() {
                return Self::parse(&env);
            }
        }
        match config_value {
            Some(v) if !v.trim().is_empty() => Self::parse(v),
            _ => Self::Capture,
        }
    }

    pub fn enable(self, out: &mut impl Write) -> io::Result<()> {
        match self {
            Self::Capture => execute!(out, EnableMouseCapture),
            Self::Off => Ok(()),
        }
    }

    pub fn disable(self, out: &mut impl Write) -> io::Result<()> {
        match self {
            Self::Capture => execute!(out, DisableMouseCapture),
            Self::Off => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_modes() {
        assert_eq!(MouseMode::parse(""), MouseMode::Capture);
        assert_eq!(MouseMode::parse("sgr"), MouseMode::Capture);
        assert_eq!(MouseMode::parse("off"), MouseMode::Off);
        assert_eq!(MouseMode::parse("alternate-scroll"), MouseMode::Off);
    }

    #[test]
    fn resolve_config_value_without_env() {
        // Tests may run with KKAGENT_MOUSE_MODE set; guard the assertions.
        std::env::remove_var("KKAGENT_MOUSE_MODE");
        assert_eq!(MouseMode::resolve(None), MouseMode::Capture);
        assert_eq!(MouseMode::resolve(Some("off")), MouseMode::Off);
        assert_eq!(MouseMode::resolve(Some("capture")), MouseMode::Capture);
        assert_eq!(MouseMode::resolve(Some("")), MouseMode::Capture);
    }

    #[test]
    fn env_overrides_config() {
        std::env::set_var("KKAGENT_MOUSE_MODE", "off");
        assert_eq!(MouseMode::resolve(Some("capture")), MouseMode::Off);
        std::env::set_var("KKAGENT_MOUSE_MODE", "capture");
        assert_eq!(MouseMode::resolve(Some("off")), MouseMode::Capture);
        // Empty env value falls back to config.
        std::env::set_var("KKAGENT_MOUSE_MODE", "");
        assert_eq!(MouseMode::resolve(Some("off")), MouseMode::Off);
        std::env::remove_var("KKAGENT_MOUSE_MODE");
    }
}
