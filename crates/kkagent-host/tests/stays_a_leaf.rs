//! `kkagent-host` and `kkagent-llm` are the two libraries another project takes as dependencies
//! (`docs/extensions.md`, "作为库依赖"). They stay that way only while neither pulls in the rest of
//! kkagent, so this fails the day one of them gains a `kkagent-*` dependency.

use std::path::Path;

/// The names under `[dependencies]` of a crate's `Cargo.toml` (this test reads the file, it does not
/// resolve the build).
fn dependencies_of(crate_dir: &str) -> Vec<String> {
    let manifest = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("..")
        .join(crate_dir)
        .join("Cargo.toml");
    let text = std::fs::read_to_string(&manifest)
        .unwrap_or_else(|e| panic!("{}: {e}", manifest.display()));
    let mut in_dependencies = false;
    let mut names = Vec::new();
    for line in text.lines() {
        let line = line.trim();
        if line.starts_with('[') {
            in_dependencies = line == "[dependencies]";
            continue;
        }
        if in_dependencies && !line.is_empty() && !line.starts_with('#') {
            if let Some((name, _)) = line.split_once('=') {
                names.push(name.trim().to_string());
            }
        }
    }
    names
}

#[test]
fn kkagent_host_depends_on_serde_and_nothing_of_kkagent() {
    let dependencies = dependencies_of("kkagent-host");
    assert!(!dependencies.is_empty(), "the manifest was not read");
    for name in &dependencies {
        assert!(
            matches!(name.as_str(), "serde" | "serde_json"),
            "kkagent-host depends on `{name}`: it carries wire types only"
        );
    }
}

#[test]
fn kkagent_llm_depends_on_no_other_kkagent_crate() {
    let dependencies = dependencies_of("kkagent-llm");
    assert!(!dependencies.is_empty(), "the manifest was not read");
    let own: Vec<_> = dependencies
        .iter()
        .filter(|name| name.starts_with("kkagent-"))
        .collect();
    assert!(own.is_empty(), "kkagent-llm depends on {own:?}: a project that takes it as a library would get the agent with it");
}
