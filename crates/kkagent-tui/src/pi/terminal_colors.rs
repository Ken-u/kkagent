//! Terminal color helpers for chrome / status.

use ratatui::style::{Color, Style};

#[derive(Debug, Clone, Copy)]
pub struct Theme {
    pub accent: Color,
    pub muted: Color,
    pub danger: Color,
    pub success: Color,
    pub border: Color,
    pub bg: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            accent: Color::Cyan,
            muted: Color::DarkGray,
            danger: Color::Red,
            success: Color::Green,
            border: Color::DarkGray,
            bg: Color::Reset,
        }
    }
}

impl Theme {
    pub fn title(&self) -> Style {
        Style::default().fg(self.accent)
    }
    pub fn dim(&self) -> Style {
        Style::default().fg(self.muted)
    }
    pub fn ok(&self) -> Style {
        Style::default().fg(self.success)
    }
    pub fn err(&self) -> Style {
        Style::default().fg(self.danger)
    }
    pub fn border(&self) -> Style {
        Style::default().fg(self.border)
    }
}
