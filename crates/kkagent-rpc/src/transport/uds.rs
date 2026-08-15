use anyhow::{Context, Result};
use std::path::Path;
use thiserror::Error;

#[cfg(windows)]
pub use tokio::net::{TcpListener as LocalListener, TcpStream as LocalStream};
#[cfg(unix)]
pub use tokio::net::{UnixListener as LocalListener, UnixStream as LocalStream};

/// Errors from [`try_connect_uds`], including automatic stale-endpoint cleanup.
#[derive(Debug, Error)]
pub enum ConnectError {
    #[error("stale local endpoint at {path}: {source}")]
    StaleSocket {
        path: String,
        #[source]
        source: anyhow::Error,
    },
    #[error("local endpoint not found at {path}: {source}")]
    NotFound {
        path: String,
        #[source]
        source: anyhow::Error,
    },
}

/// Connect to kkagent's local IPC endpoint.
///
/// Unix uses a native domain socket. Windows uses a loopback TCP listener whose
/// randomly assigned address is stored in the endpoint file. Keeping the public
/// abstraction identical lets CLI/server behavior remain portable without
/// exposing the listener beyond the local machine.
#[cfg(unix)]
pub async fn connect_uds(path: &Path) -> Result<LocalStream> {
    LocalStream::connect(path)
        .await
        .with_context(|| format!("failed to connect to local socket {}", path.display()))
}

#[cfg(windows)]
pub async fn connect_uds(path: &Path) -> Result<LocalStream> {
    let address = tokio::fs::read_to_string(path)
        .await
        .with_context(|| format!("failed to read local endpoint {}", path.display()))?;
    LocalStream::connect(address.trim())
        .await
        .with_context(|| format!("failed to connect to local endpoint {}", path.display()))
}

/// Try to connect; if the endpoint file exists but is dead, remove it and
/// return [`ConnectError::StaleSocket`].
pub async fn try_connect_uds(path: &Path) -> Result<LocalStream, ConnectError> {
    match connect_uds(path).await {
        Ok(stream) => Ok(stream),
        Err(error) if path.exists() => {
            let _ = remove_stale_endpoint(path);
            Err(ConnectError::StaleSocket {
                path: path.display().to_string(),
                source: error,
            })
        }
        Err(error) => Err(ConnectError::NotFound {
            path: path.display().to_string(),
            source: error,
        }),
    }
}

#[cfg(unix)]
pub fn bind_uds(path: &Path) -> Result<LocalListener> {
    prepare_endpoint(path)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    LocalListener::bind(path)
        .with_context(|| format!("failed to bind local socket {}", path.display()))
}

#[cfg(windows)]
pub fn bind_uds(path: &Path) -> Result<LocalListener> {
    prepare_endpoint(path)?;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))?;
    listener.set_nonblocking(true)?;
    let address = listener.local_addr()?;
    write_endpoint_file(path, address.to_string().as_bytes())?;
    LocalListener::from_std(listener).context("failed to initialize Windows local listener")
}

fn remove_stale_endpoint(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(unix)]
fn prepare_endpoint(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    match std::os::unix::net::UnixStream::connect(path) {
        Ok(_) => anyhow::bail!(
            "a kkagent server is already listening at {}",
            path.display()
        ),
        Err(error)
            if matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionRefused | std::io::ErrorKind::NotFound
            ) =>
        {
            remove_stale_endpoint(path)
        }
        Err(error) => {
            Err(error).with_context(|| format!("cannot inspect local socket {}", path.display()))
        }
    }
}

#[cfg(windows)]
fn prepare_endpoint(path: &Path) -> Result<()> {
    if !path.exists() {
        return Ok(());
    }
    let address = std::fs::read_to_string(path)
        .ok()
        .and_then(|value| value.trim().parse::<std::net::SocketAddr>().ok());
    if let Some(address) = address {
        if std::net::TcpStream::connect_timeout(&address, std::time::Duration::from_millis(250))
            .is_ok()
        {
            anyhow::bail!(
                "a kkagent server is already listening at {}",
                path.display()
            );
        }
    }
    remove_stale_endpoint(path)
}

#[cfg(windows)]
fn write_endpoint_file(path: &Path, contents: &[u8]) -> Result<()> {
    use std::io::Write;
    let temporary = path.with_extension(format!("tmp-{}", uuid::Uuid::new_v4()));
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)?;
    file.write_all(contents)?;
    file.sync_all()?;
    drop(file);
    std::fs::rename(&temporary, path).or_else(|error| {
        let _ = std::fs::remove_file(&temporary);
        Err(error)
    })?;
    Ok(())
}

pub fn remove_endpoint(path: &Path) -> Result<()> {
    remove_stale_endpoint(path)
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[tokio::test]
    async fn local_endpoint_roundtrip() {
        let path = std::env::temp_dir().join(format!("kkagent-ipc-{}.sock", uuid::Uuid::new_v4()));
        let listener = bind_uds(&path).unwrap();
        let client = connect_uds(&path).await.unwrap();
        let (_server, _) = listener.accept().await.unwrap();
        drop(client);
        remove_endpoint(&path).unwrap();
    }

    #[tokio::test]
    async fn refuses_to_unlink_a_live_server() {
        let path = std::env::temp_dir().join(format!("kkagent-ipc-{}.sock", uuid::Uuid::new_v4()));
        let _listener = bind_uds(&path).unwrap();
        let error = bind_uds(&path).unwrap_err().to_string();
        assert!(error.contains("already listening"));
        remove_endpoint(&path).unwrap();
    }

    #[tokio::test]
    async fn try_connect_cleans_stale_socket() {
        let path = std::env::temp_dir().join(format!("kk-stale-{}.sock", std::process::id()));
        // A leftover endpoint file that is not an accepting listener.
        std::fs::write(&path, b"stale").unwrap();
        assert!(path.exists());
        let err = try_connect_uds(&path).await.unwrap_err();
        assert!(matches!(err, ConnectError::StaleSocket { .. }));
        assert!(!path.exists(), "stale socket should be removed");
    }

    #[tokio::test]
    async fn try_connect_not_found_when_missing() {
        let path = std::env::temp_dir().join(format!("kk-miss-{}.sock", std::process::id()));
        let err = try_connect_uds(&path).await.unwrap_err();
        assert!(matches!(err, ConnectError::NotFound { .. }));
    }
}
