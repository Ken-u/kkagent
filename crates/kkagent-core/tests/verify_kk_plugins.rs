use std::path::PathBuf;

#[tokio::test]
async fn verify_official_subagent_plugin_manifests() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../plugins/official");
    let manager = kkagent_core::PluginManager::discover(&dir).await;
    let plugins = manager.list().await;
    let names: Vec<String> = plugins.iter().map(|p| p.name.clone()).collect();
    assert!(
        names.contains(&"kk-wiki-agent".to_string()),
        "plugins: {names:?}"
    );
    assert!(
        names.contains(&"kk-cursor-agent".to_string()),
        "plugins: {names:?}"
    );
    let external_subagents = manager.external_subagents().await;
    let (subs, conflicts) = (&external_subagents.0, &external_subagents.1);
    assert!(conflicts.is_empty(), "conflicts: {conflicts:?}");
    let wiki = subs.iter().find(|(n, _)| n == "kk-wiki-agent.search");
    assert!(
        wiki.is_some(),
        "subs: {:?}",
        subs.iter().map(|(n, _)| n).collect::<Vec<_>>()
    );
    let (_, spec) = wiki.unwrap();
    assert_eq!(spec.transport, "internal");
    assert!(spec.tools.contains(&"wiki_search".to_string()));
    assert_eq!(spec.mcp_servers.len(), 1);
    let cursor = subs.iter().find(|(n, _)| n == "kk-cursor-agent.cursor");
    assert!(cursor.is_some());
    let (_, spec) = cursor.unwrap();
    assert_eq!(spec.transport, "acp");
    assert_eq!(spec.transport_config.command, vec!["agent", "acp"]);
}

#[tokio::test]
async fn verify_kk_plugins_repo_manifests_when_present() {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../../kk-plugins");
    if !dir.is_dir() {
        // Sibling kk-plugins checkout is optional (absent on public CI).
        return;
    }
    let manager = kkagent_core::PluginManager::discover(&dir).await;
    let plugins = manager.list().await;
    let names: Vec<String> = plugins.iter().map(|p| p.name.clone()).collect();
    assert!(
        names.contains(&"wiki-agent".to_string()),
        "plugins: {names:?}"
    );
    assert!(
        names.contains(&"cursor-agent".to_string()),
        "plugins: {names:?}"
    );
    let external_subagents = manager.external_subagents().await;
    let (subs, conflicts) = (&external_subagents.0, &external_subagents.1);
    assert!(conflicts.is_empty(), "conflicts: {conflicts:?}");
    let wiki = subs.iter().find(|(n, _)| n == "wiki-agent.search");
    assert!(
        wiki.is_some(),
        "subs: {:?}",
        subs.iter().map(|(n, _)| n).collect::<Vec<_>>()
    );
    let (_, spec) = wiki.unwrap();
    assert_eq!(spec.transport, "internal");
    assert!(spec
        .tools
        .contains(&"wiki-agent_knowledge_query".to_string()));
    assert_eq!(spec.mcp_servers.len(), 1);
    let cursor = subs.iter().find(|(n, _)| n == "cursor-agent.delegate");
    assert!(cursor.is_some());
    let (_, spec) = cursor.unwrap();
    assert_eq!(spec.transport, "acp");
    assert_eq!(spec.transport_config.command, vec!["agent", "acp"]);
}
