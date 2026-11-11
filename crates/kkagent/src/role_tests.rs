//! Roles and extension toggles at the process level.
//!
//! Sessions created through the ACP host must get the role's system prompt,
//! tool surface and extension switches, and nothing a session sends can widen
//! the ceiling the process was started with.

use super::runtime_http_tests::test_server_state_with_policy;
use super::*;
use kkagent_acp::{AcpHost, SessionSetup};
use kkagent_config::ExtensionsConfig;
use kkagent_core::role::ProcessPolicy;
use serde_json::json;

const INSTRUCTIONS_MARK: &str = "PROJECT-INSTRUCTIONS-MARK-7731";
const PLUGIN_MARK: &str = "PLUGIN-APPENDIX-MARK-5520";

pub(super) fn config() -> AppConfig {
    let mut config = AppConfig::default();
    config.providers.insert(
        "test".into(),
        serde_json::from_value(json!({
            "type": "openai-chat", "api_key": "test", "base_url": "http://127.0.0.1:9"
        }))
        .unwrap(),
    );
    config.models.insert(
        "test/model".into(),
        serde_json::from_value(json!({
            "provider": "test", "model": "test-model", "max_context_size": 128000,
            "max_output_size": 1000, "capabilities": ["tool_use"]
        }))
        .unwrap(),
    );
    config.default_model = Some("test/model".into());
    for (name, role) in [
        (
            "sup",
            json!({
                "system_prompt": "ROLE-PROMPT-XYZ",
                "prompt_mode": "replace",
                "tools": ["Read", "Grep"]
            }),
        ),
        ("wide", json!({"tools": ["*"]})),
        (
            "appender",
            json!({"system_prompt": "APPENDED-ROLE-TEXT", "tools": ["Read"]}),
        ),
    ] {
        config
            .agent_roles
            .insert(name.into(), serde_json::from_value(role).unwrap());
    }
    config
}

pub(super) struct Harness {
    pub(super) state: Arc<ServerState>,
    pub(super) host: AgentAcpHost,
    pub(super) workspace: PathBuf,
}

impl Harness {
    pub(super) async fn new(
        role: Option<&str>,
        cli: ExtensionsConfig,
        plugins_dir: Option<PathBuf>,
    ) -> Self {
        let config = config();
        let policy = ProcessPolicy::from_config(&config, role, cli).unwrap();
        Self::with_policy(config, policy, plugins_dir).await
    }

    pub(super) async fn with_policy(
        config: AppConfig,
        policy: ProcessPolicy,
        plugins_dir: Option<PathBuf>,
    ) -> Self {
        let state = test_server_state_with_policy(config, plugins_dir, policy).await;
        let workspace =
            std::env::temp_dir().join(format!("kkagent-role-test-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&workspace).unwrap();
        std::fs::write(
            workspace.join("AGENTS.md"),
            format!("# Rules\n{INSTRUCTIONS_MARK}\n"),
        )
        .unwrap();
        let host = AgentAcpHost {
            state: state.clone(),
        };
        Self {
            state,
            host,
            workspace,
        }
    }

    pub(super) async fn try_new_session(&self, setup: &SessionSetup) -> Result<String, String> {
        let session_id = uuid::Uuid::new_v4().to_string();
        self.host
            .create_session(&session_id, &self.workspace.to_string_lossy(), setup)
            .await?;
        Ok(session_id)
    }

    pub(super) async fn new_session(&self, setup: &SessionSetup) -> String {
        self.try_new_session(setup).await.expect("create session")
    }

    pub(super) async fn prompt_of(&self, session_id: &str) -> String {
        self.state
            .sessions
            .lock()
            .await
            .get(session_id)
            .expect("live session")
            .system_prompt
            .clone()
    }

    pub(super) async fn tool_names(&self, session_id: &str) -> Vec<String> {
        let (event_tx, _event_rx) = mpsc::channel::<AgentEvent>(8);
        let tools = build_turn_tool_registry(
            &self.state,
            event_tx,
            Vec::new(),
            session_id,
            &self.workspace,
        )
        .await;
        let mut names: Vec<String> = tools
            .tool_definitions()
            .into_iter()
            .map(|definition| definition.name)
            .collect();
        names.sort();
        names
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.workspace);
    }
}

pub(super) fn host_meta(host: serde_json::Value) -> SessionSetup {
    SessionSetup {
        meta: Some(json!({ "kkagent/host": host })),
        ..Default::default()
    }
}

fn plugins_dir_with_prompt() -> PathBuf {
    let dir = std::env::temp_dir().join(format!("kkagent-role-plugins-{}", uuid::Uuid::new_v4()));
    let root = dir.join("prompt-plug");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(
        root.join("kk.plugin.json"),
        json!({"name": "prompt-plug", "version": "1.0.0", "systemPrompt": PLUGIN_MARK}).to_string(),
    )
    .unwrap();
    dir
}

#[tokio::test]
async fn default_process_is_unrestricted() {
    let h = Harness::new(None, ExtensionsConfig::default(), None).await;
    let sid = h.new_session(&SessionSetup::default()).await;

    let names = h.tool_names(&sid).await;
    for expected in ["Bash", "Read", "Grep"] {
        assert!(
            names.iter().any(|n| n == expected),
            "{expected} in {names:?}"
        );
    }
    assert!(!h.host.tools_restricted(Some(&sid)).await);
    assert!(!h.host.tools_restricted(None).await);
    assert!(h.prompt_of(&sid).await.contains(INSTRUCTIONS_MARK));
    assert!(h.state.hooks_for_session(&sid).await.is_some());
}

#[tokio::test]
async fn process_role_shapes_prompt_tools_and_acp_surface() {
    let h = Harness::new(Some("sup"), ExtensionsConfig::default(), None).await;
    let sid = h.new_session(&SessionSetup::default()).await;

    let prompt = h.prompt_of(&sid).await;
    assert!(prompt.starts_with("ROLE-PROMPT-XYZ"), "{prompt}");
    assert!(
        !prompt.contains(&kkagent_core::session::runtime::default_system_prompt()),
        "replace mode drops the built-in prompt"
    );
    assert_eq!(h.tool_names(&sid).await, vec!["Grep", "Read"]);
    assert!(h.host.tools_restricted(Some(&sid)).await);
    assert!(h.host.tools_restricted(None).await);
}

#[tokio::test]
async fn append_mode_keeps_the_builtin_prompt_and_adds_the_role_last() {
    let h = Harness::new(None, ExtensionsConfig::default(), None).await;
    let sid = h.new_session(&host_meta(json!({"role": "appender"}))).await;

    let prompt = h.prompt_of(&sid).await;
    assert!(
        prompt.trim_end().ends_with("APPENDED-ROLE-TEXT"),
        "{prompt}"
    );
    assert!(prompt.contains(INSTRUCTIONS_MARK));
    assert_eq!(h.tool_names(&sid).await, vec!["Read"]);
}

#[tokio::test]
async fn session_parameters_only_narrow_the_process_ceiling() {
    let h = Harness::new(Some("sup"), ExtensionsConfig::default(), None).await;

    // Narrower request: the intersection.
    let narrow = h
        .new_session(&host_meta(json!({"toolAllowlist": ["Read", "Bash"]})))
        .await;
    assert_eq!(h.tool_names(&narrow).await, vec!["Read"]);

    // Asking for everything still stops at the process ceiling.
    let greedy = h
        .new_session(&host_meta(json!({"toolAllowlist": ["*"]})))
        .await;
    assert_eq!(h.tool_names(&greedy).await, vec!["Grep", "Read"]);

    // Selecting a wider named role cannot widen it either.
    let wide_role = h.new_session(&host_meta(json!({"role": "wide"}))).await;
    assert_eq!(h.tool_names(&wide_role).await, vec!["Grep", "Read"]);

    // An empty allowlist means no tools at all.
    let none = h
        .new_session(&host_meta(json!({"toolAllowlist": []})))
        .await;
    assert!(h.tool_names(&none).await.is_empty());

    // An unknown role is an error, not a silent fallback to "unrestricted".
    let error = h
        .try_new_session(&host_meta(json!({"role": "missing"})))
        .await
        .unwrap_err();
    assert!(error.contains("missing"), "{error}");
}

#[tokio::test]
async fn an_unrestricted_process_lets_the_host_pick_a_strict_session_role() {
    let h = Harness::new(None, ExtensionsConfig::default(), None).await;
    let sid = h.new_session(&host_meta(json!({"role": "sup"}))).await;

    assert_eq!(h.tool_names(&sid).await, vec!["Grep", "Read"]);
    assert!(h.host.tools_restricted(Some(&sid)).await);
    // Another session on the same process is not affected.
    let plain = h.new_session(&SessionSetup::default()).await;
    assert!(!h.host.tools_restricted(Some(&plain)).await);
    assert!(h.tool_names(&plain).await.len() > 2);
}

#[tokio::test]
async fn standard_system_prompt_parameter_applies_and_wins_over_the_role() {
    let h = Harness::new(None, ExtensionsConfig::default(), None).await;
    let setup = SessionSetup {
        system_prompt: Some(kkagent_acp::SystemPromptParam {
            mode: kkagent_acp::SystemPromptParamMode::Override,
            text: "HOST-OVERRIDE-PROMPT".into(),
        }),
        meta: Some(json!({"kkagent/host": {"role": "sup"}})),
        ..Default::default()
    };
    let sid = h.new_session(&setup).await;
    let prompt = h.prompt_of(&sid).await;
    assert!(prompt.starts_with("HOST-OVERRIDE-PROMPT"), "{prompt}");
    assert!(!prompt.contains("ROLE-PROMPT-XYZ"));
}

#[tokio::test]
async fn project_instructions_toggle_only_turns_off() {
    // Per-session switch.
    let h = Harness::new(None, ExtensionsConfig::default(), None).await;
    let off = h
        .new_session(&host_meta(
            json!({"extensions": {"loadProjectInstructions": false}}),
        ))
        .await;
    assert!(!h.prompt_of(&off).await.contains(INSTRUCTIONS_MARK));

    // Process switch (`--no-project-instructions`): a session asking for
    // `true` cannot bring them back.
    let cli = ExtensionsConfig {
        load_project_instructions: false,
        ..ExtensionsConfig::default()
    };
    let h = Harness::new(None, cli, None).await;
    let plain = h.new_session(&SessionSetup::default()).await;
    assert!(!h.prompt_of(&plain).await.contains(INSTRUCTIONS_MARK));
    let asks_for_them = h
        .new_session(&host_meta(
            json!({"extensions": {"loadProjectInstructions": true}}),
        ))
        .await;
    assert!(!h
        .prompt_of(&asks_for_them)
        .await
        .contains(INSTRUCTIONS_MARK));
}

#[tokio::test]
async fn config_extensions_section_is_the_process_default() {
    let mut config = config();
    config.extensions.load_project_instructions = false;
    config.extensions.load_hooks = false;
    let policy = ProcessPolicy::from_config(&config, None, ExtensionsConfig::default()).unwrap();
    assert_eq!(policy.extensions, config.extensions);
    assert_eq!(
        default_process_policy(&config).extensions,
        config.extensions
    );
}

#[tokio::test]
async fn hooks_toggle_at_process_and_session_level() {
    let h = Harness::new(None, ExtensionsConfig::default(), None).await;
    assert!(h.state.hooks.is_enabled());
    let on = h.new_session(&SessionSetup::default()).await;
    let off = h
        .new_session(&host_meta(json!({"extensions": {"loadHooks": false}})))
        .await;
    assert!(h.state.hooks_for_session(&on).await.is_some());
    assert!(h.state.hooks_for_session(&off).await.is_none());

    let cli = ExtensionsConfig {
        load_hooks: false,
        ..ExtensionsConfig::default()
    };
    let h = Harness::new(None, cli, None).await;
    assert!(!h.state.hooks.is_enabled());
    let sid = h
        .new_session(&host_meta(json!({"extensions": {"loadHooks": true}})))
        .await;
    assert!(h.state.hooks_for_session(&sid).await.is_none());
}

#[tokio::test]
async fn plugins_toggle_at_process_and_session_level() {
    let plugins_dir = plugins_dir_with_prompt();

    let h = Harness::new(None, ExtensionsConfig::default(), Some(plugins_dir.clone())).await;
    assert!(h.state.plugins.is_enabled());
    let on = h.new_session(&SessionSetup::default()).await;
    assert!(h.prompt_of(&on).await.contains(PLUGIN_MARK));
    let off = h
        .new_session(&host_meta(json!({"extensions": {"loadPlugins": false}})))
        .await;
    assert!(!h.prompt_of(&off).await.contains(PLUGIN_MARK));

    // Process switch: nothing is even discovered, and a session cannot ask for
    // plugins back.
    let cli = ExtensionsConfig {
        load_plugins: false,
        ..ExtensionsConfig::default()
    };
    let h = Harness::new(None, cli, Some(plugins_dir.clone())).await;
    assert!(!h.state.plugins.is_enabled());
    assert!(h.state.plugins.list().await.is_empty());
    let sid = h
        .new_session(&host_meta(json!({"extensions": {"loadPlugins": true}})))
        .await;
    assert!(!h.prompt_of(&sid).await.contains(PLUGIN_MARK));

    let _ = std::fs::remove_dir_all(plugins_dir);
}

#[tokio::test]
async fn resumed_sessions_get_their_role_again() {
    let h = Harness::new(None, ExtensionsConfig::default(), None).await;
    let sid = h.new_session(&SessionSetup::default()).await;
    // Simulate a process restart: the session leaves memory, the transcript
    // stays.
    h.state.sessions.lock().await.remove(&sid);
    h.state.session_roles.lock().await.remove(&sid);

    h.host
        .load_session(
            &sid,
            &h.workspace.to_string_lossy(),
            &host_meta(json!({"role": "sup"})),
        )
        .await
        .expect("load");
    assert!(h.prompt_of(&sid).await.starts_with("ROLE-PROMPT-XYZ"));
    assert_eq!(h.tool_names(&sid).await, vec!["Grep", "Read"]);

    // Loading the live session again with a wider request cannot widen it.
    h.host
        .load_session(
            &sid,
            &h.workspace.to_string_lossy(),
            &host_meta(json!({"role": "wide"})),
        )
        .await
        .expect("load again");
    assert_eq!(h.tool_names(&sid).await, vec!["Grep", "Read"]);
}

#[tokio::test]
async fn restricted_role_gets_no_delegation_tools() {
    let h = Harness::new(Some("sup"), ExtensionsConfig::default(), None).await;
    let sid = h.new_session(&SessionSetup::default()).await;
    let (event_tx, _event_rx) = mpsc::channel::<AgentEvent>(8);
    let tools = build_turn_tool_registry(&h.state, event_tx, Vec::new(), &sid, &h.workspace).await;
    // Delegation tools are not in the allowlist, so a restricted role cannot
    // spawn subagents at all; the registry itself refuses them.
    for name in [
        "Agent",
        "Task",
        "AgentSwarm",
        "Bash",
        "Skill",
        "Cron",
        "Goal",
    ] {
        assert!(tools.get(name).is_none(), "{name} must not be registered");
        assert!(!tools.permits(name));
    }
}
