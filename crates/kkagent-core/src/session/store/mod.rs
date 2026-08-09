//! Disk-backed session store (kimi-code `session/store`).

mod index;
#[allow(clippy::module_inception)]
mod store;
mod workdir_key;

pub use index::{
    append_session_index_deletion, append_session_index_entry, read_session_index,
    session_index_path, SessionIndexEntry,
};
pub use store::{SessionStore, SessionSummary};
pub use workdir_key::{
    encode_work_dir_key, is_safe_session_id, normalize_work_dir, workspace_root_key,
};
