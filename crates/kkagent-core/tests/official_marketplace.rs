//! Official marketplace catalog loads and resolves plugin sources.

#[tokio::test]
async fn official_marketplace_catalog_loads() {
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("plugins");
    let catalog = kkagent_core::plugin_marketplace::load_marketplace(
        root.join("marketplace.json").to_str().unwrap(),
        &root,
    )
    .await
    .expect("load marketplace");
    assert!(catalog.plugins.len() >= 4);
    assert!(catalog.plugins.iter().any(|p| p.id == "kk-weather"));
    assert!(catalog.plugins.iter().any(|p| p.id == "kk-browser"));
    assert!(catalog.plugins.iter().any(|p| p.id == "kk-web-override"));
    assert!(catalog.plugins.iter().any(|p| p.id == "kk-web-brave"));

    // The override example must parse as a valid plugin manifest with the
    // expected override wiring. `source` is already resolved to an absolute
    // path by `load_marketplace`.
    let source = catalog
        .plugins
        .iter()
        .find(|p| p.id == "kk-web-override")
        .unwrap();
    let dir = std::path::PathBuf::from(&source.source);
    let manifest_text = tokio::fs::read_to_string(dir.join("kk.plugin.json"))
        .await
        .expect("read override example manifest");
    let manifest: kkagent_core::plugin::PluginManifest =
        serde_json::from_str(&manifest_text).expect("parse override example manifest");
    assert_eq!(
        manifest.tool_overrides.get("Web").map(String::as_str),
        Some("web.everything_search")
    );
    assert!(manifest.slash_commands.len() == 1);

    // The services example must parse with the Brave search backend wired.
    let source = catalog
        .plugins
        .iter()
        .find(|p| p.id == "kk-web-brave")
        .unwrap();
    let dir = std::path::PathBuf::from(&source.source);
    let manifest_text = tokio::fs::read_to_string(dir.join("kk.plugin.json"))
        .await
        .expect("read services example manifest");
    let manifest: kkagent_core::plugin::PluginManifest =
        serde_json::from_str(&manifest_text).expect("parse services example manifest");
    let search = manifest
        .services
        .web_search
        .as_ref()
        .expect("web search service override");
    assert_eq!(search.provider.as_deref(), Some("brave"));
    assert!(
        manifest.mcp_servers.is_empty(),
        "services plugin needs no MCP"
    );
}
