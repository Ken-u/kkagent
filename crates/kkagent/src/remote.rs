//! Remote workspace support: SSH bridge, remote server connection,
//! workspace routing, and event forwarding.

use anyhow::{Context, Result};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{mpsc, watch, Mutex, RwLock};

use kkagent_protocol::Frame;
use kkagent_rpc::{RpcClient, RpcConnectionState};

// ---------------------------------------------------------------------------
// Bridge (runs on remote host)
// ---------------------------------------------------------------------------

/// Bidirectional proxy: stdin/stdout ↔ local UDS.
///
/// stdout carries only NDJSON RPC frames. All diagnostics go to stderr.
pub async fn run_bridge(listen: Option<String>) -> Result<()> {
    let socket_path = listen
        .map(PathBuf::from)
        .unwrap_or_else(kkagent_config::default_server_socket_path);

    eprintln!("kkagent bridge: connecting to {}", socket_path.display());

    let uds = kkagent_rpc::transport::uds::connect_uds(&socket_path)
        .await
        .with_context(|| {
            format!(
                "bridge: cannot connect to local server at {}",
                socket_path.display()
            )
        })?;

    eprintln!("kkagent bridge: connected");

    let (uds_read, uds_write) = tokio::io::split(uds);
    let stdin = tokio::io::stdin();
    let stdout = tokio::io::stdout();

    let mut buf_stdin = BufReader::new(stdin);
    let mut buf_uds_write = tokio::io::BufWriter::new(uds_write);
    let mut buf_uds_read = BufReader::new(uds_read);
    let mut buf_stdout = tokio::io::BufWriter::new(stdout);

    let copy_in = tokio::io::copy(&mut buf_stdin, &mut buf_uds_write);
    let copy_out = tokio::io::copy(&mut buf_uds_read, &mut buf_stdout);

    tokio::select! {
        result = copy_in => {
            if let Err(e) = result {
                eprintln!("kkagent bridge: stdin→server copy ended: {e}");
            }
        }
        result = copy_out => {
            if let Err(e) = result {
                eprintln!("kkagent bridge: server→stdout copy ended: {e}");
            }
        }
    }

    eprintln!("kkagent bridge: exiting");
    Ok(())
}

// ---------------------------------------------------------------------------
// SSH Connection Management
// ---------------------------------------------------------------------------

/// Manages an SSH ControlMaster connection to a remote host.
pub struct SshControlMaster {
    _host: String,
    socket_path: PathBuf,
    child: Option<Child>,
}

impl SshControlMaster {
    /// Establish an interactive SSH ControlMaster connection.
    ///
    /// This spawns `ssh -M -N -S <socket>` with the terminal attached so
    /// the user can complete any authentication prompts (password, passphrase,
    /// 2FA, keyboard-interactive).
    ///
    /// `port` overrides the SSH port; `None` defers to `~/.ssh/config` or
    /// the OpenSSH default (22).
    pub async fn establish(host: &str, port: Option<u16>) -> Result<Self> {
        let socket_dir = kkagent_config::default_config_dir().join("ssh");
        std::fs::create_dir_all(&socket_dir)?;
        // Sanitize host for use as a filename (replace @ : / with -)
        let safe_host: String = host
            .chars()
            .map(|c| {
                if c.is_alphanumeric() || c == '.' || c == '-' {
                    c
                } else {
                    '-'
                }
            })
            .collect();
        let port_suffix = port.map(|p| format!("-{p}")).unwrap_or_default();
        let socket_path = socket_dir.join(format!("ctrl-{safe_host}{port_suffix}"));

        // If a ControlMaster already exists and is alive, reuse it.
        if socket_path.exists() {
            let mut check = Command::new("ssh");
            check
                .args(["-S", &socket_path.to_string_lossy()])
                .args(["-O", "check"]);
            if let Some(p) = port {
                check.args(["-p", &p.to_string()]);
            }
            check
                .arg(host)
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            let status = check.status().await;
            if status.is_ok_and(|s| s.success()) {
                eprintln!("Reusing existing SSH connection to {host}");
                return Ok(Self {
                    _host: host.to_string(),
                    socket_path,
                    child: None,
                });
            }
            let _ = std::fs::remove_file(&socket_path);
        }

        eprintln!("Establishing SSH connection to {host}...");

        let mut cmd = Command::new("ssh");
        cmd.arg("-M") // ControlMaster
            .arg("-N") // no remote command
            .arg("-S")
            .arg(&socket_path)
            .arg("-o")
            .arg("ControlPersist=yes")
            .arg("-o")
            .arg("ServerAliveInterval=30")
            .arg("-o")
            .arg("ServerAliveCountMax=3");
        if let Some(p) = port {
            cmd.args(["-p", &p.to_string()]);
        }
        let child = cmd
            .arg(host)
            .stdin(Stdio::inherit())
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .spawn()
            .with_context(|| format!("failed to spawn ssh for {host}"))?;

        // Wait for the ControlMaster to become ready.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(120);
        let mut delay = Duration::from_millis(100);
        loop {
            if socket_path.exists() {
                let check = Command::new("ssh")
                    .args(["-S", &socket_path.to_string_lossy()])
                    .args(["-O", "check"])
                    .arg(host)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .await;
                if check.is_ok_and(|s| s.success()) {
                    break;
                }
            }
            if tokio::time::Instant::now() > deadline {
                anyhow::bail!("SSH authentication to {host} timed out");
            }
            tokio::time::sleep(delay).await;
            delay = (delay * 2).min(Duration::from_millis(1000));
        }

        eprintln!("SSH connection to {host} established");

        Ok(Self {
            _host: host.to_string(),
            socket_path,
            child: Some(child),
        })
    }

    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }
}

impl Drop for SshControlMaster {
    fn drop(&mut self) {
        if let Some(mut child) = self.child.take() {
            let _ = child.start_kill();
        }
    }
}

// ---------------------------------------------------------------------------
// Remote Server Ensure
// ---------------------------------------------------------------------------

/// Build an SSH command that reuses the ControlMaster connection.
fn ssh_via_control(ssh_socket: &Path, host: &str) -> Command {
    let mut cmd = Command::new("ssh");
    cmd.arg("-S")
        .arg(ssh_socket)
        .arg("-o")
        .arg("BatchMode=yes")
        .arg(host);
    cmd
}

/// Shell preamble that ensures `kkagent` is on PATH even in a
/// non-interactive SSH session (which skips .bashrc/.zshrc).
const REMOTE_PATH_PREAMBLE: &str = concat!(
    "export PATH=\"$HOME/.cargo/bin:$HOME/.local/bin:$HOME/bin:",
    "/usr/local/bin:/usr/bin:/bin:$PATH\"; "
);

/// Expand a leading `~` using the remote host's home directory and normalize
/// the result (strip trailing slash) for use as a remote workspace path.
pub fn expand_remote_tilde(path: &str, remote_home: &Path) -> String {
    let expanded = if path == "~" {
        remote_home.to_string_lossy().into_owned()
    } else if let Some(rest) = path.strip_prefix("~/") {
        remote_home.join(rest).to_string_lossy().into_owned()
    } else {
        path.to_string()
    };
    if expanded.len() > 1 {
        expanded.trim_end_matches('/').to_string()
    } else {
        expanded
    }
}

/// Check whether a remote kkagent server is running; start one if not.
///
/// Returns the absolute remote home directory (resolved shell-side via
/// `$HOME`, so `~` workspace paths can be expanded correctly).
pub async fn ensure_remote_server(ssh_socket: &Path, host: &str) -> Result<PathBuf> {
    // First, verify kkagent exists on the remote host.
    let which = ssh_via_control(ssh_socket, host)
        .arg(format!(
            "{REMOTE_PATH_PREAMBLE}command -v kkagent || echo __NOT_FOUND__"
        ))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;

    let which_out = String::from_utf8_lossy(&which.stdout);
    if which_out.contains("__NOT_FOUND__") || !which.status.success() {
        anyhow::bail!(
            "kkagent is not installed on {host} (not found in PATH).\n\
             Install it on the remote host first, e.g.:\n  \
             ssh {host} 'curl -fsSL https://your-install-url | sh'"
        );
    }
    let remote_kkagent = which_out.trim().to_string();
    eprintln!("Remote kkagent found at: {remote_kkagent}");

    // Resolve the remote home directory so `~` workspace paths can be
    // expanded to an absolute path before any session is created.
    let home_out = ssh_via_control(ssh_socket, host)
        .arg(format!("{REMOTE_PATH_PREAMBLE}printf '%s' \"$HOME\""))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;
    let remote_home = PathBuf::from(String::from_utf8_lossy(&home_out.stdout).trim());
    if !home_out.status.success() || remote_home.as_os_str().is_empty() {
        anyhow::bail!("cannot resolve remote home directory on {host}");
    }

    // Try to reach the remote server via a status probe.
    let probe = ssh_via_control(ssh_socket, host)
        .arg(format!("{REMOTE_PATH_PREAMBLE}kkagent server status"))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;

    if probe.status.success() {
        eprintln!("Remote kkagent server on {host} is already running");
        return Ok(remote_home);
    }

    eprintln!("Starting remote kkagent server on {host}...");

    // Start a daemonized server on the remote host.
    // SSH remote commands are executed by the user's login shell. We cannot
    // rely on `disown` (dash and other minimal shells lack it), so the server
    // is started in a plain background subshell; `nohup` plus stdin/stdout
    // redirection already keeps it alive after the SSH session closes.
    let start_cmd = format!(
        "{REMOTE_PATH_PREAMBLE}\
         nohup kkagent server </dev/null >\"$HOME/.kkagent/server-start.log\" 2>&1 &"
    );
    let start = ssh_via_control(ssh_socket, host)
        .arg(&start_cmd)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;

    if !start.status.success() {
        let stderr = String::from_utf8_lossy(&start.stderr);
        anyhow::bail!("failed to start remote kkagent server on {host}: {stderr}");
    }

    // Wait for the remote server to become ready.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(15);
    let mut delay = Duration::from_millis(300);
    loop {
        let check = ssh_via_control(ssh_socket, host)
            .arg(format!("{REMOTE_PATH_PREAMBLE}kkagent server status"))
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .output()
            .await;
        match &check {
            Ok(output) if output.status.success() => {
                eprintln!("Remote kkagent server on {host} is ready");
                return Ok(remote_home);
            }
            _ => {}
        }
        if tokio::time::Instant::now() > deadline {
            // Try to read the startup log for diagnostics.
            let log = ssh_via_control(ssh_socket, host)
                .arg(format!(
                    "{REMOTE_PATH_PREAMBLE}tail -20 \"$HOME/.kkagent/server-start.log\" 2>/dev/null || echo '(no log)'"
                ))
                .stdin(Stdio::null())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .output()
                .await
                .ok()
                .map(|o| String::from_utf8_lossy(&o.stdout).to_string())
                .unwrap_or_default();
            anyhow::bail!(
                "remote kkagent server on {host} did not become ready within 15s\n\
                 Remote startup log:\n{log}"
            );
        }
        tokio::time::sleep(delay).await;
        delay = (delay * 2).min(Duration::from_millis(1000));
    }
}

// ---------------------------------------------------------------------------
// Remote Server Connection (lives inside local kkagent server)
// ---------------------------------------------------------------------------

/// Connection state for a remote server.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteConnectionState {
    Connected,
    Disconnected {
        reason: String,
    },
    /// Background reconnect failed because SSH requires interactive auth.
    AuthenticationRequired,
    /// Bridge is being (re)established.
    Connecting,
}

/// A long-lived connection to a remote kkagent server via SSH bridge.
pub struct RemoteServerConnection {
    pub server_id: String,
    pub host: String,
    ssh_socket: PathBuf,
    state: watch::Sender<RemoteConnectionState>,
    state_rx: watch::Receiver<RemoteConnectionState>,
    /// RPC client talking to the remote server through the bridge.
    rpc_client: RwLock<Option<RpcClient>>,
    /// Sender to forward remote events to the local server.
    remote_event_tx: mpsc::Sender<(String, Frame)>,
    /// Handle to the SSH bridge child process.
    bridge_child: Mutex<Option<Child>>,
    /// Serializes `connect()` so a scheduled reconnect and an explicit
    /// reconnect (e.g. `remote.register` from a fresh `kk ssh`) can never
    /// race each other into two live bridges.
    connect_lock: tokio::sync::Mutex<()>,
    /// Bumped on every `connect()`; stale forwarder/reconnect tasks from a
    /// superseded bridge compare against it and stand down instead of
    /// clobbering connection state.
    epoch: AtomicU64,
}

impl RemoteServerConnection {
    pub fn new(
        server_id: String,
        host: String,
        ssh_socket: PathBuf,
        remote_event_tx: mpsc::Sender<(String, Frame)>,
    ) -> Arc<Self> {
        let (state_tx, state_rx) = watch::channel(RemoteConnectionState::Connecting);
        Arc::new(Self {
            server_id,
            host,
            ssh_socket,
            state: state_tx,
            state_rx,
            rpc_client: RwLock::new(None),
            remote_event_tx,
            bridge_child: Mutex::new(None),
            connect_lock: tokio::sync::Mutex::new(()),
            epoch: AtomicU64::new(0),
        })
    }

    pub fn connection_state(&self) -> RemoteConnectionState {
        self.state_rx.borrow().clone()
    }

    /// The ControlMaster socket this connection bridges through.
    pub fn ssh_socket(&self) -> &Path {
        &self.ssh_socket
    }

    #[allow(dead_code)]
    pub fn state_receiver(&self) -> watch::Receiver<RemoteConnectionState> {
        self.state_rx.clone()
    }

    /// Establish (or re-establish) the SSH bridge connection.
    pub async fn connect(self: &Arc<Self>) -> Result<()> {
        // Serialize concurrent connect attempts (scheduled reconnect vs
        // explicit remote.register reconnect).
        let _connect_guard = self.connect_lock.lock().await;
        self.state.send_replace(RemoteConnectionState::Connecting);

        // Kill any existing bridge child.
        if let Some(mut old_child) = self.bridge_child.lock().await.take() {
            let _ = old_child.kill().await;
        }

        // The remote command is passed as a single string so SSH hands it
        // to the user's login shell verbatim; PATH preamble ensures kkagent
        // is found even in non-interactive SSH sessions.
        let remote_cmd = format!("{REMOTE_PATH_PREAMBLE}exec kkagent bridge --stdio");
        let mut cmd = Command::new("ssh");
        cmd.arg("-S")
            .arg(&self.ssh_socket)
            .arg("-o")
            .arg("BatchMode=yes")
            .arg("-o")
            .arg("ServerAliveInterval=30")
            .arg("-o")
            .arg("ServerAliveCountMax=3")
            .arg(&self.host)
            .arg(&remote_cmd)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());

        let mut child = cmd
            .spawn()
            .with_context(|| format!("failed to spawn SSH bridge to {}", self.host))?;

        let child_stdin = child
            .stdin
            .take()
            .context("SSH bridge child has no stdin")?;
        let child_stdout = child
            .stdout
            .take()
            .context("SSH bridge child has no stdout")?;

        // Log stderr in background (diagnostics only).
        if let Some(stderr) = child.stderr.take() {
            let host = self.host.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(stderr);
                let mut line = String::new();
                loop {
                    line.clear();
                    match reader.read_line(&mut line).await {
                        Ok(0) => break,
                        Ok(_) => {
                            tracing::debug!(remote_host = %host, "bridge stderr: {}", line.trim_end());
                        }
                        Err(_) => break,
                    }
                }
            });
        }

        // Combine child stdin+stdout into a single AsyncTransport via
        // tokio::io::join (duplex from two halves).
        let transport = tokio::io::join(child_stdout, child_stdin);

        let (event_tx, mut event_rx) = mpsc::channel::<Frame>(256);
        let rpc_client = RpcClient::new(transport, event_tx);

        // Wait for the Ready frame with a timeout.
        let ready = tokio::time::timeout(Duration::from_secs(15), async {
            // The first frame from the server through the bridge should be Ready.
            // RpcClient sends incoming non-response frames to event_rx.
            while let Some(frame) = event_rx.recv().await {
                if matches!(frame, Frame::Ready) {
                    return Ok(());
                }
            }
            anyhow::bail!("bridge closed before sending Ready")
        })
        .await;

        match ready {
            Ok(Ok(())) => {}
            Ok(Err(e)) => {
                let _ = child.kill().await;
                self.state
                    .send_replace(RemoteConnectionState::Disconnected {
                        reason: e.to_string(),
                    });
                return Err(e);
            }
            Err(_) => {
                let _ = child.kill().await;
                self.state
                    .send_replace(RemoteConnectionState::Disconnected {
                        reason: "bridge Ready timeout".into(),
                    });
                anyhow::bail!("bridge to {} did not become ready within 15s", self.host);
            }
        }

        *self.rpc_client.write().await = Some(rpc_client.clone());
        *self.bridge_child.lock().await = Some(child);
        self.state.send_replace(RemoteConnectionState::Connected);

        // Version compatibility check.
        match rpc_client.call("runtime.status", None).await {
            Ok(status) => {
                let remote_version = status
                    .get("version")
                    .and_then(|v| v.as_str())
                    .unwrap_or("unknown");
                let local_version = env!("CARGO_PKG_VERSION");
                check_version_compatibility(local_version, remote_version, &self.host);
            }
            Err(e) => {
                tracing::warn!(
                    host = %self.host,
                    "cannot query remote server version: {e}; \
                     continuing but RPC incompatibilities may occur"
                );
            }
        }

        // Forward remote events to local server.
        let server_id = self.server_id.clone();
        let event_fwd_tx = self.remote_event_tx.clone();
        let conn = Arc::clone(self);
        // Tag this bridge generation so a forwarder from a superseded bridge
        // cannot clobber connection state or schedule a competing reconnect.
        let bridge_epoch = self.epoch.fetch_add(1, Ordering::SeqCst) + 1;
        tokio::spawn(async move {
            while let Some(frame) = event_rx.recv().await {
                if event_fwd_tx.send((server_id.clone(), frame)).await.is_err() {
                    break;
                }
            }
            // A newer connect() superseded this bridge; it owns state now.
            if conn.epoch.load(Ordering::SeqCst) != bridge_epoch {
                return;
            }
            // Event stream ended → remote disconnected.
            let reason = match rpc_client.connection_state() {
                RpcConnectionState::Disconnected { reason } => reason,
                _ => "bridge event stream closed".to_string(),
            };
            tracing::warn!(
                host = %conn.host,
                %reason,
                "remote server connection lost"
            );
            conn.state
                .send_replace(RemoteConnectionState::Disconnected {
                    reason: reason.clone(),
                });
            conn.schedule_reconnect();
        });

        Ok(())
    }

    /// Forward an RPC call to the remote server.
    pub async fn call(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, (i32, String)> {
        let client = self.rpc_client.read().await;
        let Some(client) = client.as_ref() else {
            return Err((
                -32003,
                format!("remote server {} is not connected", self.host),
            ));
        };
        client
            .call(method, params)
            .await
            .map_err(|e| (-32003, format!("remote RPC to {}: {e}", self.host)))
    }

    /// Try to reconnect in the background using BatchMode=yes (no password).
    fn schedule_reconnect(self: &Arc<Self>) {
        let conn = Arc::clone(self);
        tokio::spawn(async move {
            let epoch_at_schedule = conn.epoch.load(Ordering::SeqCst);
            let mut delay = Duration::from_secs(2);
            let max_delay = Duration::from_secs(60);

            for attempt in 1u32.. {
                // A newer connect() superseded this schedule; stand down.
                if conn.epoch.load(Ordering::SeqCst) != epoch_at_schedule {
                    return;
                }
                tokio::time::sleep(delay).await;

                // Check if the SSH ControlMaster socket is still alive.
                let alive = Command::new("ssh")
                    .args(["-S", &conn.ssh_socket.to_string_lossy()])
                    .args(["-O", "check"])
                    .arg(&conn.host)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .status()
                    .await
                    .is_ok_and(|s| s.success());

                if !alive {
                    // Try BatchMode=yes reconnect (key/agent only).
                    let reauth = Command::new("ssh")
                        .arg("-o")
                        .arg("BatchMode=yes")
                        .arg("-o")
                        .arg("ConnectTimeout=10")
                        .arg("-N")
                        .arg("-f")
                        .arg("-M")
                        .arg("-S")
                        .arg(&conn.ssh_socket)
                        .arg(&conn.host)
                        .stdin(Stdio::null())
                        .stdout(Stdio::null())
                        .stderr(Stdio::null())
                        .status()
                        .await;

                    if !reauth.is_ok_and(|s| s.success()) {
                        tracing::info!(
                            host = %conn.host,
                            attempt,
                            "background SSH reconnect failed (may need interactive auth)"
                        );
                        // Only surface this if the connection is still the one
                        // we were scheduled for.
                        if conn.epoch.load(Ordering::SeqCst) == epoch_at_schedule
                            && matches!(
                                conn.connection_state(),
                                RemoteConnectionState::Disconnected { .. }
                            )
                        {
                            conn.state
                                .send_replace(RemoteConnectionState::AuthenticationRequired);
                        }
                        return;
                    }
                }

                // A newer connect() superseded this schedule while we waited.
                if conn.epoch.load(Ordering::SeqCst) != epoch_at_schedule {
                    return;
                }
                tracing::info!(host = %conn.host, attempt, "attempting bridge reconnect");
                match conn.connect().await {
                    Ok(()) => {
                        tracing::info!(host = %conn.host, "bridge reconnected");
                        return;
                    }
                    Err(e) => {
                        tracing::warn!(host = %conn.host, %e, "bridge reconnect failed");
                    }
                }

                delay = (delay * 2).min(max_delay);
            }
        });
    }
}

// ---------------------------------------------------------------------------
// Remote Server Registry (lives in local ServerState)
// ---------------------------------------------------------------------------

/// Manages all remote server connections and workspace/session routing.
pub struct RemoteRegistry {
    connections: RwLock<HashMap<String, Arc<RemoteServerConnection>>>,
    /// Maps workspace keys to their owning remote server and the remote-side
    /// path. Local workspaces are not in this map.
    workspace_owners: RwLock<HashMap<String, WorkspaceRoute>>,
    /// Maps external (opaque) session IDs to (server_id, remote_session_id).
    session_routes: RwLock<HashMap<String, SessionRoute>>,
    /// Channel that receives events from all remote servers.
    remote_event_tx: mpsc::Sender<(String, Frame)>,
    pub remote_event_rx: Mutex<Option<mpsc::Receiver<(String, Frame)>>>,
    id_seq: AtomicU64,
}

#[derive(Debug, Clone)]
pub struct SessionRoute {
    pub server_id: String,
    pub remote_session_id: String,
}

/// Routing entry for a remote workspace: which server owns it and what path
/// the remote server should use for it.
#[derive(Debug, Clone)]
pub struct WorkspaceRoute {
    pub server_id: String,
    pub remote_path: String,
}

/// Information about a remote workspace.
#[derive(Debug, Clone, serde::Serialize)]
pub struct RemoteWorkspaceInfo {
    pub server_id: String,
    pub host: String,
    pub path: String,
    pub state: String,
}

impl RemoteRegistry {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::channel(1024);
        Self {
            connections: RwLock::new(HashMap::new()),
            workspace_owners: RwLock::new(HashMap::new()),
            session_routes: RwLock::new(HashMap::new()),
            remote_event_tx: tx,
            remote_event_rx: Mutex::new(Some(rx)),
            id_seq: AtomicU64::new(1),
        }
    }

    fn next_opaque_id(&self) -> String {
        let n = self.id_seq.fetch_add(1, Ordering::Relaxed);
        format!("r{n}-{}", uuid::Uuid::new_v4())
    }

    /// Register a remote server connection.
    #[allow(dead_code)]
    pub async fn register_connection(&self, conn: Arc<RemoteServerConnection>) {
        self.connections
            .write()
            .await
            .insert(conn.server_id.clone(), conn);
    }

    /// Create a new remote server connection and register it.
    pub async fn add_server(
        &self,
        server_id: String,
        host: String,
        ssh_socket: PathBuf,
    ) -> Arc<RemoteServerConnection> {
        let conn = RemoteServerConnection::new(
            server_id.clone(),
            host,
            ssh_socket,
            self.remote_event_tx.clone(),
        );
        self.connections
            .write()
            .await
            .insert(server_id, conn.clone());
        conn
    }

    /// Register a workspace as belonging to a remote server.
    pub async fn register_workspace(
        &self,
        workspace_key: &str,
        server_id: &str,
        remote_path: &str,
    ) {
        self.workspace_owners.write().await.insert(
            workspace_key.to_string(),
            WorkspaceRoute {
                server_id: server_id.to_string(),
                remote_path: remote_path.to_string(),
            },
        );
    }

    /// Get the route that owns a workspace, or None for local.
    pub async fn workspace_route(&self, workspace_key: &str) -> Option<WorkspaceRoute> {
        self.workspace_owners
            .read()
            .await
            .get(workspace_key)
            .cloned()
    }

    /// Register a session route mapping.
    pub async fn register_session(&self, external_id: &str, route: SessionRoute) {
        self.session_routes
            .write()
            .await
            .insert(external_id.to_string(), route);
    }

    /// Look up the route for a session. Returns None for local sessions.
    pub async fn session_route(&self, external_id: &str) -> Option<SessionRoute> {
        self.session_routes.read().await.get(external_id).cloned()
    }

    /// Get a connection by server ID.
    pub async fn connection(&self, server_id: &str) -> Option<Arc<RemoteServerConnection>> {
        self.connections.read().await.get(server_id).cloned()
    }

    /// List all registered remote workspaces.
    pub async fn list_workspaces(&self) -> Vec<RemoteWorkspaceInfo> {
        let owners = self.workspace_owners.read().await;
        let conns = self.connections.read().await;
        let mut result = Vec::new();
        for route in owners.values() {
            if let Some(conn) = conns.get(&route.server_id) {
                result.push(RemoteWorkspaceInfo {
                    server_id: route.server_id.clone(),
                    host: conn.host.clone(),
                    path: route.remote_path.clone(),
                    state: format!("{:?}", conn.connection_state()),
                });
            }
        }
        result
    }

    /// Create a remote session: forward sessions.create to the remote server,
    /// generate an opaque external ID, and register the route.
    pub async fn create_remote_session(
        &self,
        server_id: &str,
        params: serde_json::Value,
    ) -> Result<(String, serde_json::Value), (i32, String)> {
        let conn = self
            .connections
            .read()
            .await
            .get(server_id)
            .cloned()
            .ok_or_else(|| (-32003, format!("unknown remote server: {server_id}")))?;

        let result = conn.call("sessions.create", Some(params)).await?;

        let remote_session_id = result
            .get("session_id")
            .and_then(|v| v.as_str())
            .ok_or_else(|| {
                (
                    -32000,
                    "remote sessions.create returned no session_id".into(),
                )
            })?
            .to_string();

        let external_id = self.next_opaque_id();
        self.register_session(
            &external_id,
            SessionRoute {
                server_id: server_id.to_string(),
                remote_session_id: remote_session_id.clone(),
            },
        )
        .await;

        let mut response = result;
        if let Some(obj) = response.as_object_mut() {
            obj.insert("session_id".into(), serde_json::json!(external_id));
            obj.insert(
                "remote_session_id".into(),
                serde_json::json!(remote_session_id),
            );
            obj.insert("remote_server".into(), serde_json::json!(server_id));
        }

        Ok((external_id, response))
    }

    /// Forward a session-scoped RPC call to the correct remote server,
    /// translating the session ID in params.
    pub async fn forward_session_call(
        &self,
        route: &SessionRoute,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, (i32, String)> {
        let conn = self
            .connections
            .read()
            .await
            .get(&route.server_id)
            .cloned()
            .ok_or_else(|| {
                (
                    -32003,
                    format!("remote server {} is not registered", route.server_id),
                )
            })?;

        // Rewrite session_id in params to the remote ID.
        let params = params.map(|mut p| {
            if let Some(obj) = p.as_object_mut() {
                obj.insert(
                    "session_id".into(),
                    serde_json::json!(route.remote_session_id),
                );
            }
            p
        });

        conn.call(method, params).await
    }

    /// Translate a remote session ID from an event back to the external opaque ID.
    pub async fn translate_remote_session_id(
        &self,
        server_id: &str,
        remote_session_id: &str,
    ) -> Option<String> {
        let routes = self.session_routes.read().await;
        for (external_id, route) in routes.iter() {
            if route.server_id == server_id && route.remote_session_id == remote_session_id {
                return Some(external_id.clone());
            }
        }
        None
    }

    /// Try to reconnect a specific remote server (used after `kk ssh` re-auth).
    pub async fn reconnect(&self, server_id: &str) -> Result<()> {
        let conn = self
            .connections
            .read()
            .await
            .get(server_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("unknown server: {server_id}"))?;
        conn.connect().await
    }

    /// List remote sessions (merge into sessions.list response).
    pub async fn list_remote_sessions(&self, limit: usize) -> Vec<serde_json::Value> {
        let conns = self.connections.read().await;
        let routes = self.session_routes.read().await;
        let mut results = Vec::new();

        for (server_id, conn) in conns.iter() {
            if !matches!(conn.connection_state(), RemoteConnectionState::Connected) {
                continue;
            }
            let remote_sessions = conn
                .call("sessions.list", Some(serde_json::json!({"limit": limit})))
                .await;
            if let Ok(data) = remote_sessions {
                if let Some(sessions) = data.get("sessions").and_then(|v| v.as_array()) {
                    for session in sessions {
                        let remote_id = session
                            .get("session_id")
                            .and_then(|v| v.as_str())
                            .unwrap_or_default();
                        // Find or create external ID.
                        let external_id = routes
                            .iter()
                            .find(|(_, r)| {
                                r.server_id == *server_id && r.remote_session_id == remote_id
                            })
                            .map(|(eid, _)| eid.clone());
                        let mut session = session.clone();
                        if let (Some(external_id), Some(obj)) =
                            (external_id, session.as_object_mut())
                        {
                            obj.insert("session_id".into(), serde_json::json!(external_id));
                            obj.insert("remote_server".into(), serde_json::json!(server_id));
                        }
                        results.push(session);
                    }
                }
            }
        }
        results
    }
}

// ---------------------------------------------------------------------------
// Event Translation
// ---------------------------------------------------------------------------

/// Translate remote session IDs in an event frame to the local opaque ID.
///
/// Both `data.session_id` and `scope` may carry the remote session ID; the
/// scope matters for clients subscribed with `Listen { scope }`.
pub fn translate_event_session_id(
    frame: &Frame,
    remote_session_id: &str,
    external_session_id: &str,
) -> Frame {
    match frame {
        Frame::Event { event, scope, data } => {
            let mut data = data.clone();
            if let Some(obj) = data.as_object_mut() {
                if obj.get("session_id").and_then(|v| v.as_str()) == Some(remote_session_id) {
                    obj.insert("session_id".into(), serde_json::json!(external_session_id));
                }
            }
            let scope = scope.as_ref().map(|s| {
                if s == remote_session_id {
                    external_session_id.to_string()
                } else {
                    s.clone()
                }
            });
            Frame::Event {
                event: event.clone(),
                scope,
                data,
            }
        }
        other => other.clone(),
    }
}

// ---------------------------------------------------------------------------
// Session-scoped RPC methods that need routing
// ---------------------------------------------------------------------------

/// RPC methods that are scoped to a session and should be routed.
pub const SESSION_SCOPED_METHODS: &[&str] = &[
    "session.prompt",
    "session.steer",
    "session.interrupt",
    "session.close",
    "session.set_permission_mode",
    "session.set_plan_mode",
    "session.set_model",
    "session.set_fallback_model",
    "session.set_prompt_queue",
    "session.btw",
    "session.btw_cancel",
    "session.btw_delete",
    "session.resume",
    "session.context",
    "session.history",
    "session.compact",
    "session.summary",
    "approval.respond",
    "question.respond",
    "goal.set",
    "goal.cancel",
    "goal.complete",
    "goal.judge.chat",
];

/// Check if a method is session-scoped and extract the session_id from params.
pub fn extract_session_id(params: &Option<serde_json::Value>) -> Option<String> {
    params
        .as_ref()
        .and_then(|p| p.get("session_id"))
        .and_then(|v| v.as_str())
        .map(str::to_string)
}

// ---------------------------------------------------------------------------
// Version compatibility
// ---------------------------------------------------------------------------

/// Compare local and remote kkagent versions.
///
/// Policy:
/// - Same version → silent.
/// - Same major.minor, different patch → warn (should still work).
/// - Different major.minor → error-level warning (RPC schema may have changed).
/// - "unknown" remote → warn (old server without version in runtime.status).
fn check_version_compatibility(local: &str, remote: &str, host: &str) {
    if remote == "unknown" {
        tracing::warn!(
            %host,
            local_version = local,
            "remote kkagent server did not report its version; \
             please upgrade the remote kkagent to {local}"
        );
        eprintln!(
            "⚠ Remote kkagent on {host} did not report its version. \
             Upgrade to {local} recommended."
        );
        return;
    }

    if local == remote {
        tracing::info!(%host, version = local, "remote server version matches");
        return;
    }

    let local_parts: Vec<&str> = local.split('.').collect();
    let remote_parts: Vec<&str> = remote.split('.').collect();

    let same_major_minor = local_parts.len() >= 2
        && remote_parts.len() >= 2
        && local_parts[0] == remote_parts[0]
        && local_parts[1] == remote_parts[1];

    if same_major_minor {
        tracing::warn!(
            %host,
            local_version = local,
            remote_version = remote,
            "remote server patch version differs; should be compatible"
        );
        eprintln!(
            "⚠ Remote kkagent on {host} is {remote} (local: {local}). \
             Patch difference only — should work."
        );
    } else {
        tracing::error!(
            %host,
            local_version = local,
            remote_version = remote,
            "remote server version is incompatible; RPC errors may occur"
        );
        eprintln!(
            "⚠ Remote kkagent on {host} is {remote} (local: {local}). \
             Major/minor version mismatch — upgrade the remote kkagent."
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn translate_event_rewrites_scope_and_data_session_id() {
        let frame = Frame::Event {
            event: "message.updated".into(),
            scope: Some("remote-abc".into()),
            data: serde_json::json!({"session_id": "remote-abc", "n": 1}),
        };
        let translated = translate_event_session_id(&frame, "remote-abc", "r1-external");
        match translated {
            Frame::Event { scope, data, .. } => {
                assert_eq!(scope.as_deref(), Some("r1-external"));
                assert_eq!(data["session_id"], "r1-external");
                assert_eq!(data["n"], 1);
            }
            other => panic!("unexpected frame: {other:?}"),
        }
    }

    #[test]
    fn translate_event_leaves_foreign_scope_alone() {
        let frame = Frame::Event {
            event: "server.status".into(),
            scope: Some("other-scope".into()),
            data: serde_json::json!({"session_id": "remote-abc"}),
        };
        let translated = translate_event_session_id(&frame, "remote-abc", "r1-external");
        match translated {
            Frame::Event { scope, data, .. } => {
                assert_eq!(scope.as_deref(), Some("other-scope"));
                assert_eq!(data["session_id"], "r1-external");
            }
            other => panic!("unexpected frame: {other:?}"),
        }
    }

    #[test]
    fn expand_remote_tilde_handles_all_forms() {
        let home = Path::new("/home/dev");
        assert_eq!(expand_remote_tilde("~", home), "/home/dev");
        assert_eq!(expand_remote_tilde("~/code", home), "/home/dev/code");
        assert_eq!(expand_remote_tilde("~/code/", home), "/home/dev/code");
        // Absolute paths are only normalized, never joined with home.
        assert_eq!(expand_remote_tilde("/data/aosp/", home), "/data/aosp");
        assert_eq!(expand_remote_tilde("/data/aosp", home), "/data/aosp");
        assert_eq!(expand_remote_tilde("~other", home), "~other");
    }

    #[test]
    fn extract_session_id_reads_params() {
        assert_eq!(
            extract_session_id(&Some(serde_json::json!({"session_id": "s1"}))),
            Some("s1".to_string())
        );
        assert_eq!(extract_session_id(&None), None);
        assert_eq!(
            extract_session_id(&Some(serde_json::json!({"other": 1}))),
            None
        );
    }
}
