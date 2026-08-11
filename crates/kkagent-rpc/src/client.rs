use bytes::BytesMut;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::{mpsc, oneshot, watch, Mutex};

use crate::codec::NdjsonCodec;
use crate::transport::AsyncTransport;
use kkagent_protocol::Frame;

type PendingCall = oneshot::Sender<Result<serde_json::Value, RpcError>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RpcConnectionState {
    Connected,
    Disconnected { reason: String },
}

fn mark_disconnected(sender: &watch::Sender<RpcConnectionState>, reason: String) {
    sender.send_if_modified(|state| {
        if matches!(state, RpcConnectionState::Connected) {
            *state = RpcConnectionState::Disconnected { reason };
            true
        } else {
            false
        }
    });
}

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

#[derive(Clone)]
pub struct RpcClient {
    write_tx: mpsc::Sender<Frame>,
    pending: Arc<Mutex<HashMap<String, PendingCall>>>,
    seq: Arc<std::sync::atomic::AtomicU64>,
    connection_state: watch::Receiver<RpcConnectionState>,
}

impl RpcClient {
    pub fn new<T: AsyncTransport>(transport: T, event_tx: mpsc::Sender<Frame>) -> Self {
        let (read_half, write_half) = tokio::io::split(transport);
        let (write_tx, mut write_rx) = mpsc::channel::<Frame>(256);
        let pending: Arc<Mutex<HashMap<String, PendingCall>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let (connection_tx, connection_state) = watch::channel(RpcConnectionState::Connected);

        let pending_clone = pending.clone();
        let event_tx_clone = event_tx.clone();
        let pending_writer = pending.clone();
        let connection_tx_writer = connection_tx.clone();

        // Writer task
        tokio::spawn(async move {
            let mut writer = write_half;
            while let Some(frame) = write_rx.recv().await {
                let data = NdjsonCodec::encode(&frame);
                if let Err(error) = writer.write_all(&data).await {
                    mark_disconnected(
                        &connection_tx_writer,
                        format!("failed to write to RPC peer: {error}"),
                    );
                    let mut map = pending_writer.lock().await;
                    for (_, sender) in map.drain() {
                        let _ = sender.send(Err(RpcError::Closed));
                    }
                    break;
                }
            }
        });

        // Reader task
        tokio::spawn(async move {
            let mut reader = BufReader::new(read_half);
            let mut line_buf = String::new();
            let disconnect_reason = loop {
                line_buf.clear();
                match reader.read_line(&mut line_buf).await {
                    Ok(0) => break "RPC peer closed the connection".to_string(),
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
                    Err(error) => break format!("failed to read from RPC peer: {error}"),
                }
            };
            mark_disconnected(&connection_tx, disconnect_reason);
            let mut map = pending_clone.lock().await;
            for (_, sender) in map.drain() {
                let _ = sender.send(Err(RpcError::Closed));
            }
        });

        Self {
            write_tx,
            pending,
            seq: Arc::new(std::sync::atomic::AtomicU64::new(1)),
            connection_state,
        }
    }

    pub fn connection_state(&self) -> RpcConnectionState {
        self.connection_state.borrow().clone()
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
        let state_client = client.clone();
        let call = tokio::spawn(async move { client.call("never.replied", None).await });
        tokio::task::yield_now().await;
        drop(server_transport);

        let result = tokio::time::timeout(std::time::Duration::from_secs(1), call)
            .await
            .expect("RPC call should be woken on disconnect")
            .unwrap();
        assert!(matches!(result, Err(RpcError::Closed)));
        assert!(matches!(
            state_client.connection_state(),
            RpcConnectionState::Disconnected { .. }
        ));
    }

    #[tokio::test]
    async fn idle_peer_disconnect_updates_connection_state() {
        let (client_transport, server_transport) = create_memory_pair();
        let (event_tx, _event_rx) = mpsc::channel(8);
        let client = RpcClient::new(client_transport, event_tx);
        assert_eq!(client.connection_state(), RpcConnectionState::Connected);

        drop(server_transport);
        let state = tokio::time::timeout(std::time::Duration::from_secs(1), async {
            loop {
                let state = client.connection_state();
                if matches!(state, RpcConnectionState::Disconnected { .. }) {
                    break state;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("disconnect state should be observable");

        assert_eq!(
            state,
            RpcConnectionState::Disconnected {
                reason: "RPC peer closed the connection".into()
            }
        );
    }
}
