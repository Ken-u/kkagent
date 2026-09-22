//! Re-export of the install helper; the macro expands in the *calling* crate
//! and therefore needs `ctor` available there (see each crate's Cargo.toml).

pub use kkagent_config::test_isolation::{cleanup, install};

/// Redirect the kkagent home to a per-process scratch dir at test-binary
/// startup, and delete it on process exit. No-op in non-test builds.
///
/// The calling crate must depend on `ctor` (dev-dependency is enough).
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
