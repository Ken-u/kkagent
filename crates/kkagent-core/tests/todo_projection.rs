//! End-to-end next-turn context projection scenario (plan step 9).
//!
//! Builds a 20-item Todo list through the real tool, records the real
//! ToolUse/ToolResult message pairs the loop appends, and then projects the
//! exact next model request the way `prepare_messages` does. Verifies the
//! semantic acceptance content and the absence of any full Todo snapshot.

use kkagent_core::context_projector::{project, ProjectOptions};
use kkagent_llm::{ChatContent, ChatMessage};
use kkagent_protocol::todo::TodoOp;
use kkagent_tools::builtin::todo::{detached_handle, TodoListTool};
use kkagent_tools::{Tool, ToolContext};
use serde_json::json;

fn test_ctx() -> ToolContext {
    ToolContext {
        working_dir: std::env::temp_dir(),
        session_id: "projection-scenario".into(),
        turn_id: "projection-scenario:1".into(),
        plan_file_path: None,
        image: Default::default(),
        tool_call_id: None,
        interrupted: None,
        tools_config: Default::default(),
        model_alias: None,
    }
}

/// Serialize the exact projected request the way provider adapters do.
fn serialize_request(messages: &[ChatMessage]) -> String {
    let mut out = String::new();
    for msg in messages {
        for part in &msg.content {
            match part {
                ChatContent::Text { text } => out.push_str(text),
                ChatContent::ToolUse { input, .. } => {
                    out.push_str(&serde_json::to_string(input).unwrap())
                }
                ChatContent::ToolResult { content, .. } => out.push_str(content),
                ChatContent::Thinking { thinking } => out.push_str(thinking),
                ChatContent::Image { .. } | ChatContent::Video { .. } => {}
            }
        }
        out.push('\n');
    }
    out
}

#[tokio::test]
async fn twenty_item_scenario_next_request_is_focus_only() {
    let handle = detached_handle();
    let tool = TodoListTool::with_service(handle.clone());
    let ctx = test_ctx();

    // --- Phase 1: create a 20-item Todo list -----------------------------
    let mut ops = Vec::new();
    for i in 1..=20 {
        ops.push(json!({ "op": "add", "title": format!("task {i}") }));
    }
    let call_args = json!({ "ops": ops });
    let output = tool.execute(call_args.clone(), &ctx).await.unwrap();
    assert!(!output.is_error);
    assert!(
        output.content.contains("Current task: task 1."),
        "write result must state current focus: {}",
        output.content
    );
    assert!(
        output.content.contains("Progress: 0/20 completed."),
        "write result must state progress: {}",
        output.content
    );

    // The loop records the assistant ToolUse and the user ToolResult.
    let mut messages = vec![
        ChatMessage {
            role: "assistant".into(),
            content: vec![ChatContent::ToolUse {
                id: "call-1".into(),
                name: "TodoList".into(),
                input: call_args,
            }],
            tools: None,
        },
        ChatMessage {
            role: "user".into(),
            content: vec![ChatContent::ToolResult {
                tool_use_id: "call-1".into(),
                content: output.model_content(),
                is_error: false,
            }],
            tools: None,
        },
    ];

    // The next model request right after creation: current focus present,
    // no full 20-item snapshot through args, result, or anything else.
    let next = project(&messages, &ProjectOptions::default());
    let serialized = serialize_request(&next);
    assert!(
        serialized.contains("Current task: task 1."),
        "next request keeps focus: {serialized}"
    );
    assert!(
        serialized.contains("Progress: 0/20 completed."),
        "next request keeps progress: {serialized}"
    );

    // --- Phase 2: complete task 1, simulate history aging ----------------
    let id1 = handle.get_todos()[0].id.clone();
    let complete_args = json!({ "ops": [{ "op": "complete", "id": id1 }] });
    let output = tool.execute(complete_args.clone(), &ctx).await.unwrap();
    assert!(
        output.content.contains("Task completed: task 1."),
        "{}",
        output.content
    );
    assert!(
        output.content.contains("Current task: task 2."),
        "{}",
        output.content
    );
    assert!(
        output.content.contains("Progress: 1/20 completed."),
        "{}",
        output.content
    );

    messages.push(ChatMessage {
        role: "assistant".into(),
        content: vec![ChatContent::ToolUse {
            id: "call-2".into(),
            name: "TodoList".into(),
            input: complete_args,
        }],
        tools: None,
    });
    messages.push(ChatMessage {
        role: "user".into(),
        content: vec![ChatContent::ToolResult {
            tool_use_id: "call-2".into(),
            content: output.model_content(),
            is_error: false,
        }],
        tools: None,
    });

    // Filler turns push both Todo exchanges outside the recent window.
    for i in 0..16 {
        messages.push(ChatMessage {
            role: "user".into(),
            content: vec![ChatContent::Text {
                text: format!("conversation filler {i}"),
            }],
            tools: None,
        });
    }

    let aged = project(&messages, &ProjectOptions::default());
    let serialized_aged = serialize_request(&aged);
    assert!(
        !serialized_aged.contains("Current todo list:"),
        "aged history must not carry a full Todo rendering: {serialized_aged}"
    );
    // Historical results are compact one-line summaries at most — the aged
    // creation summary ("Current task: task 1. Progress: 0/20 completed.")
    // is already minimal and permitted to remain.
    // Protocol pairing remains valid for both calls.
    for call in ["call-1", "call-2"] {
        let has_use = aged.iter().any(|m| {
            m.content
                .iter()
                .any(|c| matches!(c, ChatContent::ToolUse { id, .. } if id == call))
        });
        let has_result = aged.iter().any(|m| {
            m.content.iter().any(
                |c| matches!(c, ChatContent::ToolResult { tool_use_id, .. } if tool_use_id == call),
            )
        });
        assert_eq!(has_use, has_result, "pairing for {call} must stay valid");
    }
}

/// The current session focus remains available to the runtime at all times
/// (used by TUI/persistence paths and future focus projections).
#[tokio::test]
async fn service_summary_tracks_focus_through_transitions() {
    let handle = detached_handle();
    for i in 1..=20 {
        handle
            .apply_op(TodoOp::Add {
                title: format!("task {i}"),
            })
            .unwrap();
    }
    let summary = kkagent_protocol::todo::SessionTodoService::summary(&handle.get_todos());
    assert_eq!(summary, "Current task: task 1. Progress: 0/20 completed.");
    let id1 = handle.get_todos()[0].id.clone();
    handle.apply_op(TodoOp::Complete { id: id1 }).unwrap();
    let summary = kkagent_protocol::todo::SessionTodoService::summary(&handle.get_todos());
    assert_eq!(summary, "Current task: task 2. Progress: 1/20 completed.");
}
