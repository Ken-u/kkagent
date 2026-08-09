use tokio::io::{duplex, DuplexStream};

pub fn create_memory_pair() -> (DuplexStream, DuplexStream) {
    duplex(64 * 1024)
}
