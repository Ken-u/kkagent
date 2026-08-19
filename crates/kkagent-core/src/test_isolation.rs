//! Test isolation: keep `cargo test` out of the real `~/.kkagent` home.
//!
//! Unit tests that construct `Session::new` (source `Startup`) register into
//! the real session store under the kkagent home directory — historically
//! that meant every `cargo test --workspace` littered `~/.kkagent/sessions`
//! with `wd_kkagent-*` entries (100+ observed) and could even resurrect
//! polluted state (e.g. `planMode: true`) across runs when tests used fixed
//! session ids.
//!
//! `install_test_home!()` installs a constructor (via `ctor`, test builds
//! only) that redirects the kkagent home to a per-process scratch directory
//! using `kkagent_config::loader::set_default_config_dir_override`. Tests
//! then write only under the OS temp dir, regardless of session ids.

/// Install the test-home redirect for this test binary.
///
/// Called from a `#[ctor]` constructor emitted by `install_test_home!`.
/// Safe to call multiple times — the first redirect wins.
pub fn install() {
    let dir = std::env::temp_dir().join(format!("kkagent-test-home-{}", std::process::id()));
    let _ = std::fs::create_dir_all(&dir);
    kkagent_config::loader::set_default_config_dir_override(dir);
}

/// Redirect the kkagent home to a per-process scratch dir at test-binary
/// startup. No-op in non-test builds.
///
/// ```ignore
/// kkagent_core::install_test_home!();
/// ```
#[macro_export]
macro_rules! install_test_home {
    () => {
        #[cfg(test)]
        #[ctor::ctor]
        fn kkagent_install_test_home() {
            $crate::test_isolation::install();
        }
    };
}

#[cfg(test)]
mod tests {
    /// If this test fails, the `install_test_home!()` constructor stopped
    /// running before tests — session-creating tests would litter the real
    /// `~/.kkagent/sessions` store again.
    #[test]
    fn test_home_redirect_is_installed() {
        let dir = kkagent_config::loader::default_config_dir();
        assert!(
            dir.to_string_lossy().contains("kkagent-test-home-"),
            "kkagent home was not redirected for tests: {}",
            dir.display()
        );
    }
}
