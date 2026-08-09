//! Terminal image protocols — Kitty / iTerm2 (pi-tui terminal-image).

use base64::Engine;
use std::io::Write;
use std::path::Path;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ImageProtocol {
    Kitty,
    ITerm2,
}

#[derive(Debug, Clone)]
pub struct TerminalCapabilities {
    pub images: Option<ImageProtocol>,
    pub true_color: bool,
    pub hyperlinks: bool,
}

pub fn detect_capabilities() -> TerminalCapabilities {
    let term_program = std::env::var("TERM_PROGRAM")
        .unwrap_or_default()
        .to_lowercase();
    let term = std::env::var("TERM").unwrap_or_default().to_lowercase();
    let colorterm = std::env::var("COLORTERM").unwrap_or_default().to_lowercase();
    let true_color = colorterm == "truecolor" || colorterm == "24bit";

    if std::env::var_os("TMUX").is_some() || term.starts_with("tmux") {
        return TerminalCapabilities {
            images: None,
            true_color,
            hyperlinks: false,
        };
    }
    if term.starts_with("screen") {
        return TerminalCapabilities {
            images: None,
            true_color,
            hyperlinks: false,
        };
    }

    let images = if term_program.contains("iterm")
        || std::env::var_os("ITERM_SESSION_ID").is_some()
    {
        Some(ImageProtocol::ITerm2)
    } else if term_program.contains("kitty")
        || term.contains("kitty")
        || std::env::var_os("KITTY_WINDOW_ID").is_some()
        || term_program.contains("wezterm")
        || term.contains("wezterm")
        || term_program.contains("ghostty")
    {
        Some(ImageProtocol::Kitty)
    } else {
        None
    };

    TerminalCapabilities {
        images,
        true_color,
        hyperlinks: true,
    }
}

#[derive(Debug, Clone)]
pub struct ImageRenderOptions {
    pub max_width_cells: Option<u32>,
    pub max_height_cells: Option<u32>,
    pub image_id: Option<u32>,
    pub move_cursor: bool,
}

impl Default for ImageRenderOptions {
    fn default() -> Self {
        Self {
            max_width_cells: Some(80),
            max_height_cells: Some(24),
            image_id: None,
            move_cursor: true,
        }
    }
}

/// Encode image bytes as a terminal escape sequence for the detected protocol.
pub fn encode_image(bytes: &[u8], mime: &str, opts: &ImageRenderOptions) -> Option<String> {
    let caps = detect_capabilities();
    match caps.images? {
        ImageProtocol::Kitty => Some(encode_kitty(bytes, mime, opts)),
        ImageProtocol::ITerm2 => Some(encode_iterm2(bytes, opts)),
    }
}

pub fn encode_image_file(path: &Path, opts: &ImageRenderOptions) -> anyhow::Result<Option<String>> {
    let bytes = std::fs::read(path)?;
    let mime = match path
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_lowercase()
        .as_str()
    {
        "png" => "image/png",
        "jpg" | "jpeg" => "image/jpeg",
        "gif" => "image/gif",
        "webp" => "image/webp",
        _ => "image/png",
    };
    Ok(encode_image(&bytes, mime, opts))
}

fn encode_kitty(bytes: &[u8], _mime: &str, opts: &ImageRenderOptions) -> String {
    let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
    let id = opts.image_id.unwrap_or(1);
    let mut out = String::new();
    // chunk into 4096-char pieces per Kitty protocol
    let chunks: Vec<&str> = b64
        .as_bytes()
        .chunks(4096)
        .map(|c| std::str::from_utf8(c).unwrap_or(""))
        .collect();
    for (i, chunk) in chunks.iter().enumerate() {
        let more = if i + 1 < chunks.len() { 1 } else { 0 };
        if i == 0 {
            let mut ctrl = format!("a=T,f=100,i={id},m={more}");
            if let Some(w) = opts.max_width_cells {
                ctrl.push_str(&format!(",c={w}"));
            }
            if let Some(h) = opts.max_height_cells {
                ctrl.push_str(&format!(",r={h}"));
            }
            if !opts.move_cursor {
                ctrl.push_str(",C=1");
            }
            out.push_str(&format!("\x1b_G{ctrl};{chunk}\x1b\\"));
        } else {
            out.push_str(&format!("\x1b_Gm={more};{chunk}\x1b\\"));
        }
    }
    out
}

fn encode_iterm2(bytes: &[u8], opts: &ImageRenderOptions) -> String {
    let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
    let mut args = format!("inline=1;size={}", bytes.len());
    if let Some(w) = opts.max_width_cells {
        args.push_str(&format!(";width={w}"));
    }
    if let Some(h) = opts.max_height_cells {
        args.push_str(&format!(";height={h}"));
    }
    format!("\x1b]1337;File={args}:{b64}\x07")
}

/// Best-effort write image to stdout (no-op when unsupported).
pub fn try_print_image_file(path: &Path) -> bool {
    match encode_image_file(path, &ImageRenderOptions::default()) {
        Ok(Some(seq)) => {
            let mut out = std::io::stdout().lock();
            let _ = out.write_all(seq.as_bytes());
            let _ = out.flush();
            true
        }
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detect_does_not_panic() {
        let _ = detect_capabilities();
    }

    #[test]
    fn kitty_encode_nonempty() {
        // Force encode path regardless of env
        let s = encode_kitty(b"\x89PNG", "image/png", &ImageRenderOptions::default());
        assert!(s.contains("\x1b_G"));
    }
}
