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
