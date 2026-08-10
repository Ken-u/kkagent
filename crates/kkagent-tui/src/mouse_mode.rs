//! Mouse capture for in-app wheel scroll.
//!
//! Full SGR mouse capture keeps the wheel inside the TUI (scrolling the
//! transcript). Releasing capture lets the terminal scroll the alternate
//! screen instead — that looks like the UI "jumps outside" the layout.
//!
//! Default: keep capture always. Hold Shift and left-click to temporarily
//! release for native drag-select; capture auto-restores after a few seconds
//! or on the next keypress.
//!
//! Set `KKAGENT_MOUSE_MODE=off` to disable mouse reporting entirely.

use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use std::io::{self, Write};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseMode {
    /// Wheel scroll via `Event::Mouse`; Shift+click releases capture for select.
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
