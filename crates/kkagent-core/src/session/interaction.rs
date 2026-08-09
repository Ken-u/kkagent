//! Unified blocking human-in-the-loop interaction kernel.

use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use tokio::sync::oneshot;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InteractionKind {
    Approval,
    Question,
    UserTool,
}

#[derive(Debug, Clone, Default)]
pub struct InteractionOrigin {
    pub agent_id: Option<String>,
    pub turn_id: Option<u64>,
}

#[derive(Debug, Clone)]
pub struct Interaction {
    pub id: String,
    pub kind: InteractionKind,
    pub payload: Value,
    pub origin: InteractionOrigin,
    pub created_at: i64,
}

struct Pending {
    interaction: Interaction,
    tx: Option<oneshot::Sender<Value>>,
}

const RECENTLY_RESOLVED_TTL_MS: i64 = 60_000;
const RECENTLY_RESOLVED_MAX: usize = 256;

#[derive(Default)]
pub struct SessionInteractionService {
    pending: Mutex<HashMap<String, Pending>>,
    recently_resolved: Mutex<HashMap<String, i64>>,
    next_id: Mutex<u64>,
}

impl SessionInteractionService {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn request(
        &self,
        kind: InteractionKind,
        payload: Value,
        origin: InteractionOrigin,
        id: Option<String>,
    ) -> Value {
        let (tx, rx) = oneshot::channel();
        let interaction = self.park(kind, payload, origin, id, Some(tx));
        rx.await.unwrap_or_else(|_| {
            serde_json::json!({
                "cancelled": true,
                "id": interaction.id,
                "reason": "channel_closed",
            })
        })
    }

    pub fn enqueue(
        &self,
        kind: InteractionKind,
        payload: Value,
        origin: InteractionOrigin,
        id: Option<String>,
    ) -> Interaction {
        self.park(kind, payload, origin, id, None)
    }

    pub fn respond(&self, id: &str, response: Value) -> bool {
        let entry = {
            let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
            pending.remove(id)
        };
        let Some(entry) = entry else {
            return false;
        };
        self.remember_resolved(id);
        if let Some(tx) = entry.tx {
            let _ = tx.send(response);
        }
        true
    }

    pub fn list_pending(&self, kind: Option<InteractionKind>) -> Vec<Interaction> {
        let pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        pending
            .values()
            .map(|p| p.interaction.clone())
            .filter(|i| kind.map(|k| i.kind == k).unwrap_or(true))
            .collect()
    }

    pub fn is_recently_resolved(&self, id: &str) -> bool {
        let mut map = self.recently_resolved.lock().unwrap_or_else(|e| e.into_inner());
        let Some(at) = map.get(id).copied() else {
            return false;
        };
        let now = chrono::Utc::now().timestamp_millis();
        if now - at > RECENTLY_RESOLVED_TTL_MS {
            map.remove(id);
            return false;
        }
        true
    }

    pub fn cancel_pending_for_turn(&self, turn_id: u64) {
        let mut pending = self.pending.lock().unwrap_or_else(|e| e.into_inner());
        let ids: Vec<String> = pending
            .iter()
            .filter(|(_, p)| p.interaction.origin.turn_id == Some(turn_id))
            .map(|(id, _)| id.clone())
            .collect();
        for id in ids {
            if let Some(entry) = pending.remove(&id) {
                self.remember_resolved(&id);
                if let Some(tx) = entry.tx {
                    let _ = tx.send(serde_json::json!({
                        "cancelled": true,
                        "reason": "turn_ended",
                    }));
                }
            }
        }
    }

    fn park(
        &self,
        kind: InteractionKind,
        payload: Value,
        origin: InteractionOrigin,
        id: Option<String>,
        tx: Option<oneshot::Sender<Value>>,
    ) -> Interaction {
        let id = id.unwrap_or_else(|| {
            let mut n = self.next_id.lock().unwrap_or_else(|e| e.into_inner());
            *n += 1;
            format!("ix-{}", *n)
        });
        let interaction = Interaction {
            id: id.clone(),
            kind,
            payload,
            origin,
            created_at: chrono::Utc::now().timestamp_millis(),
        };
        self.pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                id,
                Pending {
                    interaction: interaction.clone(),
                    tx,
                },
            );
        interaction
    }

    fn remember_resolved(&self, id: &str) {
        let mut map = self.recently_resolved.lock().unwrap_or_else(|e| e.into_inner());
        map.insert(id.to_string(), chrono::Utc::now().timestamp_millis());
        if map.len() > RECENTLY_RESOLVED_MAX {
            if let Some(oldest) = map
                .iter()
                .min_by_key(|(_, t)| *t)
                .map(|(k, _)| k.clone())
            {
                map.remove(&oldest);
            }
        }
    }
}

/// Shared handle for parking interactions across await points.
pub type SharedInteraction = Arc<SessionInteractionService>;

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn request_respond() {
        let svc = Arc::new(SessionInteractionService::new());
        let s2 = svc.clone();
        let handle = tokio::spawn(async move {
            svc.request(
                InteractionKind::Approval,
                serde_json::json!({"tool":"Bash"}),
                InteractionOrigin::default(),
                Some("a1".into()),
            )
            .await
        });
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        assert!(s2.respond("a1", serde_json::json!({"ok": true})));
        let v = handle.await.unwrap();
        assert_eq!(v["ok"], true);
    }
}
