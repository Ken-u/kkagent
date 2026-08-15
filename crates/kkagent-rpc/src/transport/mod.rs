use std::path::PathBuf;
use tokio::io::{AsyncRead, AsyncWrite};

pub mod memory;
pub mod uds;

pub use uds::{try_connect_uds, ConnectError};

#[derive(Debug, Clone)]
pub enum TransportConfig {
    Memory,
    Uds {
        path: PathBuf,
    },
    #[cfg(windows)]
    NamedPipe {
        name: String,
    },
}

pub trait AsyncTransport: AsyncRead + AsyncWrite + Unpin + Send + 'static {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send + 'static> AsyncTransport for T {}
