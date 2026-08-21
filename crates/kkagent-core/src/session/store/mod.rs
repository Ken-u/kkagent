//! Disk-backed session store (kimi-code `session/store`).

mod index;
#[allow(clippy::module_inception)]
mod store;
mod workdir_key;

use std::sync::atomic::{AtomicU64, Ordering};

static LIST_GENERATION: AtomicU64 = AtomicU64::new(1);

pub(crate) fn invalidate_list_cache() {
    LIST_GENERATION.fetch_add(1, Ordering::AcqRel);
}

pub(crate) fn list_generation() -> u64 {
    LIST_GENERATION.load(Ordering::Acquire)
}

pub use index::{
    append_session_index_deletion, append_session_index_entry, read_session_index,
    session_index_path, SessionIndexEntry,
};
pub use store::{SessionStore, SessionSummary};
pub use workdir_key::{
    encode_work_dir_key, is_safe_session_id, normalize_work_dir, workspace_root_key,
};
