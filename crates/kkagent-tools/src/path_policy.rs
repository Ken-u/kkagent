//! Shared path access helpers (sensitive files, binary sniffing, CRLF).

use std::path::Path;

const SENSITIVE_GLOBS: &[&str] = &[
    "!**/.env",
    "!**/.env.*",
    "!**/id_rsa",
    "!**/id_ed25519",
    "!**/id_ecdsa",
    "!**/*.pem",
    "!**/*.key",
    "!**/.netrc",
    "!**/.npmrc",
    "!**/credentials",
];

pub fn sensitive_glob_excludes() -> &'static [&'static str] {
    SENSITIVE_GLOBS
}

const BINARY_EXTS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "bmp", "ico", "pdf", "zip", "gz", "tar", "7z", "exe",
    "dll", "so", "dylib", "bin", "wasm", "mp4", "mov", "avi", "mkv", "mp3", "wav", "woff", "woff2",
    "ttf", "otf", "class", "o", "a", "pyc", "pyo",
];

pub fn is_sensitive_path(path: &Path) -> bool {
    let components: Vec<String> = path
        .components()
        .filter_map(|part| part.as_os_str().to_str())
        .map(str::to_ascii_lowercase)
        .collect();
    let Some(filename) = components.last() else {
        return false;
    };

    let env_file = filename == ".env"
        || (filename.starts_with(".env.")
            && !matches!(
                filename.as_str(),
                ".env.example" | ".env.sample" | ".env.template"
            ));
    let private_key = matches!(filename.as_str(), "id_rsa" | "id_ed25519" | "id_ecdsa")
        || filename.ends_with(".pem")
        || filename.ends_with(".key");
    let credential_file = matches!(
        filename.as_str(),
        "credentials" | ".netrc" | ".npmrc" | "secrets.json" | "tokens.json"
    );
    let cloud_credentials = components
        .windows(2)
        .any(|pair| matches!(pair, [dir, file] if (dir == ".aws" || dir == "aws" || dir == "gcloud") && file == "credentials"));

    env_file || private_key || credential_file || cloud_credentials
}

pub fn looks_binary_ext(path: &Path) -> bool {
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| BINARY_EXTS.iter().any(|x| e.eq_ignore_ascii_case(x)))
        .unwrap_or(false)
}

pub fn sniff_binary(bytes: &[u8]) -> bool {
    if bytes.starts_with(&[0x00]) || bytes.contains(&0) {
        // NUL in first chunk → binary
        let sample = &bytes[..bytes.len().min(8192)];
        return sample.contains(&0);
    }
    false
}

pub fn detect_crlf(content: &str) -> bool {
    content.contains("\r\n")
}

pub fn restore_line_endings(content: &str, crlf: bool) -> String {
    if !crlf {
        return content.replace("\r\n", "\n");
    }
    // Normalize to LF then expand to CRLF
    content.replace("\r\n", "\n").replace('\n', "\r\n")
}

/// Decode text bytes: UTF-8, UTF-16 LE/BE BOM, or lossy UTF-8.
pub fn decode_text(bytes: &[u8]) -> Result<String, String> {
    if bytes.starts_with(&[0xFF, 0xFE]) {
        let u16s: Vec<u16> = bytes[2..]
            .chunks(2)
            .filter_map(|c| {
                if c.len() == 2 {
                    Some(u16::from_le_bytes([c[0], c[1]]))
                } else {
                    None
                }
            })
            .collect();
        return String::from_utf16(&u16s).map_err(|_| "Invalid UTF-16 LE".into());
    }
    if bytes.starts_with(&[0xFE, 0xFF]) {
        let u16s: Vec<u16> = bytes[2..]
            .chunks(2)
            .filter_map(|c| {
                if c.len() == 2 {
                    Some(u16::from_be_bytes([c[0], c[1]]))
                } else {
                    None
                }
            })
            .collect();
        return String::from_utf16(&u16s).map_err(|_| "Invalid UTF-16 BE".into());
    }
    if sniff_binary(bytes) {
        return Err("File appears to be binary".into());
    }
    Ok(String::from_utf8_lossy(bytes).into_owned())
}

pub const MAX_LINE_LENGTH: usize = 2000;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sensitive_matching_avoids_source_code_false_positives() {
        assert!(is_sensitive_path(Path::new("project/.env.local")));
        assert!(is_sensitive_path(Path::new("keys/client.pem")));
        assert!(is_sensitive_path(Path::new("~/.aws/credentials")));
        assert!(!is_sensitive_path(Path::new("project/.env.example")));
        assert!(!is_sensitive_path(Path::new("src/token_counting.rs")));
        assert!(!is_sensitive_path(Path::new("src/secretary.rs")));
    }
}
