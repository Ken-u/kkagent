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
    pub async fn establish(host: &str) -> Result<Self> {
        let socket_dir = kkagent_config::default_config_dir().join("ssh");
        std::fs::create_dir_all(&socket_dir)?;
        let socket_path = socket_dir.join(format!("ctrl-{host}"));

        // If a ControlMaster already exists and is alive, reuse it.
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

        let child = Command::new("ssh")
            .arg("-M") // ControlMaster
            .arg("-N") // no remote command
            .arg("-S")
            .arg(&socket_path)
            .arg("-o")
            .arg("ControlPersist=yes")
            .arg("-o")
            .arg("ServerAliveInterval=30")
            .arg("-o")
            .arg("ServerAliveCountMax=3")
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

/// Check whether a remote kkagent server is running; start one if not.
///
/// Returns Ok(()) when the remote server is known to be reachable.
pub async fn ensure_remote_server(ssh_socket: &Path, host: &str) -> Result<()> {
    // Try to reach the remote server via a quick bridge probe.
    let probe = Command::new("ssh")
        .arg("-S")
        .arg(ssh_socket)
        .arg("-o")
        .arg("BatchMode=yes")
        .arg(host)
        .arg("kkagent")
        .arg("server")
        .arg("status")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await?;

    if probe.status.success() {
        eprintln!("Remote kkagent server on {host} is running");
        return Ok(());
    }

    eprintln!("Starting remote kkagent server on {host}...");

    // Start a daemonized server on the remote host.
    let start = Command::new("ssh")
        .arg("-S")
        .arg(ssh_socket)
        .arg("-o")
        .arg("BatchMode=yes")
        .arg(host)
        .args([
            "sh",
            "-c",
            "nohup kkagent server </dev/null >/dev/null 2>&1 &",
        ])
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .status()
        .await?;

    if !start.success() {
        anyhow::bail!("failed to start remote kkagent server on {host}");
    }

    // Wait for the remote server to become ready.
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    let mut delay = Duration::from_millis(200);
    loop {
        let check = Command::new("ssh")
            .arg("-S")
            .arg(ssh_socket)
            .arg("-o")
            .arg("BatchMode=yes")
            .arg(host)
            .arg("kkagent")
            .arg("server")
            .arg("status")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
        if check.is_ok_and(|s| s.success()) {
            eprintln!("Remote kkagent server on {host} is ready");
            return Ok(());
        }
        if tokio::time::Instant::now() > deadline {
            anyhow::bail!("remote kkagent server on {host} did not become ready within 10s");
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
    /// Background reconnect task handle.
    reconnect_handle: Mutex<Option<tokio::task::JoinHandle<()>>>,
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
            reconnect_handle: Mutex::new(None),
        })
    }

    pub fn connection_state(&self) -> RemoteConnectionState {
        self.state_rx.borrow().clone()
    }

    #[allow(dead_code)]
    pub fn state_receiver(&self) -> watch::Receiver<RemoteConnectionState> {
        self.state_rx.clone()
    }

    /// Establish (or re-establish) the SSH bridge connection.
    pub async fn connect(self: &Arc<Self>) -> Result<()> {
        self.state.send_replace(RemoteConnectionState::Connecting);

        // Kill any existing bridge child.
        if let Some(mut old_child) = self.bridge_child.lock().await.take() {
            let _ = old_child.kill().await;
        }

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
            .arg("kkagent")
            .arg("bridge")
            .arg("--stdio")
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
        tokio::spawn(async move {
            while let Some(frame) = event_rx.recv().await {
                if event_fwd_tx.send((server_id.clone(), frame)).await.is_err() {
                    break;
                }
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
        let handle = tokio::spawn(async move {
            let mut delay = Duration::from_secs(2);
            let max_delay = Duration::from_secs(60);

            for attempt in 1u32.. {
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
                        conn.state
                            .send_replace(RemoteConnectionState::AuthenticationRequired);
                        return;
                    }
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
        // Fire-and-forget; store handle so we can cancel if needed.
        let conn = Arc::clone(self);
        tokio::spawn(async move {
            *conn.reconnect_handle.lock().await = Some(handle);
        });
    }
}

// ---------------------------------------------------------------------------
// Remote Server Registry (lives in local ServerState)
// ---------------------------------------------------------------------------

/// Manages all remote server connections and workspace/session routing.
pub struct RemoteRegistry {
    connections: RwLock<HashMap<String, Arc<RemoteServerConnection>>>,
    /// Maps workspace paths to server IDs. Local workspaces are not in this map.
    workspace_owners: RwLock<HashMap<String, String>>,
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
    pub async fn register_workspace(&self, workspace_key: &str, server_id: &str) {
        self.workspace_owners
            .write()
            .await
            .insert(workspace_key.to_string(), server_id.to_string());
    }

    /// Get the server ID that owns a workspace, or None for local.
    pub async fn workspace_server(&self, workspace_key: &str) -> Option<String> {
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
        for (path, server_id) in owners.iter() {
            if let Some(conn) = conns.get(server_id) {
                result.push(RemoteWorkspaceInfo {
                    server_id: server_id.clone(),
                    host: conn.host.clone(),
                    path: path.clone(),
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
    #[allow(dead_code)]
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

/// Translate session IDs in a remote event frame to local opaque IDs.
pub fn translate_event_session_id(frame: &Frame, external_session_id: &str) -> Frame {
    match frame {
        Frame::Event { event, scope, data } => {
            let mut data = data.clone();
            if let Some(obj) = data.as_object_mut() {
                if obj.contains_key("session_id") {
                    obj.insert("session_id".into(), serde_json::json!(external_session_id));
                }
            }
            let scope = scope.as_ref().map(|s| {
                // If scope is the remote session ID, replace with external ID.
                if s == external_session_id {
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
