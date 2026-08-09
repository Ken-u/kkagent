//! Lightweight service container inspired by kimi-code `_base/di`.
//!
//! Supports typed registration, sync/async factories, child scopes, and
//! disposable services. Not a full TypeScript DI port, but covers the host
//! wiring patterns used by agent / telemetry / MCP.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum DiError {
    #[error("service not registered: {0}")]
    NotRegistered(&'static str),
    #[error("service already registered: {0}")]
    AlreadyRegistered(&'static str),
    #[error("factory failed: {0}")]
    Factory(String),
}

pub type DiResult<T> = Result<T, DiError>;

/// Marker for services that should be disposed when a scope ends.
pub trait Disposable: Send + Sync {
    fn dispose(&self);
}

/// Typed service identifier (uses TypeId of the trait object / concrete type).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct ServiceId {
    type_id: TypeId,
    name: &'static str,
}

impl ServiceId {
    pub fn of<T: 'static>(name: &'static str) -> Self {
        Self {
            type_id: TypeId::of::<T>(),
            name,
        }
    }

    pub fn name(&self) -> &'static str {
        self.name
    }
}

enum ServiceEntry {
    Instance(Arc<dyn Any + Send + Sync>),
    Factory(Arc<dyn Fn(&ServiceContainer) -> DiResult<Arc<dyn Any + Send + Sync>> + Send + Sync>),
}

/// Root or child service container.
pub struct ServiceContainer {
    parent: Option<Arc<ServiceContainer>>,
    entries: RwLock<HashMap<TypeId, ServiceEntry>>,
    disposables: RwLock<Vec<Arc<dyn Disposable>>>,
    name: String,
}

impl ServiceContainer {
    pub fn new(name: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            parent: None,
            entries: RwLock::new(HashMap::new()),
            disposables: RwLock::new(Vec::new()),
            name: name.into(),
        })
    }

    pub fn create_child(self: &Arc<Self>, name: impl Into<String>) -> Arc<Self> {
        Arc::new(Self {
            parent: Some(Arc::clone(self)),
            entries: RwLock::new(HashMap::new()),
            disposables: RwLock::new(Vec::new()),
            name: name.into(),
        })
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn register_instance<T>(&self, instance: Arc<T>) -> DiResult<()>
    where
        T: Any + Send + Sync + 'static,
    {
        let mut entries = self.entries.write();
        let id = TypeId::of::<T>();
        if entries.contains_key(&id) {
            return Err(DiError::AlreadyRegistered(std::any::type_name::<T>()));
        }
        entries.insert(id, ServiceEntry::Instance(instance));
        Ok(())
    }

    pub fn register_factory<T, F>(&self, factory: F) -> DiResult<()>
    where
        T: Any + Send + Sync + 'static,
        F: Fn(&ServiceContainer) -> DiResult<Arc<T>> + Send + Sync + 'static,
    {
        let mut entries = self.entries.write();
        let id = TypeId::of::<T>();
        if entries.contains_key(&id) {
            return Err(DiError::AlreadyRegistered(std::any::type_name::<T>()));
        }
        entries.insert(
            id,
            ServiceEntry::Factory(Arc::new(move |c| {
                let v = factory(c)?;
                Ok(v as Arc<dyn Any + Send + Sync>)
            })),
        );
        Ok(())
    }

    pub fn get<T>(&self) -> DiResult<Arc<T>>
    where
        T: Any + Send + Sync + 'static,
    {
        let id = TypeId::of::<T>();
        if let Some(arc) = self.resolve_local(id)? {
            return downcast_arc(arc);
        }
        if let Some(parent) = &self.parent {
            return parent.get::<T>();
        }
        Err(DiError::NotRegistered(std::any::type_name::<T>()))
    }

    pub fn try_get<T>(&self) -> Option<Arc<T>>
    where
        T: Any + Send + Sync + 'static,
    {
        self.get::<T>().ok()
    }

    pub fn register_disposable(&self, d: Arc<dyn Disposable>) {
        self.disposables.write().push(d);
    }

    pub fn dispose(&self) {
        let items: Vec<_> = self.disposables.write().drain(..).collect();
        for d in items.into_iter().rev() {
            d.dispose();
        }
    }

    fn resolve_local(&self, id: TypeId) -> DiResult<Option<Arc<dyn Any + Send + Sync>>> {
        // Fast path: already an instance
        {
            let entries = self.entries.read();
            if let Some(ServiceEntry::Instance(arc)) = entries.get(&id) {
                return Ok(Some(Arc::clone(arc)));
            }
        }
        // Factory path: create once and cache
        let factory = {
            let entries = self.entries.read();
            match entries.get(&id) {
                Some(ServiceEntry::Factory(f)) => Some(Arc::clone(f)),
                Some(ServiceEntry::Instance(_)) => None,
                None => return Ok(None),
            }
        };
        if let Some(factory) = factory {
            let created = factory(self)?;
            let mut entries = self.entries.write();
            entries.insert(id, ServiceEntry::Instance(Arc::clone(&created)));
            return Ok(Some(created));
        }
        Ok(None)
    }
}

fn downcast_arc<T: Any + Send + Sync + 'static>(
    arc: Arc<dyn Any + Send + Sync>,
) -> DiResult<Arc<T>> {
    Arc::downcast::<T>(arc).map_err(|_| DiError::Factory("downcast failed".into()))
}

/// Convenience accessor used by hosts (kimi ServicesAccessor style).
pub struct ServicesAccessor {
    container: Arc<ServiceContainer>,
}

impl ServicesAccessor {
    pub fn new(container: Arc<ServiceContainer>) -> Self {
        Self { container }
    }

    pub fn get<T>(&self) -> DiResult<Arc<T>>
    where
        T: Any + Send + Sync + 'static,
    {
        self.container.get::<T>()
    }

    pub fn container(&self) -> &Arc<ServiceContainer> {
        &self.container
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Counter(std::sync::atomic::AtomicUsize);

    #[test]
    fn register_and_resolve() {
        let c = ServiceContainer::new("root");
        c.register_instance(Arc::new(Counter(std::sync::atomic::AtomicUsize::new(7))))
            .unwrap();
        let got = c.get::<Counter>().unwrap();
        assert_eq!(got.0.load(std::sync::atomic::Ordering::SeqCst), 7);
    }

    #[test]
    fn child_inherits_parent() {
        let root = ServiceContainer::new("root");
        root.register_instance(Arc::new(String::from("hello")))
            .unwrap();
        let child = root.create_child("child");
        assert_eq!(child.get::<String>().unwrap().as_str(), "hello");
    }
}
