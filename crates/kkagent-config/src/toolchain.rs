//! Declarative toolchain sandbox profiles (Rust/Node/Python/Go/Java).

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::{Path, PathBuf};

fn default_true() -> bool {
    true
}

/// Top-level `[toolchain]` config.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolchainConfig {
    /// Master switch. When false, no mounts/env/denies are applied.
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Root for agent-owned caches (`~/.kkagent/toolchains` when unset).
    #[serde(default)]
    pub root: Option<String>,
    /// Soft capacity hint for cache cleanup guidance (bytes). `0` = unlimited.
    #[serde(default = "default_toolchain_cache_bytes")]
    pub max_cache_bytes: u64,
    /// Named profiles. Built-in defaults are merged at runtime when a name is missing.
    #[serde(default)]
    pub profiles: HashMap<String, ToolchainProfileConfig>,
}

impl Default for ToolchainConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            root: None,
            max_cache_bytes: default_toolchain_cache_bytes(),
            profiles: HashMap::new(),
        }
    }
}

fn default_toolchain_cache_bytes() -> u64 {
    20 * 1024 * 1024 * 1024 // 20 GiB soft limit
}

/// Per-ecosystem toolchain profile.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ToolchainProfileConfig {
    #[serde(default = "default_true")]
    pub enabled: bool,
    /// Host runtime/SDK paths mounted read-only inside the sandbox.
    #[serde(default)]
    pub runtime_read_only: Vec<String>,
    /// Agent-owned cache directories mounted read-write.
    #[serde(default)]
    pub agent_cache_read_write: Vec<String>,
    /// Environment overrides injected for this profile (`CARGO_HOME`, …).
    #[serde(default)]
    pub env: HashMap<String, String>,
    /// Optional network override for this profile (`None` = inherit sandbox.network).
    #[serde(default)]
    pub network: Option<bool>,
    /// Extra deny substrings (merged with built-in global-install denylist).
    #[serde(default)]
    pub deny_patterns: Vec<String>,
}

impl ToolchainConfig {
    /// Resolve the toolchain cache root directory.
    pub fn cache_root(&self) -> PathBuf {
        if let Some(root) = self.root.as_deref().filter(|s| !s.trim().is_empty()) {
            return expand_tilde(root);
        }
        dirs::home_dir()
            .unwrap_or_else(|| PathBuf::from("."))
            .join(".kkagent")
            .join("toolchains")
    }

    /// Merge user overrides onto built-in profile defaults.
    pub fn resolved_profile(&self, name: &str) -> Option<ResolvedToolchainProfile> {
        let builtins = builtin_profiles(&self.cache_root());
        let base = builtins.get(name).cloned();
        let overlay = self.profiles.get(name).cloned();
        match (base, overlay) {
            (None, None) => None,
            (Some(mut base), Some(over)) => {
                if !over.enabled {
                    return None;
                }
                merge_profile(&mut base, over);
                Some(base)
            }
            (Some(base), None) => {
                if base.enabled {
                    Some(base)
                } else {
                    None
                }
            }
            (None, Some(over)) => {
                if over.enabled {
                    Some(ResolvedToolchainProfile {
                        name: name.to_string(),
                        enabled: true,
                        runtime_read_only: over
                            .runtime_read_only
                            .into_iter()
                            .map(expand_tilde)
                            .collect(),
                        agent_cache_read_write: over
                            .agent_cache_read_write
                            .into_iter()
                            .map(expand_tilde)
                            .collect(),
                        env: over.env,
                        network: over.network,
                        deny_patterns: over.deny_patterns,
                    })
                } else {
                    None
                }
            }
        }
    }

    pub fn all_resolved(&self) -> Vec<ResolvedToolchainProfile> {
        let mut names: Vec<String> = builtin_profiles(&self.cache_root())
            .keys()
            .cloned()
            .collect();
        for name in self.profiles.keys() {
            if !names.iter().any(|n| n == name) {
                names.push(name.clone());
            }
        }
        names.sort();
        names
            .into_iter()
            .filter_map(|name| self.resolved_profile(&name))
            .collect()
    }
}

/// Fully resolved profile ready for sandbox injection.
#[derive(Debug, Clone)]
pub struct ResolvedToolchainProfile {
    pub name: String,
    pub enabled: bool,
    pub runtime_read_only: Vec<PathBuf>,
    pub agent_cache_read_write: Vec<PathBuf>,
    pub env: HashMap<String, String>,
    pub network: Option<bool>,
    pub deny_patterns: Vec<String>,
}

fn merge_profile(base: &mut ResolvedToolchainProfile, over: ToolchainProfileConfig) {
    base.enabled = over.enabled;
    if !over.runtime_read_only.is_empty() {
        base.runtime_read_only = over
            .runtime_read_only
            .into_iter()
            .map(expand_tilde)
            .collect();
    }
    if !over.agent_cache_read_write.is_empty() {
        base.agent_cache_read_write = over
            .agent_cache_read_write
            .into_iter()
            .map(expand_tilde)
            .collect();
    }
    for (k, v) in over.env {
        base.env.insert(k, v);
    }
    if over.network.is_some() {
        base.network = over.network;
    }
    base.deny_patterns.extend(over.deny_patterns);
}

fn expand_tilde(raw: impl AsRef<str>) -> PathBuf {
    let raw = raw.as_ref();
    if let Some(rest) = raw.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    if raw == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
    }
    PathBuf::from(raw)
}

fn builtin_profiles(cache_root: &Path) -> HashMap<String, ResolvedToolchainProfile> {
    let mut out = HashMap::new();

    let rust_cache = cache_root.join("rust").join("default");
    out.insert(
        "rust".into(),
        ResolvedToolchainProfile {
            name: "rust".into(),
            enabled: true,
            runtime_read_only: vec![expand_tilde("~/.rustup"), expand_tilde("~/.cargo/bin")],
            agent_cache_read_write: vec![rust_cache.clone()],
            env: HashMap::from([
                (
                    "CARGO_HOME".into(),
                    rust_cache.join("cargo").display().to_string(),
                ),
                (
                    "RUSTUP_HOME".into(),
                    expand_tilde("~/.rustup").display().to_string(),
                ),
                (
                    "CARGO_TARGET_DIR".into(),
                    "target".into(), // relative to session cwd
                ),
            ]),
            network: None,
            deny_patterns: vec![
                "rustup toolchain install".into(),
                "rustup install".into(),
                "rustup update".into(),
                "rustup self update".into(),
            ],
        },
    );

    let node_cache = cache_root.join("node").join("default");
    out.insert(
        "node".into(),
        ResolvedToolchainProfile {
            name: "node".into(),
            enabled: true,
            runtime_read_only: existing_paths(&[
                "/usr/local/lib/node_modules",
                "/opt/homebrew/lib/node_modules",
            ]),
            agent_cache_read_write: vec![node_cache.clone()],
            env: HashMap::from([
                (
                    "npm_config_cache".into(),
                    node_cache.join("npm").display().to_string(),
                ),
                (
                    "NPM_CONFIG_CACHE".into(),
                    node_cache.join("npm").display().to_string(),
                ),
                (
                    "PNPM_STORE_PATH".into(),
                    node_cache.join("pnpm").display().to_string(),
                ),
                (
                    "YARN_CACHE_FOLDER".into(),
                    node_cache.join("yarn").display().to_string(),
                ),
            ]),
            network: None,
            deny_patterns: vec![
                "npm install -g".into(),
                "npm i -g".into(),
                "yarn global add".into(),
                "pnpm add -g".into(),
                "pnpm install -g".into(),
            ],
        },
    );

    let py_cache = cache_root.join("python").join("default");
    out.insert(
        "python".into(),
        ResolvedToolchainProfile {
            name: "python".into(),
            enabled: true,
            runtime_read_only: Vec::new(),
            agent_cache_read_write: vec![py_cache.clone()],
            env: HashMap::from([
                (
                    "PIP_CACHE_DIR".into(),
                    py_cache.join("pip").display().to_string(),
                ),
                (
                    "UV_CACHE_DIR".into(),
                    py_cache.join("uv").display().to_string(),
                ),
                (
                    "PYTHONUSERBASE".into(),
                    py_cache.join("userbase").display().to_string(),
                ),
            ]),
            network: None,
            deny_patterns: vec![
                "pip install --user".into(),
                "pip3 install --user".into(),
                "python -m pip install --user".into(),
            ],
        },
    );

    let go_cache = cache_root.join("go").join("default");
    out.insert(
        "go".into(),
        ResolvedToolchainProfile {
            name: "go".into(),
            enabled: true,
            runtime_read_only: existing_paths(&["/usr/local/go", "/opt/homebrew/opt/go"]),
            agent_cache_read_write: vec![go_cache.clone()],
            env: HashMap::from([
                (
                    "GOMODCACHE".into(),
                    go_cache.join("pkg/mod").display().to_string(),
                ),
                (
                    "GOCACHE".into(),
                    go_cache.join("build").display().to_string(),
                ),
                (
                    "GOPATH".into(),
                    go_cache.join("gopath").display().to_string(),
                ),
            ]),
            network: None,
            deny_patterns: vec!["go install".into()],
        },
    );

    let java_cache = cache_root.join("java").join("default");
    out.insert(
        "java".into(),
        ResolvedToolchainProfile {
            name: "java".into(),
            enabled: true,
            runtime_read_only: Vec::new(),
            agent_cache_read_write: vec![java_cache.clone()],
            env: HashMap::from([
                (
                    "GRADLE_USER_HOME".into(),
                    java_cache.join("gradle").display().to_string(),
                ),
                (
                    "MAVEN_OPTS".into(),
                    format!("-Dmaven.repo.local={}", java_cache.join("m2").display()),
                ),
            ]),
            network: None,
            deny_patterns: vec!["sdkmanager".into()],
        },
    );

    out
}

fn existing_paths(paths: &[&str]) -> Vec<PathBuf> {
    paths
        .iter()
        .map(PathBuf::from)
        .filter(|p| p.exists())
        .collect()
}

/// Built-in global-install / toolchain-mutation denylist (always on when toolchain enabled).
pub fn builtin_deny_patterns() -> &'static [&'static str] {
    &[
        "rustup toolchain install",
        "rustup install",
        "rustup update",
        "rustup self update",
        "npm install -g",
        "npm i -g",
        "yarn global add",
        "pnpm add -g",
        "pnpm install -g",
        "pip install --user",
        "pip3 install --user",
        "python -m pip install --user",
        "go install ",
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolves_builtin_rust_profile() {
        let cfg = ToolchainConfig::default();
        let rust = cfg.resolved_profile("rust").expect("rust");
        assert!(rust.env.contains_key("CARGO_HOME"));
        assert!(!rust.agent_cache_read_write.is_empty());
    }

    #[test]
    fn overlay_can_disable_profile() {
        let mut cfg = ToolchainConfig::default();
        cfg.profiles.insert(
            "rust".into(),
            ToolchainProfileConfig {
                enabled: false,
                ..Default::default()
            },
        );
        assert!(cfg.resolved_profile("rust").is_none());
    }

    #[test]
    fn cache_root_defaults_under_kkagent() {
        let root = ToolchainConfig::default().cache_root();
        assert!(root.ends_with("toolchains") || root.to_string_lossy().contains("toolchains"));
    }
}
