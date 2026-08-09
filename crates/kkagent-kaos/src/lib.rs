//! Kaos — local + SSH remote execution environment (ref/kaos subset).

use async_trait::async_trait;
use std::path::{Path, PathBuf};
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
}

#[derive(Debug, Clone)]
pub struct SshTarget {
    pub host: String,
    pub port: u16,
    pub username: String,
    pub identity_file: Option<PathBuf>,
    pub password: Option<String>,
}

impl Default for SshTarget {
    fn default() -> Self {
        Self {
            host: "localhost".into(),
            port: 22,
            username: whoami_user(),
            identity_file: None,
            password: None,
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
}

pub struct LocalKaos {
    pub root: PathBuf,
}

impl LocalKaos {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn cwd() -> Self {
        Self {
            root: std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
        }
    }
}

#[async_trait]
impl Environment for LocalKaos {
    async fn exec(&self, command: &str, cwd: Option<&Path>) -> Result<ExecResult, KaosError> {
        let dir = cwd.unwrap_or(&self.root);
        let output = if cfg!(windows) {
            Command::new("cmd")
                .args(["/C", command])
                .current_dir(dir)
                .output()
                .await
        } else {
            Command::new("sh")
                .args(["-lc", command])
                .current_dir(dir)
                .output()
                .await
        }
        .map_err(|e| KaosError::Io(e.to_string()))?;
        Ok(ExecResult {
            status: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
        })
    }

    async fn read_file(&self, path: &Path) -> Result<Vec<u8>, KaosError> {
        let p = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        };
        tokio::fs::read(&p)
            .await
            .map_err(|e| KaosError::Io(e.to_string()))
    }

    async fn write_file(&self, path: &Path, data: &[u8]) -> Result<(), KaosError> {
        let p = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        };
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
        let p = if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.root.join(path)
        };
        Ok(tokio::fs::try_exists(&p).await.unwrap_or(false))
    }

    fn kind(&self) -> &'static str {
        "local"
    }
}

/// SSH kaos via system `ssh`/`scp` (portable win/mac/linux).
pub struct SshKaos {
    pub target: SshTarget,
}

impl SshKaos {
    pub fn new(target: SshTarget) -> Self {
        Self { target }
    }

    fn ssh_base(&self) -> Command {
        let mut cmd = Command::new("ssh");
        cmd.arg("-p")
            .arg(self.target.port.to_string())
            .arg("-o")
            .arg("BatchMode=yes")
            .arg("-o")
            .arg("StrictHostKeyChecking=accept-new");
        if let Some(id) = &self.target.identity_file {
            cmd.arg("-i").arg(id);
        }
        cmd.arg(format!("{}@{}", self.target.username, self.target.host));
        cmd
    }
}

#[async_trait]
impl Environment for SshKaos {
    async fn exec(&self, command: &str, cwd: Option<&Path>) -> Result<ExecResult, KaosError> {
        let remote = if let Some(dir) = cwd {
            format!("cd {} && {}", shell_quote(&dir.to_string_lossy()), command)
        } else {
            command.to_string()
        };
        let output = self
            .ssh_base()
            .arg(remote)
            .output()
            .await
            .map_err(|e| KaosError::Ssh(e.to_string()))?;
        Ok(ExecResult {
            status: output.status.code().unwrap_or(-1),
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
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
            return Err(KaosError::Ssh(String::from_utf8_lossy(&output.stderr).into()));
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
        let remote = format!(
            "{}@{}:{}",
            self.target.username,
            self.target.host,
            path.display()
        );
        let mut cmd = Command::new("scp");
        cmd.arg("-P").arg(self.target.port.to_string());
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
            return Err(KaosError::Ssh(String::from_utf8_lossy(&output.stderr).into()));
        }
        Ok(())
    }

    async fn exists(&self, path: &Path) -> Result<bool, KaosError> {
        let r = self
            .exec(
                &format!("test -e {} && echo yes || echo no", shell_quote(&path.to_string_lossy())),
                None,
            )
            .await?;
        Ok(r.stdout.trim() == "yes")
    }

    fn kind(&self) -> &'static str {
        "ssh"
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_echo() {
        let k = LocalKaos::cwd();
        let r = k.exec("echo hello-kaos", None).await.unwrap();
        assert!(r.stdout.contains("hello-kaos"));
        assert_eq!(k.kind(), "local");
    }
}
