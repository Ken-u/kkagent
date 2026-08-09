//! Session lifecycle hooks — create / close vocabulary + slots.

use std::sync::Arc;
use tokio::sync::RwLock;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionCreateSource {
    Startup,
    Resume,
    Fork,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionCloseReason {
    Exit,
    Archive,
}

pub type LifecycleHook = Arc<dyn Fn(LifecycleEvent) + Send + Sync>;

#[derive(Debug, Clone)]
pub enum LifecycleEvent {
    Created { source: SessionCreateSource },
    WillClose { reason: SessionCloseReason },
}

#[derive(Default)]
pub struct SessionLifecycleHooks {
    hooks: RwLock<Vec<LifecycleHook>>,
}

impl SessionLifecycleHooks {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn register(&self, hook: LifecycleHook) {
        self.hooks.write().await.push(hook);
    }

    pub async fn fire_created(&self, source: SessionCreateSource) {
        let hooks = self.hooks.read().await.clone();
        let ev = LifecycleEvent::Created { source };
        for h in hooks {
            h(ev.clone());
        }
    }

    pub async fn fire_will_close(&self, reason: SessionCloseReason) {
        let hooks = self.hooks.read().await.clone();
        let ev = LifecycleEvent::WillClose { reason };
        for h in hooks {
            h(ev.clone());
        }
    }
}
