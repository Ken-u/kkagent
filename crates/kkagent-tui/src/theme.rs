use ratatui::style::Color;

/// 对齐 kimi-code dark 主题色板
pub struct Theme {
    pub primary: Color,
    pub accent: Color,
    pub text: Color,
    pub text_strong: Color,
    pub text_dim: Color,
    pub text_muted: Color,
    pub border: Color,
    pub border_focus: Color,
    pub success: Color,
    pub warning: Color,
    pub error: Color,
    pub role_user: Color,
    pub shell_mode: Color,
    pub plan_mode: Color,
}

impl Default for Theme {
    fn default() -> Self {
        Self {
            primary: Color::Rgb(0x4F, 0xA8, 0xFF),
            accent: Color::Rgb(0x5B, 0xC0, 0xBE),
            text: Color::Rgb(0xE0, 0xE0, 0xE0),
            text_strong: Color::Rgb(0xF5, 0xF5, 0xF5),
            text_dim: Color::Rgb(0x88, 0x88, 0x88),
            text_muted: Color::Rgb(0x6B, 0x6B, 0x6B),
            border: Color::Rgb(0x5A, 0x5A, 0x5A),
            border_focus: Color::Rgb(0xE8, 0xA8, 0x38),
            success: Color::Rgb(0x4E, 0xC8, 0x7E),
            warning: Color::Rgb(0xE8, 0xA8, 0x38),
            error: Color::Rgb(0xE8, 0x54, 0x54),
            role_user: Color::Rgb(0xFF, 0xCB, 0x6B),
            shell_mode: Color::Rgb(0xBD, 0x93, 0xF9),
            plan_mode: Color::Rgb(0x4F, 0xA8, 0xFF),
        }
    }
}
