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
async fn verify_internal_subagent_streamable_http_contract() {
    // Self-contained fixture: the manifest shape the internal-subagent +
    // plugin-private streamable-http MCP contract supports, without reading
    // any real (sibling-checkout) plugin repository.
    let root = std::env::temp_dir().join(format!(
        "kkagent-fixture-plugins-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let plugin_dir = root.join("fixture-wiki");
    std::fs::create_dir_all(&plugin_dir).unwrap();
    std::fs::write(
        plugin_dir.join("kk.plugin.json"),
        r#"{
  "name": "fixture-wiki",
  "version": "0.0.1",
  "description": "Fixture plugin for the internal-subagent streamable-http contract test.",
  "subagents": [
    {
      "name": "search",
      "transport": "internal",
      "description": "Fixture internal subagent with a plugin-private remote MCP server.",
      "tools": [
        "Read",
        "fixture-wiki_knowledge_search",
        "mcp__fixture-wiki_knowledge__search"
      ],
      "mcpServers": {
        "knowledge": {
          "transport": "streamable-http",
          "url": "http://127.0.0.1:9/mcp",
          "headers": { "Authorization": "Bearer fixture-token" },
          "timeout_ms": 12345
        }
      }
    }
  ]
}"#,
    )
    .unwrap();

    let manager = kkagent_core::PluginManager::discover(&root).await;
    let plugins = manager.list().await;
    let names: Vec<String> = plugins.iter().map(|p| p.name.clone()).collect();
    assert!(
        names.contains(&"fixture-wiki".to_string()),
        "plugins: {names:?}"
    );

    let external_subagents = manager.external_subagents().await;
    let (subs, conflicts) = (&external_subagents.0, &external_subagents.1);
    assert!(conflicts.is_empty(), "conflicts: {conflicts:?}");
    let fixture = subs.iter().find(|(n, _)| n == "fixture-wiki.search");
    assert!(
        fixture.is_some(),
        "subs: {:?}",
        subs.iter().map(|(n, _)| n).collect::<Vec<_>>()
    );
    let (_, spec) = fixture.unwrap();
    assert_eq!(spec.transport, "internal");
    assert!(spec
        .tools
        .contains(&"fixture-wiki_knowledge_search".to_string()));
    assert!(spec
        .tools
        .contains(&"mcp__fixture-wiki_knowledge__search".to_string()));
    assert_eq!(spec.mcp_servers.len(), 1);
    let (_, server) = spec.mcp_servers.iter().next().unwrap();
    assert_eq!(server.transport_type.as_deref(), Some("streamable-http"));
    assert_eq!(
        server.url.as_deref(),
        Some("http://127.0.0.1:9/mcp"),
        "remote url must survive manifest parsing"
    );
    assert_eq!(
        server.headers.get("Authorization").map(String::as_str),
        Some("Bearer fixture-token"),
        "auth headers must survive manifest parsing"
    );
    assert_eq!(server.timeout_ms, Some(12345));

    let _ = std::fs::remove_dir_all(&root);
}
