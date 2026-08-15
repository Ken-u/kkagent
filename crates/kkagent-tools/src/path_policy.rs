//! Shared path access helpers (sensitive files, binary sniffing, CRLF).

use std::path::{Component, Path, PathBuf};

// ---------------------------------------------------------------------------
// S0-2: Glob-based excludes — kept in sync with `is_sensitive_path`.
// ---------------------------------------------------------------------------

const SENSITIVE_GLOBS: &[&str] = &[
    // .env files (but NOT .env.example/.sample/.template)
    "!**/.env",
    "!**/.env.local",
    "!**/.env.production",
    "!**/.env.staging",
    "!**/.env.development",
    "!**/.env.dev",
    "!**/.env.prod",
    "!**/.env.stag",
    // SSH private keys + variant backups (S0-1)
    "!**/id_rsa",
    "!**/id_rsa-*",
    "!**/id_rsa_*",
    "!**/id_rsa.bak",
    "!**/id_rsa.backup",
    "!**/id_rsa.copy",
    "!**/id_rsa.disabled",
    "!**/id_rsa.old",
    "!**/id_rsa.orig",
    "!**/id_rsa.save",
    "!**/id_rsa.tmp",
    "!**/id_ed25519",
    "!**/id_ed25519-*",
    "!**/id_ed25519_*",
    "!**/id_ed25519.bak",
    "!**/id_ed25519.backup",
    "!**/id_ed25519.copy",
    "!**/id_ed25519.disabled",
    "!**/id_ed25519.old",
    "!**/id_ed25519.orig",
    "!**/id_ed25519.save",
    "!**/id_ed25519.tmp",
    "!**/id_ecdsa",
    "!**/id_ecdsa-*",
    "!**/id_ecdsa_*",
    "!**/id_ecdsa.bak",
    "!**/id_ecdsa.backup",
    "!**/id_ecdsa.copy",
    "!**/id_ecdsa.disabled",
    "!**/id_ecdsa.old",
    "!**/id_ecdsa.orig",
    "!**/id_ecdsa.save",
    "!**/id_ecdsa.tmp",
    // Certificate / key files
    "!**/*.pem",
    "!**/*.key",
    // Netrc / npmrc
    "!**/.netrc",
    "!**/.npmrc",
    // Credentials + variant backups (S0-1)
    "!**/credentials",
    "!**/credentials-*",
    "!**/credentials_*",
    "!**/credentials.bak",
    "!**/credentials.backup",
    "!**/credentials.copy",
    "!**/credentials.disabled",
    "!**/credentials.old",
    "!**/credentials.orig",
    "!**/credentials.save",
    "!**/credentials.tmp",
    // Secret/token files
    "!**/secrets.json",
    "!**/tokens.json",
    // Cloud credential paths (S0-3)
    "!**/.aws/credentials",
    "!**/.aws/config",
    "!**/.gcp/credentials",
    "!**/.gcp/application_default_credentials.json",
    "!**/.config/gcloud/credentials.db",
    "!**/.config/gcloud/application_default_credentials.json",
    "!**/.kube/config",
    "!**/.docker/config.json",
    "!**/.docker/credentials",
];

pub fn sensitive_glob_excludes() -> &'static [&'static str] {
    SENSITIVE_GLOBS
}

// ---------------------------------------------------------------------------
// S0-1 / S0-2 / S0-3: Sensitive path detection
// ---------------------------------------------------------------------------

/// Basenames whose variants should also be treated as sensitive.
const SENSITIVE_PREFIXES: &[&str] = &["id_rsa", "id_ed25519", "id_ecdsa", "credentials"];

/// Extension variants for backup/temp copies.
const VARIANT_EXTS: &[&str] = &[
    "bak", "backup", "copy", "disabled", "old", "orig", "save", "tmp", "key", "pem",
];

/// `.env` suffixes that are NOT secrets (templates/examples).
const ENV_SAFE_SUFFIXES: &[&str] = &["example", "sample", "template"];

/// Cloud credential directory/file pairs.
/// Each entry: (dir_basename, [file_basenames])
const CLOUD_CRED_DIRS: &[(&str, &[&str])] = &[
    (".aws", &["credentials", "config"]),
    ("aws", &["credentials", "config"]),
    (
        ".gcp",
        &["credentials", "application_default_credentials.json"],
    ),
    (
        ".config",
        &[
            "gcloud", // .config/gcloud/credentials.db handled specially
        ],
    ),
    (".kube", &["config"]),
    (".docker", &["config.json", "credentials"]),
    ("gcloud", &["credentials"]),
];

/// Returns `true` if the given path points to a sensitive file (credentials,
/// private keys, `.env` files, cloud credentials, etc.).
///
/// Uses **lexical** path analysis only — never calls `canonicalize()` — to
/// avoid TOCTOU races (S2-7).  This means a symlink named `id_rsa` will still
/// be detected because the check operates on the user-supplied path string,
/// not the resolved target.
pub fn is_sensitive_path(path: &Path) -> bool {
    let components: Vec<String> = path
        .components()
        .filter_map(|part| part.as_os_str().to_str())
        .map(str::to_ascii_lowercase)
        .collect();
    let Some(filename) = components.last() else {
        return false;
    };
    let filename = filename.as_str();

    // S0-2: Public key exemption — `.pub` files are never sensitive.
    if filename.ends_with(".pub") {
        return false;
    }

    // .env files (but NOT .env.example / .env.sample / .env.template)
    if is_env_file(filename) {
        return true;
    }

    // Private key files (exact match or .pem/.key extension)
    if is_private_key_file(filename) {
        return true;
    }

    // S0-1: Sensitive basename variants
    // e.g. id_rsa-old, id_rsa_backup, credentials.bak, credentials.pem
    if is_sensitive_variant(filename) {
        return true;
    }

    // Credential / secret files (exact match)
    if is_credential_file(filename) {
        return true;
    }

    // S0-3: Cloud credential paths
    if is_cloud_credential_path(&components) {
        return true;
    }

    false
}

fn is_env_file(filename: &str) -> bool {
    if filename == ".env" {
        return true;
    }
    if let Some(suffix) = filename.strip_prefix(".env.") {
        // Allow templates/examples
        if ENV_SAFE_SUFFIXES.contains(&suffix) {
            return false;
        }
        return true;
    }
    false
}

fn is_private_key_file(filename: &str) -> bool {
    matches!(filename, "id_rsa" | "id_ed25519" | "id_ecdsa")
        || filename.ends_with(".pem")
        || filename.ends_with(".key")
}

/// S0-1: Check if `filename` is a variant of a sensitive basename.
///
/// Matches patterns like:
/// - `id_rsa-old`, `id_rsa_backup`  (separator + arbitrary suffix, no extension)
/// - `id_rsa.bak`, `credentials.pem` (dot + known variant extension)
///
/// When the filename has a dot-delimited extension that is NOT in the known
/// variant set (e.g. `.rs`, `.py`, `.txt`), the separator variant does NOT
/// trigger — this prevents false positives like `id_rsa_helper.rs`.
fn is_sensitive_variant(filename: &str) -> bool {
    for prefix in SENSITIVE_PREFIXES {
        if filename == *prefix {
            continue; // exact match handled elsewhere
        }
        if let Some(rest) = filename.strip_prefix(prefix) {
            // Separator variant: prefix + `-` or `_` + at least 1 char
            if (rest.starts_with('-') || rest.starts_with('_')) && rest.len() > 1 {
                // Only flag if there is no extension, or the extension is a
                // known variant extension.  This avoids false positives like
                // `id_rsa_helper.rs` (source files).
                let has_non_variant_ext = rest
                    .rsplit_once('.')
                    .is_some_and(|(_, ext)| !VARIANT_EXTS.contains(&ext) && !matches!(ext, "pub"));
                if !has_non_variant_ext {
                    return true;
                }
            }
            // Extension variant: prefix + `.` + known extension
            if let Some(ext) = rest.strip_prefix('.') {
                if VARIANT_EXTS.contains(&ext) {
                    return true;
                }
            }
        }
    }
    false
}

fn is_credential_file(filename: &str) -> bool {
    matches!(
        filename,
        "credentials" | ".netrc" | ".npmrc" | "secrets.json" | "tokens.json"
    )
}

/// S0-3: Check cloud credential paths via directory/file component pairs.
fn is_cloud_credential_path(components: &[String]) -> bool {
    // Check pairs of consecutive components (dir, file)
    for window in components.windows(2) {
        let dir = window[0].as_str();
        let file = window[1].as_str();

        // Special case: .config/gcloud/credentials.db
        if dir == ".config" && file == "gcloud" {
            // The actual file is the next component after "gcloud"
            // e.g. ["...",".config","gcloud","credentials.db"]
            // This is handled by the three-component check below.
            continue;
        }

        for (cred_dir, cred_files) in CLOUD_CRED_DIRS {
            if dir == *cred_dir && cred_files.contains(&file) {
                return true;
            }
        }
    }

    // Three-component check for .config/gcloud/credentials.db
    for window in components.windows(3) {
        if window[0] == ".config"
            && window[1] == "gcloud"
            && (window[2] == "credentials.db"
                || window[2] == "application_default_credentials.json"
                || window[2] == "legacy_credentials")
        {
            return true;
        }
    }

    false
}

// ---------------------------------------------------------------------------
// S1-4: Workspace directory constraint helpers
// ---------------------------------------------------------------------------

/// Lexically normalizes a path (resolves `.` and `..` without touching the
/// filesystem).  Used for security decisions to avoid TOCTOU races (S2-7).
fn lexical_normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for comp in path.components() {
        match comp {
            Component::CurDir => {} // skip "."
            Component::ParentDir => {
                // Pop unless the last element is a root/prefix
                match out.components().next_back() {
                    Some(Component::Prefix(_)) | Some(Component::RootDir) | None => {}
                    Some(Component::Normal(_)) => {
                        out.pop();
                    }
                    Some(Component::ParentDir) => {
                        // Keep stacking `..` (e.g. path starts with `../..`)
                        out.push("..");
                    }
                    Some(Component::CurDir) => unreachable!(),
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Returns `true` if `path` is lexically within `base` (i.e. `base` is an
/// ancestor of or equal to `path`).  Does NOT touch the filesystem.
pub fn is_within_directory(base: &Path, path: &Path) -> bool {
    let base_n = lexical_normalize(base);
    let path_n = lexical_normalize(path);

    if base_n == path_n {
        return true;
    }
    path_n.starts_with(&base_n)
}

/// Returns `true` if `path` is within the workspace or any of the
/// `additional_dirs`.
pub fn is_within_workspace(working_dir: &Path, additional_dirs: &[PathBuf], path: &Path) -> bool {
    if is_within_directory(working_dir, path) {
        return true;
    }
    additional_dirs.iter().any(|d| is_within_directory(d, path))
}

// ---------------------------------------------------------------------------
// Binary sniffing / CRLF / text decoding
// ---------------------------------------------------------------------------

const BINARY_EXTS: &[&str] = &[
    "png", "jpg", "jpeg", "gif", "webp", "bmp", "ico", "pdf", "zip", "gz", "tar", "7z", "exe",
    "dll", "so", "dylib", "bin", "wasm", "mp4", "mov", "avi", "mkv", "mp3", "wav", "woff", "woff2",
    "ttf", "otf", "class", "o", "a", "pyc", "pyo",
];

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

/// Decode text bytes: UTF-8, UTF-16 LE/BE BOM, UTF-16 without BOM (NUL parity), or lossy UTF-8.
pub fn decode_text(bytes: &[u8]) -> Result<String, String> {
    if bytes.starts_with(&[0xFF, 0xFE]) {
        return decode_utf16(&bytes[2..], true);
    }
    if bytes.starts_with(&[0xFE, 0xFF]) {
        return decode_utf16(&bytes[2..], false);
    }
    // Heuristic: even length + many NULs in odd/even positions → UTF-16 without BOM.
    if bytes.len() >= 4 && bytes.len().is_multiple_of(2) {
        let sample = &bytes[..bytes.len().min(512)];
        let even_nul = sample.iter().step_by(2).filter(|&&b| b == 0).count();
        let odd_nul = sample
            .iter()
            .skip(1)
            .step_by(2)
            .filter(|&&b| b == 0)
            .count();
        let pairs = sample.len() / 2;
        if pairs > 0 && even_nul * 2 > pairs {
            return decode_utf16(bytes, true); // LE: ASCII as XX 00
        }
        if pairs > 0 && odd_nul * 2 > pairs {
            return decode_utf16(bytes, false); // BE: ASCII as 00 XX
        }
    }
    if sniff_binary(bytes) {
        return Err("File appears to be binary".into());
    }
    Ok(String::from_utf8_lossy(bytes).into_owned())
}

fn decode_utf16(bytes: &[u8], little_endian: bool) -> Result<String, String> {
    let u16s: Vec<u16> = bytes
        .chunks(2)
        .filter_map(|c| {
            if c.len() == 2 {
                Some(if little_endian {
                    u16::from_le_bytes([c[0], c[1]])
                } else {
                    u16::from_be_bytes([c[0], c[1]])
                })
            } else {
                None
            }
        })
        .collect();
    String::from_utf16(&u16s).map_err(|_| {
        if little_endian {
            "Invalid UTF-16 LE".into()
        } else {
            "Invalid UTF-16 BE".into()
        }
    })
}

pub const MAX_LINE_LENGTH: usize = 2000;

#[cfg(test)]
mod tests {
    use super::*;

    // --- S0-1: Sensitive file variant coverage ---

    #[test]
    fn sensitive_variant_backup_files() {
        assert!(is_sensitive_path(Path::new("~/.ssh/id_rsa.bak")));
        assert!(is_sensitive_path(Path::new("~/.ssh/id_rsa-old")));
        assert!(is_sensitive_path(Path::new("~/.ssh/id_rsa_backup")));
        assert!(is_sensitive_path(Path::new("keys/id_ed25519.old")));
        assert!(is_sensitive_path(Path::new("keys/id_ed25519-copy")));
        assert!(is_sensitive_path(Path::new("keys/id_ecdsa.orig")));
        assert!(is_sensitive_path(Path::new("config/credentials.bak")));
        assert!(is_sensitive_path(Path::new("config/credentials-backup")));
        assert!(is_sensitive_path(Path::new("config/credentials.pem")));
        assert!(is_sensitive_path(Path::new("config/credentials.key")));
    }

    #[test]
    fn sensitive_variant_not_triggered_on_similar_names() {
        // These should NOT be flagged — not actual variants of sensitive files
        assert!(!is_sensitive_path(Path::new("src/id_rsa_helper.rs")));
        assert!(!is_sensitive_path(Path::new("src/credentials_checker.rs")));
        assert!(!is_sensitive_path(Path::new("docs/id_rsa_guide.md")));
    }

    // --- S0-2: Public key exemption ---

    #[test]
    fn public_key_files_not_flagged() {
        assert!(!is_sensitive_path(Path::new("~/.ssh/id_rsa.pub")));
        assert!(!is_sensitive_path(Path::new("~/.ssh/id_ed25519.pub")));
        assert!(!is_sensitive_path(Path::new("~/.ssh/id_ecdsa.pub")));
        // .pub variant should not be sensitive even with a weird stem
        assert!(!is_sensitive_path(Path::new("keys/random.pub")));
    }

    #[test]
    fn private_key_files_still_flagged() {
        assert!(is_sensitive_path(Path::new("~/.ssh/id_rsa")));
        assert!(is_sensitive_path(Path::new("~/.ssh/id_ed25519")));
        assert!(is_sensitive_path(Path::new("~/.ssh/id_ecdsa")));
        assert!(is_sensitive_path(Path::new("keys/client.pem")));
        assert!(is_sensitive_path(Path::new("keys/client.key")));
    }

    // --- S0-3: Cloud credential path completion ---

    #[test]
    fn cloud_credential_paths_detected() {
        assert!(is_sensitive_path(Path::new("~/.aws/credentials")));
        assert!(is_sensitive_path(Path::new("~/.aws/config")));
        assert!(is_sensitive_path(Path::new("~/.gcp/credentials")));
        assert!(is_sensitive_path(Path::new(
            "~/.gcp/application_default_credentials.json"
        )));
        assert!(is_sensitive_path(Path::new("~/.kube/config")));
        assert!(is_sensitive_path(Path::new("~/.docker/config.json")));
        assert!(is_sensitive_path(Path::new("~/.docker/credentials")));
        assert!(is_sensitive_path(Path::new(
            "~/.config/gcloud/credentials.db"
        )));
        assert!(is_sensitive_path(Path::new(
            "~/.config/gcloud/application_default_credentials.json"
        )));
    }

    // --- Existing tests ---

    #[test]
    fn sensitive_matching_avoids_source_code_false_positives() {
        assert!(is_sensitive_path(Path::new("project/.env.local")));
        assert!(is_sensitive_path(Path::new("keys/client.pem")));
        assert!(is_sensitive_path(Path::new("~/.aws/credentials")));
        assert!(!is_sensitive_path(Path::new("project/.env.example")));
        assert!(!is_sensitive_path(Path::new("src/token_counting.rs")));
        assert!(!is_sensitive_path(Path::new("src/secretary.rs")));
    }

    #[test]
    fn env_file_variants() {
        assert!(is_sensitive_path(Path::new(".env")));
        assert!(is_sensitive_path(Path::new(".env.local")));
        assert!(is_sensitive_path(Path::new(".env.production")));
        assert!(!is_sensitive_path(Path::new(".env.example")));
        assert!(!is_sensitive_path(Path::new(".env.sample")));
        assert!(!is_sensitive_path(Path::new(".env.template")));
    }

    #[test]
    fn credential_files() {
        assert!(is_sensitive_path(Path::new("credentials")));
        assert!(is_sensitive_path(Path::new(".netrc")));
        assert!(is_sensitive_path(Path::new(".npmrc")));
        assert!(is_sensitive_path(Path::new("secrets.json")));
        assert!(is_sensitive_path(Path::new("tokens.json")));
    }

    // --- S1-4: Workspace directory constraint ---

    #[test]
    fn is_within_directory_basic() {
        assert!(is_within_directory(
            Path::new("/workspace/project"),
            Path::new("/workspace/project/src/main.rs"),
        ));
        assert!(is_within_directory(
            Path::new("/workspace/project"),
            Path::new("/workspace/project"),
        ));
        assert!(!is_within_directory(
            Path::new("/workspace/project"),
            Path::new("/etc/passwd"),
        ));
        assert!(!is_within_directory(
            Path::new("/workspace/project"),
            Path::new("/workspace/other/file.rs"),
        ));
    }

    #[test]
    fn is_within_directory_handles_dot_dot() {
        assert!(!is_within_directory(
            Path::new("/workspace/project"),
            Path::new("/workspace/project/../other/file.rs"),
        ));
        assert!(is_within_directory(
            Path::new("/workspace/project"),
            Path::new("/workspace/project/sub/../file.rs"),
        ));
    }

    #[test]
    fn is_within_workspace_with_additional_dirs() {
        let working = Path::new("/workspace/project");
        let additional = vec![PathBuf::from("/workspace/shared")];
        assert!(is_within_workspace(
            working,
            &additional,
            Path::new("/workspace/project/src/main.rs"),
        ));
        assert!(is_within_workspace(
            working,
            &additional,
            Path::new("/workspace/shared/lib.rs"),
        ));
        assert!(!is_within_workspace(
            working,
            &additional,
            Path::new("/etc/passwd"),
        ));
    }

    #[test]
    fn lexical_normalize_strips_dot_and_dotdot() {
        assert_eq!(
            lexical_normalize(Path::new("/a/b/./c")),
            PathBuf::from("/a/b/c")
        );
        assert_eq!(
            lexical_normalize(Path::new("/a/b/../c")),
            PathBuf::from("/a/c")
        );
        assert_eq!(
            lexical_normalize(Path::new("a/b/../c")),
            PathBuf::from("a/c")
        );
    }
}
