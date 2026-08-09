//! Session-scoped keyed state registry (agent-core-v2 `sessionState`).

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::{Arc, RwLock};

/// Type-erased per-session state bag. Keys are string names; values are `Arc<dyn Any + Send + Sync>`.
#[derive(Default)]
pub struct SessionStateService {
    inner: RwLock<HashMap<String, Arc<dyn Any + Send + Sync>>>,
    by_type: RwLock<HashMap<TypeId, String>>,
}

impl SessionStateService {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set<T: Any + Send + Sync + 'static>(&self, key: impl Into<String>, value: T) {
        let key = key.into();
        let tid = TypeId::of::<T>();
        self.by_type
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(tid, key.clone());
        self.inner
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(key, Arc::new(value));
    }

    pub fn get<T: Any + Send + Sync + 'static>(&self, key: &str) -> Option<Arc<T>> {
        let guard = self.inner.read().unwrap_or_else(|e| e.into_inner());
        guard.get(key)?.clone().downcast::<T>().ok()
    }

    pub fn keys(&self) -> Vec<String> {
        self.inner
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .keys()
            .cloned()
            .collect()
    }

    pub fn inspect(&self) -> HashMap<String, String> {
        self.keys()
            .into_iter()
            .map(|k| (k.clone(), format!("present:{k}")))
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn set_get() {
        let s = SessionStateService::new();
        s.set("n", 42u32);
        assert_eq!(*s.get::<u32>("n").unwrap(), 42);
    }
}
