//! Test isolation: keep `cargo test` out of the real `~/.kkagent` home.
//!
//! Any code path that goes through [`crate::default_config_dir`] (session
//! store, transcript DB, skills, credentials, telemetry spill, …) honors the
//! in-process override installed by [`install`]. Call `install_test_home!()`
//! once at the bottom of each crate's `lib.rs` / `main.rs` that runs tests.
//!
//! Cleanup:
//! - [`cleanup`] runs on process exit via `ctor::dtor`
//! - [`install`] also sweeps stale `kkagent-test-home-<pid>` dirs whose
//!   owning PID is no longer alive (covers crashes / killed test processes)

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

static TEST_HOME: OnceLock<PathBuf> = OnceLock::new();

const PREFIX: &str = "kkagent-test-home-";

/// Install the test-home redirect for this test binary.
///
/// Safe to call multiple times — the first redirect wins (`OnceLock`).
pub fn install() {
    let _ = sweep_stale_test_homes();
    let dir = TEST_HOME.get_or_init(|| {
        let dir = std::env::temp_dir().join(format!("{PREFIX}{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        dir
    });
    crate::loader::set_default_config_dir_override(dir.clone());
}

/// Remove the scratch home created by [`install`]. Idempotent; ignores errors
/// (e.g. files still held open on Windows).
pub fn cleanup() {
    if let Some(dir) = TEST_HOME.get() {
        let _ = std::fs::remove_dir_all(dir);
    }
}

/// Delete `kkagent-test-home-<pid>` directories under the OS temp dir when
/// `<pid>` is not a live process. Best-effort; never panics.
fn sweep_stale_test_homes() -> usize {
    let Ok(entries) = std::fs::read_dir(std::env::temp_dir()) else {
        return 0;
    };
    let mut removed = 0;
    for entry in entries.flatten() {
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        let Some(pid_str) = name.strip_prefix(PREFIX) else {
            continue;
        };
        // Skip UUID-style leftovers from older ad-hoc tests.
        if !pid_str.chars().all(|c| c.is_ascii_digit()) {
            continue;
        }
        let Ok(pid) = pid_str.parse::<u32>() else {
            continue;
        };
        if pid == std::process::id() || process_alive(pid) {
            continue;
        }
        if remove_dir_best_effort(&entry.path()) {
            removed += 1;
        }
    }
    removed
}

fn remove_dir_best_effort(path: &Path) -> bool {
    match std::fs::remove_dir_all(path) {
        Ok(()) => true,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
        Err(_) => false,
    }
}

fn process_alive(pid: u32) -> bool {
    #[cfg(target_os = "linux")]
    {
        Path::new(&format!("/proc/{pid}")).exists()
    }
    #[cfg(any(
        target_os = "macos",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd"
    ))]
    {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    }
    #[cfg(windows)]
    {
        std::process::Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/NH"])
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::null())
            .output()
            .map(|out| String::from_utf8_lossy(&out.stdout).contains(&pid.to_string()))
            .unwrap_or(false)
    }
    #[cfg(not(any(
        target_os = "linux",
        target_os = "macos",
        target_os = "freebsd",
        target_os = "openbsd",
        target_os = "netbsd",
        windows
    )))]
    {
        let _ = pid;
        false
    }
}

/// Redirect the kkagent home to a per-process scratch dir at test-binary
/// startup, and delete it on process exit. No-op in non-test builds.
///
/// ```ignore
/// kkagent_config::install_test_home!();
/// ```
#[macro_export]
macro_rules! install_test_home {
    () => {
        #[cfg(test)]
        #[ctor::ctor]
        fn kkagent_install_test_home() {
            $crate::test_isolation::install();
        }

        #[cfg(test)]
        #[ctor::dtor]
        fn kkagent_cleanup_test_home() {
            $crate::test_isolation::cleanup();
        }
    };
}

#[cfg(test)]
mod tests {
    #[test]
    fn test_home_redirect_is_installed() {
        let dir = crate::loader::default_config_dir();
        assert!(
            dir.to_string_lossy().contains("kkagent-test-home-"),
            "kkagent home was not redirected for tests: {}",
            dir.display()
        );
    }

    #[test]
    fn sweep_removes_dead_pid_dirs() {
        let fake = std::env::temp_dir().join(format!("{}{}", super::PREFIX, u32::MAX));
        let _ = std::fs::create_dir_all(&fake);
        assert!(fake.is_dir());
        let removed = super::sweep_stale_test_homes();
        assert!(
            removed >= 1,
            "expected to sweep at least the fake dead-pid dir"
        );
        assert!(!fake.exists(), "dead-pid test home should be removed");
    }
}
