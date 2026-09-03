use kkagent_config::UiConfig;
use ratatui::style::Color;

/// 对齐 kimi-code dark 主题色板
#[derive(Clone)]
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
    pub background: Color,
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
            // yolo / auto badge — same warm amber as kimi
            warning: Color::Rgb(0xE8, 0xA8, 0x38),
            error: Color::Rgb(0xE8, 0x54, 0x54),
            background: Color::Rgb(0x1E, 0x1E, 0x1E),
            // user marker + message body (kimi yellow user bubble)
            role_user: Color::Rgb(0xE8, 0xA8, 0x38),
            shell_mode: Color::Rgb(0xBD, 0x93, 0xF9),
            plan_mode: Color::Rgb(0x4F, 0xA8, 0xFF),
        }
    }
}

impl Theme {
    /// Build the palette from `[ui.theme]` config overrides. Unset entries
    /// (or values that fail to parse) keep the built-in default for that
    /// color, so partial customization is always safe.
    pub fn from_ui(ui: &UiConfig) -> Self {
        let mut theme = Self::default();
        let t = &ui.theme;
        apply_override(&mut theme.primary, &t.primary);
        apply_override(&mut theme.accent, &t.accent);
        apply_override(&mut theme.text, &t.text);
        apply_override(&mut theme.text_strong, &t.text_strong);
        apply_override(&mut theme.text_dim, &t.text_dim);
        apply_override(&mut theme.text_muted, &t.text_muted);
        apply_override(&mut theme.border, &t.border);
        apply_override(&mut theme.border_focus, &t.border_focus);
        apply_override(&mut theme.success, &t.success);
        apply_override(&mut theme.warning, &t.warning);
        apply_override(&mut theme.error, &t.error);
        apply_override(&mut theme.background, &t.background);
        apply_override(&mut theme.role_user, &t.role_user);
        apply_override(&mut theme.shell_mode, &t.shell_mode);
        apply_override(&mut theme.plan_mode, &t.plan_mode);
        theme
    }

    /// Palette for the goal judge window: the global `[ui.theme]` palette
    /// with `[ui.theme.goal_judge]` overrides layered on top. Unset
    /// per-window entries therefore inherit the global colors.
    pub fn from_ui_with_goal_judge(ui: &UiConfig) -> Self {
        let mut theme = Self::from_ui(ui);
        let t = &ui.theme.goal_judge;
        apply_override(&mut theme.border, &t.border);
        apply_override(&mut theme.accent, &t.accent);
        apply_override(&mut theme.primary, &t.primary);
        apply_override(&mut theme.error, &t.error);
        apply_override(&mut theme.text_muted, &t.text_muted);
        theme
    }
}

fn apply_override(slot: &mut Color, value: &Option<String>) {
    if let Some(color) = value.as_deref().and_then(parse_color) {
        *slot = color;
    }
}

/// Parse a user-supplied color value: `#RGB` or `#RRGGBB` hex, with the
/// leading `#` optional. Anything else (named colors, 8-digit hex, empty)
/// is rejected so typos fall back to the default palette.
fn parse_color(value: &str) -> Option<Color> {
    let value = value.trim();
    let value = value.strip_prefix('#').unwrap_or(value);
    let digits: Vec<u8> = value
        .chars()
        .map(|c| c.to_digit(16).map(|d| d as u8))
        .collect::<Option<_>>()?;
    match digits.as_slice() {
        [r, g, b] => Some(Color::Rgb(r * 17, g * 17, b * 17)),
        [r1, r2, g1, g2, b1, b2] => Some(Color::Rgb(r1 * 16 + r2, g1 * 16 + g2, b1 * 16 + b2)),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_color_accepts_rgb_and_rrggbb() {
        assert_eq!(parse_color("#4FA8FF"), Some(Color::Rgb(0x4F, 0xA8, 0xFF)));
        assert_eq!(parse_color("5bc0be"), Some(Color::Rgb(0x5B, 0xC0, 0xBE)));
        assert_eq!(parse_color("#F0A"), Some(Color::Rgb(0xFF, 0x00, 0xAA)));
        assert_eq!(parse_color(" nope "), None);
        assert_eq!(parse_color("#12345"), None);
        assert_eq!(parse_color(""), None);
    }

    #[test]
    fn from_ui_applies_overrides_and_keeps_defaults() {
        let mut ui = UiConfig::default();
        ui.theme.border = Some("#FF0000".into());
        let theme = Theme::from_ui(&ui);
        assert_eq!(theme.border, Color::Rgb(255, 0, 0));
        assert_eq!(theme.accent, Theme::default().accent);

        // Invalid values fall back to the default palette entry.
        ui.theme.accent = Some("oops".into());
        assert_eq!(Theme::from_ui(&ui).accent, Theme::default().accent);
    }

    #[test]
    fn goal_judge_overrides_layer_on_top_of_the_global_theme() {
        let mut ui = UiConfig::default();
        ui.theme.border = Some("#00FF00".into());
        ui.theme.goal_judge.border = Some("#FF0000".into());

        let base = Theme::from_ui(&ui);
        let judge = Theme::from_ui_with_goal_judge(&ui);
        assert_eq!(base.border, Color::Rgb(0, 255, 0));
        assert_eq!(judge.border, Color::Rgb(255, 0, 0));
        // Unset goal-judge entries inherit the global [ui.theme] value.
        assert_eq!(judge.accent, base.accent);
    }
}
