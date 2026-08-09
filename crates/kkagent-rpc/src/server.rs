use bytes::BytesMut;
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
use tokio::sync::mpsc;

use crate::codec::NdjsonCodec;
use crate::transport::AsyncTransport;
use kkagent_protocol::Frame;

pub type RequestHandler = Arc<
    dyn Fn(
            String,
            String,
            Option<serde_json::Value>,
            mpsc::Sender<Frame>,
        ) -> futures::future::BoxFuture<'static, Result<serde_json::Value, (i32, String)>>
        + Send
        + Sync,
>;

pub struct RpcServer {
    handler: RequestHandler,
}

impl RpcServer {
    pub fn new(handler: RequestHandler) -> Self {
        Self { handler }
    }

    pub async fn serve<T: AsyncTransport>(&self, transport: T) {
        let (read_half, write_half) = tokio::io::split(transport);
        let (write_tx, mut write_rx) = mpsc::channel::<Frame>(256);

        let handler = self.handler.clone();

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

        // Send ready
        let _ = write_tx.send(Frame::Ready).await;

        // Reader loop
        let mut reader = BufReader::new(read_half);
        let mut line_buf = String::new();
        loop {
            line_buf.clear();
            match reader.read_line(&mut line_buf).await {
                Ok(0) => break,
                Ok(_) => {
                    let mut buf = BytesMut::from(line_buf.as_bytes());
                    if let Some(frame) = NdjsonCodec::decode(&mut buf) {
                        match frame {
                            Frame::Hello { .. } => {}
                            Frame::Call { id, method, params } => {
                                let h = handler.clone();
                                let tx = write_tx.clone();
                                tokio::spawn(async move {
                                    let result = h(id.clone(), method, params, tx.clone()).await;
                                    let reply = match result {
                                        Ok(data) => Frame::Result { id, data },
                                        Err((code, message)) => Frame::Error { id, code, message },
                                    };
                                    let _ = tx.send(reply).await;
                                });
                            }
                            _ => {}
                        }
                    }
                }
                Err(_) => break,
            }
        }
    }
}
