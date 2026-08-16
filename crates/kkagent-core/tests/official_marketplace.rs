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
    assert!(catalog.plugins.len() >= 2);
    assert!(catalog.plugins.iter().any(|p| p.id == "kk-weather"));
    assert!(catalog.plugins.iter().any(|p| p.id == "kk-browser"));
}
