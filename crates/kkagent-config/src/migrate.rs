//! Config migration: versioned, lossless rewrites of user-maintained files.
//!
//! kkagent owns user-authored files — `config.toml` (TOML, with user comments
//! worth preserving) and installed plugin manifests (JSON). Rewrites are
//! therefore lossless:
//!
//! - TOML is edited through `toml_edit` (comments / inline comments /
//!   whitespace survive); every write goes through
//!   [`atomic_write_with_backup`], which copies the original to
//!   `config.toml.bak` first.
//! - JSON manifests have no comments; only manifests that actually contain a
//!   legacy value are rewritten (pretty-printed).
//!
//! Each breaking change is one [`MigrationStep`] in [`MIGRATIONS`] — the
//! registry that records which config schema versions are incompatible and
//! how to move between them. Steps are idempotent: re-running on an
//! already-migrated file changes nothing. The applied level is stamped into
//! `config.toml` as `config_schema_version`; unstamped files are treated as
//! v1 and left byte-identical when nothing needs migrating (hand-maintained
//! configs must not gain surprise diffs).
//!
//! Startup flow: `loader::load_config` runs [`migrate_config_file`] and
//! [`migrate_plugin_manifests`] before parsing. Human-readable summaries
//! accumulate in a process-global queue that the TUI drains via
//! [`take_startup_notices`]; headless runs find the same details in the log.

use anyhow::{Context, Result};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};

/// Current config schema version, stamped into `config.toml` on migration.
pub const CONFIG_SCHEMA_VERSION: u32 = 2;

/// Top-level `config.toml` key recording the applied schema version.
pub const CONFIG_SCHEMA_VERSION_KEY: &str = "config_schema_version";

/// Reserved symbolic model tokens of the current schema.
const CURRENT_MODEL_TOKENS: [&str; 4] = ["quality", "balance", "fast", "current"];

/// Map a legacy symbolic model token to its replacement, if any.
///
/// v1 → v2 rename: `default` → `quality`, `secondary` → `balance`.
pub fn migrate_model_token(token: &str) -> Option<&'static str> {
    match token.trim().to_ascii_lowercase().as_str() {
        "default" => Some("quality"),
        "secondary" => Some("balance"),
        _ => None,
    }
}

struct MigrationStep {
    /// Schema level the file must be at *before* this step applies.
    from: u32,
    description: &'static str,
    apply: fn(&mut toml_edit::DocumentMut, &mut Vec<String>),
}

/// Registry of breaking config-schema changes, ordered by version.
///
/// Each entry records that config written for schema `from` needs `apply`
/// before `from + 1` semantics hold. Startup applies every step whose `from`
/// is >= the file's stamped version (unstamped files count as v1); steps are
/// idempotent so re-application is safe.
static MIGRATIONS: &[MigrationStep] = &[MigrationStep {
    from: 1,
    description: "model slots: default → quality, secondary → balance (tokens and \
                  the `secondary_model` key)",
    apply: migrate_v1_to_v2,
}];

/// v1 → v2: model tier naming overhaul.
///
/// - `[subagent.default_models]` tokens: `default` → `quality`,
///   `secondary` → `balance`
/// - top-level key: `secondary_model` → `balance_model` (inline comments
///   and surrounding whitespace preserved)
fn migrate_v1_to_v2(doc: &mut toml_edit::DocumentMut, changes: &mut Vec<String>) {
    // Top-level `secondary_model` → `balance_model`. `doc.remove` detaches
    // the item with its decor intact, so re-inserting under the new key
    // keeps the value, whitespace, and inline comments byte-identical.
    if let Some(item) = doc.remove("secondary_model") {
        let old_value = item.as_value().map(|v| v.to_string()).unwrap_or_default();
        doc.insert("balance_model", item);
        changes.push(format!("secondary_model → balance_model ({old_value})"));
    }

    // `[subagent.default_models]` legacy tokens.
    let Some(table) = doc
        .get_mut("subagent")
        .and_then(|item| item.as_table_mut())
        .and_then(|table| table.get_mut("default_models"))
        .and_then(|item| item.as_table_mut())
    else {
        return;
    };
    for (key, item) in table.iter_mut() {
        let Some(old) = item.as_str().map(str::to_owned) else {
            continue;
        };
        let Some(new) = migrate_model_token(&old) else {
            continue;
        };
        replace_string_value(item, new);
        changes.push(format!(
            "subagent.default_models.{key}: \"{old}\" → \"{new}\""
        ));
    }
}

/// Replace a TOML value's string content while preserving its decor
/// (surrounding whitespace and inline comments).
fn replace_string_value(item: &mut toml_edit::Item, new: &str) {
    let Some(value) = item.as_value_mut() else {
        return;
    };
    let prefix = value.decor().prefix().cloned();
    let suffix = value.decor().suffix().cloned();
    let mut replacement = toml_edit::Value::from(new);
    if let Some(prefix) = prefix {
        replacement.decor_mut().set_prefix(prefix);
    }
    if let Some(suffix) = suffix {
        replacement.decor_mut().set_suffix(suffix);
    }
    *item = toml_edit::Item::Value(replacement);
}

/// Apply all pending schema migrations to raw `config.toml` text, losslessly.
///
/// Returns the (possibly unchanged) text plus human-readable change and
/// warning lists. Files already stamped at (or newer than)
/// [`CONFIG_SCHEMA_VERSION`] are returned untouched. Unstamped files are
/// treated as v1; the stamp is added only when a migration actually changed
/// something.
pub fn migrate_config_str(content: &str) -> Result<(String, Vec<String>, Vec<String>)> {
    let mut doc: toml_edit::DocumentMut = content.parse().context("parse config for migration")?;
    let stamped = doc
        .get(CONFIG_SCHEMA_VERSION_KEY)
        .and_then(|item| item.as_integer())
        .map(|value| value.max(0) as u32);
    let mut changes = Vec::new();
    if stamped >= Some(CONFIG_SCHEMA_VERSION) {
        return Ok((content.to_string(), changes, Vec::new()));
    }
    for step in MIGRATIONS {
        if stamped.is_none_or(|version| step.from >= version) {
            let before = changes.len();
            (step.apply)(&mut doc, &mut changes);
            if changes.len() != before {
                tracing::info!(
                    "config migration applied v{} → v{}: {}",
                    step.from,
                    step.from + 1,
                    step.description
                );
            }
        }
    }

    let warnings = model_alias_collision_warnings(&doc);

    if !changes.is_empty() {
        let mut key = toml_edit::Key::new(CONFIG_SCHEMA_VERSION_KEY);
        key.leaf_decor_mut()
            .set_prefix("# Config schema level, maintained by kkagent startup migration.\n");
        doc.insert(
            &key,
            toml_edit::Item::Value(toml_edit::Value::from(i64::from(CONFIG_SCHEMA_VERSION))),
        );
    }
    Ok((doc.to_string(), changes, warnings))
}

/// Warn when a `[models]` alias collides with a current symbolic token —
/// validation rejects those, and renaming is a user decision (references
/// elsewhere must follow).
fn model_alias_collision_warnings(doc: &toml_edit::DocumentMut) -> Vec<String> {
    let mut warnings = Vec::new();
    let Some(models) = doc.get("models") else {
        return warnings;
    };
    let keys: Vec<String> = match models {
        toml_edit::Item::Table(table) => table.iter().map(|(k, _)| k.to_string()).collect(),
        toml_edit::Item::Value(value) => value
            .as_inline_table()
            .map(|table| table.iter().map(|(k, _)| k.to_string()).collect())
            .unwrap_or_default(),
        _ => Vec::new(),
    };
    for key in keys {
        if CURRENT_MODEL_TOKENS.contains(&key.trim().to_ascii_lowercase().as_str()) {
            warnings.push(format!(
                "[models].\"{key}\" collides with a reserved symbolic token; rename it \
                 (and every reference to it) — startup validation will reject it"
            ));
        }
    }
    warnings
}

/// Migrate the config file at `path` in place (`.toml.bak` backup first).
///
/// Missing files are a no-op. The returned preview is `Some` only when the
/// file was rewritten; warnings are surfaced as startup notices either way.
pub fn migrate_config_file(path: &Path) -> Result<Option<MigrationPreview>> {
    if !path.exists() {
        return Ok(None);
    }
    let raw = std::fs::read_to_string(path).with_context(|| format!("read {}", path.display()))?;
    let (migrated, changes, warnings) = migrate_config_str(&raw)?;
    for warning in &warnings {
        tracing::warn!("config warning: {warning}");
        push_notice(format!("config warning: {warning}"));
    }
    if changes.is_empty() {
        return Ok(None);
    }
    let backup_path = atomic_write_with_backup(path, &migrated)?;
    let plural = if changes.len() == 1 { "" } else { "s" };
    push_notice(format!(
        "Config migrated to schema v{CONFIG_SCHEMA_VERSION}: {} change{plural} \
         (backup: {}).",
        changes.len(),
        backup_path.display()
    ));
    for change in &changes {
        tracing::info!("config migration: {change}");
        push_notice(format!("  {change}"));
    }
    Ok(Some(MigrationPreview {
        path: path.to_path_buf(),
        backup_path,
        changes,
        unknown_fields_preserved: true,
    }))
}

/// Manifest file names probed inside each installed plugin directory,
/// mirroring `PluginManager` discovery order.
const PLUGIN_MANIFEST_PATHS: [&str; 3] =
    ["kk.plugin.json", ".kk-plugin/plugin.json", "plugin.json"];

/// Migrate legacy symbolic model tokens in installed plugin manifests under
/// `plugins_dir`. Returns the number of manifests rewritten.
pub fn migrate_plugin_manifests(plugins_dir: &Path) -> Result<usize> {
    let entries = match std::fs::read_dir(plugins_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => {
            return Err(error).context(format!("read {}", plugins_dir.display()));
        }
    };
    let mut updated = 0usize;
    for entry in entries {
        let Ok(entry) = entry else { continue };
        let dir = entry.path();
        if !dir.is_dir() {
            continue;
        }
        for rel in PLUGIN_MANIFEST_PATHS {
            let manifest = dir.join(rel);
            if manifest.is_file() {
                if migrate_manifest_file(&manifest)? {
                    updated += 1;
                }
                break;
            }
        }
    }
    Ok(updated)
}

fn migrate_manifest_file(path: &Path) -> Result<bool> {
    let raw = std::fs::read_to_string(path)
        .with_context(|| format!("read plugin manifest {}", path.display()))?;
    let mut value: serde_json::Value = serde_json::from_str(&raw)
        .with_context(|| format!("parse plugin manifest {}", path.display()))?;
    let mut changes = Vec::new();
    if let Some(subagents) = value.get_mut("subagents").and_then(|v| v.as_array_mut()) {
        for agent in subagents.iter_mut() {
            let Some(old) = agent
                .get("model")
                .and_then(|v| v.as_str())
                .map(str::to_owned)
            else {
                continue;
            };
            let Some(new) = migrate_model_token(&old) else {
                continue;
            };
            if let Some(obj) = agent.as_object_mut() {
                obj.insert("model".into(), serde_json::Value::String(new.into()));
            }
            changes.push(format!("subagent model \"{old}\" → \"{new}\""));
        }
    }
    if changes.is_empty() {
        return Ok(false);
    }
    let pretty = format!("{}\n", serde_json::to_string_pretty(&value)?);
    std::fs::write(path, pretty)
        .with_context(|| format!("write plugin manifest {}", path.display()))?;
    push_notice(format!(
        "Plugin manifest migrated to schema v{CONFIG_SCHEMA_VERSION}: {}",
        path.display()
    ));
    for change in &changes {
        tracing::info!("plugin manifest migration ({}): {change}", path.display());
        push_notice(format!("  {change}"));
    }
    Ok(true)
}

static STARTUP_NOTICES: OnceLock<Mutex<Vec<String>>> = OnceLock::new();

fn push_notice(message: String) {
    let queue = STARTUP_NOTICES.get_or_init(|| Mutex::new(Vec::new()));
    queue
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
        .push(message);
}

/// Drain user-visible migration notices accumulated during startup.
///
/// The embedded server performs migrations in the same process as the TUI,
/// so draining at TUI start surfaces them as system messages. Remote/server
/// deployments find the same lines in the log instead.
pub fn take_startup_notices() -> Vec<String> {
    match STARTUP_NOTICES.get() {
        Some(queue) => std::mem::take(&mut *queue.lock().unwrap_or_else(|p| p.into_inner())),
        None => Vec::new(),
    }
}

/// Write `contents` atomically after copying the existing file to `.bak`.
pub fn atomic_write_with_backup(path: &Path, contents: &str) -> Result<PathBuf> {
    let backup = path.with_extension("toml.bak");
    if path.exists() {
        std::fs::copy(path, &backup).with_context(|| format!("backup {}", path.display()))?;
    }
    let tmp = path.with_extension("toml.tmp");
    std::fs::write(&tmp, contents)?;
    #[cfg(windows)]
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    std::fs::rename(&tmp, path)?;
    Ok(backup)
}

#[derive(Debug, Clone)]
pub struct MigrationPreview {
    pub path: PathBuf,
    pub backup_path: PathBuf,
    pub changes: Vec<String>,
    pub unknown_fields_preserved: bool,
}

/// Dry-run: report what a rewrite would do without writing.
pub fn preview_migration(config_path: &Path) -> Result<MigrationPreview> {
    let path = config_path.to_path_buf();
    let backup_path = path.with_extension("toml.bak");
    let mut changes = Vec::new();
    if !path.exists() {
        changes.push("config file missing — would create defaults".into());
        return Ok(MigrationPreview {
            path,
            backup_path,
            changes,
            unknown_fields_preserved: true,
        });
    }
    let raw = std::fs::read_to_string(&path).with_context(|| format!("read {}", path.display()))?;
    match migrate_config_str(&raw) {
        Ok((_, pending, _)) if !pending.is_empty() => {
            for change in pending {
                changes.push(format!("{change} (auto-migrates at startup)"));
            }
        }
        Ok(_) => {}
        Err(error) => changes.push(format!("parse error — fix before migrate: {error:#}")),
    }
    if raw.contains("moonshot_api_key") || raw.contains("[services.moonshot") {
        changes.push("legacy moonshot_* web search keys → prefer [services.web_search]".into());
    }
    if !raw.contains("[ui]") {
        changes.push("optional: add [ui] for high_contrast / keybindings / check_updates".into());
    }
    if changes.is_empty() {
        changes.push("no schema migrations required".into());
    }
    Ok(MigrationPreview {
        path,
        backup_path,
        changes,
        unknown_fields_preserved: true,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preview_missing_is_ok() {
        let p = preview_migration(Path::new("/tmp/kkagent-no-such-config-xyz.toml")).unwrap();
        assert!(!p.changes.is_empty());
    }
}

#[cfg(test)]
mod migration_tests {
    use super::*;

    fn temp_dir(tag: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "kkagent-migrate-{tag}-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    const LEGACY: &str = "# my config header comment\ndefault_model = \"m\"\n\n[subagent]\nmax_depth = 2\n\n[subagent.default_models]\n# inline table comment\ncoder = \"secondary\" # trailing comment\ngeneral = \"default\"\n\n[models.m]\nprovider = \"p\"\nmodel = \"m\"\n";

    #[test]
    fn v1_tokens_are_renamed_losslessly() {
        let (out, changes, warnings) = migrate_config_str(LEGACY).unwrap();
        assert_eq!(changes.len(), 2, "changes: {changes:?}");
        assert!(changes[0].contains("coder"));
        assert!(out.contains("# my config header comment"));
        assert!(out.contains("# inline table comment"));
        assert!(out.contains("coder = \"balance\" # trailing comment"));
        assert!(out.contains("general = \"quality\""));
        assert!(out.contains("config_schema_version = 2"));
        assert!(warnings.is_empty());
    }

    #[test]
    fn v1_secondary_model_key_is_renamed_with_decor() {
        let raw = "# header\nsecondary_model = \"mid/model\" # mid tier\n\n[subagent.default_models]\ngeneral = \"secondary\"\n";
        let (out, changes, _) = migrate_config_str(raw).unwrap();
        assert!(
            changes
                .iter()
                .any(|c| c.starts_with("secondary_model → balance_model")),
            "changes: {changes:?}"
        );
        assert!(out.contains("balance_model = \"mid/model\" # mid tier"));
        assert!(!out.contains("secondary_model ="));
        assert!(out.contains("general = \"balance\""));
    }

    #[test]
    fn v1_without_secondary_model_key_is_fine() {
        let raw = "[subagent.default_models]\ngeneral = \"default\"\n";
        let (out, changes, _) = migrate_config_str(raw).unwrap();
        assert_eq!(changes.len(), 1);
        assert!(out.contains("general = \"quality\""));
        assert!(!out.contains("balance_model"));
    }

    #[test]
    fn migration_is_idempotent() {
        let (once, _, _) = migrate_config_str(LEGACY).unwrap();
        let (twice, changes, _) = migrate_config_str(&once).unwrap();
        assert!(changes.is_empty(), "unexpected changes: {changes:?}");
        assert_eq!(once, twice);
    }

    #[test]
    fn stamped_current_file_is_untouched() {
        // Root-level keys must precede the first table header.
        let stamped = format!("config_schema_version = 2\n{LEGACY}");
        let (out, changes, _) = migrate_config_str(&stamped).unwrap();
        assert!(changes.is_empty());
        assert_eq!(out, stamped);
    }

    #[test]
    fn rename_is_case_insensitive() {
        let raw = "[subagent.default_models]\ncoder = \"Secondary\"\ngeneral = \"DEFAULT\"\n";
        let (out, changes, _) = migrate_config_str(raw).unwrap();
        assert_eq!(changes.len(), 2);
        assert!(out.contains("coder = \"balance\""));
        assert!(out.contains("general = \"quality\""));
    }

    #[test]
    fn non_token_values_are_untouched() {
        let raw = "[subagent.default_models]\ncoder = \"my-suffix-model\"\n";
        let (out, changes, _) = migrate_config_str(raw).unwrap();
        assert!(changes.is_empty());
        assert_eq!(out, raw);
    }

    #[test]
    fn model_alias_collision_warns() {
        let raw = "[models]\nBalance = { provider = \"p\", model = \"b\" }\n[subagent.default_models]\ngeneral = \"secondary\"\n";
        let (_, changes, warnings) = migrate_config_str(raw).unwrap();
        assert_eq!(changes.len(), 1);
        assert!(
            warnings.iter().any(|w| w.contains("Balance")),
            "{warnings:?}"
        );
    }

    #[test]
    fn parse_error_is_reported() {
        assert!(migrate_config_str("not [ valid toml").is_err());
    }

    #[test]
    fn config_file_migration_writes_backup_and_notices() {
        let dir = temp_dir("config-file");
        let path = dir.join("config.toml");
        std::fs::write(&path, LEGACY).unwrap();

        take_startup_notices(); // drain leftovers from other tests
        let preview = migrate_config_file(&path).unwrap().unwrap();
        assert_eq!(preview.changes.len(), 2);
        assert!(path.with_extension("toml.bak").exists());
        let migrated = std::fs::read_to_string(&path).unwrap();
        assert!(migrated.contains("general = \"quality\""));

        let notices = take_startup_notices();
        assert!(
            notices
                .iter()
                .any(|n| n.contains("Config migrated to schema v2")),
            "notices: {notices:?}"
        );

        // Second run: nothing to do, no new notices.
        assert!(migrate_config_file(&path).unwrap().is_none());
        let later = take_startup_notices();
        assert!(!later.iter().any(|n| n.contains("Config migrated")));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn plugin_manifest_tokens_migrate() {
        let dir = temp_dir("manifest");
        let plugin = dir.join("my-plugin");
        std::fs::create_dir_all(&plugin).unwrap();
        let manifest = plugin.join("kk.plugin.json");
        std::fs::write(
            &manifest,
            r#"{"name":"my-plugin","subagents":[{"name":"a","transport":"internal","model":"secondary"},{"name":"b","model":"fast"}]}"#,
        )
        .unwrap();

        let updated = migrate_plugin_manifests(&dir).unwrap();
        assert_eq!(updated, 1);
        let migrated = std::fs::read_to_string(&manifest).unwrap();
        assert!(migrated.contains("\"model\": \"balance\""));
        assert!(migrated.contains("\"model\": \"fast\""));

        // Idempotent.
        assert_eq!(migrate_plugin_manifests(&dir).unwrap(), 0);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn plugin_manifest_migration_skips_non_plugin_dirs() {
        let dir = temp_dir("manifest-skip");
        std::fs::write(dir.join("stray.json"), "{\"model\":\"secondary\"}").unwrap();
        assert_eq!(migrate_plugin_manifests(&dir).unwrap(), 0);
        let _ = std::fs::remove_dir_all(dir);
    }
}
