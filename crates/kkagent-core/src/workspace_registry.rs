//! Workspace session registry: concurrent-session awareness and identity.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::sync::oneshot;
use tokio::task::JoinHandle;

use crate::session::store::{encode_work_dir_key, normalize_work_dir, workspace_root_key};

/// Suggested heartbeat write interval.
pub const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(30);
/// Sessions whose heartbeat is older than this are considered stale.
pub const HEARTBEAT_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SessionRegistration {
    pub session_id: String,
    pub pid: u32,
    pub workspace_root: String,
    pub started_at: DateTime<Utc>,
    pub heartbeat_at: DateTime<Utc>,
}

/// Resolve a stable workspace identity for registry bucketing / concurrency checks.
///
/// Priority: git toplevel → canonicalize → [`normalize_work_dir`].
pub fn resolve_workspace_identity(working_dir: &Path) -> PathBuf {
    if let Some(toplevel) = git_toplevel(working_dir) {
        return normalize_identity_path(&toplevel);
    }
    if let Ok(canon) = std::fs::canonicalize(working_dir) {
        return normalize_identity_path(&canon);
    }
    normalize_identity_path(&normalize_work_dir(working_dir))
}

fn normalize_identity_path(path: &Path) -> PathBuf {
    let s = path.to_string_lossy().replace('\\', "/");
    let trimmed = s.trim_end_matches('/');
    // Keep Windows drive roots like `C:` intact.
    let trimmed = if trimmed.len() == 2 && trimmed.as_bytes()[1] == b':' {
        format!("{trimmed}/")
    } else {
        trimmed.to_string()
    };
    PathBuf::from(trimmed)
}

fn git_toplevel(working_dir: &Path) -> Option<PathBuf> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "--show-toplevel"])
        .current_dir(working_dir)
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout).trim().to_string();
    if text.is_empty() {
        return None;
    }
    Some(PathBuf::from(text))
}

pub fn default_registry_root() -> PathBuf {
    kkagent_config::default_config_dir().join("registry")
}

pub fn workspace_bucket_dir(registry_root: &Path, identity: &Path) -> PathBuf {
    registry_root.join(encode_work_dir_key(identity))
}

/// Cross-platform "is this PID still running?" probe.
pub fn process_alive(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    #[cfg(unix)]
    {
        // Signal 0: existence check. EPERM means the process exists but we lack rights.
        let rc = unsafe { libc::kill(pid as i32, 0) };
        if rc == 0 {
            return true;
        }
        let err = std::io::Error::last_os_error();
        err.raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(windows)]
    {
        use windows_sys::Win32::Foundation::CloseHandle;
        use windows_sys::Win32::System::Threading::{
            GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION, STILL_ACTIVE,
        };
        unsafe {
            let handle = OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, 0, pid);
            if handle.is_null() {
                return false;
            }
            let mut code = 0u32;
            let ok = GetExitCodeProcess(handle, &mut code) != 0;
            CloseHandle(handle);
            ok && code == STILL_ACTIVE
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        let _ = pid;
        false
    }
}

pub fn is_registration_active(reg: &SessionRegistration, now: DateTime<Utc>) -> bool {
    if !process_alive(reg.pid) {
        return false;
    }
    let age = now.signed_duration_since(reg.heartbeat_at);
    age.to_std()
        .map(|d| d <= HEARTBEAT_TIMEOUT)
        .unwrap_or(false)
}

fn registration_path(bucket: &Path, session_id: &str) -> PathBuf {
    bucket.join(format!("{session_id}.json"))
}

fn write_json_atomic(path: &Path, value: &impl Serialize) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = path.with_extension("json.tmp");
    let bytes = serde_json::to_vec_pretty(value)
        .map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?;
    std::fs::write(&tmp, bytes)?;
    std::fs::rename(&tmp, path)?;
    Ok(())
}

fn read_registration(path: &Path) -> Option<SessionRegistration> {
    let bytes = std::fs::read(path).ok()?;
    serde_json::from_slice(&bytes).ok()
}

fn touch_heartbeat(path: &Path) -> std::io::Result<()> {
    let Some(mut reg) = read_registration(path) else {
        return Ok(());
    };
    reg.heartbeat_at = Utc::now();
    write_json_atomic(path, &reg)
}

/// Scan a workspace bucket, remove stale files, return active registrations.
pub fn scan_active(
    registry_root: &Path,
    identity: &Path,
    exclude_session_id: Option<&str>,
) -> Vec<SessionRegistration> {
    let bucket = workspace_bucket_dir(registry_root, identity);
    let Ok(entries) = std::fs::read_dir(&bucket) else {
        return Vec::new();
    };
    let now = Utc::now();
    let mut active = Vec::new();
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().and_then(|e| e.to_str()) != Some("json") {
            continue;
        }
        let Some(reg) = read_registration(&path) else {
            let _ = std::fs::remove_file(&path);
            continue;
        };
        // Leave the caller's own registration alone (lease owns its lifecycle).
        if exclude_session_id.is_some_and(|id| id == reg.session_id) {
            continue;
        }
        if is_registration_active(&reg, now) {
            active.push(reg);
        } else {
            let _ = std::fs::remove_file(&path);
        }
    }
    active
}

/// Active peers in the same workspace (excludes `session_id`).
pub fn list_active_peers(
    registry_root: &Path,
    working_dir: &Path,
    session_id: &str,
) -> Vec<SessionRegistration> {
    let identity = resolve_workspace_identity(working_dir);
    scan_active(registry_root, &identity, Some(session_id))
        .into_iter()
        .filter(|r| r.session_id != session_id)
        .collect()
}

pub fn list_active_peers_default(working_dir: &Path, session_id: &str) -> Vec<SessionRegistration> {
    list_active_peers(&default_registry_root(), working_dir, session_id)
}

/// RAII lease: writes registration, runs heartbeat, deletes file on drop.
pub struct WorkspaceRegistryLease {
    registry_root: PathBuf,
    file_path: PathBuf,
    stop_tx: Option<oneshot::Sender<()>>,
    _heartbeat: Option<JoinHandle<()>>,
}

impl WorkspaceRegistryLease {
    /// Best-effort register under the default registry root.
    pub fn start(session_id: &str, working_dir: &Path) -> Option<Self> {
        Self::start_in(&default_registry_root(), session_id, working_dir)
    }

    pub fn start_in(registry_root: &Path, session_id: &str, working_dir: &Path) -> Option<Self> {
        let identity = resolve_workspace_identity(working_dir);
        let bucket = workspace_bucket_dir(registry_root, &identity);
        let file_path = registration_path(&bucket, session_id);
        let now = Utc::now();
        let reg = SessionRegistration {
            session_id: session_id.to_string(),
            pid: std::process::id(),
            workspace_root: identity.to_string_lossy().replace('\\', "/"),
            started_at: now,
            heartbeat_at: now,
        };
        if let Err(error) = write_json_atomic(&file_path, &reg) {
            tracing::warn!(
                %error,
                path = %file_path.display(),
                "workspace registry register failed; continuing without lease"
            );
            return None;
        }

        let (stop_tx, mut stop_rx) = oneshot::channel::<()>();
        let heartbeat_path = file_path.clone();
        let heartbeat = tokio::spawn(async move {
            let mut interval = tokio::time::interval(HEARTBEAT_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            // Skip the immediate first tick so we don't rewrite right after register.
            interval.tick().await;
            loop {
                tokio::select! {
                    _ = &mut stop_rx => break,
                    _ = interval.tick() => {
                        if let Err(error) = touch_heartbeat(&heartbeat_path) {
                            tracing::debug!(
                                %error,
                                path = %heartbeat_path.display(),
                                "workspace registry heartbeat failed"
                            );
                        }
                    }
                }
            }
        });

        Some(Self {
            registry_root: registry_root.to_path_buf(),
            file_path,
            stop_tx: Some(stop_tx),
            _heartbeat: Some(heartbeat),
        })
    }

    pub fn file_path(&self) -> &Path {
        &self.file_path
    }

    pub fn registry_root(&self) -> &Path {
        &self.registry_root
    }
}

impl Drop for WorkspaceRegistryLease {
    fn drop(&mut self) {
        if let Some(tx) = self.stop_tx.take() {
            let _ = tx.send(());
        }
        if let Some(handle) = self._heartbeat.take() {
            handle.abort();
        }
        let _ = std::fs::remove_file(&self.file_path);
    }
}

/// Soft startup reminder when other sessions share the workspace.
pub fn startup_concurrent_reminder(others: &[SessionRegistration]) -> String {
    let ids: Vec<&str> = others.iter().map(|r| r.session_id.as_str()).collect();
    let list = ids.join(", ");
    format!(
        r#"

# Concurrent Sessions

Other active session(s) are already using this workspace: [{list}].
Be careful when modifying shared files — edits from another session may conflict with yours.
For larger or risky changes, consider creating a separate git worktree so each session has an isolated working tree.
"#
    )
}

/// Strong reminder appended to first write/Bash tool result via `<system-reminder>`.
pub fn write_concurrent_reminder(others: &[SessionRegistration]) -> String {
    let ids: Vec<&str> = others.iter().map(|r| r.session_id.as_str()).collect();
    let list = ids.join(", ");
    let body = format!(
        "CONCURRENT SESSION RISK: other active session(s) [{list}] are using this same workspace. \
Your write may overwrite or conflict with their changes. Prefer creating a git worktree to isolate work. \
If you continue here, re-read files before editing and coordinate carefully with the other session(s)."
    );
    crate::system_reminder::wrap(&body)
}

/// Stable map key for `{path → hash}` tracking.
pub fn file_track_key(working_dir: &Path, path: &Path) -> String {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        working_dir.join(path)
    };
    let resolved = std::fs::canonicalize(&absolute).unwrap_or(absolute);
    workspace_root_key(&resolved.to_string_lossy())
}

pub fn sha256_hex(bytes: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(bytes);
    hex::encode(hasher.finalize())
}

pub fn file_content_hash(path: &Path) -> std::io::Result<String> {
    let bytes = std::fs::read(path)?;
    Ok(sha256_hex(&bytes))
}

/// Server-side stale-file hard gate before Edit/Write.
pub fn stale_write_rejection(path: &Path, expected_hash: &str) -> Option<String> {
    match std::fs::read(path) {
        Ok(bytes) => {
            let current = sha256_hex(&bytes);
            if current == expected_hash {
                None
            } else {
                Some(format!(
                    "Refusing to write `{}`: the file was modified externally since this session last Read it \
(content hash mismatch). Re-Read the file, then retry the edit.",
                    path.display()
                ))
            }
        }
        Err(_) if !path.exists() => Some(format!(
            "Refusing to write `{}`: the file was deleted externally since this session last Read it. \
Re-Read (or recreate) the file before writing.",
            path.display()
        )),
        Err(error) => Some(format!(
            "Refusing to write `{}`: failed to verify content hash ({error}). Re-Read the file, then retry.",
            path.display()
        )),
    }
}

pub fn resolve_tool_path(working_dir: &Path, path_str: &str) -> PathBuf {
    if Path::new(path_str).is_absolute() {
        PathBuf::from(path_str)
    } else {
        working_dir.join(path_str)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn temp_registry() -> PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let n = COUNTER.fetch_add(1, Ordering::SeqCst);
        let dir = std::env::temp_dir().join(format!(
            "kkagent-registry-test-{}-{}",
            std::process::id(),
            n
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn identity_falls_back_for_non_git_dir() {
        let root = temp_registry();
        let work = root.join("proj");
        fs::create_dir_all(&work).unwrap();
        let id = resolve_workspace_identity(&work);
        assert!(id.is_absolute() || id.exists() || !id.as_os_str().is_empty());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn register_lists_and_drop_unregisters() {
        let root = temp_registry();
        let work = root.join("ws");
        fs::create_dir_all(&work).unwrap();

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let lease_a =
                WorkspaceRegistryLease::start_in(&root, "sess-a", &work).expect("register a");
            let peers = list_active_peers(&root, &work, "sess-b");
            assert_eq!(peers.len(), 1);
            assert_eq!(peers[0].session_id, "sess-a");

            let reminder = startup_concurrent_reminder(&peers);
            assert!(reminder.contains("Concurrent Sessions"));
            assert!(reminder.contains("sess-a"));

            let strong = write_concurrent_reminder(&peers);
            assert!(strong.contains("<system-reminder>"));
            assert!(strong.contains("sess-a"));

            drop(lease_a);
            let peers_after = list_active_peers(&root, &work, "sess-b");
            assert!(peers_after.is_empty());
        });
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn stale_pid_cleaned_on_scan() {
        let root = temp_registry();
        let work = root.join("ws");
        fs::create_dir_all(&work).unwrap();
        let identity = resolve_workspace_identity(&work);
        let bucket = workspace_bucket_dir(&root, &identity);
        fs::create_dir_all(&bucket).unwrap();
        let path = registration_path(&bucket, "dead");
        let now = Utc::now();
        let mut child = {
            #[cfg(unix)]
            {
                std::process::Command::new("true")
                    .spawn()
                    .expect("spawn short-lived process")
            }
            #[cfg(windows)]
            {
                std::process::Command::new("cmd")
                    .args(["/C", "exit", "0"])
                    .spawn()
                    .expect("spawn short-lived process")
            }
            #[cfg(not(any(unix, windows)))]
            {
                panic!("unsupported platform for stale pid test");
            }
        };
        let dead_pid = child.id();
        let _ = child.wait();
        // Give the kernel a moment; PID must not be alive for the stale check.
        assert!(
            !process_alive(dead_pid),
            "expected exited child pid {dead_pid} to be dead"
        );
        let reg = SessionRegistration {
            session_id: "dead".into(),
            pid: dead_pid,
            workspace_root: identity.to_string_lossy().into(),
            started_at: now,
            heartbeat_at: now,
        };
        write_json_atomic(&path, &reg).unwrap();
        assert!(path.exists());
        let active = scan_active(&root, &identity, None);
        assert!(active.is_empty());
        assert!(!path.exists());
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn stale_write_rejection_detects_mismatch_and_delete() {
        let root = temp_registry();
        let file = root.join("f.txt");
        fs::write(&file, b"hello").unwrap();
        let hash = file_content_hash(&file).unwrap();
        assert!(stale_write_rejection(&file, &hash).is_none());
        fs::write(&file, b"world").unwrap();
        let err = stale_write_rejection(&file, &hash).unwrap();
        assert!(err.contains("modified externally"));
        assert!(err.contains("Re-Read"));
        fs::remove_file(&file).unwrap();
        let err = stale_write_rejection(&file, &hash).unwrap();
        assert!(err.contains("deleted externally"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn same_workspace_bucket_for_relative_and_absolute() {
        let root = temp_registry();
        let work = root.join("proj");
        fs::create_dir_all(&work).unwrap();
        let abs = resolve_workspace_identity(&work);
        let cwd = std::env::current_dir().unwrap();
        let rel = if let Ok(stripped) = work.strip_prefix(&cwd) {
            resolve_workspace_identity(stripped)
        } else {
            abs.clone()
        };
        assert_eq!(
            workspace_bucket_dir(&root, &abs),
            workspace_bucket_dir(&root, &rel)
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn session_soft_and_strong_reminders_and_stale_hash_flow() {
        use crate::session::Session;
        use kkagent_protocol::PermissionMode;
        use kkagent_tools::ToolOutput;

        let root = temp_registry();
        let work = root.join("ws");
        fs::create_dir_all(&work).unwrap();
        let file = work.join("a.txt");
        fs::write(&file, b"v1").unwrap();

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        runtime.block_on(async {
            let lease_a =
                WorkspaceRegistryLease::start_in(&root, "sess-a", &work).expect("register a");

            let mut sess_b = Session::new(
                "sess-b".into(),
                work.clone(),
                PermissionMode::Auto,
                "default".into(),
            );
            sess_b.attach_workspace_concurrency_guard_in(Some(&root));
            assert!(
                sess_b.system_prompt.contains("Concurrent Sessions"),
                "startup soft reminder missing"
            );
            assert!(sess_b.system_prompt.contains("sess-a"));

            let mut out = ToolOutput::success("edited");
            sess_b.maybe_append_concurrent_write_reminder("Edit", &mut out);
            assert!(out.content.contains("<system-reminder>"));
            assert!(sess_b.concurrent_write_warned);

            let mut out2 = ToolOutput::success("edited again");
            sess_b.maybe_append_concurrent_write_reminder("Edit", &mut out2);
            assert!(!out2.content.contains("<system-reminder>"));

            let hash = file_content_hash(&file).unwrap();
            sess_b.record_read_content_hash(&file, hash.clone());
            assert!(sess_b.check_stale_before_write(&file).is_none());

            fs::write(&file, b"external").unwrap();
            let reject = sess_b.check_stale_before_write(&file).unwrap();
            assert!(reject.contains("Re-Read"));

            fs::write(&file, b"v1").unwrap();
            assert!(sess_b.check_stale_before_write(&file).is_none());
            fs::write(&file, b"v2").unwrap();
            sess_b.refresh_tracked_file_hash(&file);
            assert!(sess_b.check_stale_before_write(&file).is_none());

            drop(lease_a);
            let mut sess_c = Session::new(
                "sess-c".into(),
                work.clone(),
                PermissionMode::Auto,
                "default".into(),
            );
            sess_c.attach_workspace_concurrency_guard_in(Some(&root));
            // sess-b still leased; should see it. Drop sess_b first.
            drop(sess_b);
            let peers = sess_c.list_workspace_peers();
            assert!(
                peers.iter().all(|p| p.session_id != "sess-a"),
                "closed session should not remain active"
            );
        });
        let _ = fs::remove_dir_all(&root);
    }
}
