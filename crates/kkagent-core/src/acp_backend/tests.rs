use super::testing::*;
use super::*;
use kkagent_acp::{AcpPermissionChoice, AcpPermissionOption};

fn user(text: &str) -> ChatMessage {
    ChatMessage::text("user", text)
}

fn assistant(text: &str) -> ChatMessage {
    ChatMessage::text("assistant", text)
}

fn tool(kind: &str, title: &str, status: &str, locations: &[&str]) -> ToolActivity {
    ToolActivity {
        kind: kind.into(),
        title: title.into(),
        status: status.into(),
        locations: locations.iter().map(|s| s.to_string()).collect(),
    }
}

#[test]
fn trailing_group_skips_tool_results_and_assistant_messages() {
    assert_eq!(trailing_user_group_start(&[]), 0);
    let history = vec![user("a"), assistant("b"), user("c"), user("d")];
    assert_eq!(trailing_user_group_start(&history), 2);
    assert_eq!(trailing_user_group_start(&[user("only")]), 0);
    // A user message that only carries a tool result belongs to the
    // previous (tool) exchange, not to the unanswered input.
    let with_result = vec![
        user("go"),
        ChatMessage::new(
            "assistant",
            vec![ChatContent::ToolUse {
                id: "t".into(),
                name: "Read".into(),
                input: serde_json::json!({}),
            }],
        ),
        ChatMessage::new(
            "user",
            vec![ChatContent::ToolResult {
                tool_use_id: "t".into(),
                content: "x".into(),
                is_error: false,
            }],
        ),
        user("next"),
    ];
    assert_eq!(trailing_user_group_start(&with_result), 3);
    // The activity summary an ACP turn leaves behind is answered history, not
    // pending input (otherwise the sync mark would never validate).
    let summary = format_activity_summary(
        "p.cursor",
        "cursor",
        &[ToolActivity {
            kind: "edit".into(),
            title: "Edit a.rs".into(),
            status: "completed".into(),
            locations: vec!["a.rs".into()],
        }],
    )
    .unwrap();
    let after_acp_turn = vec![user("go"), assistant("done"), user(&summary), user("next")];
    assert_eq!(trailing_user_group_start(&after_acp_turn), 3);
    // Nothing pending but the summary: empty group.
    assert_eq!(trailing_user_group_start(&after_acp_turn[..3]), 3);
}

#[test]
fn prompt_text_strips_harness_blocks_and_notes_attachments() {
    let reminder = user("<system-reminder>\nplan mode\n</system-reminder>");
    let real = user("<system-reminder>\nx\n</system-reminder>\nfix the bug");
    assert_eq!(prompt_text(&[reminder.clone(), real]), "fix the bug");
    // Only harness content pending: fall back to the raw text instead of
    // sending an empty prompt.
    assert!(prompt_text(&[reminder]).contains("plan mode"));
    let with_image = ChatMessage::new(
        "user",
        vec![
            ChatContent::Text {
                text: "look".into(),
            },
            ChatContent::Image {
                media_type: "image/png".into(),
                data: "AAAA".into(),
            },
        ],
    );
    let text = prompt_text(&[with_image]);
    assert!(text.starts_with("look\n\n[1 image/video attachment(s) were not forwarded"));
    assert_eq!(prompt_text(&[user("a"), user("b")]), "a\n\nb");
}

#[test]
fn preface_quotes_history_but_skips_harness_messages() {
    let activity = format_activity_summary(
        STUB_AGENT_ID,
        "stub",
        &[tool("edit", "Edit `a.rs`", "completed", &["/w/a.rs"])],
    )
    .unwrap();
    let history = vec![
        user("first question"),
        assistant("first answer"),
        user(&activity),
        user("<system-reminder>\nTodoList is stale\n</system-reminder>"),
        ChatMessage::new(
            "user",
            vec![
                ChatContent::Text {
                    text: "see this".into(),
                },
                ChatContent::Image {
                    media_type: "image/png".into(),
                    data: "AAAA".into(),
                },
            ],
        ),
    ];
    let preface = build_preface(&history).unwrap();
    assert!(preface.starts_with("<kkagent-context>"));
    assert!(preface.ends_with("</kkagent-context>"));
    assert!(preface.contains("[user]\nfirst question"));
    assert!(preface.contains("[assistant]\nfirst answer"));
    assert!(preface.contains("[agent tool activity]\n- edit · Edit `a.rs` (completed) — /w/a.rs"));
    assert!(preface.contains("see this\n[image]"));
    assert!(!preface.contains("TodoList is stale"), "{preface}");
    assert_eq!(build_preface(&[]), None);
    assert_eq!(
        build_preface(&[user("<system-reminder>\nonly harness\n</system-reminder>")]),
        None
    );
}

#[test]
fn preface_is_bounded_and_keeps_the_newest_messages() {
    let history: Vec<ChatMessage> = (0..60)
        .flat_map(|i| {
            [
                user(&format!("question {i} {}", "x".repeat(1_000))),
                assistant(&format!("answer {i} {}", "y".repeat(1_000))),
            ]
        })
        .collect();
    let preface = build_preface(&history).unwrap();
    assert!(
        preface.chars().count() < PREFACE_MAX_CHARS + 1_000,
        "preface too long: {}",
        preface.chars().count()
    );
    assert!(preface.contains("older message(s) omitted"));
    assert!(preface.contains("question 59"));
    assert!(preface.contains("answer 59"));
    assert!(!preface.contains("question 0 "));
    // A single huge message is quoted partially, never dropped entirely.
    let huge = build_preface(&[user(&"z".repeat(100_000))]).unwrap();
    assert!(huge.chars().count() < PREFACE_MESSAGE_MAX_CHARS + 600);
    assert!(huge.contains('…'));
}

#[test]
fn activity_summary_is_harness_only_and_identifies_the_agent() {
    assert_eq!(format_activity_summary(STUB_AGENT_ID, "stub", &[]), None);
    let activity: Vec<ToolActivity> = (0..45)
        .map(|i| tool("execute", &format!("`cmd {i}`"), "completed", &[]))
        .collect();
    let summary = format_activity_summary(STUB_AGENT_ID, "stub", &activity).unwrap();
    assert!(
        summary.starts_with("<system-reminder>\n[acp-activity agent=stub-plugin.stub name=stub]")
    );
    assert!(summary.contains("- execute · `cmd 39` (completed)"));
    assert!(!summary.contains("`cmd 40`"));
    assert!(summary.contains("… and 5 more tool call(s)"));
    // Harness-only: invisible to user-turn detection and titles.
    assert!(kkagent_protocol::harness::is_harness_only_user_text(
        &summary
    ));
    assert_eq!(kkagent_protocol::harness::visible_user_text(&summary), "");

    let messages = vec![
        user("hi"),
        assistant("done"),
        user(&summary),
        user("again"),
        assistant("done"),
        user(
            &format_activity_summary("other.agent", "other", &[tool("read", "Read", "", &[])])
                .unwrap(),
        ),
        user(&summary),
    ];
    assert_eq!(external_agents_in(&messages), vec!["stub", "other"]);
    // Pasted marker text inside a normal user message is not a summary.
    assert!(external_agents_in(&[user("quote: [acp-activity agent=x name=y]")]).is_empty());
}

#[test]
fn activity_line_lists_paths_and_folds_extras() {
    let summary = format_activity_summary(
        STUB_AGENT_ID,
        "stub",
        &[tool(
            "edit",
            "Edit",
            "failed",
            &["/a", "/b", "/c", "/d", "/e"],
        )],
    )
    .unwrap();
    assert!(summary.contains("- edit · Edit (failed) — /a, /b, /c (+2 more)"));
}

#[test]
fn tool_names_are_namespaced_by_agent() {
    assert_eq!(acp_tool_name("cursor", Some("edit")), "cursor/edit");
    assert_eq!(acp_tool_name("cursor", None), "cursor/tool");
    assert_eq!(acp_tool_name("cursor", Some("  ")), "cursor/tool");
}

#[test]
fn sync_mark_tracks_count_and_tail() {
    let messages = vec![user("a"), assistant("b"), user("c")];
    assert_eq!(SyncMark::of(&[]), None);
    let mark = SyncMark::of(&messages).unwrap();
    assert_eq!(mark.count, 3);
    assert!(mark.matches(&messages));
    // New messages appended: still a valid prefix.
    let mut longer = messages.clone();
    longer.push(assistant("d"));
    assert!(mark.matches(&longer));
    // Truncated (undo) or tail edited: no longer valid.
    assert!(!mark.matches(&messages[..2]));
    let mut edited = messages.clone();
    edited[2] = user("c'");
    assert!(!mark.matches(&edited));
    // Compaction replaced everything but kept the length by accident.
    let rewritten = vec![user("summary"), assistant("x"), user("y")];
    assert!(!mark.matches(&rewritten));
    // Images only count by size, so huge payloads stay cheap to compare.
    let image = |data: &str| {
        ChatMessage::new(
            "user",
            vec![ChatContent::Image {
                media_type: "image/png".into(),
                data: data.into(),
            }],
        )
    };
    assert_eq!(fingerprint(&image("AAAA")), fingerprint(&image("BBBB")));
    assert_ne!(fingerprint(&image("AAAA")), fingerprint(&image("AAAAAA")));
}

#[tokio::test]
async fn resolve_reports_missing_disabled_and_non_acp_agents() {
    let plugin = StubPlugin::new().await;
    let hub = AcpBackendHub::with_connector(plugin.manager.clone(), StubAgent::new().connector());
    let spec = hub.resolve(STUB_AGENT_ID).await.unwrap();
    assert_eq!(spec.transport, "acp");
    assert_eq!(spec.plugin_id, STUB_AGENT_ID);
    for missing in [
        "stub-plugin.nope",
        "nope.stub",
        "no-dot",
        "stub-plugin.inner",
    ] {
        let error = hub.resolve(missing).await.unwrap_err().to_string();
        assert!(error.contains("not available"), "{missing}: {error}");
        assert!(error.contains("/model"), "{missing}: {error}");
    }
}

#[tokio::test]
async fn plugin_manager_lists_acp_backends_and_agents() {
    let plugin = StubPlugin::new().await;
    let backends = plugin.manager.acp_backends().await;
    assert_eq!(backends.len(), 1);
    assert_eq!(backends[0].0, STUB_AGENT_ID);
    let info = plugin.manager.list().await;
    assert_eq!(info.len(), 1);
    assert_eq!(info[0].acp_agents.len(), 1);
    let agent = &info[0].acp_agents[0];
    assert_eq!(agent.id, STUB_AGENT_ID);
    assert_eq!(agent.agent, "stub");
    assert_eq!(agent.alias(), STUB_ALIAS);
    assert_eq!(
        kkagent_config::acp_agent_id(&agent.alias()),
        Some(STUB_AGENT_ID)
    );
}

#[tokio::test]
async fn checkout_reuses_the_process_and_session_across_turns() {
    let plugin = StubPlugin::new().await;
    let agent = StubAgent::new();
    let hub = AcpBackendHub::with_connector(plugin.manager.clone(), agent.connector());
    let cwd = PathBuf::from("/work");

    let lease = hub.checkout("s1", STUB_AGENT_ID, &cwd).await.unwrap();
    assert_eq!(lease.agent_name(), "stub");
    assert_eq!(lease.acp_session_id(), "stub-session-1");
    assert_eq!(lease.mode().as_deref(), Some("agent"));
    assert_eq!(lease.idle_timeout(), Duration::from_secs(5));
    drop(lease);

    let lease = hub.checkout("s1", STUB_AGENT_ID, &cwd).await.unwrap();
    assert_eq!(lease.acp_session_id(), "stub-session-1");
    drop(lease);
    assert_eq!(agent.spawns.load(Ordering::SeqCst), 1);
    assert_eq!(agent.sessions.load(Ordering::SeqCst), 1);

    // Another kkagent session gets its own process.
    let other = hub.checkout("s2", STUB_AGENT_ID, &cwd).await.unwrap();
    assert_eq!(agent.spawns.load(Ordering::SeqCst), 2);
    drop(other);
    assert_eq!(hub.len().await, 2);
    hub.close_all().await;
}

#[tokio::test]
async fn checkout_serializes_turns_of_the_same_session() {
    let plugin = StubPlugin::new().await;
    let hub = AcpBackendHub::with_connector(plugin.manager.clone(), StubAgent::new().connector());
    let cwd = PathBuf::from("/work");
    let first = hub.checkout("s1", STUB_AGENT_ID, &cwd).await.unwrap();
    let second = {
        let hub = hub.clone();
        let cwd = cwd.clone();
        tokio::spawn(async move { hub.checkout("s1", STUB_AGENT_ID, &cwd).await.map(|_| ()) })
    };
    tokio::time::sleep(Duration::from_millis(150)).await;
    assert!(!second.is_finished(), "second turn must wait for the lease");
    drop(first);
    second.await.unwrap().unwrap();
    hub.close_all().await;
}

#[tokio::test]
async fn dead_clients_are_replaced_on_the_next_checkout() {
    let plugin = StubPlugin::new().await;
    let agent = StubAgent::new();
    let hub = AcpBackendHub::with_connector(plugin.manager.clone(), agent.connector());
    let cwd = PathBuf::from("/work");
    let lease = hub.checkout("s1", STUB_AGENT_ID, &cwd).await.unwrap();
    lease.client().shutdown().await;
    drop(lease);
    let lease = hub.checkout("s1", STUB_AGENT_ID, &cwd).await.unwrap();
    assert!(lease.client().is_alive());
    assert_eq!(agent.spawns.load(Ordering::SeqCst), 2);
    // A fresh process has a fresh ACP session and nothing synced.
    assert_eq!(lease.acp_session_id(), "stub-session-2");
    drop(lease);
    hub.close_all().await;
}

#[tokio::test]
async fn invalidate_kills_the_process_and_forces_a_fresh_start() {
    let plugin = StubPlugin::new().await;
    let agent = StubAgent::new();
    let hub = AcpBackendHub::with_connector(plugin.manager.clone(), agent.connector());
    let cwd = PathBuf::from("/work");
    let lease = hub.checkout("s1", STUB_AGENT_ID, &cwd).await.unwrap();
    let client = lease.client();
    lease.invalidate().await;
    assert!(!client.is_alive());
    let lease = hub.checkout("s1", STUB_AGENT_ID, &cwd).await.unwrap();
    assert_eq!(agent.spawns.load(Ordering::SeqCst), 2);
    drop(lease);
    hub.close_all().await;
}

#[tokio::test]
async fn close_session_kills_only_that_sessions_backends() {
    let plugin = StubPlugin::new().await;
    let hub = AcpBackendHub::with_connector(plugin.manager.clone(), StubAgent::new().connector());
    let cwd = PathBuf::from("/work");
    let a = hub.checkout("s1", STUB_AGENT_ID, &cwd).await.unwrap();
    let a_client = a.client();
    let b = hub.checkout("s2", STUB_AGENT_ID, &cwd).await.unwrap();
    let b_client = b.client();
    drop(a);
    drop(b);
    hub.close_session("s1").await;
    assert!(!a_client.is_alive());
    assert!(b_client.is_alive());
    assert_eq!(hub.len().await, 1);
    hub.close_all().await;
    assert!(!b_client.is_alive());
    assert!(hub.is_empty().await);
}

#[tokio::test]
async fn close_session_also_kills_a_backend_that_is_mid_turn() {
    let plugin = StubPlugin::new().await;
    let hub = AcpBackendHub::with_connector(plugin.manager.clone(), StubAgent::new().connector());
    let lease = hub
        .checkout("s1", STUB_AGENT_ID, Path::new("/work"))
        .await
        .unwrap();
    let client = lease.client();
    // The lease (turn lock) is still held; deleting the session must not wait.
    tokio::time::timeout(Duration::from_secs(5), hub.close_session("s1"))
        .await
        .expect("close_session must not block on a running turn");
    assert!(!client.is_alive());
    drop(lease);
}

#[tokio::test]
async fn reap_idle_only_collects_unused_stale_backends() {
    let plugin = StubPlugin::new().await;
    let connector = StubAgent::new().connector();
    let hub = Arc::new(AcpBackendHub {
        plugins: plugin.manager.clone(),
        connector,
        entries: Mutex::new(HashMap::new()),
        reaper_started: AtomicBool::new(true),
        idle_reap_after: Duration::from_millis(50),
    });
    let cwd = PathBuf::from("/work");
    let busy = hub.checkout("busy", STUB_AGENT_ID, &cwd).await.unwrap();
    let idle = hub.checkout("idle", STUB_AGENT_ID, &cwd).await.unwrap();
    let idle_client = idle.client();
    drop(idle);
    tokio::time::sleep(Duration::from_millis(120)).await;
    hub.reap_idle().await;
    assert!(!idle_client.is_alive(), "idle backend should be reclaimed");
    assert_eq!(hub.len().await, 1, "a leased backend must survive reaping");
    assert!(busy.client().is_alive());
    drop(busy);
    hub.close_all().await;
}

#[tokio::test]
async fn first_turn_sends_the_whole_history_as_preface() {
    let plugin = StubPlugin::new().await;
    let hub = AcpBackendHub::with_connector(plugin.manager.clone(), StubAgent::new().connector());
    let cwd = PathBuf::from("/work");
    let mut lease = hub.checkout("s1", STUB_AGENT_ID, &cwd).await.unwrap();
    let messages = vec![
        user("earlier question"),
        assistant("earlier answer"),
        user("new question"),
    ];
    let prepared = lease.prepare(&messages, &cwd).await.unwrap();
    assert!(!prepared.rebuilt, "a fresh session needs no rebuild");
    assert!(prepared.has_preface);
    assert!(prepared.text.contains("[user]\nearlier question"));
    assert!(prepared.text.contains("[assistant]\nearlier answer"));
    assert!(prepared
        .text
        .ends_with("</kkagent-context>\n\nnew question"));

    // A brand-new conversation has nothing to introduce.
    let mut fresh = hub.checkout("s2", STUB_AGENT_ID, &cwd).await.unwrap();
    let prepared = fresh.prepare(&[user("hello")], &cwd).await.unwrap();
    assert!(!prepared.has_preface);
    assert_eq!(prepared.text, "hello");
    drop(lease);
    drop(fresh);
    hub.close_all().await;
}

#[tokio::test]
async fn follow_up_turns_only_carry_what_the_agent_has_not_seen() {
    let plugin = StubPlugin::new().await;
    let agent = StubAgent::new();
    let hub = AcpBackendHub::with_connector(plugin.manager.clone(), agent.connector());
    let cwd = PathBuf::from("/work");
    let mut lease = hub.checkout("s1", STUB_AGENT_ID, &cwd).await.unwrap();

    let mut transcript = vec![user("q1")];
    lease.prepare(&transcript, &cwd).await.unwrap();
    transcript.push(assistant("a1"));
    lease.mark_synced(&transcript);

    // Ordinary follow-up: no preface at all.
    transcript.push(user("q2"));
    let prepared = lease.prepare(&transcript, &cwd).await.unwrap();
    assert!(!prepared.has_preface && !prepared.rebuilt);
    assert_eq!(prepared.text, "q2");
    transcript.push(assistant("a2"));
    lease.mark_synced(&transcript);

    // Another model answered two turns in between: only those are quoted.
    transcript.extend([
        user("q3 (other model)"),
        assistant("a3 (other model)"),
        user("q4 (other model)"),
        assistant("a4 (other model)"),
        user("q5"),
    ]);
    let prepared = lease.prepare(&transcript, &cwd).await.unwrap();
    assert!(!prepared.rebuilt, "same ACP session keeps its own memory");
    assert!(prepared.has_preface);
    assert!(prepared.text.contains("q3 (other model)"));
    assert!(prepared.text.contains("a4 (other model)"));
    assert!(!prepared.text.contains("[user]\nq1"));
    assert!(!prepared.text.contains("[user]\nq2"));
    assert!(prepared.text.ends_with("\n\nq5"));
    assert_eq!(agent.sessions.load(Ordering::SeqCst), 1);
    drop(lease);
    hub.close_all().await;
}

#[tokio::test]
async fn rewritten_history_rebuilds_the_acp_session() {
    let plugin = StubPlugin::new().await;
    let agent = StubAgent::new();
    let hub = AcpBackendHub::with_connector(plugin.manager.clone(), agent.connector());
    let cwd = PathBuf::from("/work");
    let mut lease = hub.checkout("s1", STUB_AGENT_ID, &cwd).await.unwrap();

    let mut transcript = vec![user("q1")];
    lease.prepare(&transcript, &cwd).await.unwrap();
    transcript.extend([assistant("a1"), user("q2"), assistant("a2")]);
    lease.mark_synced(&transcript);
    assert_eq!(lease.acp_session_id(), "stub-session-1");

    // /undo of the last turn, then a new question.
    let mut undone = transcript[..2].to_vec();
    undone.push(user("q2 again"));
    let prepared = lease.prepare(&undone, &cwd).await.unwrap();
    assert!(prepared.rebuilt);
    assert_eq!(lease.acp_session_id(), "stub-session-2");
    assert!(prepared.text.contains("[user]\nq1"));
    assert!(prepared.text.contains("[assistant]\na1"));
    assert!(!prepared.text.contains("a2"));
    assert!(prepared.text.ends_with("\n\nq2 again"));

    // /compact replaced the transcript: stale again, another rebuild.
    lease.mark_synced(&undone);
    let compacted = vec![
        user("The conversation so far has been compacted. summary text"),
        user("q3"),
    ];
    let prepared = lease.prepare(&compacted, &cwd).await.unwrap();
    assert!(prepared.rebuilt);
    assert!(prepared.text.contains("summary text"));
    assert_eq!(agent.sessions.load(Ordering::SeqCst), 3);
    drop(lease);
    hub.close_all().await;
}

#[tokio::test]
async fn cwd_change_starts_a_new_acp_session_on_the_same_process() {
    let plugin = StubPlugin::new().await;
    let agent = StubAgent::new();
    let hub = AcpBackendHub::with_connector(plugin.manager.clone(), agent.connector());
    let mut lease = hub
        .checkout("s1", STUB_AGENT_ID, Path::new("/work"))
        .await
        .unwrap();
    let mut transcript = vec![user("q1")];
    lease
        .prepare(&transcript, Path::new("/work"))
        .await
        .unwrap();
    transcript.push(assistant("a1"));
    lease.mark_synced(&transcript);
    transcript.push(user("q2"));
    let prepared = lease
        .prepare(&transcript, Path::new("/elsewhere"))
        .await
        .unwrap();
    assert!(prepared.rebuilt);
    assert_eq!(agent.spawns.load(Ordering::SeqCst), 1);
    assert_eq!(agent.sessions.load(Ordering::SeqCst), 2);
    drop(lease);
    hub.close_all().await;
}

#[tokio::test]
async fn ensure_mode_switches_once_and_ignores_unknown_modes() {
    let plugin = StubPlugin::new().await;
    let agent = StubAgent::new();
    let hub = AcpBackendHub::with_connector(plugin.manager.clone(), agent.connector());
    let mut lease = hub
        .checkout("s1", STUB_AGENT_ID, Path::new("/work"))
        .await
        .unwrap();
    lease.ensure_mode("agent").await; // already there
    lease.ensure_mode("plan").await;
    lease.ensure_mode("plan").await; // cached
    lease.ensure_mode("turbo").await; // not offered by the agent
    assert_eq!(
        lock(&agent.modes).clone(),
        vec![("stub-session-1".to_string(), "plan".to_string())]
    );
    assert_eq!(lease.mode().as_deref(), Some("plan"));
    drop(lease);
    hub.close_all().await;
}

fn levels(values: &[&str]) -> Vec<String> {
    values.iter().map(|v| v.to_string()).collect()
}

#[test]
fn thinking_levels_match_exactly_or_to_the_nearest_offered() {
    let cursor = levels(&["low", "medium", "high", "xhigh", "max"]);
    for level in ["low", "medium", "high", "xhigh", "max"] {
        assert_eq!(
            match_thought_level(level, &cursor),
            Some((level.to_string(), true)),
            "{level}"
        );
    }
    // Spellings are normalized, the agent's own spelling is returned.
    assert_eq!(
        match_thought_level("Extra-High", &levels(&["Low", "Extra High"])),
        Some(("Extra High".to_string(), true))
    );
    assert_eq!(
        match_thought_level("OFF", &levels(&["none", "low"])),
        Some(("none".to_string(), true))
    );
    // Missing levels fall back to the closest one: below, above, between.
    assert_eq!(
        match_thought_level("minimal", &cursor),
        Some(("low".to_string(), false))
    );
    assert_eq!(
        match_thought_level("none", &cursor),
        Some(("low".to_string(), false))
    );
    assert_eq!(
        match_thought_level("ultra", &cursor),
        Some(("max".to_string(), false))
    );
    // A tie between two offered levels goes to the lower one.
    assert_eq!(
        match_thought_level("medium", &levels(&["low", "high"])),
        Some(("low".to_string(), false))
    );
    // Unknown spellings only match themselves.
    assert_eq!(match_thought_level("turbo", &cursor), None);
    assert_eq!(
        match_thought_level("turbo", &levels(&["turbo", "low"])),
        Some(("turbo".to_string(), true))
    );
    // Nothing comparable is on offer.
    assert_eq!(
        match_thought_level("high", &levels(&["thinking", "fast"])),
        None
    );
    assert_eq!(match_thought_level("high", &[]), None);
}

#[tokio::test]
async fn lease_reports_the_models_thinking_level_without_changing_anything() {
    let plugin = StubPlugin::new().await;
    let agent = StubAgent::new();
    agent.offer_thought_levels(&["low", "medium", "high", "xhigh", "max"], "high");
    let hub = AcpBackendHub::with_connector(plugin.manager.clone(), agent.connector());
    let lease = hub
        .checkout("s1", STUB_AGENT_ID, Path::new("/work"))
        .await
        .unwrap();
    assert_eq!(
        lease.agent_snapshot(),
        AcpAgentSnapshot {
            model: Some("stub-model".into()),
            thought_level: Some("high".into()),
            thought_levels: levels(&["low", "medium", "high", "xhigh", "max"]),
        }
    );
    assert!(agent.config_call_values().is_empty());
    drop(lease);

    // An agent without config options reports nothing.
    let bare = StubAgent::new();
    let hub = AcpBackendHub::with_connector(plugin.manager.clone(), bare.connector());
    let lease = hub
        .checkout("s1", STUB_AGENT_ID, Path::new("/work"))
        .await
        .unwrap();
    assert_eq!(lease.agent_snapshot(), AcpAgentSnapshot::default());
    drop(lease);
    hub.close_all().await;
}

#[tokio::test]
async fn ensure_thought_level_applies_a_changed_level_once() {
    let plugin = StubPlugin::new().await;
    let agent = StubAgent::new();
    agent.offer_thought_levels(&["low", "medium", "high", "xhigh", "max"], "high");
    let hub = AcpBackendHub::with_connector(plugin.manager.clone(), agent.connector());
    let mut lease = hub
        .checkout("s1", STUB_AGENT_ID, Path::new("/work"))
        .await
        .unwrap();

    // Already there: no request.
    assert_eq!(
        lease.ensure_thought_level("high").await,
        ThoughtLevelOutcome::Unchanged
    );
    assert!(agent.config_call_values().is_empty());

    let outcome = lease.ensure_thought_level(" MAX ").await;
    assert_eq!(
        outcome,
        ThoughtLevelOutcome::Applied {
            from: Some("high".into()),
            to: "max".into(),
            exact: true
        }
    );
    assert_eq!(outcome.note("stub", "max"), None);
    assert_eq!(
        agent.config_call_values(),
        vec![("reasoning_effort".to_string(), "max".to_string())]
    );
    assert_eq!(agent.thought_level().as_deref(), Some("max"));
    assert_eq!(lease.agent_snapshot().thought_level.as_deref(), Some("max"));

    // Asking again for the same level is free.
    assert_eq!(
        lease.ensure_thought_level("max").await,
        ThoughtLevelOutcome::Unchanged
    );
    assert_eq!(agent.config_call_values().len(), 1);
    drop(lease);
    hub.close_all().await;
}

#[tokio::test]
async fn ensure_thought_level_uses_the_closest_level_and_says_so() {
    let plugin = StubPlugin::new().await;
    let agent = StubAgent::new();
    agent.offer_thought_levels(&["low", "medium", "high"], "high");
    let hub = AcpBackendHub::with_connector(plugin.manager.clone(), agent.connector());
    let mut lease = hub
        .checkout("s1", STUB_AGENT_ID, Path::new("/work"))
        .await
        .unwrap();

    let outcome = lease.ensure_thought_level("xhigh").await;
    // The agent has nothing above `high`, where it already is.
    assert_eq!(outcome, ThoughtLevelOutcome::Unchanged);

    let outcome = lease.ensure_thought_level("minimal").await;
    assert_eq!(
        outcome,
        ThoughtLevelOutcome::Applied {
            from: Some("high".into()),
            to: "low".into(),
            exact: false
        }
    );
    let note = outcome.note("stub", "minimal").unwrap();
    assert!(note.contains("no thinking level 'minimal'"), "{note}");
    assert!(note.contains("'low'"), "{note}");
    assert_eq!(agent.thought_level().as_deref(), Some("low"));
    drop(lease);
    hub.close_all().await;
}

#[tokio::test]
async fn ensure_thought_level_reports_agents_that_cannot_change_it_once() {
    let plugin = StubPlugin::new().await;

    // No thinking-level option at all.
    let bare = StubAgent::new();
    let hub = AcpBackendHub::with_connector(plugin.manager.clone(), bare.connector());
    let mut lease = hub
        .checkout("s1", STUB_AGENT_ID, Path::new("/work"))
        .await
        .unwrap();
    let outcome = lease.ensure_thought_level("max").await;
    assert_eq!(outcome, ThoughtLevelOutcome::Unsupported);
    let note = outcome.note("stub", "max").unwrap();
    assert!(note.contains("does not let clients choose"), "{note}");
    // The same request is not reported again; a different one is.
    assert_eq!(
        lease.ensure_thought_level("max").await,
        ThoughtLevelOutcome::Unchanged
    );
    assert_eq!(
        lease.ensure_thought_level("low").await,
        ThoughtLevelOutcome::Unsupported
    );
    drop(lease);
    hub.close_all().await;

    // An agent that turns the change down.
    let strict = StubAgent::new();
    strict.offer_thought_levels(&["low", "high"], "low");
    strict.reject_config_changes();
    let hub = AcpBackendHub::with_connector(plugin.manager.clone(), strict.connector());
    let mut lease = hub
        .checkout("s1", STUB_AGENT_ID, Path::new("/work"))
        .await
        .unwrap();
    let outcome = lease.ensure_thought_level("high").await;
    let ThoughtLevelOutcome::Refused { reason } = &outcome else {
        panic!("expected a refusal, got {outcome:?}");
    };
    assert!(reason.contains("Invalid params"), "{reason}");
    assert!(outcome
        .note("stub", "high")
        .unwrap()
        .contains("did not take thinking level 'high'"));
    assert_eq!(
        lease.ensure_thought_level("high").await,
        ThoughtLevelOutcome::Unchanged
    );
    assert_eq!(strict.config_call_values().len(), 1);
    assert_eq!(lease.agent_snapshot().thought_level.as_deref(), Some("low"));

    // Nothing comparable on offer.
    let odd = StubAgent::new();
    odd.offer_thought_levels(&["thinking", "fast"], "fast");
    let hub2 = AcpBackendHub::with_connector(plugin.manager.clone(), odd.connector());
    let mut lease2 = hub2
        .checkout("s1", STUB_AGENT_ID, Path::new("/work"))
        .await
        .unwrap();
    let outcome = lease2.ensure_thought_level("high").await;
    let ThoughtLevelOutcome::Refused { reason } = &outcome else {
        panic!("expected a refusal, got {outcome:?}");
    };
    assert!(reason.contains("thinking, fast"), "{reason}");
    drop(lease);
    drop(lease2);
    hub.close_all().await;
    hub2.close_all().await;
}

#[tokio::test]
async fn a_rebuilt_acp_session_is_back_on_the_agents_own_level() {
    let plugin = StubPlugin::new().await;
    let agent = StubAgent::new();
    agent.offer_thought_levels(&["low", "medium", "high"], "high");
    let hub = AcpBackendHub::with_connector(plugin.manager.clone(), agent.connector());
    let mut lease = hub
        .checkout("s1", STUB_AGENT_ID, Path::new("/work"))
        .await
        .unwrap();
    lease.ensure_thought_level("low").await;
    assert_eq!(lease.agent_snapshot().thought_level.as_deref(), Some("low"));

    // A new ACP session (changed cwd) reads the options afresh; the stub
    // keeps its level like Cursor does, so the snapshot follows the agent.
    let mut transcript = vec![user("q1")];
    lease
        .prepare(&transcript, Path::new("/work"))
        .await
        .unwrap();
    transcript.push(assistant("a1"));
    lease.mark_synced(&transcript);
    transcript.push(user("q2"));
    let prepared = lease
        .prepare(&transcript, Path::new("/elsewhere"))
        .await
        .unwrap();
    assert!(prepared.rebuilt);
    assert_eq!(lease.agent_snapshot().thought_level.as_deref(), Some("low"));
    drop(lease);
    hub.close_all().await;
}

#[tokio::test]
async fn bridge_cancels_when_nobody_listens_and_routes_when_attached() {
    let bridge = Arc::new(InteractionBridge::default());
    let request = |id: &str| AcpPermissionRequest {
        session_id: "s".into(),
        tool_call_id: id.into(),
        title: "`ls`".into(),
        tool_kind: Some("execute".into()),
        options: vec![AcpPermissionOption {
            option_id: "allow-once".into(),
            name: "Allow".into(),
            kind: "allow_once".into(),
        }],
        ..Default::default()
    };
    assert_eq!(
        bridge.request_permission(request("t0")).await,
        AcpPermissionOutcome::Cancelled
    );

    let (tx, mut rx) = mpsc::channel(4);
    let attachment = bridge.attach(tx);
    let answering = tokio::spawn(async move {
        let Some(AcpInteraction::Permission { request, reply }) = rx.recv().await else {
            panic!("expected a permission interaction");
        };
        let option = request.option_for(AcpPermissionChoice::AllowOnce).unwrap();
        let _ = reply.send(AcpPermissionOutcome::Selected(option.option_id.clone()));
    });
    assert_eq!(
        bridge.request_permission(request("t1")).await,
        AcpPermissionOutcome::Selected("allow-once".into())
    );
    answering.await.unwrap();

    // The runner went away without answering (turn ended): cancelled.
    let (tx, rx) = mpsc::channel(4);
    drop(attachment);
    let attachment = bridge.attach(tx);
    drop(rx);
    assert_eq!(
        bridge.request_permission(request("t2")).await,
        AcpPermissionOutcome::Cancelled
    );
    drop(attachment);
    assert_eq!(
        bridge.ask_question(AcpAskQuestionRequest::default()).await,
        AcpAskQuestionOutcome::Cancelled
    );
    assert_eq!(
        bridge.create_plan(AcpCreatePlanRequest::default()).await,
        AcpCreatePlanOutcome::Cancelled
    );
}
