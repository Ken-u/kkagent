//! Session todo types moved to `kkagent-protocol` so the tools crate can
//! share the authoritative service without a dependency cycle.

pub use kkagent_protocol::todo::{
    render_todo_list, SessionTodoService, TodoItem, TodoOp, TodoOpResult, TodoStatus,
};
