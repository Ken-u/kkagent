//! Disk-backed append log for background-shell output
//! (issues/bash_background_issues.md #6).
//!
//! `BackgroundShellManager`'s in-memory job output is bounded (a rolling
//! tail window, see `bash.rs::append_output`) so very chatty or very
//! long-running jobs don't grow unbounded RAM. That bound previously meant
//! output beyond the cap was lost outright. This module gives each
//! background job its own append-only file under
//! `<config_dir>/bg-shell-output/<session>/<job_id>.log` so:
//!
//! - polling can request only the bytes written since a previous
//!   `since_offset` (true incremental catch-up, not a head/tail guess);
//! - `/ps` and TaskOutput can read the full log, not just the memory window;
//! - a job's full output survives longer than the in-memory cap, even
//!   though it does not survive a process restart (the child process itself
//!   doesn't either — kkagent does not reattach to orphaned children).
//!
//! All operations are best-effort: a filesystem error never fails the
//! command itself, since the in-memory buffer remains the source of truth
//! for the "job succeeded/failed" outcome.

use std::path::{Path, PathBuf};
use std::sync::{Arc, RwLock};
use tokio::io::{AsyncReadExt, AsyncSeekExt, AsyncWriteExt};

/// Sanitize a path fragment (session id / job id) for use as a file or
/// directory name: keep alphanumerics/-/_ , collapse everything else.
fn sanitize_fragment(raw: &str) -> String {
    let mut out = String::with_capacity(raw.len());
    for ch in raw.chars() {
        if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "unknown".to_string()
    } else {
        out
    }
}

/// Test-only root override: when set, every [`OutputLog`] is created under
/// this directory instead of the user's real config dir, so `cargo test`
/// never writes into `~/.config`/`%APPDATA%`. Tests (including bash.rs's,
/// which produce output through the manager) call
/// [`set_root_for_tests`] with a `tempfile::TempDir` path.
static ROOT_OVERRIDE: RwLock<Option<Arc<Path>>> = RwLock::new(None);

/// Redirect the output-log root to `dir` (pass `None` to restore the real
/// config-dir location). Only meant for tests; see [`ROOT_OVERRIDE`].
pub fn set_root_for_tests(dir: Option<std::sync::Arc<Path>>) {
    *ROOT_OVERRIDE.write().expect("root override lock") = dir;
}

/// Root directory holding every session's background-shell output logs.
fn root_dir() -> PathBuf {
    match ROOT_OVERRIDE.read().expect("root override lock").clone() {
        Some(dir) => dir.to_path_buf(),
        None => kkagent_config::default_config_dir().join("bg-shell-output"),
    }
}

/// Handle to one job's append-only output file. Cheap to clone (just a
/// path); every operation opens the file fresh so no lock is held across
/// awaits.
#[derive(Debug, Clone)]
pub struct OutputLog {
    path: PathBuf,
}

impl OutputLog {
    pub fn new(session_id: &str, job_id: &str) -> Self {
        Self::with_root(&root_dir(), session_id, job_id)
    }

    /// Like [`OutputLog::new`] but under an explicit root directory.
    fn with_root(root: &Path, session_id: &str, job_id: &str) -> Self {
        let dir = root.join(sanitize_fragment(session_id));
        let path = dir.join(format!("{}.log", sanitize_fragment(job_id)));
        Self { path }
    }

    /// Absolute path of this job's append-only log file.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Append `chunk` to the log, creating the session directory and file
    /// on first write. Errors are swallowed (best-effort persistence).
    pub async fn append(&self, chunk: &str) {
        if chunk.is_empty() {
            return;
        }
        if let Some(dir) = self.path.parent() {
            if tokio::fs::create_dir_all(dir).await.is_err() {
                return;
            }
        }
        let file = tokio::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .await;
        if let Ok(mut file) = file {
            let _ = file.write_all(chunk.as_bytes()).await;
        }
    }

    /// Read every byte written after `since_offset`, plus the file's
    /// current total length (the offset the caller should pass next time).
    /// Returns `(new_content, total_len)`; both are `0`/empty when the log
    /// doesn't exist yet.
    pub async fn read_from(&self, since_offset: u64) -> (String, u64) {
        let (content, _next, total) = self.read_from_limited(since_offset, u64::MAX).await;
        (content, total)
    }

    /// Like [`read_from`], but returns at most `max_bytes` of new content
    /// starting at `since_offset`. `next_offset` advances only by the bytes
    /// actually returned (at a UTF-8 boundary), so callers can page through
    /// a large log without silently dropping the middle.
    ///
    /// Returns `(content, next_offset, total_len)`.
    pub async fn read_from_limited(&self, since_offset: u64, max_bytes: u64) -> (String, u64, u64) {
        let Ok(mut file) = tokio::fs::File::open(&self.path).await else {
            return (String::new(), since_offset, 0);
        };
        let total_len = match file.metadata().await {
            Ok(meta) => meta.len(),
            Err(_) => return (String::new(), since_offset, 0),
        };
        if since_offset >= total_len || max_bytes == 0 {
            return (String::new(), since_offset.max(total_len), total_len);
        }
        if file
            .seek(std::io::SeekFrom::Start(since_offset))
            .await
            .is_err()
        {
            return (String::new(), since_offset, total_len);
        }
        let remaining = (total_len - since_offset) as usize;
        let want = remaining.min(max_bytes as usize);
        // Read a little past the cap so we can back up to a char boundary.
        let mut buf = vec![0u8; want.saturating_add(4).min(remaining)];
        let n = match file.read(&mut buf).await {
            Ok(n) => n,
            Err(_) => return (String::new(), since_offset, total_len),
        };
        buf.truncate(n);
        if buf.is_empty() {
            return (String::new(), since_offset, total_len);
        }
        let take = {
            let mut end = want.min(buf.len());
            while end > 0 && !is_utf8_char_boundary(&buf, end) {
                end -= 1;
            }
            if end == 0 {
                buf.len()
            } else {
                end
            }
        };
        buf.truncate(take);
        let next_offset = since_offset + take as u64;
        (
            String::from_utf8_lossy(&buf).into_owned(),
            next_offset,
            total_len,
        )
    }

    /// Best-effort delete, called when the owning job is evicted from
    /// `BackgroundShellManager`'s history so disk usage doesn't grow
    /// forever. Also removes the now-empty session directory; a directory
    /// that still holds sibling jobs' logs is left alone.
    pub async fn remove(&self) {
        let _ = tokio::fs::remove_file(&self.path).await;
        if let Some(dir) = self.path.parent() {
            let _ = tokio::fs::remove_dir(dir).await;
        }
    }
}

fn is_utf8_char_boundary(buf: &[u8], index: usize) -> bool {
    index >= buf.len() || (buf[index] as i8) >= -0x40
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unique_log() -> (OutputLog, tempfile::TempDir) {
        let root = tempfile::tempdir().expect("tempdir");
        let log = OutputLog::with_root(
            root.path(),
            &format!("session-{}", uuid::Uuid::new_v4()),
            &format!("job-{}", uuid::Uuid::new_v4()),
        );
        (log, root)
    }

    #[tokio::test]
    async fn missing_log_reads_as_empty() {
        let (log, _root) = unique_log();
        let (content, total_len) = log.read_from(0).await;
        assert!(content.is_empty());
        assert_eq!(total_len, 0);
    }

    #[tokio::test]
    async fn append_then_read_from_offset_returns_only_new_bytes() {
        let (log, _root) = unique_log();
        log.append("hello ").await;
        log.append("world\n").await;

        let (all, len_after_first_read) = log.read_from(0).await;
        assert_eq!(all, "hello world\n");
        assert_eq!(len_after_first_read, all.len() as u64);

        log.append("more\n").await;
        let (delta, total_len) = log.read_from(len_after_first_read).await;
        assert_eq!(delta, "more\n");
        assert_eq!(total_len, "hello world\nmore\n".len() as u64);
    }

    #[tokio::test]
    async fn read_from_beyond_end_returns_empty_but_reports_length() {
        let (log, _root) = unique_log();
        log.append("abc").await;
        let (content, total_len) = log.read_from(100).await;
        assert!(content.is_empty());
        assert_eq!(total_len, 3);
    }

    #[tokio::test]
    async fn read_from_limited_pages_without_skipping_bytes() {
        let (log, _root) = unique_log();
        log.append("abcdefghijklmnopqrstuvwxyz\n").await;
        let (page1, next, total) = log.read_from_limited(0, 10).await;
        assert_eq!(page1, "abcdefghij");
        assert_eq!(next, 10);
        assert_eq!(total, 27);
        let (page2, next2, _) = log.read_from_limited(next, 10).await;
        assert_eq!(page2, "klmnopqrst");
        assert_eq!(next2, 20);
        let (page3, next3, _) = log.read_from_limited(next2, 10).await;
        assert_eq!(page3, "uvwxyz\n");
        assert_eq!(next3, 27);
    }

    #[tokio::test]
    async fn remove_is_idempotent_and_cleans_the_session_dir() {
        let (log, root) = unique_log();
        log.remove().await;
        log.append("x").await;
        assert!(log.path().is_file());
        log.remove().await;
        assert!(!log.path().exists());
        let (content, total_len) = log.read_from(0).await;
        assert!(content.is_empty());
        assert_eq!(total_len, 0);
        let session_dir = log.path().parent().expect("session dir");
        assert!(!session_dir.exists());
        assert!(root.path().read_dir().expect("root").next().is_none());
    }

    #[tokio::test]
    async fn remove_leaves_sibling_logs_intact() {
        let root = tempfile::tempdir().expect("tempdir");
        let session = "shared-session";
        let first = OutputLog::with_root(root.path(), session, "job-1");
        let second = OutputLog::with_root(root.path(), session, "job-2");
        first.append("one").await;
        second.append("two").await;
        first.remove().await;
        assert!(!first.path().exists());
        assert_eq!(second.read_from(0).await, ("two".to_string(), 3));
    }

    #[test]
    fn sanitizes_path_hostile_fragments() {
        assert_eq!(sanitize_fragment("../../etc/passwd"), "______etc_passwd");
        assert_eq!(sanitize_fragment(""), "unknown");
        assert_eq!(sanitize_fragment("normal-id_1"), "normal-id_1");
    }
}
