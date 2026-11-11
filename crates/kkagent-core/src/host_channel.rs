//! The channel through which kkagent talks *back* to an embedding host.
//!
//! The host is the ACP client on the other end of `kkagent acp`. Host tools
//! (`_kkagent/tool/call`) and host-backed storage (`_kkagent/store/*`) are
//! agent→client requests; this trait is the one seam they go through, so the
//! ACP bridge, tests and other transports are interchangeable.

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, RwLock};
use std::time::Duration;

use async_trait::async_trait;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum HostChannelError {
    /// No host connection is bound (yet).
    #[error("host channel is not connected")]
    NotConnected,
    /// The host went away while a request was in flight.
    #[error("host disconnected")]
    Closed,
    #[error("host request timed out")]
    TimedOut,
    /// The caller's cancel flag was raised; the host was told to stop.
    #[error("host request cancelled")]
    Cancelled,
    /// The host answered with a JSON-RPC error.
    #[error("host error {code}: {message}")]
    Rpc { code: i64, message: String },
}

/// Per-request knobs.
#[derive(Debug, Clone, Default)]
pub struct RequestOptions {
    /// Cooperative cancellation: when the flag turns true the request is
    /// abandoned, the host is notified (`$/cancel_request`) and the call
    /// returns [`HostChannelError::Cancelled`].
    pub cancel: Option<Arc<AtomicBool>>,
    /// Give up after this long. `None` leaves it to the transport default.
    pub timeout: Option<Duration>,
}

impl RequestOptions {
    pub fn with_timeout(timeout: Duration) -> Self {
        Self {
            cancel: None,
            timeout: Some(timeout),
        }
    }
}

/// agent→host messaging.
#[async_trait]
pub trait HostChannel: Send + Sync {
    /// Send a request and wait for the host's response.
    async fn request(
        &self,
        method: &str,
        params: Value,
        options: RequestOptions,
    ) -> Result<Value, HostChannelError>;

    /// Send a notification (no response).
    async fn notify(&self, method: &str, params: Value);
}

/// A late-bound channel. The agent state (and the stores built on it) is
/// created before the host connects; the transport binds itself here once the
/// connection exists. Until then every request fails with
/// [`HostChannelError::NotConnected`], so a host-backed store fails closed
/// instead of silently writing somewhere else.
#[derive(Default)]
pub struct HostChannelSlot {
    inner: RwLock<Option<Arc<dyn HostChannel>>>,
}

impl HostChannelSlot {
    pub fn new() -> Self {
        Self::default()
    }

    /// Bind the live channel. The first binding wins for the lifetime of the
    /// connection; use [`Self::unbind`] first to replace it.
    pub fn bind(&self, channel: Arc<dyn HostChannel>) -> bool {
        let mut slot = self
            .inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if slot.is_some() {
            return false;
        }
        *slot = Some(channel);
        true
    }

    pub fn unbind(&self) {
        *self
            .inner
            .write()
            .unwrap_or_else(std::sync::PoisonError::into_inner) = None;
    }

    pub fn is_bound(&self) -> bool {
        self.current().is_some()
    }

    fn current(&self) -> Option<Arc<dyn HostChannel>> {
        self.inner
            .read()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clone()
    }
}

#[async_trait]
impl HostChannel for HostChannelSlot {
    async fn request(
        &self,
        method: &str,
        params: Value,
        options: RequestOptions,
    ) -> Result<Value, HostChannelError> {
        match self.current() {
            Some(channel) => channel.request(method, params, options).await,
            None => Err(HostChannelError::NotConnected),
        }
    }

    async fn notify(&self, method: &str, params: Value) {
        if let Some(channel) = self.current() {
            channel.notify(method, params).await;
        }
    }
}

/// The ACP connection is the production host channel.
#[async_trait]
impl HostChannel for kkagent_acp::ClientBridge {
    async fn request(
        &self,
        method: &str,
        params: Value,
        options: RequestOptions,
    ) -> Result<Value, HostChannelError> {
        use kkagent_acp::BridgeError;
        self.request_with(method, params, options.timeout, options.cancel)
            .await
            .map_err(|error| match error {
                BridgeError::Disconnected => HostChannelError::Closed,
                BridgeError::TimedOut => HostChannelError::TimedOut,
                BridgeError::Cancelled => HostChannelError::Cancelled,
                BridgeError::Rpc { code, message } => HostChannelError::Rpc { code, message },
            })
    }

    async fn notify(&self, method: &str, params: Value) {
        kkagent_acp::ClientBridge::notify(self, method, params).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    struct Echo;

    #[async_trait]
    impl HostChannel for Echo {
        async fn request(
            &self,
            method: &str,
            params: Value,
            _options: RequestOptions,
        ) -> Result<Value, HostChannelError> {
            Ok(json!({"method": method, "params": params}))
        }
        async fn notify(&self, _method: &str, _params: Value) {}
    }

    #[tokio::test]
    async fn unbound_slot_fails_closed_then_forwards_once_bound() {
        let slot = HostChannelSlot::new();
        assert!(!slot.is_bound());
        let error = slot
            .request("m", json!(1), RequestOptions::default())
            .await
            .unwrap_err();
        assert_eq!(error, HostChannelError::NotConnected);

        assert!(slot.bind(Arc::new(Echo)));
        // A second binding does not replace the live channel.
        assert!(!slot.bind(Arc::new(Echo)));
        let reply = slot
            .request("m", json!(1), RequestOptions::default())
            .await
            .unwrap();
        assert_eq!(reply["method"], "m");

        slot.unbind();
        assert!(!slot.is_bound());
    }
}
