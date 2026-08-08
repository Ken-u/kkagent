use tokio::io::{DuplexStream, duplex};

pub fn create_memory_pair() -> (DuplexStream, DuplexStream) {
    duplex(64 * 1024)
}
