//! Kaos — local + SSH remote execution environment (ref/kaos subset).
//!
//! Decouples bash/filesystem tools from the host OS so the same `Environment`
//! trait can target local processes or a remote SSH host.

use async_trait::async_trait;
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;
use tokio::process::Command;

#[derive(Debug, thiserror::Error)]
pub enum KaosError {
    #[error("io: {0}")]
    Io(String),
    #[error("ssh: {0}")]
    Ssh(String),
    #[error("not found: {0}")]
    NotFound(String),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

#[derive(Debug, Clone)]
pub struct ExecResult {
    pub status: i32,
    pub stdout: String,
    pub stderr: String,
    /// Working directory after the command (best-effort; local tracks `cd`).
    pub cwd: Option<PathBuf>,
}

#[derive(Debug, Clone)]
pub struct SshTarget {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub identity_file: Option<PathBuf>,
    pub password: Option<String>,
    /// Optional remote starting directory.
    pub remote_cwd: Option<PathBuf>,
}

impl Default for SshTarget {
    fn default() -> Self {
        Self {
            host: "localhost".into(),
            port: 22,
            username: whoami_user(),
            identity_file: None,
            password: None,
            remote_cwd: None,
        }
    }
}

fn whoami_user() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("USERNAME"))
        .unwrap_or_else(|_| "user".into())
}

#[async_trait]
pub trait Environment: Send + Sync {
    async fn exec(&self, command: &str, cwd: Option<&Path>) -> Result<ExecResult, KaosError>;
    async fn read_file(&self, path: &Path) -> Result<Vec<u8>, KaosError>;
    async fn write_file(&self, path: &Path, data: &[u8]) -> Result<(), KaosError>;
    async fn exists(&self, path: &Path) -> Result<bool, KaosError>;
    fn kind(&self) -> &'static str;
    fn cwd(&self) -> PathBuf;
    fn set_env(&self, key: &str, value: &str);
    fn env_snapshot(&self) -> HashMap<String, String>;
}

/// Probe login-shell PATH from common profile files (best-effort, no shell spawn).
pub fn detect_login_shell_path() -> Option<String> {
    let mut parts: Vec<String> = Vec::new();

    #[cfg(target_os = "macos")]
    {
        if let Ok(text) = std::fs::read_to_string("/etc/paths") {
            for line in text.lines() {
                let line = line.trim();
                if !line.is_empty() && Path::new(line).is_dir() {
                    parts.push(line.to_string());
                }
            }
        }
        if let Ok(rd) = std::fs::read_dir("/etc/paths.d") {
            let mut entries: Vec<_> = rd.flatten().collect();
            entries.sort_by_key(|e| e.file_name());
            for entry in entries {
                if let Ok(text) = std::fs::read_to_string(entry.path()) {
                    for line in text.lines() {
                        let line = line.trim();
                        if !line.is_empty() && Path::new(line).is_dir() {
                            parts.push(line.to_string());
                        }
                    }
                }
            }
        }
    }

    if let Some(home) = dirs_home() {
        for rel in [
            ".zshenv",
            ".zprofile",
            ".zshrc",
            ".bash_profile",
            ".bashrc",
            ".profile",
        ] {
            let path = home.join(rel);
            if let Ok(text) = std::fs::read_to_string(&path) {
                for line in text.lines() {
                    if let Some(extra) = parse_path_export(line) {
                        for p in extra.split(':') {
                            if !p.is_empty() && !parts.iter().any(|x| x == p) {
                                parts.push(p.to_string());
                            }
                        }
                    }
                }
            }
        }
    }

    // Always keep the current process PATH segments that still exist.
    if let Ok(current) = std::env::var("PATH") {
        for p in current.split(':') {
            if !p.is_empty() && !parts.iter().any(|x| x == p) {
                parts.push(p.to_string());
            }
        }
    }

    if parts.is_empty() {
        None
    } else {
        Some(parts.join(":"))
    }
}

fn dirs_home() -> Option<PathBuf> {
    std::env::var_os("HOME")
        .or_else(|| std::env::var_os("USERPROFILE"))
        .map(PathBuf::from)
}

fn parse_path_export(line: &str) -> Option<String> {
    let line = line.trim();
    if line.starts_with('#') {
        return None;
    }
    // export PATH=... or PATH=...
    let rest = line.strip_prefix("export ").unwrap_or(line).trim_start();
    let rest = rest.strip_prefix("PATH=")?;
    let rest = rest.trim().trim_matches('"').trim_matches('\'');
    // Expand $HOME / ~
    let home = dirs_home().unwrap_or_default();
    let expanded = rest
        .replace("$HOME", &home.to_string_lossy())
        .replace("~", &home.to_string_lossy());
    // Drop `$PATH` references — caller merges current PATH separately.
    let cleaned = expanded
        .split(':')
        .filter(|p| *p != "$PATH" && *p != "${PATH}" && !p.is_empty())
        .collect::<Vec<_>>()
        .join(":");
    if cleaned.is_empty() {
        None
    } else {
        Some(cleaned)
    }
}

pub struct LocalKaos {
    root: RwLock<PathBuf>,
    env: RwLock<HashMap<String, String>>,
    login_path: Option<String>,
}

impl LocalKaos {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        let mut env = HashMap::new();
        let login_path = detect_login_shell_path();
        if let Some(path) = login_path.clone() {
            env.insert("PATH".into(), path);
        } else if let Ok(path) = std::env::var("PATH") {
            env.insert("PATH".into(), path);
        }
        Self {
            root: RwLock::new(root.into()),
            env: RwLock::new(env),
            login_path,
        }
    }

    pub fn cwd() -> Self {
        Self::new(std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")))
    }

    pub fn login_path(&self) -> Option<&str> {
        self.login_path.as_deref()
    }

    pub fn set_cwd(&self, path: impl Into<PathBuf>) {
        *self.root.write().unwrap_or_else(|e| e.into_inner()) = path.into();
    }
}

#[async_trait]
impl Environment for LocalKaos {
    async fn exec(&self, command: &str, cwd: Option<&Path>) -> Result<ExecResult, KaosError> {
        let dir = cwd.map(Path::to_path_buf).unwrap_or_else(|| self.cwd());
        let env = self.env_snapshot();
        let mut cmd = if cfg!(windows) {
            let mut c = Command::new("cmd");
            c.args(["/C", command]);
            c
        } else {
            let mut c = Command::new("bash");
            c.args(["-lc", command]);
            c
        };
        cmd.current_dir(&dir);
        for (k, v) in &env {
            cmd.env(k, v);
        }
        let output = cmd
            .output()
            .await
            .map_err(|e| KaosError::Io(e.to_string()))?;

        // Best-effort cwd tracking for simple `cd <path>` prefixes.
        if let Some(new_cwd) = track_cd(command, &dir) {
            self.set_cwd(new_cwd.clone());
            return Ok(ExecResult {
                status: output.status.code().unwrap_or(-1),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                cwd: Some(new_cwd),
            });
        }

        Ok(ExecResult {
            status: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            cwd: Some(dir),
        })
    }

    async fn read_file(&self, path: &Path) -> Result<Vec<u8>, KaosError> {
        let p = resolve_path(&self.cwd(), path);
        tokio::fs::read(&p)
            .await
            .map_err(|e| KaosError::Io(e.to_string()))
    }

    async fn write_file(&self, path: &Path, data: &[u8]) -> Result<(), KaosError> {
        let p = resolve_path(&self.cwd(), path);
        if let Some(parent) = p.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| KaosError::Io(e.to_string()))?;
        }
        tokio::fs::write(&p, data)
            .await
            .map_err(|e| KaosError::Io(e.to_string()))
    }

    async fn exists(&self, path: &Path) -> Result<bool, KaosError> {
        let p = resolve_path(&self.cwd(), path);
        Ok(tokio::fs::try_exists(&p).await.unwrap_or(false))
    }

    fn kind(&self) -> &'static str {
        "local"
    }

    fn cwd(&self) -> PathBuf {
        self.root.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    fn set_env(&self, key: &str, value: &str) {
        self.env
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key.to_string(), value.to_string());
    }

    fn env_snapshot(&self) -> HashMap<String, String> {
        self.env.read().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

fn resolve_path(root: &Path, path: &Path) -> PathBuf {
    if path.is_absolute() {
        path.to_path_buf()
    } else {
        root.join(path)
    }
}

fn track_cd(command: &str, base: &Path) -> Option<PathBuf> {
    let trimmed = command.trim();
    let rest = trimmed.strip_prefix("cd ")?.trim();
    let target = rest.split_whitespace().next()?;
    if target.is_empty() || target.contains('&') || target.contains('|') || target.contains(';') {
        return None;
    }
    let path = if target.starts_with('/') || (cfg!(windows) && target.contains(':')) {
        PathBuf::from(target)
    } else {
        base.join(target)
    };
    if path.is_dir() {
        Some(path)
    } else {
        None
    }
}

/// SSH kaos via system `ssh`/`scp` (portable win/mac/linux).
pub struct SshKaos {
    pub target: SshTarget,
    cwd: RwLock<Option<PathBuf>>,
    env: RwLock<HashMap<String, String>>,
}

impl SshKaos {
    pub fn new(target: SshTarget) -> Self {
        let cwd = RwLock::new(target.remote_cwd.clone());
        Self {
            target,
            cwd,
            env: RwLock::new(HashMap::new()),
        }
    }

    fn ssh_base(&self) -> Command {
        let mut cmd = Command::new("ssh");
        cmd.arg("-p")
            .arg(self.target.port.to_string())
            .arg("-o")
            .arg("BatchMode=yes")
            .arg("-o")
            .arg("StrictHostKeyChecking=accept-new")
            .arg("-o")
            .arg("ConnectTimeout=10");
        if let Some(id) = &self.target.identity_file {
            cmd.arg("-i").arg(id);
        }
        cmd.arg(format!("{}@{}", self.target.username, self.target.host));
        cmd
    }

    fn wrap_remote(&self, command: &str, cwd: Option<&Path>) -> String {
        let mut parts = Vec::new();
        let env = self.env_snapshot();
        for (k, v) in env {
            parts.push(format!("export {}={}", k, shell_quote(&v)));
        }
        let dir = cwd
            .map(Path::to_path_buf)
            .or_else(|| self.cwd.read().ok().and_then(|g| g.clone()));
        if let Some(dir) = dir {
            parts.push(format!(
                "cd {} || exit 1",
                shell_quote(&dir.to_string_lossy())
            ));
        }
        parts.push(command.to_string());
        parts.join("; ")
    }
}

#[async_trait]
impl Environment for SshKaos {
    async fn exec(&self, command: &str, cwd: Option<&Path>) -> Result<ExecResult, KaosError> {
        let remote = self.wrap_remote(command, cwd);
        let output = self
            .ssh_base()
            .arg(remote)
            .output()
            .await
            .map_err(|e| KaosError::Ssh(e.to_string()))?;
        let result_cwd = cwd
            .map(Path::to_path_buf)
            .or_else(|| self.cwd.read().ok().and_then(|g| g.clone()));
        if let Some(new_cwd) = track_cd(command, result_cwd.as_deref().unwrap_or(Path::new("/"))) {
            *self.cwd.write().unwrap_or_else(|e| e.into_inner()) = Some(new_cwd.clone());
            return Ok(ExecResult {
                status: output.status.code().unwrap_or(-1),
                stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
                stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
                cwd: Some(new_cwd),
            });
        }
        Ok(ExecResult {
            status: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            cwd: result_cwd,
        })
    }

    async fn read_file(&self, path: &Path) -> Result<Vec<u8>, KaosError> {
        let remote = format!(
            "{}@{}:{}",
            self.target.username,
            self.target.host,
            path.display()
        );
        let tmp = std::env::temp_dir().join(format!("kkagent-scp-{}", uuid_lite()));
        let mut cmd = Command::new("scp");
        cmd.arg("-P").arg(self.target.port.to_string());
        cmd.arg("-o").arg("BatchMode=yes");
        cmd.arg("-o").arg("ConnectTimeout=10");
        if let Some(id) = &self.target.identity_file {
            cmd.arg("-i").arg(id);
        }
        let output = cmd
            .arg(&remote)
            .arg(&tmp)
            .output()
            .await
            .map_err(|e| KaosError::Ssh(e.to_string()))?;
        if !output.status.success() {
            return Err(KaosError::Ssh(
                String::from_utf8_lossy(&output.stderr).into(),
            ));
        }
        let data = tokio::fs::read(&tmp)
            .await
            .map_err(|e| KaosError::Io(e.to_string()))?;
        let _ = tokio::fs::remove_file(&tmp).await;
        Ok(data)
    }

    async fn write_file(&self, path: &Path, data: &[u8]) -> Result<(), KaosError> {
        let tmp = std::env::temp_dir().join(format!("kkagent-scp-{}", uuid_lite()));
        tokio::fs::write(&tmp, data)
            .await
            .map_err(|e| KaosError::Io(e.to_string()))?;
        // Ensure remote parent exists.
        if let Some(parent) = path.parent() {
            let _ = self
                .exec(
                    &format!("mkdir -p {}", shell_quote(&parent.to_string_lossy())),
                    None,
                )
                .await;
        }
        let remote = format!(
            "{}@{}:{}",
            self.target.username,
            self.target.host,
            path.display()
        );
        let mut cmd = Command::new("scp");
        cmd.arg("-P").arg(self.target.port.to_string());
        cmd.arg("-o").arg("BatchMode=yes");
        cmd.arg("-o").arg("ConnectTimeout=10");
        if let Some(id) = &self.target.identity_file {
            cmd.arg("-i").arg(id);
        }
        let output = cmd
            .arg(&tmp)
            .arg(&remote)
            .output()
            .await
            .map_err(|e| KaosError::Ssh(e.to_string()))?;
        let _ = tokio::fs::remove_file(&tmp).await;
        if !output.status.success() {
            return Err(KaosError::Ssh(
                String::from_utf8_lossy(&output.stderr).into(),
            ));
        }
        Ok(())
    }

    async fn exists(&self, path: &Path) -> Result<bool, KaosError> {
        let r = self
            .exec(
                &format!(
                    "test -e {} && echo yes || echo no",
                    shell_quote(&path.to_string_lossy())
                ),
                None,
            )
            .await?;
        Ok(r.stdout.trim() == "yes")
    }

    fn kind(&self) -> &'static str {
        "ssh"
    }

    fn cwd(&self) -> PathBuf {
        self.cwd
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
            .unwrap_or_else(|| PathBuf::from("~"))
    }

    fn set_env(&self, key: &str, value: &str) {
        self.env
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key.to_string(), value.to_string());
    }

    fn env_snapshot(&self) -> HashMap<String, String> {
        self.env.read().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

fn shell_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

fn uuid_lite() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let t = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    format!("{t:x}")
}

/// Build an [`Environment`] from optional SSH remote config.
pub fn environment_from_remote(remote: Option<&RemoteConfig>, local_root: PathBuf) -> KaosHandle {
    match remote {
        Some(cfg) if cfg.enabled => {
            let target = SshTarget {
                host: cfg.host.clone(),
                port: cfg.port,
                username: cfg.user.clone().unwrap_or_else(whoami_user),
                identity_file: cfg.identity_file.as_ref().map(PathBuf::from),
                password: None,
                remote_cwd: cfg.remote_cwd.as_ref().map(PathBuf::from),
            };
            KaosHandle::Ssh(std::sync::Arc::new(SshKaos::new(target)))
        }
        _ => KaosHandle::Local(std::sync::Arc::new(LocalKaos::new(local_root))),
    }
}

/// Owned handle that is dyn-free and easy to clone into tools.
#[derive(Clone)]
pub enum KaosHandle {
    Local(std::sync::Arc<LocalKaos>),
    Ssh(std::sync::Arc<SshKaos>),
}

impl KaosHandle {
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Local(k) => k.kind(),
            Self::Ssh(k) => k.kind(),
        }
    }

    pub fn as_environment(&self) -> &dyn Environment {
        match self {
            Self::Local(k) => k.as_ref(),
            Self::Ssh(k) => k.as_ref(),
        }
    }

    pub fn local_login_path(&self) -> Option<String> {
        match self {
            Self::Local(k) => k.login_path().map(str::to_string),
            Self::Ssh(_) => None,
        }
    }

    pub async fn exec(&self, command: &str, cwd: Option<&Path>) -> Result<ExecResult, KaosError> {
        self.as_environment().exec(command, cwd).await
    }
}

impl std::fmt::Debug for KaosHandle {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("KaosHandle")
            .field("kind", &self.kind())
            .finish()
    }
}

/// Optional `[remote]` SSH session target (mirrors config `[remote]`).
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize, Default)]
pub struct RemoteConfig {
    #[serde(default)]
    pub enabled: bool,
    #[serde(default)]
    pub host: String,
    #[serde(default = "default_ssh_port")]
    pub port: u16,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub identity_file: Option<String>,
    #[serde(default)]
    pub remote_cwd: Option<String>,
}

fn default_ssh_port() -> u16 {
    22
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_echo_and_cwd_track() {
        let k = LocalKaos::cwd();
        let r = k.exec("echo hello-kaos", None).await.unwrap();
        assert!(r.stdout.contains("hello-kaos"));
        assert_eq!(k.kind(), "local");
        assert!(k.login_path().is_some() || std::env::var("PATH").is_ok());
    }

    #[tokio::test]
    async fn local_env_injection() {
        let k = LocalKaos::cwd();
        k.set_env("KKAGENT_KAOS_TEST", "1");
        let r = k
            .exec(
                if cfg!(windows) {
                    "echo %KKAGENT_KAOS_TEST%"
                } else {
                    "echo $KKAGENT_KAOS_TEST"
                },
                None,
            )
            .await
            .unwrap();
        assert!(r.stdout.contains('1'), "{:?}", r.stdout);
    }

    #[tokio::test]
    async fn local_file_roundtrip() {
        let dir = std::env::temp_dir().join(format!("kkagent-kaos-{}", uuid_lite()));
        std::fs::create_dir_all(&dir).unwrap();
        let k = LocalKaos::new(&dir);
        k.write_file(Path::new("note.txt"), b"hi").await.unwrap();
        assert!(k.exists(Path::new("note.txt")).await.unwrap());
        let data = k.read_file(Path::new("note.txt")).await.unwrap();
        assert_eq!(data, b"hi");
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn detects_login_path_or_falls_back() {
        let path = detect_login_shell_path();
        assert!(path.is_some());
    }

    #[test]
    fn parse_path_export_expands_home() {
        let home = dirs_home().unwrap();
        let line = format!("export PATH=\"{}/bin:$PATH\"", home.display());
        let parsed = parse_path_export(&line).unwrap();
        assert!(parsed.contains("bin"));
        assert!(!parsed.contains("$PATH"));
    }

    #[test]
    fn remote_config_defaults_disabled() {
        let cfg = RemoteConfig::default();
        assert!(!cfg.enabled);
        let env = environment_from_remote(Some(&cfg), PathBuf::from("."));
        assert_eq!(env.kind(), "local");
        assert_eq!(env.as_environment().kind(), "local");
    }
}
