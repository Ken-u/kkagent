use std::path::Path;
use tokio::net::{UnixListener, UnixStream};
use anyhow::Result;

pub async fn connect_uds(path: &Path) -> Result<UnixStream> {
    let stream = UnixStream::connect(path).await?;
    Ok(stream)
}

pub fn bind_uds(path: &Path) -> Result<UnixListener> {
    if path.exists() {
        std::fs::remove_file(path)?;
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let listener = UnixListener::bind(path)?;
    Ok(listener)
}
