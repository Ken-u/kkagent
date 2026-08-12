use anyhow::{Context, Result};
use chrono::Utc;
use serde_json::{json, Value};
use std::fs::{self, OpenOptions};
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::Arc;

const RUN_RECORD_VERSION: u64 = 1;

/// A durable process-lifetime record used to distinguish clean exits, panics,
/// handled termination signals, and uncatchable process termination.
pub(crate) struct RunDiagnostics {
    record: Arc<RunRecord>,
    finished: bool,
}

struct RunRecord {
    path: PathBuf,
    run_id: String,
    pid: u32,
    mode: String,
    started_at: String,
}

impl RunDiagnostics {
    pub(crate) fn start(mode: &str) -> Result<Self> {
        Self::start_in(&kkagent_config::default_config_dir(), mode)
    }

    fn start_in(config_dir: &Path, mode: &str) -> Result<Self> {
        let runs_dir = config_dir.join("diagnostics").join("runs");
        fs::create_dir_all(&runs_dir).with_context(|| {
            format!(
                "failed to create diagnostics directory {}",
                runs_dir.display()
            )
        })?;

        let run_id = uuid::Uuid::new_v4().to_string();
        let pid = std::process::id();
        let started_at = Utc::now().to_rfc3339();
        let path = runs_dir.join(format!("{run_id}.json"));
        let record = Arc::new(RunRecord {
            path,
            run_id,
            pid,
            mode: mode.to_string(),
            started_at,
        });
        record.write_status("running", None)?;

        tracing::info!(
            run_id = %record.run_id,
            pid = record.pid,
            mode = %record.mode,
            diagnostics = %record.path.display(),
            version = env!("CARGO_PKG_VERSION"),
            os = std::env::consts::OS,
            arch = std::env::consts::ARCH,
            "kkagent process started"
        );

        Ok(Self {
            record,
            finished: false,
        })
    }

    pub(crate) fn install_panic_hook(&self) {
        let previous = std::panic::take_hook();
        let record = Arc::clone(&self.record);
        std::panic::set_hook(Box::new(move |panic_info| {
            let payload = panic_info
                .payload()
                .downcast_ref::<&str>()
                .copied()
                .or_else(|| {
                    panic_info
                        .payload()
                        .downcast_ref::<String>()
                        .map(String::as_str)
                })
                .unwrap_or("non-string panic payload");
            let location = panic_info
                .location()
                .map(|location| {
                    format!(
                        "{}:{}:{}",
                        location.file(),
                        location.line(),
                        location.column()
                    )
                })
                .unwrap_or_else(|| "unknown".to_string());
            let detail = json!({
                "message": payload,
                "location": location,
                "thread": std::thread::current().name().unwrap_or("unnamed"),
            });
            let _ = record.write_status("panic", Some(detail));
            previous(panic_info);
        }));
    }

    pub(crate) fn start_signal_watchers(&self) {
        #[cfg(unix)]
        self.start_unix_signal_watcher();
        #[cfg(windows)]
        self.start_windows_signal_watcher();
    }

    #[cfg(unix)]
    fn start_unix_signal_watcher(&self) {
        use tokio::signal::unix::{signal, SignalKind};

        let record = Arc::clone(&self.record);
        tokio::spawn(async move {
            let Ok(mut terminate) = signal(SignalKind::terminate()) else {
                return;
            };
            let Ok(mut hangup) = signal(SignalKind::hangup()) else {
                return;
            };
            let Ok(mut quit) = signal(SignalKind::quit()) else {
                return;
            };
            let (name, number) = tokio::select! {
                _ = terminate.recv() => ("SIGTERM", 15),
                _ = hangup.recv() => ("SIGHUP", 1),
                _ = quit.recv() => ("SIGQUIT", 3),
            };
            record_and_exit(record, name, number);
        });
    }

    #[cfg(windows)]
    fn start_windows_signal_watcher(&self) {
        use tokio::signal::windows::{ctrl_break, ctrl_close, ctrl_logoff, ctrl_shutdown};

        let record = Arc::clone(&self.record);
        tokio::spawn(async move {
            let Ok(mut break_signal) = ctrl_break() else {
                return;
            };
            let Ok(mut close_signal) = ctrl_close() else {
                return;
            };
            let Ok(mut logoff_signal) = ctrl_logoff() else {
                return;
            };
            let Ok(mut shutdown_signal) = ctrl_shutdown() else {
                return;
            };
            let name = tokio::select! {
                _ = break_signal.recv() => "CTRL_BREAK",
                _ = close_signal.recv() => "CTRL_CLOSE",
                _ = logoff_signal.recv() => "CTRL_LOGOFF",
                _ = shutdown_signal.recv() => "CTRL_SHUTDOWN",
            };
            record_and_exit(record, name, 1);
        });
    }

    pub(crate) fn finish(&mut self, error: Option<&anyhow::Error>) {
        let (status, detail) = match error {
            Some(error) => ("error", Some(json!({ "message": format!("{error:#}") }))),
            None => ("completed", None),
        };
        if let Err(write_error) = self.record.write_status(status, detail) {
            tracing::error!(%write_error, "failed to finalize process diagnostics");
        }
        tracing::info!(
            run_id = %self.record.run_id,
            pid = self.record.pid,
            status,
            "kkagent process exiting"
        );
        self.finished = true;
    }
}

impl Drop for RunDiagnostics {
    fn drop(&mut self) {
        if !self.finished && !std::thread::panicking() {
            let _ = self.record.write_status(
                "dropped_without_finish",
                Some(json!({ "message": "diagnostics guard dropped before main completed" })),
            );
        }
    }
}

impl RunRecord {
    fn write_status(&self, status: &str, detail: Option<Value>) -> Result<()> {
        let value = json!({
            "schema_version": RUN_RECORD_VERSION,
            "run_id": self.run_id,
            "pid": self.pid,
            "mode": self.mode,
            "version": env!("CARGO_PKG_VERSION"),
            "os": std::env::consts::OS,
            "arch": std::env::consts::ARCH,
            "started_at": self.started_at,
            "updated_at": Utc::now().to_rfc3339(),
            "status": status,
            "detail": detail,
        });
        write_json_atomically(&self.path, &value)
    }
}

fn write_json_atomically(path: &Path, value: &Value) -> Result<()> {
    let tmp = path.with_extension(format!("tmp-{}", uuid::Uuid::new_v4()));
    let mut options = OpenOptions::new();
    options.create_new(true).write(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let mut file = options
        .open(&tmp)
        .with_context(|| format!("failed to create diagnostics file {}", tmp.display()))?;
    serde_json::to_writer_pretty(&mut file, value)?;
    file.write_all(b"\n")?;
    file.sync_all()?;
    drop(file);

    #[cfg(windows)]
    if path.exists() {
        fs::remove_file(path)?;
    }
    if let Err(error) = fs::rename(&tmp, path) {
        let _ = fs::remove_file(&tmp);
        return Err(error)
            .with_context(|| format!("failed to replace diagnostics file {}", path.display()));
    }
    Ok(())
}

fn record_and_exit(record: Arc<RunRecord>, signal: &str, exit_code: i32) -> ! {
    let _ = record.write_status("signal", Some(json!({ "signal": signal })));
    tracing::error!(
        run_id = %record.run_id,
        pid = record.pid,
        signal,
        "kkagent received termination signal"
    );
    std::process::exit(128 + exit_code);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_running_and_completed_states() {
        let root =
            std::env::temp_dir().join(format!("kkagent-diagnostics-test-{}", uuid::Uuid::new_v4()));
        let mut diagnostics = RunDiagnostics::start_in(&root, "test").unwrap();
        let path = diagnostics.record.path.clone();

        let running: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(running["status"], "running");
        assert_eq!(running["mode"], "test");

        diagnostics.finish(None);
        let completed: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(completed["status"], "completed");
        assert_eq!(completed["run_id"], running["run_id"]);

        fs::remove_dir_all(root).unwrap();
    }

    #[test]
    fn records_error_details() {
        let root =
            std::env::temp_dir().join(format!("kkagent-diagnostics-test-{}", uuid::Uuid::new_v4()));
        let mut diagnostics = RunDiagnostics::start_in(&root, "test").unwrap();
        let path = diagnostics.record.path.clone();
        let error = anyhow::anyhow!("test failure");

        diagnostics.finish(Some(&error));
        let failed: Value = serde_json::from_slice(&fs::read(&path).unwrap()).unwrap();
        assert_eq!(failed["status"], "error");
        assert_eq!(failed["detail"]["message"], "test failure");

        fs::remove_dir_all(root).unwrap();
    }
}
