//! Lightweight pub/sub event bus for telemetry / TUI / hooks subscribers.

use kkagent_protocol::AgentEvent;
use std::sync::Arc;
use tokio::sync::{broadcast, RwLock};

#[derive(Clone)]
pub struct EventBus {
    tx: broadcast::Sender<AgentEvent>,
    last: Arc<RwLock<Option<AgentEvent>>>,
}

impl EventBus {
    pub fn new(capacity: usize) -> Self {
        let (tx, _) = broadcast::channel(capacity.max(16));
        Self {
            tx,
            last: Arc::new(RwLock::new(None)),
        }
    }

    pub async fn publish(&self, event: AgentEvent) {
        *self.last.write().await = Some(event.clone());
        let _ = self.tx.send(event);
    }

    pub fn subscribe(&self) -> broadcast::Receiver<AgentEvent> {
        self.tx.subscribe()
    }

    pub async fn last(&self) -> Option<AgentEvent> {
        self.last.read().await.clone()
    }
}

impl Default for EventBus {
    fn default() -> Self {
        Self::new(256)
    }
}
