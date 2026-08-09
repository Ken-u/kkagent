use bytes::BytesMut;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot, Mutex};

use crate::codec::NdjsonCodec;
use crate::transport::AsyncTransport;
use kkagent_protocol::Frame;

type PendingCall = oneshot::Sender<Result<serde_json::Value, RpcError>>;

#[derive(Debug, thiserror::Error)]
pub enum RpcError {
    #[error("RPC error {code}: {message}")]
    Remote { code: i32, message: String },
    #[error("Connection closed")]
    Closed,
    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),
    #[error("{0}")]
    Other(String),
}

pub struct RpcClient {
    write_tx: mpsc::Sender<Frame>,
    pending: Arc<Mutex<HashMap<String, PendingCall>>>,
    seq: Arc<std::sync::atomic::AtomicU64>,
}

impl RpcClient {
    pub fn new<T: AsyncTransport>(transport: T, event_tx: mpsc::Sender<Frame>) -> Self {
        let (read_half, write_half) = tokio::io::split(transport);
        let (write_tx, mut write_rx) = mpsc::channel::<Frame>(256);
        let pending: Arc<Mutex<HashMap<String, PendingCall>>> =
            Arc::new(Mutex::new(HashMap::new()));

        let pending_clone = pending.clone();
        let event_tx_clone = event_tx.clone();

        // Writer task
        tokio::spawn(async move {
            let mut writer = write_half;
            while let Some(frame) = write_rx.recv().await {
                let data = NdjsonCodec::encode(&frame);
                if writer.write_all(&data).await.is_err() {
                    break;
                }
            }
        });

        // Reader task
        tokio::spawn(async move {
            let mut reader = BufReader::new(read_half);
            let mut line_buf = String::new();
            loop {
                line_buf.clear();
                match reader.read_line(&mut line_buf).await {
                    Ok(0) => break,
                    Ok(_) => {
                        let mut buf = BytesMut::from(line_buf.as_bytes());
                        if let Some(frame) = NdjsonCodec::decode(&mut buf) {
                            match &frame {
                                Frame::Result { id, .. } | Frame::Error { id, .. } => {
                                    let mut map = pending_clone.lock().await;
                                    if let Some(tx) = map.remove(id) {
                                        let result = match frame {
                                            Frame::Result { data, .. } => Ok(data),
                                            Frame::Error { code, message, .. } => {
                                                Err(RpcError::Remote { code, message })
                                            }
                                            _ => unreachable!(),
                                        };
                                        let _ = tx.send(result);
                                    }
                                }
                                Frame::Event { .. }
                                | Frame::StreamData { .. }
                                | Frame::StreamEnd { .. } => {
                                    let _ = event_tx_clone.send(frame).await;
                                }
                                _ => {
                                    let _ = event_tx_clone.send(frame).await;
                                }
                            }
                        }
                    }
                    Err(_) => break,
                }
            }
            let mut map = pending_clone.lock().await;
            for (_, sender) in map.drain() {
                let _ = sender.send(Err(RpcError::Closed));
            }
        });

        Self {
            write_tx,
            pending,
            seq: Arc::new(std::sync::atomic::AtomicU64::new(1)),
        }
    }

    fn next_id(&self) -> String {
        let n = self.seq.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        format!("c{n}")
    }

    pub async fn call(
        &self,
        method: &str,
        params: Option<serde_json::Value>,
    ) -> Result<serde_json::Value, RpcError> {
        let id = self.next_id();
        let (tx, rx) = oneshot::channel();
        {
            let mut map = self.pending.lock().await;
            map.insert(id.clone(), tx);
        }
        let frame = Frame::Call {
            id: id.clone(),
            method: method.to_string(),
            params,
        };
        self.write_tx
            .send(frame)
            .await
            .map_err(|_| RpcError::Closed)?;
        rx.await.map_err(|_| RpcError::Closed)?
    }

    pub async fn send_frame(&self, frame: Frame) -> Result<(), RpcError> {
        self.write_tx
            .send(frame)
            .await
            .map_err(|_| RpcError::Closed)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transport::memory::create_memory_pair;

    #[tokio::test]
    async fn pending_calls_fail_when_the_peer_disconnects() {
        let (client_transport, server_transport) = create_memory_pair();
        let (event_tx, _event_rx) = mpsc::channel(8);
        let client = RpcClient::new(client_transport, event_tx);
        let call = tokio::spawn(async move { client.call("never.replied", None).await });
        tokio::task::yield_now().await;
        drop(server_transport);

        let result = tokio::time::timeout(std::time::Duration::from_secs(1), call)
            .await
            .expect("RPC call should be woken on disconnect")
            .unwrap();
        assert!(matches!(result, Err(RpcError::Closed)));
    }
}
