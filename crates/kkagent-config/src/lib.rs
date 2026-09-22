pub mod loader;
pub mod migrate;
pub mod plugin_policy;
pub mod schema;
/// No-op outside test binaries that opt in via `install_test_home!`.
pub mod test_isolation;
pub mod toolchain;
pub mod workspace_trust;

pub use loader::*;
pub use migrate::{
    atomic_write_with_backup, migrate_config_file, migrate_model_token, migrate_plugin_manifests,
    preview_migration, take_startup_notices, MigrationPreview, CONFIG_SCHEMA_VERSION,
    CONFIG_SCHEMA_VERSION_KEY,
};
pub use schema::*;
pub use toolchain::{
    builtin_deny_patterns, ResolvedToolchainProfile, ToolchainConfig, ToolchainProfileConfig,
};
pub use workspace_trust::{
    git_environment, git_metadata_accessible, workspace_trust_path, WorkspaceTrust,
    WorkspaceTrustStore,
};

// Keep this crate's own tests out of the real ~/.kkagent home.
crate::install_test_home!();
