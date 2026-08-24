//! One-shot compat check: real legacy-format plugins discovered and loaded
//! by the current PluginManager. Run with:
//!   cargo test -p kkagent-core --test legacy_plugin_compat -- --nocapture
//! Sample plugins are created under /tmp by the test itself.

use kkagent_core::plugin::PluginManager;
use std::path::PathBuf;

fn base_dir(tag: &str) -> PathBuf {
    std::env::temp_dir().join(format!(
        "kkagent-legacy-compat-{tag}-{}",
        uuid::Uuid::new_v4()
    ))
}

async fn write(dir: &std::path::Path, rel: &str, body: &str) {
    let path = dir.join(rel);
    if let Some(parent) = path.parent() {
        tokio::fs::create_dir_all(parent).await.unwrap();
    }
    tokio::fs::write(path, body).await.unwrap();
}

#[tokio::test]
async fn legacy_manifests_load_with_full_capabilities() {
    let dir = base_dir("mixed");

    // 1. Oldest form: plugin.json + prompt_append + plain-string
    //    slash_commands + unknown fields.
    write(
        &dir,
        "legacy-simple/plugin.json",
        r#"{
            "name": "legacy-simple",
            "version": "0.9.0",
            "description": "Old format plugin",
            "prompt_append": "Prefer legacy tools when available.",
            "slash_commands": ["oldcmd"],
            "future_field_xyz": {"anything": true},
            "mcpServers": {
                "srv": { "command": "npx", "args": ["-y", "server-everything"] }
            }
        }"#,
    )
    .await;

    // 2. .kk-plugin/plugin.json manifest location.
    write(
        &dir,
        "legacy-json/.kk-plugin/plugin.json",
        r#"{
            "name": "legacy-json",
            "version": "1.0.0",
            "systemPrompt": "JSON plugin appended prompt."
        }"#,
    )
    .await;

    // 3. prompt_append alias without MCP servers.
    write(
        &dir,
        "legacy-prompt-append/kk.plugin.json",
        r#"{
            "name": "legacy-prompt-append",
            "prompt_append": "Appended via legacy alias.",
            "slashCommands": ["legacyonly"]
        }"#,
    )
    .await;

    // 4. New-style keys present but empty — behaves like a plain old plugin.
    write(
        &dir,
        "legacy-unknown/kk.plugin.json",
        r#"{
            "name": "legacy-unknown",
            "version": "1.0.0",
            "slashCommands": ["plaincmd"],
            "toolOverrides": {},
            "services": {},
            "someNewerField": 123
        }"#,
    )
    .await;

    let manager = PluginManager::discover(&dir).await;
    let list = manager.list().await;
    println!(
        "loaded plugins: {:?}",
        list.iter().map(|p| p.name.as_str()).collect::<Vec<_>>()
    );

    // All four discovered, enabled by default, no diagnostics.
    assert_eq!(list.len(), 4, "all legacy plugins must load");
    for info in &list {
        assert!(info.enabled, "{} enabled by default", info.name);
        assert!(
            info.diagnostics.is_empty(),
            "{} clean: {:?}",
            info.name,
            info.diagnostics
        );
        assert!(info.tool_overrides.is_empty(), "{} no overrides", info.name);
        assert!(
            !info.replaces_system_prompt,
            "{} no prompt replacement",
            info.name
        );
    }

    // Legacy prompt_append / systemPrompt still APPEND (not replace).
    assert!(manager.system_prompt_override().await.is_none());
    let appended = manager.prompt_append_all().await;
    assert!(
        appended.contains("Prefer legacy tools when available."),
        "alias honored"
    );
    assert!(
        appended.contains("Appended via legacy alias."),
        "alias honored"
    );
    assert!(
        appended.contains("JSON plugin appended prompt."),
        "append channel intact"
    );

    // MCP server config from the legacy plugin still normalizes.
    let mcp = manager.mcp_server_configs().await;
    assert_eq!(mcp.len(), 1, "legacy mcpServers still discovered");
    assert_eq!(mcp[0].name, "plugin-legacy-simple:srv");

    // Plain-string slash commands surface with no templates.
    let simple = list.iter().find(|p| p.name == "legacy-simple").unwrap();
    assert_eq!(simple.slash_commands.len(), 1);
    assert_eq!(simple.slash_commands[0].name(), "oldcmd");
    assert!(simple.slash_commands[0].prompt_template().is_none());

    let _ = tokio::fs::remove_dir_all(dir).await;
}

#[tokio::test]
async fn legacy_installed_state_still_honored() {
    let dir = base_dir("installed");
    // Managed-layout plugin with a disabled record — the supported form.
    let plugin_dir = dir.join("managed/old-plugin");
    {
        let path = plugin_dir.join("plugin.json");
        tokio::fs::create_dir_all(&plugin_dir).await.unwrap();
        tokio::fs::write(path, r#"{ "name": "old-plugin", "prompt_append": "x" }"#)
            .await
            .unwrap();
    }
    write(
        &dir,
        "installed.json",
        &format!(
            r#"{{ "version": 1, "plugins": [ {{ "id": "old-plugin", "root": "{}", "source": "./old-plugin", "enabled": false, "installedAt": "2024-01-01T00:00:00Z" }} ] }}"#,
            plugin_dir.display()
        ),
    )
    .await;

    let manager = PluginManager::discover(&dir).await;
    let list = manager.list().await;
    assert_eq!(list.len(), 1);
    assert!(!list[0].enabled, "legacy disabled state honored");
    assert!(manager.prompt_append_all().await.is_empty());

    // Direct-directory plugins without a record stay enabled even when
    // installed.json exists (pre-existing semantics, must not regress).
    let extra = dir.join("extra-plugin");
    tokio::fs::create_dir_all(&extra).await.unwrap();
    tokio::fs::write(
        extra.join("plugin.json"),
        r#"{ "name": "extra-plugin", "prompt_append": "y" }"#,
    )
    .await
    .unwrap();
    manager.reload().await.unwrap();
    let list = manager.list().await;
    assert_eq!(list.len(), 2);
    let extra_info = list.iter().find(|p| p.name == "extra-plugin").unwrap();
    assert!(extra_info.enabled, "unrecorded direct plugin stays enabled");

    // A broken installed.json (missing required fields) must degrade to a
    // directory scan instead of wiping every plugin.
    write(
        &dir,
        "installed.json",
        r#"{ "plugins": [ { "id": "x" } ] }"#,
    )
    .await;
    manager.reload().await.unwrap();
    let list = manager.list().await;
    assert!(
        !list.is_empty(),
        "broken installed.json must not hide plugins"
    );

    let _ = tokio::fs::remove_dir_all(dir).await;
}
