//! Mouse protocol for the TUI.
//!
//! Full SGR mouse capture (`?1000h`) gives wheel scroll but steals drag-select
//! from the terminal. Alternate-scroll (`?1007h`) is the compatible default:
//! the terminal turns the wheel into Up/Down (or PgUp/PgDn) keys while leaving
//! click-drag selection native.
//!
//! Override with `KKAGENT_MOUSE_MODE=alternate-scroll|sgr|off`.

use crossterm::event::{DisableMouseCapture, EnableMouseCapture};
use crossterm::execute;
use std::io::{self, Write};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MouseMode {
    /// Wheel → cursor/page keys; native drag-select works.
    AlternateScroll,
    /// Full mouse capture; wheel is `Event::Mouse`; use Shift/Option+drag to select.
    Sgr,
    /// No mouse reporting.
    Off,
}

impl MouseMode {
    pub fn from_env() -> Self {
        match std::env::var("KKAGENT_MOUSE_MODE")
            .unwrap_or_default()
            .to_ascii_lowercase()
            .as_str()
        {
            "sgr" | "capture" | "full" => Self::Sgr,
            "off" | "none" | "0" => Self::Off,
            _ => Self::AlternateScroll,
        }
    }

    pub fn enable(self, out: &mut impl Write) -> io::Result<()> {
        match self {
            Self::AlternateScroll => {
                out.write_all(b"\x1b[?1007h")?;
                out.flush()
            }
            Self::Sgr => execute!(out, EnableMouseCapture),
            Self::Off => Ok(()),
        }
    }

    pub fn disable(self, out: &mut impl Write) -> io::Result<()> {
        match self {
            Self::AlternateScroll => {
                out.write_all(b"\x1b[?1007l")?;
                out.flush()
            }
            Self::Sgr => execute!(out, DisableMouseCapture),
            Self::Off => Ok(()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_is_alternate_scroll() {
        // Ensure unknown values fall back without panicking.
        assert_eq!(MouseMode::from_env(), MouseMode::AlternateScroll);
    }
}
