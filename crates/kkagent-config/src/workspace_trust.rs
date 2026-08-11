use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::fs::OpenOptions;
use std::io::Write;
use std::path::{Path, PathBuf};

const TRUST_STORE_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceTrustStore {
    #[serde(default = "trust_store_version")]
    pub version: u32,
    #[serde(default)]
    pub workspaces: Vec<WorkspaceTrust>,
}

impl Default for WorkspaceTrustStore {
    fn default() -> Self {
        Self {
            version: TRUST_STORE_VERSION,
            workspaces: Vec::new(),
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WorkspaceTrust {
    /// Canonical workspace root. Presence in the store means the workspace was trusted.
    pub workspace: String,
    /// `None` means not reviewed, `Some(false)` means explicitly denied.
    #[serde(default)]
    pub git_metadata_allowed: Option<bool>,
    /// Canonical external Git metadata roots detected during the last review.
    /// They are usable only when `git_metadata_allowed == Some(true)`.
    #[serde(default)]
    pub git_metadata_paths: Vec<String>,
    /// `None` means not reviewed, `Some(false)` means keep Git globally isolated.
    #[serde(default)]
    pub global_git_config_allowed: Option<bool>,
    /// Top-level global config files loaded in Git precedence order.
    #[serde(default)]
    pub global_git_config_roots: Vec<String>,
    /// AOSP repo's per-user `.repoconfig/config`, kept separate from Git roots.
    #[serde(default)]
    pub repo_config_path: Option<String>,
    /// Config roots plus every recursively included config file.
    #[serde(default)]
    pub global_git_config_paths: Vec<String>,
    #[serde(default)]
    pub global_git_ignore_path: Option<String>,
    #[serde(default)]
    pub global_git_attributes_path: Option<String>,
    /// Capability categories only; values from Git config are never persisted.
    #[serde(default)]
    pub global_git_risks: Vec<String>,
}

impl WorkspaceTrust {
    pub fn new(workspace: &Path) -> Self {
        Self {
            workspace: workspace.to_string_lossy().into_owned(),
            git_metadata_allowed: None,
            git_metadata_paths: Vec::new(),
            global_git_config_allowed: None,
            global_git_config_roots: Vec::new(),
            repo_config_path: None,
            global_git_config_paths: Vec::new(),
            global_git_ignore_path: None,
            global_git_attributes_path: None,
            global_git_risks: Vec::new(),
        }
    }

    pub fn workspace_path(&self) -> PathBuf {
        PathBuf::from(&self.workspace)
    }

    pub fn global_git_read_paths(&self) -> impl Iterator<Item = &str> {
        self.global_git_config_paths
            .iter()
            .map(String::as_str)
            .chain(self.global_git_ignore_path.as_deref())
            .chain(self.global_git_attributes_path.as_deref())
    }

    pub fn validate(&self) -> Result<()> {
        validate_absolute("workspace", &self.workspace)?;
        for path in &self.git_metadata_paths {
            validate_absolute("Git metadata", path)?;
        }
        for path in &self.global_git_config_roots {
            validate_absolute("global Git config root", path)?;
        }
        if self.global_git_config_roots.len() > 2 {
            anyhow::bail!("at most two global Git config roots are supported");
        }
        for path in &self.global_git_config_paths {
            validate_absolute("global Git config", path)?;
        }
        if let Some(path) = &self.repo_config_path {
            validate_absolute("repo user config", path)?;
        }
        if let Some(path) = &self.global_git_ignore_path {
            validate_absolute("global Git ignore", path)?;
        }
        if let Some(path) = &self.global_git_attributes_path {
            validate_absolute("global Git attributes", path)?;
        }
        Ok(())
    }
}

impl WorkspaceTrustStore {
    pub fn load(config_path: &Path) -> Result<Self> {
        let path = workspace_trust_path(config_path);
        if !path.exists() {
            return Ok(Self::default());
        }
        let body = std::fs::read_to_string(&path)
            .with_context(|| format!("Failed to read workspace trust store: {}", path.display()))?;
        let store: Self = toml::from_str(&body).with_context(|| {
            format!("Failed to parse workspace trust store: {}", path.display())
        })?;
        if store.version != TRUST_STORE_VERSION {
            anyhow::bail!(
                "Unsupported workspace trust store version {} in {}",
                store.version,
                path.display()
            );
        }
        for entry in &store.workspaces {
            entry.validate()?;
        }
        Ok(store)
    }

    pub fn save(&self, config_path: &Path) -> Result<()> {
        for entry in &self.workspaces {
            entry.validate()?;
        }
        let path = workspace_trust_path(config_path);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).with_context(|| {
                format!(
                    "Failed to create workspace trust directory: {}",
                    parent.display()
                )
            })?;
        }
        let body = toml::to_string_pretty(self).context("Failed to serialize workspace trust")?;
        let contents =
            format!("# Managed by kkagent. Git credentials are not granted by this file.\n{body}");
        atomic_private_write(&path, contents.as_bytes())
    }

    pub fn matching(&self, workspace: &Path) -> Option<&WorkspaceTrust> {
        let workspace = canonical_or_owned(workspace);
        self.workspaces
            .iter()
            .filter(|entry| workspace.starts_with(canonical_or_owned(&entry.workspace_path())))
            .max_by_key(|entry| {
                canonical_or_owned(&entry.workspace_path())
                    .components()
                    .count()
            })
    }

    pub fn exact(&self, workspace: &Path) -> Option<&WorkspaceTrust> {
        let workspace = canonical_or_owned(workspace);
        self.workspaces
            .iter()
            .find(|entry| canonical_or_owned(&entry.workspace_path()) == workspace)
    }

    pub fn upsert(&mut self, entry: WorkspaceTrust) {
        let workspace = canonical_or_owned(&entry.workspace_path());
        if let Some(existing) = self
            .workspaces
            .iter_mut()
            .find(|item| canonical_or_owned(&item.workspace_path()) == workspace)
        {
            *existing = entry;
        } else {
            self.workspaces.push(entry);
        }
        self.workspaces
            .sort_by(|a, b| a.workspace.cmp(&b.workspace));
    }
}

pub fn workspace_trust_path(config_path: &Path) -> PathBuf {
    let name = config_path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("config.toml");
    config_path.with_file_name(format!("{name}.trust.toml"))
}

/// Environment overlay used by every Git invocation performed on behalf of a
/// workspace. Repository-local config remains active; global/system config is
/// replaced by the explicitly approved, read-only roots.
pub fn git_environment(trust: Option<&WorkspaceTrust>) -> Vec<(String, String)> {
    let null_device = if cfg!(windows) { "NUL" } else { "/dev/null" };
    let allowed = trust.filter(|entry| entry.global_git_config_allowed == Some(true));
    let roots = allowed
        .into_iter()
        .flat_map(|entry| entry.global_git_config_roots.iter())
        .filter(|path| Path::new(path).is_file())
        .collect::<Vec<_>>();
    let (system_config, global_config) = match roots.as_slice() {
        [] => (null_device.to_string(), null_device.to_string()),
        [global] => (null_device.to_string(), (*global).clone()),
        [system, global, ..] => ((*system).clone(), (*global).clone()),
    };
    let mut environment = vec![
        ("GIT_CONFIG_SYSTEM".into(), system_config),
        ("GIT_CONFIG_GLOBAL".into(), global_config),
    ];
    let mut values: Vec<(&str, String)> = Vec::new();
    if let Some(trust) = allowed {
        if let Some(path) = &trust.global_git_ignore_path {
            values.push(("core.excludesFile", path.clone()));
        }
        if let Some(path) = &trust.global_git_attributes_path {
            values.push(("core.attributesFile", path.clone()));
        }
        if let Some(path) = trust.repo_config_path.as_deref().map(Path::new) {
            if let Some(repo_config_dir) = path.parent().and_then(Path::parent) {
                environment.push((
                    "REPO_CONFIG_DIR".into(),
                    repo_config_dir.to_string_lossy().into_owned(),
                ));
            }
        }
    } else {
        values.push(("core.excludesFile", null_device.into()));
        values.push(("core.attributesFile", null_device.into()));
    }
    environment.push(("GIT_CONFIG_COUNT".into(), values.len().to_string()));
    for (index, (key, value)) in values.into_iter().enumerate() {
        environment.push((format!("GIT_CONFIG_KEY_{index}"), key.into()));
        environment.push((format!("GIT_CONFIG_VALUE_{index}"), value));
    }
    environment
}

pub fn git_metadata_accessible(trust: Option<&WorkspaceTrust>) -> bool {
    trust.is_none_or(|entry| {
        entry.git_metadata_paths.is_empty() || entry.git_metadata_allowed == Some(true)
    })
}

fn trust_store_version() -> u32 {
    TRUST_STORE_VERSION
}

fn validate_absolute(kind: &str, path: &str) -> Result<()> {
    if !Path::new(path).is_absolute() {
        anyhow::bail!("{kind} path must be absolute: {path}");
    }
    Ok(())
}

fn canonical_or_owned(path: &Path) -> PathBuf {
    if let Ok(path) = std::fs::canonicalize(path) {
        return path;
    }
    let mut ancestor = path;
    while !ancestor.exists() {
        let Some(parent) = ancestor.parent() else {
            return path.to_path_buf();
        };
        ancestor = parent;
    }
    let Ok(canonical) = std::fs::canonicalize(ancestor) else {
        return path.to_path_buf();
    };
    path.strip_prefix(ancestor)
        .map(|suffix| canonical.join(suffix))
        .unwrap_or_else(|_| path.to_path_buf())
}

fn atomic_private_write(path: &Path, contents: &[u8]) -> Result<()> {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let tmp = path.with_extension(format!("tmp.{}.{stamp}", std::process::id()));
    let mut options = OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options.open(&tmp).with_context(|| {
        format!(
            "Failed to create trust store temporary file: {}",
            tmp.display()
        )
    })?;
    if let Err(error) = file.write_all(contents).and_then(|_| file.sync_all()) {
        let _ = std::fs::remove_file(&tmp);
        return Err(error).context("Failed to write workspace trust store");
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&tmp, std::fs::Permissions::from_mode(0o600))?;
    }
    if let Err(first) = std::fs::rename(&tmp, path) {
        #[cfg(windows)]
        {
            if path.exists() {
                std::fs::remove_file(path)?;
                std::fs::rename(&tmp, path)?;
            } else {
                let _ = std::fs::remove_file(&tmp);
                return Err(first).context("Failed to install workspace trust store");
            }
        }
        #[cfg(not(windows))]
        {
            let _ = std::fs::remove_file(&tmp);
            return Err(first).context("Failed to install workspace trust store");
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_dir() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "kkagent-trust-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::create_dir_all(&path).unwrap();
        path
    }

    #[test]
    fn saves_loads_and_matches_most_specific_workspace() {
        let root = temp_dir();
        let child = root.join("child");
        std::fs::create_dir_all(&child).unwrap();
        let config = root.join("custom.toml");
        let mut store = WorkspaceTrustStore::default();
        store.upsert(WorkspaceTrust::new(&root));
        let mut child_entry = WorkspaceTrust::new(&child);
        child_entry.global_git_config_allowed = Some(true);
        store.upsert(child_entry.clone());
        store.save(&config).unwrap();

        let loaded = WorkspaceTrustStore::load(&config).unwrap();
        assert_eq!(loaded.matching(&child.join("nested")), Some(&child_entry));
        assert!(workspace_trust_path(&config).ends_with("custom.toml.trust.toml"));
        assert!(std::fs::read_to_string(workspace_trust_path(&config))
            .unwrap()
            .contains("Git credentials are not granted"));
        std::fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn git_environment_is_isolated_until_global_config_is_allowed() {
        let isolated = git_environment(None);
        assert!(isolated
            .iter()
            .any(|(key, value)| key == "GIT_CONFIG_COUNT" && value == "2"));
        assert!(isolated.iter().any(|(key, value)| {
            key == "GIT_CONFIG_VALUE_0" && (value == "/dev/null" || value == "NUL")
        }));

        let mut trust = WorkspaceTrust::new(Path::new("/workspace"));
        trust.global_git_config_allowed = Some(true);
        trust.global_git_config_roots = vec![std::env::current_exe()
            .unwrap()
            .to_string_lossy()
            .into_owned()];
        trust.global_git_ignore_path = trust.global_git_config_roots.first().cloned();
        let inherited = git_environment(Some(&trust));
        assert!(inherited
            .iter()
            .any(|(key, value)| key == "GIT_CONFIG_COUNT" && value == "1"));
        assert!(inherited
            .iter()
            .any(|(key, value)| key == "GIT_CONFIG_GLOBAL" && value != "/dev/null"));
        assert!(inherited
            .iter()
            .any(|(key, value)| key == "GIT_CONFIG_KEY_0" && value == "core.excludesFile"));
    }

    #[test]
    fn git_environment_relocates_aosp_repo_config_without_changing_home() {
        let mut trust = WorkspaceTrust::new(Path::new("/workspace"));
        trust.global_git_config_allowed = Some(true);
        trust.repo_config_path = Some(
            Path::new("/approved-home/.repoconfig/config")
                .to_string_lossy()
                .into_owned(),
        );

        let environment = git_environment(Some(&trust));
        assert!(environment.iter().any(|(key, value)| {
            key == "REPO_CONFIG_DIR" && value == &Path::new("/approved-home").to_string_lossy()
        }));
        assert!(!environment.iter().any(|(key, _)| key == "HOME"));
    }

    #[test]
    fn external_git_metadata_fails_closed_until_approved() {
        let mut trust = WorkspaceTrust::new(Path::new("/workspace"));
        trust.git_metadata_paths = vec!["/outside/repository.git".into()];
        assert!(!git_metadata_accessible(Some(&trust)));
        trust.git_metadata_allowed = Some(false);
        assert!(!git_metadata_accessible(Some(&trust)));
        trust.git_metadata_allowed = Some(true);
        assert!(git_metadata_accessible(Some(&trust)));
    }
}
