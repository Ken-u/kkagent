//! Tests of the ACP turn runner.
//!
//! Every scenario drives a whole `AgentLoop::run_turn` against the in-process
//! stub ACP agent (`acp_backend::testing`): a real `AcpClient`, hub and
//! interaction bridge, with a scripted agent on the other end of the pipe.

use super::*;
use crate::acp_backend::testing::{
    text_chunk, thought_chunk, tool_finished, tool_started, StubAgent, StubPlugin, StubStep,
    STUB_ALIAS,
};
use crate::acp_backend::{external_agents_in, AcpBackendHub};
use crate::permission::PermissionChain;
use kkagent_config::AppConfig;
use kkagent_protocol::QuestionResponse;
use kkagent_tools::ToolRegistry;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::sync::Mutex;

// ---------------------------------------------------------------------------
// Harness
// ---------------------------------------------------------------------------

type ApprovalFn = Box<dyn FnMut(&ApprovalRequest) -> ApprovalResponse>;
type QuestionFn = Box<dyn FnMut(&QuestionPayload) -> QuestionResponse>;
type EventFn = Box<dyn FnMut(&AgentEvent)>;

/// How the "user" reacts while a turn runs.
#[derive(Default)]
struct Answers {
    approval: Option<ApprovalFn>,
    question: Option<QuestionFn>,
    on_event: Option<EventFn>,
}

impl Answers {
    fn approvals(mut self, f: impl FnMut(&ApprovalRequest) -> ApprovalResponse + 'static) -> Self {
        self.approval = Some(Box::new(f));
        self
    }

    fn questions(mut self, f: impl FnMut(&QuestionPayload) -> QuestionResponse + 'static) -> Self {
        self.question = Some(Box::new(f));
        self
    }

    fn on_event(mut self, f: impl FnMut(&AgentEvent) + 'static) -> Self {
        self.on_event = Some(Box::new(f));
        self
    }
}

struct Rig {
    agent: Arc<StubAgent>,
    hub: Arc<AcpBackendHub>,
    loop_: AgentLoop,
    session: Session,
    events: Option<mpsc::Receiver<AgentEvent>>,
    workspace: PathBuf,
    _plugin: StubPlugin,
}

impl Rig {
    async fn new(mode: PermissionMode) -> Self {
        Self::build(mode, STUB_ALIAS, true).await
    }

    /// A rig whose permission chain carries `rules` (e.g. a `deny` rule).
    async fn with_rules(mode: PermissionMode, rules: Vec<kkagent_config::PermissionRule>) -> Self {
        Self::build_with_rules(mode, STUB_ALIAS, true, rules).await
    }

    async fn build(mode: PermissionMode, alias: &str, with_hub: bool) -> Self {
        Self::build_with_rules(mode, alias, with_hub, Vec::new()).await
    }

    async fn build_with_rules(
        mode: PermissionMode,
        alias: &str,
        with_hub: bool,
        rules: Vec<kkagent_config::PermissionRule>,
    ) -> Self {
        let plugin = StubPlugin::new().await;
        let agent = StubAgent::new();
        let hub = AcpBackendHub::with_connector(plugin.manager.clone(), agent.connector());
        let (event_tx, events) = mpsc::channel(8_192);
        let mut loop_ = AgentLoop::new(
            Arc::new(AppConfig::default()),
            Arc::new(ToolRegistry::new()),
            Arc::new(Mutex::new(PermissionChain::new(mode, rules))),
            event_tx,
            Arc::new(Mutex::new(HashMap::new())),
        );
        if with_hub {
            loop_ = loop_.with_acp_backends(hub.clone());
        }
        let workspace =
            std::env::temp_dir().join(format!("kkagent-acp-turn-{}", uuid::Uuid::new_v4()));
        std::fs::create_dir_all(&workspace).unwrap();
        let session = Session::for_subagent(
            format!("acp-turn-{}", uuid::Uuid::new_v4().simple()),
            workspace.clone(),
            mode,
            alias.into(),
        );
        Self {
            agent,
            hub,
            loop_,
            session,
            events: Some(events),
            workspace,
            _plugin: plugin,
        }
    }

    fn say(&mut self, text: &str) {
        self.session.add_user_message(text.into());
    }

    /// Run one `run_turn` while answering approvals / questions as scripted.
    /// Returns every event the turn produced.
    async fn run(&mut self, answers: Answers) -> Vec<AgentEvent> {
        let mut events = self.events.take().expect("event receiver");
        let approvals = self.session.approval_tx.clone();
        let questions = self.session.question_tx.clone();
        let loop_ = &self.loop_;
        let session = &mut self.session;
        let mut answers = answers;
        let (done_tx, mut done_rx) = tokio::sync::oneshot::channel::<()>();

        let turn = async {
            let result = loop_.run_turn(session).await;
            let _ = done_tx.send(());
            result
        };
        let observe = async {
            let mut seen: Vec<AgentEvent> = Vec::new();
            loop {
                tokio::select! {
                    event = events.recv() => {
                        let Some(event) = event else { break };
                        if let Some(on_event) = answers.on_event.as_mut() {
                            on_event(&event);
                        }
                        match &event {
                            AgentEvent::ApprovalRequested { request, .. } => {
                                let answer = match answers.approval.as_mut() {
                                    Some(f) => f(request),
                                    None => panic!("unexpected approval card: {request:?}"),
                                };
                                approvals.send(answer).await.unwrap();
                            }
                            AgentEvent::QuestionAsked { question, .. } => {
                                let answer = match answers.question.as_mut() {
                                    Some(f) => f(question),
                                    None => panic!("unexpected question card: {question:?}"),
                                };
                                questions.send(answer).await.unwrap();
                            }
                            _ => {}
                        }
                        seen.push(event);
                    }
                    _ = &mut done_rx => break,
                }
            }
            while let Ok(event) = events.try_recv() {
                seen.push(event);
            }
            seen
        };
        let (result, seen) = tokio::time::timeout(Duration::from_secs(30), async {
            tokio::join!(turn, observe)
        })
        .await
        .expect("the turn hung");
        result.expect("run_turn failed");
        self.events = Some(events);
        seen
    }

    async fn run_plain(&mut self) -> Vec<AgentEvent> {
        self.run(Answers::default()).await
    }
}

impl Drop for Rig {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.workspace);
    }
}

fn approve(request: &ApprovalRequest, scope: ApprovalScope) -> ApprovalResponse {
    ApprovalResponse {
        approval_id: request.approval_id.clone(),
        decision: ApprovalDecision::Approved,
        scope: Some(scope),
        feedback: None,
        selected_label: None,
    }
}

fn decide(request: &ApprovalRequest, decision: ApprovalDecision) -> ApprovalResponse {
    ApprovalResponse {
        approval_id: request.approval_id.clone(),
        decision,
        scope: None,
        feedback: None,
        selected_label: None,
    }
}

fn choose(question: &QuestionPayload, ids: &[&str]) -> QuestionResponse {
    QuestionResponse {
        question_id: question.question_id.clone(),
        selected_option_ids: ids.iter().map(|id| id.to_string()).collect(),
        free_text: None,
        cancelled: false,
    }
}

fn permission_step(id: &str, kind: &str, title: &str, raw_input: Value) -> StubStep {
    StubStep::Request {
        method: "session/request_permission",
        params: json!({
            "sessionId": "stub-session-1",
            "toolCall": {
                "toolCallId": id, "title": title, "kind": kind, "rawInput": raw_input
            },
            "options": [
                { "optionId": "allow-once", "name": "Allow once", "kind": "allow_once" },
                { "optionId": "allow-always", "name": "Allow always", "kind": "allow_always" },
                { "optionId": "reject-once", "name": "Reject", "kind": "reject_once" }
            ]
        }),
    }
}

fn plan_step() -> StubStep {
    StubStep::Request {
        method: "cursor/create_plan",
        params: json!({
            "toolCallId": "plan-1",
            "name": "Add tests",
            "overview": "Cover the parser",
            "plan": "1. Write the tests\n2. Run them",
            "todos": [ { "id": "1", "content": "Write the tests", "status": "pending" } ],
            "isProject": false
        }),
    }
}

fn user(text: &str) -> ChatMessage {
    ChatMessage {
        role: "user".into(),
        content: vec![ChatContent::Text { text: text.into() }],
        tools: None,
    }
}

fn assistant(text: &str) -> ChatMessage {
    ChatMessage {
        role: "assistant".into(),
        content: vec![ChatContent::Text { text: text.into() }],
        tools: None,
    }
}

fn text_of(message: &ChatMessage) -> String {
    message
        .content
        .iter()
        .filter_map(|part| match part {
            ChatContent::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join("\n")
}

fn roles(messages: &[ChatMessage]) -> Vec<&str> {
    messages.iter().map(|m| m.role.as_str()).collect()
}

fn has_tool_blocks(messages: &[ChatMessage]) -> bool {
    messages.iter().any(|message| {
        message.content.iter().any(|part| {
            matches!(
                part,
                ChatContent::ToolUse { .. } | ChatContent::ToolResult { .. }
            )
        })
    })
}

fn streamed_text(events: &[AgentEvent]) -> String {
    events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::MessageDelta { text, .. } => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

fn errors(events: &[AgentEvent]) -> Vec<String> {
    events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::Error { message, .. } => Some(message.clone()),
            _ => None,
        })
        .collect()
}

fn cards(events: &[AgentEvent]) -> Vec<&ApprovalRequest> {
    events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ApprovalRequested { request, .. } => Some(request),
            _ => None,
        })
        .collect()
}

fn tool_calls(events: &[AgentEvent]) -> Vec<(&str, &str, &Value)> {
    events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ToolCall {
                tool_call_id,
                tool_name,
                input,
                ..
            } => Some((tool_call_id.as_str(), tool_name.as_str(), input)),
            _ => None,
        })
        .collect()
}

fn tool_results(events: &[AgentEvent]) -> Vec<(&str, &str, &str, bool)> {
    events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::ToolResult {
                tool_call_id,
                tool_name,
                output,
                is_error,
                ..
            } => Some((
                tool_call_id.as_str(),
                tool_name.as_str(),
                output.as_str(),
                *is_error,
            )),
            _ => None,
        })
        .collect()
}

fn has_event(events: &[AgentEvent], pred: impl Fn(&AgentEvent) -> bool) -> bool {
    events.iter().any(pred)
}

/// `optionId` of a recorded `session/request_permission` answer.
fn selected_option(frame: &Value) -> String {
    frame["result"]["outcome"]["optionId"]
        .as_str()
        .unwrap_or("<none>")
        .to_string()
}

fn outcome(frame: &Value) -> &Value {
    &frame["result"]["outcome"]
}

fn summary_of(messages: &[ChatMessage]) -> String {
    messages
        .iter()
        .map(text_of)
        .find(|text| text.contains("[acp-activity"))
        .unwrap_or_default()
}

// ---------------------------------------------------------------------------
// Event mapping and transcript shape
// ---------------------------------------------------------------------------

#[tokio::test]
async fn plain_turn_streams_text_and_stores_a_text_only_transcript() {
    let mut rig = Rig::new(PermissionMode::Manual).await;
    rig.agent.script(vec![
        thought_chunk("hmm"),
        text_chunk("Hello"),
        text_chunk(" world"),
    ]);
    rig.say("hi");
    let events = rig.run_plain().await;

    assert_eq!(streamed_text(&events), "Hello world");
    assert!(has_event(&events, |e| matches!(
        e,
        AgentEvent::ThinkingDelta { text, .. } if text == "hmm"
    )));
    assert!(has_event(&events, |e| matches!(
        e,
        AgentEvent::TurnEnd { .. }
    )));
    assert!(
        has_event(&events, |e| matches!(
            e,
            AgentEvent::StatusUpdate {
                status: SessionStatus::Idle,
                ..
            }
        )),
        "the turn must end idle"
    );
    assert!(errors(&events).is_empty(), "{:?}", errors(&events));

    assert_eq!(rig.agent.prompt_texts(), vec!["hi".to_string()]);
    assert_eq!(roles(&rig.session.messages), ["user", "assistant"]);
    assert_eq!(text_of(&rig.session.messages[1]), "Hello world");
    assert!(!has_tool_blocks(&rig.session.messages));
}

#[tokio::test]
async fn tools_become_namespaced_cards_and_a_harness_only_summary() {
    let mut rig = Rig::new(PermissionMode::Auto).await;
    rig.agent.script(vec![
        text_chunk("Looking."),
        tool_started("t1", "execute", "`ls`", json!({ "command": "ls" })),
        tool_finished("t1", "completed", json!({ "stdout": "a\nb" })),
        // Announced with an empty input first: no card until it says more.
        StubStep::Update(json!({
            "sessionUpdate": "tool_call", "toolCallId": "t2", "title": "Edit a.rs",
            "kind": "edit", "status": "pending", "rawInput": {}
        })),
        StubStep::Update(json!({
            "sessionUpdate": "tool_call_update", "toolCallId": "t2",
            "status": "in_progress", "locations": [ { "path": "a.rs" } ]
        })),
        tool_finished("t2", "completed", json!("patched")),
        tool_started("t3", "read", "Read b.rs", json!({ "path": "b.rs" })),
        tool_finished(
            "t3",
            "failed",
            json!({ "stdout": "", "stderr": "no such file", "exitCode": 1 }),
        ),
        text_chunk("Done."),
    ]);
    rig.say("go");
    let events = rig.run_plain().await;
    assert!(errors(&events).is_empty(), "{:?}", errors(&events));

    let calls = tool_calls(&events);
    let ids: Vec<&str> = calls.iter().map(|c| c.0).collect();
    assert_eq!(ids, ["t1", "t2", "t3"], "one card per tool call");
    assert_eq!(calls[0].1, "stub/execute");
    assert_eq!(calls[0].2["command"], "ls");
    assert_eq!(calls[0].2["kind"], "execute");
    assert_eq!(calls[1].1, "stub/edit");
    assert_eq!(calls[1].2["path"], "a.rs");
    assert_eq!(calls[2].1, "stub/read");

    let results = tool_results(&events);
    assert_eq!(results.len(), 3);
    assert_eq!(results[0], ("t1", "stub/execute", "a\nb", false));
    assert_eq!(results[1], ("t2", "stub/edit", "patched", false));
    assert_eq!(results[2].0, "t3");
    assert!(results[2].3, "a failed call is an error result");
    assert!(results[2].2.contains("no such file"));

    // The user sees both text chunks as streamed ...
    assert_eq!(streamed_text(&events), "Looking.Done.");
    // ... the transcript keeps them apart where a card sat between them.
    let messages = &rig.session.messages;
    assert_eq!(roles(messages), ["user", "assistant", "user"]);
    assert_eq!(text_of(&messages[1]), "Looking.\n\nDone.");
    assert!(!has_tool_blocks(messages), "no ToolUse/ToolResult blocks");

    let summary = text_of(&messages[2]);
    assert!(summary.contains("[acp-activity agent=stub-plugin.stub name=stub]"));
    assert!(
        summary.contains("- execute · `ls` (completed)"),
        "{summary}"
    );
    assert!(
        summary.contains("- edit · Edit a.rs (completed) — a.rs"),
        "{summary}"
    );
    assert!(summary.contains("- read · Read b.rs (failed)"), "{summary}");
    assert!(kkagent_protocol::is_harness_only_user_text(&summary));
    assert_eq!(external_agents_in(messages), vec!["stub".to_string()]);

    // The summary is not a user prompt: undo / edit / fork never see it.
    assert_eq!(rig.session.user_turn_starts(), vec![0]);
    assert_eq!(crate::conversation_turns::editable_turns(messages).len(), 1);
}

#[tokio::test]
async fn a_turn_with_only_tools_keeps_the_user_assistant_alternation() {
    let mut rig = Rig::new(PermissionMode::Auto).await;
    rig.agent.script(vec![
        tool_started("t1", "execute", "`make`", json!({ "command": "make" })),
        tool_finished("t1", "completed", json!("built")),
    ]);
    rig.say("build it");
    let events = rig.run_plain().await;
    assert!(errors(&events).is_empty(), "{:?}", errors(&events));

    let messages = &rig.session.messages;
    assert_eq!(roles(messages), ["user", "assistant", "user"]);
    assert_eq!(text_of(&messages[1]), NO_TEXT_REPLY);
    assert!(summary_of(messages).contains("- execute · `make` (completed)"));
}

#[tokio::test]
async fn unfinished_tool_cards_are_closed_at_the_end_of_the_turn() {
    let mut rig = Rig::new(PermissionMode::Auto).await;
    rig.agent.script(vec![
        tool_started("t1", "execute", "`sleep`", json!({ "command": "sleep 1" })),
        text_chunk("done"),
    ]);
    rig.say("go");
    let events = rig.run_plain().await;

    let results = tool_results(&events);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].0, "t1");
    assert_eq!(results[0].2, "[no result reported]");
    assert!(!results[0].3, "a clean end is not an error");
}

#[tokio::test]
async fn todos_update_the_panel_and_the_session() {
    let mut rig = Rig::new(PermissionMode::Auto).await;
    rig.agent.script(vec![
        StubStep::Request {
            method: "cursor/update_todos",
            params: json!({
                "toolCallId": "td-1", "merge": false,
                "todos": [
                    { "id": "1", "content": "Read the code", "status": "in_progress" },
                    { "id": "2", "content": "Fix the bug", "status": "pending" }
                ]
            }),
        },
        StubStep::Request {
            method: "cursor/update_todos",
            params: json!({
                "toolCallId": "td-2", "merge": true,
                "todos": [ { "id": "1", "content": "Read the code", "status": "completed" } ]
            }),
        },
        text_chunk("ok"),
    ]);
    rig.say("go");
    let events = rig.run_plain().await;

    let lists: Vec<&Vec<TodoItemEvent>> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::TodoUpdated { items, .. } => Some(items),
            _ => None,
        })
        .collect();
    assert_eq!(lists.len(), 2);
    assert_eq!(lists[0].len(), 2);
    assert_eq!(lists[0][0].status, "in_progress");
    assert_eq!(lists[1][0].status, "completed");
    assert_eq!(lists[1][1].content, "Fix the bug");
    assert_eq!(rig.session.todo_items().len(), 2);
}

// ---------------------------------------------------------------------------
// Permissions
// ---------------------------------------------------------------------------

#[tokio::test]
async fn manual_mode_asks_and_remembers_a_session_approval_per_tool_kind() {
    let mut rig = Rig::new(PermissionMode::Manual).await;
    rig.agent.script(vec![
        permission_step(
            "p1",
            "execute",
            "`cargo test`",
            json!({ "command": "cargo test" }),
        ),
        tool_finished("p1", "completed", json!("ok")),
        // Same kind: covered by the session approval, no second card.
        permission_step(
            "p2",
            "execute",
            "`cargo build`",
            json!({ "command": "cargo build" }),
        ),
        tool_finished("p2", "completed", json!("ok")),
        // Another kind: asked again.
        permission_step("p3", "edit", "Edit a.rs", json!({ "path": "a.rs" })),
        tool_finished("p3", "failed", json!("rejected")),
        text_chunk("done"),
    ]);
    rig.say("go");
    let asked = Arc::new(AtomicUsize::new(0));
    let counter = asked.clone();
    let events = rig
        .run(Answers::default().approvals(move |request| {
            if counter.fetch_add(1, Ordering::SeqCst) == 0 {
                approve(request, ApprovalScope::Session)
            } else {
                decide(request, ApprovalDecision::Rejected)
            }
        }))
        .await;

    let shown = cards(&events);
    assert_eq!(shown.len(), 2, "{shown:?}");
    assert_eq!(shown[0].tool_name, "stub/execute");
    assert!(
        shown[0].action.contains("cargo test"),
        "{}",
        shown[0].action
    );
    assert_eq!(shown[0].tool_call_id, "p1");
    assert_eq!(shown[1].tool_name, "stub/edit");

    let replies = rig.agent.reply_values();
    let picked: Vec<String> = replies.iter().map(selected_option).collect();
    assert_eq!(picked, ["allow-once", "allow-once", "reject-once"]);
    // Cursor keeps no memory of the approval: kkagent owns the scope.
    assert!(!picked.iter().any(|id| id == "allow-always"));
}

#[tokio::test]
async fn a_refused_tool_is_an_error_card_although_the_agent_calls_it_completed() {
    let mut rig = Rig::new(PermissionMode::Manual).await;
    rig.agent.script(vec![
        permission_step(
            "p1",
            "execute",
            "`rm -rf build`",
            json!({ "command": "rm -rf build" }),
        ),
        // What the real Cursor CLI sends for a refused call: plain
        // `completed`, no output.
        StubStep::Update(json!({
            "sessionUpdate": "tool_call_update", "toolCallId": "p1", "status": "completed"
        })),
        text_chunk("REFUSED"),
    ]);
    rig.say("clean up");
    let events = rig
        .run(Answers::default().approvals(|request| ApprovalResponse {
            feedback: Some("not now".into()),
            ..decide(request, ApprovalDecision::Rejected)
        }))
        .await;

    let results = tool_results(&events);
    assert_eq!(results.len(), 1, "{results:?}");
    assert_eq!(results[0].0, "p1");
    assert_eq!(results[0].2, "[rejected: not now]");
    assert!(results[0].3, "a refused call must not look like a success");
    // Models that take the session over must not believe the command ran.
    let summary = summary_of(&rig.session.messages);
    assert!(summary.contains("(rejected)"), "{summary}");
    assert!(!summary.contains("(completed)"), "{summary}");
}

#[tokio::test]
async fn a_tool_denied_by_policy_is_an_error_card_too() {
    // A `deny` rule refuses the call without asking: no card, but the call is
    // marked refused all the same.
    let mut rig = Rig::with_rules(
        PermissionMode::Manual,
        vec![kkagent_config::PermissionRule {
            decision: "deny".into(),
            pattern: "stub/execute".into(),
            scope: None,
        }],
    )
    .await;
    rig.agent.script(vec![
        permission_step("p1", "execute", "`ls`", json!({ "command": "ls" })),
        StubStep::Update(json!({
            "sessionUpdate": "tool_call_update", "toolCallId": "p1", "status": "completed"
        })),
        text_chunk("ok"),
    ]);
    rig.say("list it");
    let events = rig.run(Answers::default()).await;

    assert!(cards(&events).is_empty(), "{:?}", cards(&events));
    let results = tool_results(&events);
    assert_eq!(results.len(), 1, "{results:?}");
    assert!(results[0].3, "{results:?}");
    assert!(results[0].2.starts_with("[denied"), "{results:?}");
    assert!(summary_of(&rig.session.messages).contains("(rejected)"));
    let picked: Vec<String> = rig
        .agent
        .reply_values()
        .iter()
        .map(selected_option)
        .collect();
    assert_eq!(picked, ["reject-once"]);
}

#[tokio::test]
async fn always_scope_is_forwarded_and_silences_later_requests() {
    let mut rig = Rig::new(PermissionMode::Manual).await;
    rig.agent.script(vec![
        permission_step("p1", "execute", "`ls`", json!({ "command": "ls" })),
        permission_step("p2", "execute", "`pwd`", json!({ "command": "pwd" })),
        text_chunk("done"),
    ]);
    rig.say("go");
    let events = rig
        .run(Answers::default().approvals(|request| approve(request, ApprovalScope::Always)))
        .await;

    assert_eq!(cards(&events).len(), 1);
    let picked: Vec<String> = rig
        .agent
        .reply_values()
        .iter()
        .map(selected_option)
        .collect();
    assert_eq!(picked, ["allow-always", "allow-once"]);
}

#[tokio::test]
async fn yolo_mode_only_asks_for_sensitive_commands() {
    let mut rig = Rig::new(PermissionMode::Yolo).await;
    rig.agent.script(vec![
        permission_step(
            "p1",
            "execute",
            "`cargo test`",
            json!({ "command": "cargo test" }),
        ),
        permission_step(
            "p2",
            "execute",
            "`cat .env`",
            json!({ "command": "cat .env" }),
        ),
        permission_step("p3", "edit", "Edit a.rs", json!({ "path": "a.rs" })),
        text_chunk("done"),
    ]);
    rig.say("go");
    let events = rig
        .run(Answers::default().approvals(|request| approve(request, ApprovalScope::Once)))
        .await;

    let shown = cards(&events);
    assert_eq!(
        shown.len(),
        1,
        "only the secret read needs a card: {shown:?}"
    );
    assert!(shown[0].action.contains(".env"), "{}", shown[0].action);
    let picked: Vec<String> = rig
        .agent
        .reply_values()
        .iter()
        .map(selected_option)
        .collect();
    assert_eq!(picked, ["allow-once", "allow-once", "allow-once"]);
}

#[tokio::test]
async fn auto_mode_never_asks() {
    let mut rig = Rig::new(PermissionMode::Auto).await;
    rig.agent.script(vec![
        permission_step(
            "p1",
            "execute",
            "`cat .env`",
            json!({ "command": "cat .env" }),
        ),
        permission_step("p2", "delete", "Delete a.rs", json!({ "path": "a.rs" })),
        text_chunk("done"),
    ]);
    rig.say("go");
    let events = rig.run_plain().await;

    assert!(cards(&events).is_empty());
    let picked: Vec<String> = rig
        .agent
        .reply_values()
        .iter()
        .map(selected_option)
        .collect();
    assert_eq!(picked, ["allow-once", "allow-once"]);
}

#[tokio::test]
async fn cancelling_an_approval_card_stops_the_turn() {
    let mut rig = Rig::new(PermissionMode::Manual).await;
    rig.agent.script(vec![
        permission_step(
            "p1",
            "execute",
            "`rm -rf x`",
            json!({ "command": "rm -rf x" }),
        ),
        StubStep::WaitCancel,
    ]);
    rig.say("go");
    let events = rig
        .run(Answers::default().approvals(|request| decide(request, ApprovalDecision::Cancelled)))
        .await;

    let reply = &rig.agent.reply_values()[0];
    assert_eq!(outcome(reply)["outcome"], "cancelled");
    assert_eq!(rig.agent.cancels.load(Ordering::SeqCst), 1);
    assert!(errors(&events).iter().any(|m| m == "Interrupted"));
    assert!(has_event(&events, |e| matches!(
        e,
        AgentEvent::TurnEnd { .. }
    )));
}

#[tokio::test]
async fn ask_question_cards_are_answered_one_by_one_and_merged() {
    let mut rig = Rig::new(PermissionMode::Manual).await;
    rig.agent.script(vec![
        StubStep::Request {
            method: "cursor/ask_question",
            params: json!({
                "toolCallId": "ask-1", "title": "Setup",
                "questions": [
                    { "id": "db", "prompt": "Which database?", "allowMultiple": false,
                      "options": [ { "id": "pg", "label": "Postgres" },
                                   { "id": "sqlite", "label": "SQLite" } ] },
                    { "id": "feat", "prompt": "Which features?", "allowMultiple": true,
                      "options": [ { "id": "a", "label": "Auth" },
                                   { "id": "b", "label": "Billing" },
                                   { "id": "c", "label": "Search" } ] }
                ]
            }),
        },
        text_chunk("noted"),
    ]);
    rig.say("go");
    let events = rig
        .run(Answers::default().questions(|question| {
            if question.text.contains("database") {
                choose(question, &["sqlite"])
            } else {
                choose(question, &["a", "c"])
            }
        }))
        .await;

    let asked: Vec<&QuestionPayload> = events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::QuestionAsked { question, .. } => Some(question),
            _ => None,
        })
        .collect();
    assert_eq!(asked.len(), 2);
    assert!(
        asked[0].text.starts_with("[1/2] Setup"),
        "{}",
        asked[0].text
    );
    assert!(asked[0].text.contains("Which database?"));
    assert!(!asked[0].allow_multiple);
    assert!(asked[1].allow_multiple);
    assert_eq!(asked[1].options.len(), 3);

    let reply = &rig.agent.reply_values()[0];
    assert_eq!(outcome(reply)["outcome"], "answered");
    assert_eq!(
        outcome(reply)["answers"],
        json!([
            { "questionId": "db", "selectedOptionIds": ["sqlite"] },
            { "questionId": "feat", "selectedOptionIds": ["a", "c"] }
        ])
    );
    let summary = summary_of(&rig.session.messages);
    assert!(
        summary.contains("Which database? → SQLite; Which features? → Auth, Search"),
        "{summary}"
    );
}

#[tokio::test]
async fn a_dismissed_or_free_text_question_is_reported_as_skipped() {
    let mut rig = Rig::new(PermissionMode::Manual).await;
    rig.agent.script(vec![
        StubStep::Request {
            method: "cursor/ask_question",
            params: json!({
                "toolCallId": "ask-1",
                "questions": [ { "id": "db", "prompt": "Which database?",
                    "options": [ { "id": "pg", "label": "Postgres" } ] } ]
            }),
        },
        StubStep::Request {
            method: "cursor/ask_question",
            params: json!({
                "toolCallId": "ask-2",
                "questions": [ { "id": "name", "prompt": "Name it?",
                    "options": [ { "id": "x", "label": "X" } ] } ]
            }),
        },
        text_chunk("ok"),
    ]);
    rig.say("go");
    let calls = Arc::new(AtomicUsize::new(0));
    let counter = calls.clone();
    rig.run(Answers::default().questions(move |question| {
        if counter.fetch_add(1, Ordering::SeqCst) == 0 {
            QuestionResponse {
                question_id: question.question_id.clone(),
                selected_option_ids: Vec::new(),
                free_text: None,
                cancelled: true,
            }
        } else {
            QuestionResponse {
                question_id: question.question_id.clone(),
                selected_option_ids: Vec::new(),
                free_text: Some("call it zed".into()),
                cancelled: false,
            }
        }
    }))
    .await;

    let replies = rig.agent.reply_values();
    assert_eq!(outcome(&replies[0])["outcome"], "skipped");
    assert_eq!(outcome(&replies[1])["outcome"], "skipped");
    assert!(outcome(&replies[1])["reason"]
        .as_str()
        .unwrap()
        .contains("call it zed"));
}

// ---------------------------------------------------------------------------
// Plans
// ---------------------------------------------------------------------------

#[tokio::test]
async fn accepting_a_plan_saves_it_and_leaves_plan_mode() {
    let mut rig = Rig::new(PermissionMode::Manual).await;
    rig.session.set_plan_mode_persisted(true).unwrap();
    rig.agent
        .script(vec![plan_step(), text_chunk("Plan is ready.")]);
    rig.say("plan the tests");
    let events = rig
        .run(Answers::default().approvals(|request| {
            assert_eq!(request.tool_name, "ExitPlanMode");
            assert_eq!(
                request.tool_input_display.as_ref().unwrap()["kind"],
                "plan_review"
            );
            ApprovalResponse {
                selected_label: Some(crate::plan_review::LABEL_EXECUTE.into()),
                ..approve(request, ApprovalScope::Once)
            }
        }))
        .await;

    let saved = events
        .iter()
        .find_map(|event| match event {
            AgentEvent::PlanFileUpdated { path, content, .. } => {
                Some((path.clone(), content.clone()))
            }
            _ => None,
        })
        .expect("plan file event");
    assert!(saved.1.contains("# Add tests"), "{}", saved.1);
    assert!(saved.1.contains("Write the tests"));
    assert!(
        std::fs::read_to_string(&saved.0)
            .unwrap()
            .contains("Run them"),
        "the plan is on disk"
    );

    let reply = &rig.agent.reply_values()[0];
    assert_eq!(outcome(reply)["outcome"], "accepted");
    assert!(outcome(reply)["planUri"]
        .as_str()
        .unwrap()
        .starts_with("file:"));

    assert!(
        !rig.session.plan_mode,
        "accepting the plan leaves plan mode"
    );
    assert!(has_event(&events, |e| matches!(
        e,
        AgentEvent::PlanModeChanged { enabled: false, .. }
    )));
    // kkagent's plan mode was mirrored onto the agent for this turn.
    assert_eq!(rig.agent.modes.lock().unwrap()[0].1, "plan");
    let summary = summary_of(&rig.session.messages);
    assert!(
        summary.contains("create_plan · Plan: Add tests (accepted)"),
        "{summary}"
    );
}

#[tokio::test]
async fn rejecting_a_plan_sends_the_feedback_and_stays_in_plan_mode() {
    let mut rig = Rig::new(PermissionMode::Manual).await;
    rig.session.set_plan_mode_persisted(true).unwrap();
    rig.agent.script(vec![plan_step(), text_chunk("Revising.")]);
    rig.say("plan the tests");
    rig.run(Answers::default().approvals(|request| ApprovalResponse {
        feedback: Some("use sqlite instead".into()),
        selected_label: Some(crate::plan_review::LABEL_REVISE.into()),
        ..decide(request, ApprovalDecision::Rejected)
    }))
    .await;

    let reply = &rig.agent.reply_values()[0];
    assert_eq!(outcome(reply)["outcome"], "rejected");
    assert!(outcome(reply)["reason"]
        .as_str()
        .unwrap()
        .contains("use sqlite instead"));
    assert!(rig.session.plan_mode);
    assert!(summary_of(&rig.session.messages).contains("(rejected)"));
}

#[tokio::test]
async fn auto_mode_accepts_a_plan_without_a_card() {
    let mut rig = Rig::new(PermissionMode::Auto).await;
    rig.session.set_plan_mode_persisted(true).unwrap();
    rig.agent.script(vec![plan_step(), text_chunk("Building.")]);
    rig.say("plan the tests");
    let events = rig.run_plain().await;

    assert!(cards(&events).is_empty());
    let reply = &rig.agent.reply_values()[0];
    assert_eq!(outcome(reply)["outcome"], "accepted");
    assert!(!rig.session.plan_mode);
}

// ---------------------------------------------------------------------------
// Interrupts and failures
// ---------------------------------------------------------------------------

#[tokio::test]
async fn esc_cancels_the_prompt_keeps_the_partial_answer_and_the_session() {
    let mut rig = Rig::new(PermissionMode::Auto).await;
    rig.agent
        .script(vec![text_chunk("partial"), StubStep::WaitCancel]);
    rig.say("start");
    let flag = rig.session.interrupted.clone();
    let events = rig
        .run(Answers::default().on_event(move |event| {
            if matches!(event, AgentEvent::MessageDelta { text, .. } if text == "partial") {
                flag.store(true, Ordering::SeqCst);
            }
        }))
        .await;

    assert_eq!(rig.agent.cancels.load(Ordering::SeqCst), 1);
    assert!(errors(&events).iter().any(|m| m == "Interrupted"));
    assert!(
        !errors(&events).iter().any(|m| m.contains("cancelled")),
        "the stopReason notice is redundant after Esc: {:?}",
        errors(&events)
    );
    assert_eq!(roles(&rig.session.messages), ["user", "assistant"]);
    assert_eq!(text_of(&rig.session.messages[1]), "partial");

    // The agent process is still healthy and keeps its session: the next
    // turn only carries the new message.
    rig.session.clear_interrupt();
    rig.say("go on");
    rig.run_plain().await;
    assert_eq!(
        rig.agent.prompt_texts(),
        vec!["start".to_string(), "go on".to_string()]
    );
    assert_eq!(rig.agent.spawns.load(Ordering::SeqCst), 1);
    assert_eq!(rig.agent.sessions.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn an_agent_that_ignores_cancel_is_killed_and_restarted_next_turn() {
    let mut rig = Rig::new(PermissionMode::Auto).await;
    rig.agent.script(vec![
        text_chunk("working"),
        tool_started("t1", "execute", "`sleep`", json!({ "command": "sleep 99" })),
        StubStep::Hang(30_000),
    ]);
    rig.say("start");
    let flag = rig.session.interrupted.clone();
    let events = rig
        .run(Answers::default().on_event(move |event| {
            if matches!(event, AgentEvent::ToolCall { .. }) {
                flag.store(true, Ordering::SeqCst);
            }
        }))
        .await;

    assert!(errors(&events).iter().any(|m| m == "Interrupted"));
    // The stuck tool card is closed as interrupted, not left spinning.
    let results = tool_results(&events);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].2, "[interrupted]");
    assert!(results[0].3);

    rig.session.clear_interrupt();
    rig.say("again");
    rig.run_plain().await;
    assert_eq!(rig.agent.spawns.load(Ordering::SeqCst), 2, "a new process");
    let texts = rig.agent.prompt_texts();
    assert!(
        texts[1].contains("<kkagent-context>") && texts[1].contains("start"),
        "the restarted agent is told what happened: {}",
        texts[1]
    );
}

#[tokio::test]
async fn a_failing_prompt_reports_the_error_and_the_message_stays_unanswered() {
    let mut rig = Rig::new(PermissionMode::Auto).await;
    rig.agent.script(vec![StubStep::Fail("boom")]);
    rig.say("q1");
    let events = rig.run_plain().await;

    let reported = errors(&events);
    assert!(
        reported.iter().any(|m| m.contains("boom")),
        "the agent's message reaches the user: {reported:?}"
    );
    assert!(has_event(&events, |e| matches!(
        e,
        AgentEvent::TurnEnd { .. }
    )));
    assert_eq!(
        roles(&rig.session.messages),
        ["user"],
        "nothing was answered"
    );

    // Retrying (or asking again) sends everything still unanswered to a
    // fresh agent process.
    rig.say("q2");
    rig.run_plain().await;
    assert_eq!(rig.agent.spawns.load(Ordering::SeqCst), 2);
    assert_eq!(rig.agent.prompt_texts()[1], "q1\n\nq2");
    assert_eq!(roles(&rig.session.messages), ["user", "user", "assistant"]);
}

#[tokio::test]
async fn a_crash_keeps_the_partial_answer_and_starts_a_new_process_next_turn() {
    let mut rig = Rig::new(PermissionMode::Auto).await;
    rig.agent.script(vec![text_chunk("par"), StubStep::Crash]);
    rig.say("q1");
    let events = rig.run_plain().await;

    assert!(!errors(&events).is_empty(), "the crash is reported");
    assert_eq!(roles(&rig.session.messages), ["user", "assistant"]);
    assert_eq!(text_of(&rig.session.messages[1]), "par");

    rig.say("q2");
    rig.run_plain().await;
    assert_eq!(rig.agent.spawns.load(Ordering::SeqCst), 2);
    let second = &rig.agent.prompt_texts()[1];
    assert!(
        second.contains("<kkagent-context>") && second.contains("par"),
        "{second}"
    );
    assert!(second.ends_with("q2"));
}

#[tokio::test]
async fn a_crash_after_tools_marks_the_unfinished_work_in_the_transcript() {
    let mut rig = Rig::new(PermissionMode::Auto).await;
    rig.agent.script(vec![
        tool_started("t1", "edit", "Edit a.rs", json!({ "path": "a.rs" })),
        StubStep::Crash,
    ]);
    rig.say("q1");
    let events = rig.run_plain().await;

    let results = tool_results(&events);
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].2, "[interrupted]");
    assert!(results[0].3);
    // Tools ran, so the message counts as answered (they may have changed
    // files) — with a note that the agent never finished.
    let messages = &rig.session.messages;
    assert_eq!(roles(messages), ["user", "assistant", "user"]);
    assert_eq!(text_of(&messages[1]), NO_TEXT_FAILED);
    assert!(summary_of(messages).contains("- edit · Edit a.rs"));
}

// ---------------------------------------------------------------------------
// Steering
// ---------------------------------------------------------------------------

#[tokio::test]
async fn steer_input_is_queued_and_sent_after_the_running_prompt() {
    let mut rig = Rig::new(PermissionMode::Auto).await;
    rig.agent
        .script(vec![text_chunk("first"), StubStep::Sleep(500)]);
    rig.agent.script(vec![text_chunk("second")]);
    rig.say("start");
    let mailbox = rig.session.steer_mailbox.clone();
    let pushed = Arc::new(AtomicUsize::new(0));
    let counter = pushed.clone();
    let events = rig
        .run(Answers::default().on_event(move |event| {
            if matches!(event, AgentEvent::MessageDelta { text, .. } if text == "first")
                && counter.fetch_add(1, Ordering::SeqCst) == 0
            {
                mailbox
                    .try_push(crate::session::SteerInput {
                        text: "also add logging".into(),
                        images: Vec::new(),
                    })
                    .expect("the running turn accepts steer input");
            }
        }))
        .await;

    // Cursor would cancel the running prompt if a second one arrived: the
    // steer waits for the first prompt to finish, then goes out on its own.
    assert_eq!(
        rig.agent.prompt_texts(),
        vec!["start".to_string(), "also add logging".to_string()]
    );
    assert_eq!(rig.agent.cancels.load(Ordering::SeqCst), 0);
    assert_eq!(streamed_text(&events), "firstsecond");
    assert_eq!(
        roles(&rig.session.messages),
        ["user", "assistant", "user", "assistant"]
    );
    assert_eq!(text_of(&rig.session.messages[2]), "also add logging");
    assert_eq!(rig.agent.spawns.load(Ordering::SeqCst), 1);
}

// ---------------------------------------------------------------------------
// Session continuity and takeover
// ---------------------------------------------------------------------------

#[tokio::test]
async fn follow_up_turns_only_carry_the_new_message() {
    let mut rig = Rig::new(PermissionMode::Auto).await;
    rig.agent.script(vec![
        tool_started("t1", "execute", "`ls`", json!({ "command": "ls" })),
        tool_finished("t1", "completed", json!("a")),
        text_chunk("listed"),
    ]);
    rig.say("q1");
    rig.run_plain().await;
    // The summary message sits between the turns: it must not force a rebuild.
    rig.say("q2");
    rig.run_plain().await;
    rig.say("q3");
    rig.run_plain().await;

    assert_eq!(
        rig.agent.prompt_texts(),
        vec!["q1".to_string(), "q2".to_string(), "q3".to_string()]
    );
    assert_eq!(rig.agent.spawns.load(Ordering::SeqCst), 1);
    assert_eq!(rig.agent.sessions.load(Ordering::SeqCst), 1);
    let sessions = rig.agent.prompt_sessions();
    assert!(sessions.windows(2).all(|pair| pair[0] == pair[1]));
}

#[tokio::test]
async fn messages_added_by_other_models_reach_the_agent_as_a_preface() {
    let mut rig = Rig::new(PermissionMode::Auto).await;
    rig.say("q1");
    rig.run_plain().await;
    // A turn another model took over in between.
    rig.session.messages.push(user("q2"));
    rig.session.messages.push(assistant("Switch to sqlite."));
    rig.say("q3");
    rig.run_plain().await;

    let texts = rig.agent.prompt_texts();
    assert!(texts[1].contains("<kkagent-context>"), "{}", texts[1]);
    assert!(texts[1].contains("q2") && texts[1].contains("Switch to sqlite."));
    assert!(
        !texts[1].contains("q1"),
        "what the agent already saw is not repeated: {}",
        texts[1]
    );
    assert!(texts[1].ends_with("q3"));
    assert_eq!(rig.agent.sessions.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn undoing_a_turn_rebuilds_the_agent_session_from_the_transcript() {
    let mut rig = Rig::new(PermissionMode::Auto).await;
    rig.say("q1");
    rig.run_plain().await;
    rig.say("q2");
    rig.run_plain().await;
    assert_eq!(rig.agent.sessions.load(Ordering::SeqCst), 1);

    // /undo: the transcript loses the second turn behind the agent's back.
    rig.session.messages.truncate(2);
    rig.say("q3");
    rig.run_plain().await;

    assert_eq!(
        rig.agent.sessions.load(Ordering::SeqCst),
        2,
        "the stale ACP session is replaced"
    );
    let texts = rig.agent.prompt_texts();
    assert!(texts[2].contains("<kkagent-context>") && texts[2].contains("q1"));
    assert!(
        !texts[2].contains("q2"),
        "the undone turn is gone: {}",
        texts[2]
    );
    assert!(texts[2].ends_with("q3"));
    // One process the whole time.
    assert_eq!(rig.agent.spawns.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn compacting_the_transcript_rebuilds_the_agent_session() {
    let mut rig = Rig::new(PermissionMode::Auto).await;
    rig.say("q1");
    rig.run_plain().await;
    rig.say("q2");
    rig.run_plain().await;

    // /compact replaced the history with a summary the agent never saw.
    rig.session.messages = vec![
        user("<conversation-summary>q1 and q2 were about parsers</conversation-summary>"),
        assistant("Understood."),
    ];
    rig.say("q3");
    rig.run_plain().await;

    assert_eq!(rig.agent.sessions.load(Ordering::SeqCst), 2);
    let texts = rig.agent.prompt_texts();
    assert!(texts[2].contains("<kkagent-context>"), "{}", texts[2]);
    assert!(
        texts[2].contains("q1 and q2 were about parsers"),
        "{}",
        texts[2]
    );
    assert!(texts[2].ends_with("q3"));
}

#[tokio::test]
async fn external_agent_transcripts_are_valid_history_for_regular_models() {
    let mut rig = Rig::new(PermissionMode::Auto).await;
    rig.agent.script(vec![
        tool_started("t1", "edit", "Edit a.rs", json!({ "path": "a.rs" })),
        tool_finished("t1", "completed", json!("patched")),
        text_chunk("Edited a.rs."),
    ]);
    rig.say("fix a.rs");
    rig.run_plain().await;
    // Another model takes the session over.
    rig.session.messages.push(user("now add a test"));

    let projected = crate::context_projector::project(
        &rig.session.messages,
        &crate::context_projector::ProjectOptions::default(),
    );
    assert_eq!(roles(&projected), ["user", "assistant", "user", "user"]);
    assert!(
        !has_tool_blocks(&projected),
        "nothing a provider could reject"
    );
    assert_eq!(text_of(&projected[1]), "Edited a.rs.");
    // The summary tells the next model what happened without pretending it
    // has the tool results.
    let summary = text_of(&projected[2]);
    assert!(summary.contains("ran these tools") && summary.contains("Edit a.rs"));
    assert!(summary.contains("<system-reminder>"));
}

#[tokio::test]
async fn switching_back_to_the_agent_after_another_model_starts_from_the_transcript() {
    let mut rig = Rig::new(PermissionMode::Auto).await;
    rig.say("q1");
    rig.run_plain().await;
    rig.hub.close_session(&rig.session.id).await;
    rig.session.messages.push(user("q2"));
    rig.session
        .messages
        .push(assistant("answer from another model"));
    rig.say("q3");
    rig.run_plain().await;

    assert_eq!(rig.agent.spawns.load(Ordering::SeqCst), 2);
    let texts = rig.agent.prompt_texts();
    assert!(texts[1].contains("q1") && texts[1].contains("answer from another model"));
    assert!(texts[1].ends_with("q3"));
}

#[tokio::test]
async fn plan_mode_is_mirrored_onto_the_agent_mode() {
    let mut rig = Rig::new(PermissionMode::Manual).await;
    rig.session.set_plan_mode_persisted(true).unwrap();
    rig.say("plan it");
    rig.run_plain().await;
    rig.session.set_plan_mode_persisted(false).unwrap();
    rig.say("build it");
    rig.run_plain().await;

    let modes: Vec<String> = rig
        .agent
        .modes
        .lock()
        .unwrap()
        .iter()
        .map(|(_, mode)| mode.clone())
        .collect();
    assert_eq!(modes, ["plan", "agent"]);
}

// ---------------------------------------------------------------------------
// Unavailable backends
// ---------------------------------------------------------------------------

#[tokio::test]
async fn an_agent_that_no_plugin_declares_fails_with_a_readable_error() {
    let mut rig = Rig::build(PermissionMode::Auto, "acp:stub-plugin.missing", true).await;
    rig.say("hi");
    let events = rig.run_plain().await;

    let reported = errors(&events);
    assert!(
        reported
            .iter()
            .any(|m| m.contains("not available") && m.contains("/model")),
        "{reported:?}"
    );
    assert!(has_event(&events, |e| matches!(
        e,
        AgentEvent::TurnEnd { .. }
    )));
    assert_eq!(roles(&rig.session.messages), ["user"]);
    assert_eq!(rig.agent.spawns.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn a_loop_without_a_backend_hub_fails_the_turn_instead_of_hanging() {
    let mut rig = Rig::build(PermissionMode::Auto, STUB_ALIAS, false).await;
    rig.say("hi");
    let events = rig.run_plain().await;

    assert!(errors(&events)
        .iter()
        .any(|m| m.contains("not available in this context")));
    assert_eq!(roles(&rig.session.messages), ["user"]);
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

#[test]
fn an_empty_announcement_never_wipes_a_known_tool_input() {
    let mut run = RunState::new("s", "stub");
    let first = run.upsert_tool(ToolUpdate {
        id: "t1",
        title: Some("Run"),
        kind: Some("execute"),
        status: Some("pending"),
        raw_input: Some(&json!({})),
        ..ToolUpdate::default()
    });
    assert!(
        !ready_to_start(&run.tools[first]),
        "an empty pending call is not ready"
    );
    run.upsert_tool(ToolUpdate {
        id: "t1",
        raw_input: Some(&json!({ "command": "ls" })),
        ..ToolUpdate::default()
    });
    run.upsert_tool(ToolUpdate {
        id: "t1",
        raw_input: Some(&json!({})),
        ..ToolUpdate::default()
    });
    assert_eq!(run.tools[first].raw_input, Some(json!({ "command": "ls" })));
    assert!(ready_to_start(&run.tools[first]));
    assert_eq!(run.tools.len(), 1);
}

#[test]
fn permission_input_exposes_command_and_the_most_sensitive_path() {
    let request = AcpPermissionRequest {
        title: "Run a thing".into(),
        tool_kind: Some("execute".into()),
        raw_input: Some(json!({ "command": "cat notes.txt" })),
        locations: vec!["notes.txt".into(), "/home/me/.ssh/id_rsa".into()],
        ..AcpPermissionRequest::default()
    };
    let input = permission_input(&request, &ToolTrack::default());
    assert_eq!(input["command"], "cat notes.txt");
    assert_eq!(input["path"], "/home/me/.ssh/id_rsa");
    assert_eq!(input["kind"], "execute");

    // Without a command the title of an execute call stands in for it.
    let request = AcpPermissionRequest {
        title: "`make all`".into(),
        tool_kind: Some("execute".into()),
        ..AcpPermissionRequest::default()
    };
    assert_eq!(
        permission_input(&request, &ToolTrack::default())["command"],
        "`make all`"
    );
    assert_eq!(
        permission_action("stub/execute", &json!({ "command": "ls" })),
        "stub/execute  ls"
    );
}

#[test]
fn plan_markdown_adds_a_title_and_the_todos() {
    let request = AcpCreatePlanRequest {
        name: Some("Add tests".into()),
        overview: Some("Cover the parser".into()),
        plan: "1. Write\n2. Run".into(),
        todos: vec![kkagent_acp::AcpPlanTodo {
            id: "1".into(),
            content: "Write".into(),
            status: "completed".into(),
        }],
        ..AcpCreatePlanRequest::default()
    };
    let markdown = plan_markdown(&request, "Add tests");
    assert!(markdown.starts_with("# Add tests\n\nCover the parser\n\n1. Write"));
    assert!(markdown.contains("- [x] Write"));
    assert!(markdown.ends_with('\n'));

    // A plan that brings its own title keeps it.
    let request = AcpCreatePlanRequest {
        plan: "# Own title\n\nbody".into(),
        ..AcpCreatePlanRequest::default()
    };
    assert!(plan_markdown(&request, "Ignored").starts_with("# Own title"));
}

#[test]
fn todo_statuses_are_normalized() {
    assert_eq!(normalize_todo_status("In-Progress"), "in_progress");
    assert_eq!(normalize_todo_status("done"), "completed");
    assert_eq!(normalize_todo_status("canceled"), "cancelled");
    assert_eq!(normalize_todo_status("whatever"), "pending");
}

#[test]
fn permission_options_fall_back_to_the_same_polarity() {
    let request = AcpPermissionRequest {
        options: vec![
            kkagent_acp::AcpPermissionOption {
                option_id: "yes".into(),
                name: "Yes".into(),
                kind: "allow_once".into(),
            },
            kkagent_acp::AcpPermissionOption {
                option_id: "no".into(),
                name: "No".into(),
                kind: "reject_once".into(),
            },
        ],
        ..AcpPermissionRequest::default()
    };
    assert_eq!(
        select_permission_option(&request, AcpPermissionChoice::AllowAlways),
        AcpPermissionOutcome::Selected("yes".into())
    );
    assert_eq!(
        select_permission_option(&request, AcpPermissionChoice::RejectOnce),
        AcpPermissionOutcome::Selected("no".into())
    );
    assert_eq!(
        select_permission_option(
            &AcpPermissionRequest::default(),
            AcpPermissionChoice::AllowOnce
        ),
        AcpPermissionOutcome::Cancelled
    );
}

#[test]
fn stop_reasons_are_explained_unless_the_user_caused_them() {
    assert!(stop_reason_notice("end_turn", "cursor", false).is_none());
    assert!(stop_reason_notice("max_tokens", "cursor", false)
        .unwrap()
        .contains("output limit"));
    assert!(stop_reason_notice("refusal", "cursor", false)
        .unwrap()
        .contains("refused"));
    assert!(stop_reason_notice("cancelled", "cursor", false).is_some());
    assert!(stop_reason_notice("cancelled", "cursor", true).is_none());
}

#[test]
fn acp_backed_sessions_are_recognized_by_their_model_alias() {
    let workspace = std::env::temp_dir();
    let acp = Session::for_subagent(
        format!("alias-{}", uuid::Uuid::new_v4().simple()),
        workspace.clone(),
        PermissionMode::Auto,
        STUB_ALIAS.into(),
    );
    let regular = Session::for_subagent(
        format!("alias-{}", uuid::Uuid::new_v4().simple()),
        workspace,
        PermissionMode::Auto,
        "local/claude".into(),
    );
    assert!(super::super::acp_backed_session(&acp));
    assert!(!super::super::acp_backed_session(&regular));
}

// ---------------------------------------------------------------------------
// Thinking level (`acp:<id>#<level>`) and the reported agent state
// ---------------------------------------------------------------------------

/// `(agent, model, thought_level, thought_levels, note)` of every
/// `AcpAgentState` event.
type AgentState = (
    String,
    Option<String>,
    Option<String>,
    Vec<String>,
    Option<String>,
);

fn agent_states(events: &[AgentEvent]) -> Vec<AgentState> {
    events
        .iter()
        .filter_map(|event| match event {
            AgentEvent::AcpAgentState {
                agent,
                model,
                thought_level,
                thought_levels,
                note,
                ..
            } => Some((
                agent.clone(),
                model.clone(),
                thought_level.clone(),
                thought_levels.clone(),
                note.clone(),
            )),
            _ => None,
        })
        .collect()
}

const CURSOR_LEVELS: [&str; 5] = ["low", "medium", "high", "xhigh", "max"];

fn level_names(levels: &[&str]) -> Vec<String> {
    levels.iter().map(|level| level.to_string()).collect()
}

#[tokio::test]
async fn the_alias_level_is_applied_to_the_agent_and_reported() {
    let mut rig = Rig::build(PermissionMode::Auto, "acp:stub-plugin.stub#max", true).await;
    rig.agent.offer_thought_levels(&CURSOR_LEVELS, "high");
    rig.say("hi");
    let events = rig.run_plain().await;
    assert!(errors(&events).is_empty(), "{:?}", errors(&events));

    assert_eq!(
        rig.agent.config_call_values(),
        vec![("reasoning_effort".to_string(), "max".to_string())]
    );
    assert_eq!(
        agent_states(&events),
        vec![(
            "stub".to_string(),
            Some("stub-model".to_string()),
            Some("max".to_string()),
            level_names(&CURSOR_LEVELS),
            None
        )]
    );
    // The prompt itself is untouched.
    assert_eq!(rig.agent.prompt_texts(), vec!["hi".to_string()]);
}

#[tokio::test]
async fn without_a_level_the_agent_keeps_its_own_and_is_only_reported() {
    let mut rig = Rig::new(PermissionMode::Auto).await;
    rig.agent.offer_thought_levels(&CURSOR_LEVELS, "high");
    rig.say("hi");
    let events = rig.run_plain().await;

    assert!(rig.agent.config_call_values().is_empty());
    assert_eq!(
        agent_states(&events),
        vec![(
            "stub".to_string(),
            Some("stub-model".to_string()),
            Some("high".to_string()),
            level_names(&CURSOR_LEVELS),
            None
        )]
    );

    // An agent without any config options reports no model or level.
    let mut bare = Rig::new(PermissionMode::Auto).await;
    bare.say("hi");
    let events = bare.run_plain().await;
    assert_eq!(
        agent_states(&events),
        vec![("stub".to_string(), None, None, Vec::new(), None)]
    );
}

#[tokio::test]
async fn a_level_the_agent_lacks_is_replaced_by_the_closest_and_explained() {
    let mut rig = Rig::build(PermissionMode::Auto, "acp:stub-plugin.stub#minimal", true).await;
    rig.agent.offer_thought_levels(&CURSOR_LEVELS, "high");
    rig.say("hi");
    let events = rig.run_plain().await;

    let states = agent_states(&events);
    assert_eq!(states.len(), 1);
    assert_eq!(states[0].2.as_deref(), Some("low"));
    let note = states[0].4.as_deref().expect("a note");
    assert!(note.contains("no thinking level 'minimal'"), "{note}");
    assert_eq!(rig.agent.thought_level().as_deref(), Some("low"));
}

#[tokio::test]
async fn an_agent_without_a_thinking_option_hears_about_it_once() {
    let mut rig = Rig::build(PermissionMode::Auto, "acp:stub-plugin.stub#max", true).await;
    rig.say("one");
    let first = agent_states(&rig.run_plain().await);
    rig.say("two");
    let second = agent_states(&rig.run_plain().await);

    let note = first[0].4.as_deref().expect("a note on the first turn");
    assert!(note.contains("does not let clients choose"), "{note}");
    assert_eq!(second[0].4, None, "the same request is not repeated");
    assert!(rig.agent.config_call_values().is_empty());
}

#[tokio::test]
async fn changing_the_alias_level_between_turns_switches_the_running_agent() {
    let mut rig = Rig::build(PermissionMode::Auto, "acp:stub-plugin.stub#max", true).await;
    rig.agent.offer_thought_levels(&CURSOR_LEVELS, "high");
    rig.say("one");
    rig.run_plain().await;
    assert_eq!(rig.agent.thought_level().as_deref(), Some("max"));

    // /effort low: the next turn asks for it on the same ACP session.
    rig.session.set_model_alias("acp:stub-plugin.stub#low");
    rig.say("two");
    let events = rig.run_plain().await;
    assert_eq!(rig.agent.thought_level().as_deref(), Some("low"));
    assert_eq!(
        rig.agent.config_call_values(),
        vec![
            ("reasoning_effort".to_string(), "max".to_string()),
            ("reasoning_effort".to_string(), "low".to_string())
        ]
    );
    assert_eq!(agent_states(&events)[0].2.as_deref(), Some("low"));
    assert_eq!(rig.agent.spawns.load(Ordering::SeqCst), 1);
    assert_eq!(rig.agent.sessions.load(Ordering::SeqCst), 1);

    // Dropping the level leaves the agent where it is.
    rig.session.set_model_alias(STUB_ALIAS);
    rig.say("three");
    let events = rig.run_plain().await;
    assert_eq!(rig.agent.config_call_values().len(), 2);
    assert_eq!(agent_states(&events)[0].2.as_deref(), Some("low"));
}

#[tokio::test]
async fn a_refused_level_is_reported_but_never_fails_the_turn() {
    let mut rig = Rig::build(PermissionMode::Auto, "acp:stub-plugin.stub#max", true).await;
    rig.agent.offer_thought_levels(&CURSOR_LEVELS, "high");
    rig.agent.reject_config_changes();
    rig.agent.script(vec![text_chunk("still answered")]);
    rig.say("hi");
    let events = rig.run_plain().await;

    assert_eq!(streamed_text(&events), "still answered");
    let states = agent_states(&events);
    assert_eq!(states[0].2.as_deref(), Some("high"));
    let note = states[0].4.as_deref().expect("a note");
    assert!(note.contains("did not take thinking level 'max'"), "{note}");
    assert!(errors(&events).is_empty(), "{:?}", errors(&events));
}

#[tokio::test]
async fn plan_mode_is_set_on_a_session_rebuilt_for_the_same_turn() {
    let mut rig = Rig::new(PermissionMode::Auto).await;
    rig.say("q1");
    rig.run_plain().await;
    rig.say("q2");
    rig.run_plain().await;

    // /undo behind the agent's back plus plan mode: the turn starts a new
    // ACP session, which must be switched to the agent's plan mode too.
    rig.session.messages.truncate(2);
    rig.session.set_plan_mode_persisted(true).unwrap();
    rig.say("q3");
    rig.run_plain().await;

    assert_eq!(rig.agent.sessions.load(Ordering::SeqCst), 2);
    let modes = rig.agent.modes.lock().unwrap().clone();
    assert_eq!(
        modes.last(),
        Some(&("stub-session-2".to_string(), "plan".to_string())),
        "{modes:?}"
    );
}
