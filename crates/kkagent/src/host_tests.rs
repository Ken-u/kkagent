//! The embedding-host surface: host tools, host storage and per-session MCP
//! servers, all driven through the same `AgentAcpHost` the ACP server uses.

use std::sync::Mutex as StdMutex;

use super::role_tests::{config, host_meta, Harness};
use super::*;
use async_trait::async_trait;
use kkagent_acp::{AcpHost, SessionSetup};
use kkagent_config::{ExtensionsConfig, StorageBackend};
use kkagent_core::host_channel::{HostChannel, HostChannelError, RequestOptions};
use kkagent_core::role::ProcessPolicy;
use kkagent_core::storage::{handle_store_request, MemoryStore, SessionStore};
use serde_json::{json, Value};

/// The host side of the connection: serves `_kkagent/store/*` from memory,
/// answers `_kkagent/tool/call` with a canned result and records everything.
struct ScriptedHost {
    data: MemoryStore,
    tool_answer: StdMutex<Result<Value, HostChannelError>>,
    requests: StdMutex<Vec<(String, Value)>>,
    notifications: StdMutex<Vec<(String, Value)>>,
}

impl ScriptedHost {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            data: MemoryStore::new(),
            tool_answer: StdMutex::new(Ok(json!({"content": "pong"}))),
            requests: StdMutex::new(Vec::new()),
            notifications: StdMutex::new(Vec::new()),
        })
    }

    fn methods(&self) -> Vec<String> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .map(|(method, _)| method.clone())
            .collect()
    }
}

#[async_trait]
impl HostChannel for ScriptedHost {
    async fn request(
        &self,
        method: &str,
        params: Value,
        _options: RequestOptions,
    ) -> Result<Value, HostChannelError> {
        self.requests
            .lock()
            .unwrap()
            .push((method.to_string(), params.clone()));
        if method == kkagent_protocol::host::METHOD_HOST_TOOL_CALL {
            return self.tool_answer.lock().unwrap().clone();
        }
        match handle_store_request(&self.data, &self.data, method, &params).await {
            Some(Ok(value)) => Ok(value),
            Some(Err(error)) => Err(HostChannelError::Rpc {
                code: error.code,
                message: error.message,
            }),
            None => Err(HostChannelError::Rpc {
                code: -32601,
                message: format!("Method not found: {method}"),
            }),
        }
    }

    async fn notify(&self, method: &str, params: Value) {
        self.notifications
            .lock()
            .unwrap()
            .push((method.to_string(), params));
    }
}

fn connect(h: &Harness) -> Arc<ScriptedHost> {
    let host = ScriptedHost::new();
    assert!(h.state.host_channel.bind(host.clone()));
    host
}

fn advertise(tools: bool, sessions: bool, events: bool) -> Value {
    json!({"_meta": {"kkagent/host": {
        "tools": tools,
        "store": {"sessions": sessions, "events": events},
    }}})
}

fn names(list: &[String]) -> Vec<&str> {
    list.iter().map(String::as_str).collect()
}

fn tool_context(session_id: &str) -> kkagent_tools::ToolContext {
    kkagent_tools::ToolContext {
        working_dir: std::env::temp_dir(),
        session_id: session_id.to_string(),
        turn_id: format!("{session_id}:1"),
        plan_file_path: None,
        image: Default::default(),
        tool_call_id: Some("call-1".into()),
        interrupted: None,
        tools_config: Default::default(),
        model_alias: None,
    }
}

// ---------------------------------------------------------------------------
// Host tools
// ---------------------------------------------------------------------------

#[tokio::test]
async fn host_tools_need_the_advertised_capability_and_a_free_name() {
    let h = Harness::new(None, ExtensionsConfig::default(), None).await;
    let ping = host_meta(json!({"hostTools": [{"name": "host_ping", "description": "ping"}]}));

    // Nothing advertised yet: refused, and no half-created session is left.
    let error = h.try_new_session(&ping).await.unwrap_err();
    assert!(error.contains("advertise"), "{error}");
    assert!(h.state.sessions.lock().await.is_empty());
    assert!(h.state.session_roles.lock().await.is_empty());

    h.host
        .initialize(&advertise(true, false, false))
        .await
        .unwrap();
    let sid = h.new_session(&ping).await;
    assert!(h.tool_names(&sid).await.iter().any(|n| n == "host_ping"));

    for (name, reason) in [("Bash", "built-in"), ("mcp__evil__x", "reserved")] {
        let setup = host_meta(json!({"hostTools": [{"name": name}]}));
        let error = h.try_new_session(&setup).await.unwrap_err();
        assert!(error.contains(reason), "{name}: {error}");
    }
}

#[tokio::test]
async fn strict_roles_decide_which_host_tools_exist() {
    // A role that does not list the host tool never sees it, however the host
    // describes it.
    let h = Harness::new(Some("sup"), ExtensionsConfig::default(), None).await;
    h.host
        .initialize(&advertise(true, false, false))
        .await
        .unwrap();
    let setup = host_meta(json!({"hostTools": [{"name": "host_ping"}]}));
    let sid = h.new_session(&setup).await;
    assert_eq!(names(&h.tool_names(&sid).await), vec!["Grep", "Read"]);

    // With a wide role the session itself can still only narrow.
    let h = Harness::new(Some("wide"), ExtensionsConfig::default(), None).await;
    h.host
        .initialize(&advertise(true, false, false))
        .await
        .unwrap();
    let setup = host_meta(json!({
        "toolAllowlist": ["host_ping"],
        "hostTools": [{"name": "host_ping"}, {"name": "host_other"}],
    }));
    let sid = h.new_session(&setup).await;
    assert_eq!(names(&h.tool_names(&sid).await), vec!["host_ping"]);
}

#[tokio::test]
async fn host_tool_calls_reach_the_host_and_carry_its_approval_choice() {
    let h = Harness::new(None, ExtensionsConfig::default(), None).await;
    let host = connect(&h);
    h.host
        .initialize(&advertise(true, false, false))
        .await
        .unwrap();
    let setup = host_meta(json!({"hostTools": [
        {"name": "host_ping", "description": "ping", "approval": "host"},
        {"name": "host_ask", "description": "asks first", "approval": "ask"},
    ]}));
    let sid = h.new_session(&setup).await;

    let (event_tx, _event_rx) = mpsc::channel::<AgentEvent>(8);
    let (tools, policies) =
        build_turn_tools(&h.state, event_tx, Vec::new(), &sid, &h.workspace).await;
    let output = tools
        .get("host_ping")
        .expect("registered")
        .execute(json!({"x": 1}), &tool_context(&sid))
        .await
        .unwrap();
    assert_eq!(output.content, "pong");
    let requests = host.requests.lock().unwrap().clone();
    let (method, params) = requests
        .iter()
        .find(|(method, _)| method.starts_with("_kkagent/tool/"))
        .expect("the call reached the host");
    assert_eq!(method, "_kkagent/tool/call");
    assert_eq!(params["sessionId"], sid.as_str());
    assert_eq!(params["name"], "host_ping");
    assert_eq!(params["input"]["x"], 1);

    // The permission chain built from the policies: the host authorizes
    // `host_ping` itself, `host_ask` goes through kkagent's own chain.
    let chain = PermissionChain::new(PermissionMode::Manual, vec![]).with_host_tools(policies);
    let decide = |tool: &str| chain.evaluate(tool, &json!({}), &h.workspace, false, None);
    assert_eq!(
        decide("host_ping"),
        kkagent_core::permission::PermissionDecision::Approve
    );
    assert_eq!(
        decide("host_ask"),
        kkagent_core::permission::PermissionDecision::Ask
    );
}

#[tokio::test]
async fn a_host_tool_cannot_ride_on_the_name_of_a_registered_tool() {
    // Even if a name slips past session-creation validation, registration
    // never replaces an existing tool and grants no approval for it.
    let h = Harness::new(None, ExtensionsConfig::default(), None).await;
    connect(&h);
    h.host
        .initialize(&advertise(true, false, false))
        .await
        .unwrap();
    let sid = h.new_session(&SessionSetup::default()).await;
    // Inject a conflicting tool directly into the bound role.
    {
        let mut roles = h.state.session_roles.lock().await;
        let mut role = (**roles.get(&sid).unwrap()).clone();
        role.host_tools =
            vec![serde_json::from_value(json!({"name": "Bash", "approval": "host"})).unwrap()];
        roles.insert(sid.clone(), Arc::new(role));
    }
    let (event_tx, _event_rx) = mpsc::channel::<AgentEvent>(8);
    let (tools, policies) =
        build_turn_tools(&h.state, event_tx, Vec::new(), &sid, &h.workspace).await;
    assert!(
        policies.is_empty(),
        "no approval granted for a refused tool"
    );
    let bash = tools.get("Bash").expect("the built-in Bash is still there");
    assert_ne!(bash.description(), "");
    let chain = PermissionChain::new(PermissionMode::Manual, vec![]).with_host_tools(policies);
    assert_eq!(
        chain.evaluate("Bash", &json!({"command": "ls"}), &h.workspace, false, None),
        kkagent_core::permission::PermissionDecision::Ask
    );
}

// ---------------------------------------------------------------------------
// Per-session MCP servers
// ---------------------------------------------------------------------------

fn mcp_setup(servers: Value) -> SessionSetup {
    SessionSetup {
        mcp_servers: servers.as_array().unwrap().clone(),
        ..Default::default()
    }
}

#[tokio::test]
async fn session_mcp_servers_are_scoped_vetted_and_stopped_with_the_session() {
    let files = json!([{"name": "files", "command": "/definitely/not/a/kkagent-mcp-binary"}]);

    let h = Harness::new(Some("wide"), ExtensionsConfig::default(), None).await;
    let sid = h.new_session(&mcp_setup(files.clone())).await;
    assert!(h.state.session_mcp.lock().await.contains_key(&sid));
    // The unreachable server only means fewer tools; the turn still builds.
    let tool_names = h.tool_names(&sid).await;
    assert!(!tool_names.iter().any(|n| n.starts_with("mcp__files__")));
    remove_session_runtime(&h.state, &sid).await;
    assert!(h.state.session_mcp.lock().await.is_empty());

    // A strict role that can never permit the server's tools never starts it.
    let h = Harness::new(Some("sup"), ExtensionsConfig::default(), None).await;
    let sid = h.new_session(&mcp_setup(files.clone())).await;
    assert!(h.state.session_mcp.lock().await.get(&sid).is_none());

    // ...but one that lists the server's namespace does.
    let mut cfg = config();
    cfg.agent_roles.insert(
        "gh".into(),
        serde_json::from_value(json!({"tools": ["Read", "mcp__files__*"]})).unwrap(),
    );
    let policy = ProcessPolicy::from_config(&cfg, Some("gh"), ExtensionsConfig::default()).unwrap();
    let h = Harness::with_policy(cfg, policy, None).await;
    let sid = h.new_session(&mcp_setup(files)).await;
    assert!(h.state.session_mcp.lock().await.contains_key(&sid));
}

#[tokio::test]
async fn bad_or_clashing_session_mcp_servers_fail_the_request_cleanly() {
    let mut cfg = config();
    cfg.mcp_servers.insert(
        "shared".into(),
        serde_json::from_value(json!({"command": "/definitely/not/a/kkagent-mcp-binary"})).unwrap(),
    );
    let policy = ProcessPolicy::from_config(&cfg, None, ExtensionsConfig::default()).unwrap();
    let h = Harness::with_policy(cfg, policy, None).await;

    for (servers, reason) in [
        (
            json!([{"name": "shared", "command": "x"}]),
            "already configured",
        ),
        (json!([{"name": "plugin-x:y", "command": "x"}]), "reserved"),
        (json!([{"name": "a"}]), "command"),
    ] {
        let error = h.try_new_session(&mcp_setup(servers)).await.unwrap_err();
        assert!(error.contains(reason), "{reason}: {error}");
    }
    assert!(h.state.sessions.lock().await.is_empty());
    assert!(h.state.session_mcp.lock().await.is_empty());
}

// ---------------------------------------------------------------------------
// Host storage
// ---------------------------------------------------------------------------

fn host_storage_policy() -> (AppConfig, ProcessPolicy) {
    let cfg = config();
    let mut policy = ProcessPolicy::from_config(&cfg, None, ExtensionsConfig::default()).unwrap();
    policy.storage = StorageBackend::Host;
    (cfg, policy)
}

#[tokio::test]
async fn host_storage_requires_the_capability_at_initialize() {
    let (cfg, policy) = host_storage_policy();
    let h = Harness::with_policy(cfg, policy, None).await;

    let error = h.host.initialize(&json!({})).await.unwrap_err();
    assert!(error.contains("store.sessions"), "{error}");
    let error = h
        .host
        .initialize(&advertise(true, false, true))
        .await
        .unwrap_err();
    assert!(error.contains("store.sessions"), "{error}");

    let meta = h
        .host
        .initialize(&advertise(false, true, true))
        .await
        .unwrap()
        .expect("negotiated hosts get an answer");
    assert_eq!(meta["kkagent/host"]["store"]["sessions"], true);
    assert_eq!(meta["kkagent/host"]["store"]["events"], true);
    assert!(h.state.host_event_store().is_some());

    // Events stay off unless the host asked for them.
    h.host
        .initialize(&advertise(false, true, false))
        .await
        .unwrap();
    assert!(h.state.host_event_store().is_none());
}

#[tokio::test]
async fn host_storage_keeps_sessions_in_the_host_and_never_in_local_sqlite() {
    let (cfg, policy) = host_storage_policy();
    let h = Harness::with_policy(cfg, policy, None).await;
    let host = connect(&h);
    h.host
        .initialize(&advertise(false, true, false))
        .await
        .unwrap();
    assert_eq!(h.state.session_store.backend(), "host");

    let sid = h.new_session(&SessionSetup::default()).await;
    assert!(host.data.get_session(&sid).await.unwrap().is_some());
    assert!(
        h.state
            .transcript
            .lock()
            .await
            .get_session(&sid)
            .unwrap()
            .is_none(),
        "nothing about the session is written to the local database"
    );

    // Dropping the live session and loading it again goes back to the host.
    h.state.sessions.lock().await.remove(&sid);
    h.host
        .load_session(
            &sid,
            &h.workspace.to_string_lossy(),
            &SessionSetup::default(),
        )
        .await
        .unwrap();
    assert!(h.state.sessions.lock().await.contains_key(&sid));
    let methods = host.methods();
    assert!(methods.contains(&"_kkagent/store/session/create".to_string()));
    assert!(methods.contains(&"_kkagent/store/session/get".to_string()));
    assert!(methods.contains(&"_kkagent/store/messages/load".to_string()));

    // A session the host does not know is simply not found.
    let error = h
        .host
        .load_session(
            "never-existed",
            &h.workspace.to_string_lossy(),
            &SessionSetup::default(),
        )
        .await
        .unwrap_err();
    assert!(error.contains("session not found"), "{error}");
}

#[tokio::test]
async fn host_storage_fails_closed_without_a_connected_host() {
    let (cfg, policy) = host_storage_policy();
    let h = Harness::with_policy(cfg, policy, None).await;
    // The channel is never bound: creating a session must fail instead of
    // quietly falling back to a local store.
    let error = h
        .try_new_session(&SessionSetup::default())
        .await
        .unwrap_err();
    assert!(error.contains("not connected"), "{error}");
    assert!(h.state.sessions.lock().await.is_empty());
    assert!(h.state.session_roles.lock().await.is_empty());
}

#[tokio::test]
async fn default_storage_is_local_sqlite_with_no_host_surface() {
    let h = Harness::new(None, ExtensionsConfig::default(), None).await;
    assert_eq!(h.state.session_store.backend(), "sqlite");
    assert!(h.state.event_store.is_none());
    assert!(h.state.host_event_store().is_none());
    // A plain client's initialize is unchanged: no `_meta` in the answer.
    assert_eq!(h.host.initialize(&json!({})).await.unwrap(), None);
    let sid = h.new_session(&SessionSetup::default()).await;
    assert!(h
        .state
        .transcript
        .lock()
        .await
        .get_session(&sid)
        .unwrap()
        .is_some());
}

// ---------------------------------------------------------------------------
// ACP session lifecycle
// ---------------------------------------------------------------------------

#[tokio::test]
async fn acp_sessions_list_close_resume_and_delete() {
    let h = Harness::new(None, ExtensionsConfig::default(), None).await;
    let sid = h.new_session(&SessionSetup::default()).await;
    let cwd = h.workspace.to_string_lossy().to_string();

    let listed = h.host.list_sessions(None).await.unwrap().unwrap();
    let entry = listed
        .iter()
        .find(|entry| entry["sessionId"] == json!(sid))
        .expect("a new session is listed");
    assert!(entry["updatedAt"].is_string());

    // `cwd` narrows the list to one workspace.
    let here = h.host.list_sessions(Some(&cwd)).await.unwrap().unwrap();
    assert!(here.iter().any(|entry| entry["sessionId"] == json!(sid)));
    let elsewhere = h
        .host
        .list_sessions(Some("/definitely/not/a/workspace"))
        .await
        .unwrap()
        .unwrap();
    assert!(elsewhere
        .iter()
        .all(|entry| entry["sessionId"] != json!(sid)));

    // Closing frees the runtime state but keeps the conversation.
    h.host.close_session(&sid).await.unwrap();
    assert!(h.state.sessions.lock().await.get(&sid).is_none());
    assert!(h.state.session_roles.lock().await.get(&sid).is_none());
    let still_listed = h.host.list_sessions(None).await.unwrap().unwrap();
    assert!(still_listed
        .iter()
        .any(|entry| entry["sessionId"] == json!(sid)));

    // ...so it can be picked up again.
    h.host
        .load_session(&sid, &cwd, &SessionSetup::default())
        .await
        .unwrap();
    assert!(h.state.sessions.lock().await.get(&sid).is_some());

    // Deleting removes it for good; an unknown id says so.
    h.host.delete_session(&sid).await.unwrap();
    assert!(h.state.sessions.lock().await.get(&sid).is_none());
    let gone = h
        .host
        .load_session(&sid, &cwd, &SessionSetup::default())
        .await
        .unwrap_err();
    assert!(gone.starts_with("session not found"), "{gone}");
    let after = h.host.list_sessions(None).await.unwrap().unwrap();
    assert!(after.iter().all(|entry| entry["sessionId"] != json!(sid)));
    let twice = h.host.delete_session(&sid).await.unwrap_err();
    assert!(twice.starts_with("session not found"), "{twice}");
}

#[tokio::test]
async fn closing_a_session_drops_its_role_binding_and_a_reload_rebinds_it() {
    let h = Harness::new(None, ExtensionsConfig::default(), None).await;
    let setup = host_meta(json!({"toolAllowlist": ["Read"]}));
    let sid = h.new_session(&setup).await;
    assert_eq!(h.tool_names(&sid).await, vec!["Read".to_string()]);

    h.host.close_session(&sid).await.unwrap();
    assert!(h.state.session_roles.lock().await.get(&sid).is_none());

    // The reloaded session is restricted again only because the host says so
    // again; the allowlist is never remembered from a store.
    let cwd = h.workspace.to_string_lossy().to_string();
    h.host.load_session(&sid, &cwd, &setup).await.unwrap();
    assert_eq!(h.tool_names(&sid).await, vec!["Read".to_string()]);
}

#[tokio::test]
async fn acp_context_window_and_title_come_from_the_session() {
    let h = Harness::new(None, ExtensionsConfig::default(), None).await;
    let sid = h.new_session(&SessionSetup::default()).await;
    assert_eq!(h.host.session_title(&sid).await, None);
    assert_eq!(h.host.session_title("missing").await, None);
    assert_eq!(h.host.context_window("missing").await, None);
    {
        let mut sessions = h.state.sessions.lock().await;
        sessions.get_mut(&sid).unwrap().title = Some("  ".into());
    }
    assert_eq!(
        h.host.session_title(&sid).await,
        None,
        "blank titles are not announced"
    );
    {
        let mut sessions = h.state.sessions.lock().await;
        sessions.get_mut(&sid).unwrap().title = Some("Fix the build".into());
    }
    assert_eq!(
        h.host.session_title(&sid).await.as_deref(),
        Some("Fix the build")
    );
}
