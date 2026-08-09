//! Mouse capture for wheel scroll, with click-to-release for native selection.
//!
//! Enabling SGR mouse capture lets the app handle the wheel, but blocks the
//! terminal's drag-select. On left-click we temporarily disable capture so the
//! same gesture (or the next drag) can select text; the next keypress restores
//! capture. ↑↓ stay bound to input history.
//!
//! Set `KKAGENT_MOUSE_MODE=off` to disable mouse reporting entirely.

use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use std::io::{self, Write};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseMode {
    /// Wheel scroll via `Event::Mouse`; left-click releases capture for select.
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
}
