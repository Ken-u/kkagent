//! First-token timeout gate shared by all streaming providers.
//!
//! The deadline is an absolute [`Instant`] set at construction time and never
//! refreshed. Keep-alive events (SSE comments, Anthropic `ping`, empty
//! choices, empty deltas) must **not** call [`FirstTokenGate::mark_content`],
//! so they cannot extend the timeout window — only genuine content resets the
//! gate.

use std::time::Instant;

use futures_util::StreamExt;

use crate::http_error::{reqwest_error, FirstTokenTimeoutError};

/// Tracks whether the first meaningful stream chunk has arrived under an
/// optional deadline.
pub(crate) struct FirstTokenGate {
    deadline: Option<Instant>,
    timeout_ms: u64,
    model: String,
    received: bool,
}

impl FirstTokenGate {
    pub(crate) fn new(timeout: Option<std::time::Duration>, model: &str) -> Self {
        let (deadline, timeout_ms) = match timeout {
            Some(duration) if !duration.is_zero() => {
                (Some(Instant::now() + duration), duration.as_millis() as u64)
            }
            _ => (None, 0),
        };
        Self {
            deadline,
            timeout_ms,
            model: model.to_string(),
            received: false,
        }
    }

    /// Mark that meaningful content has been received, disabling the timeout
    /// for all subsequent chunks.
    pub(crate) fn mark_content(&mut self) {
        self.received = true;
    }

    /// Fetch the next chunk from the byte stream, applying the first-token
    /// timeout only until [`Self::mark_content`] is called.
    ///
    /// After the first content chunk the gate is open and chunks are awaited
    /// without an additional deadline (the client's per-read idle timeout
    /// still bounds each individual chunk wait).
    pub(crate) async fn next_chunk<S, B>(&mut self, stream: &mut S) -> anyhow::Result<Option<B>>
    where
        S: StreamExt<Item = Result<B, reqwest::Error>> + Unpin,
    {
        if self.received || self.deadline.is_none() {
            return stream.next().await.transpose().map_err(reqwest_error);
        }
        let deadline = self.deadline.expect("deadline checked above");
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            return Err(FirstTokenTimeoutError {
                timeout_ms: self.timeout_ms,
                model: self.model.clone(),
            }
            .into());
        }
        match tokio::time::timeout(remaining, stream.next()).await {
            Ok(Some(Ok(chunk))) => Ok(Some(chunk)),
            Ok(Some(Err(error))) => Err(reqwest_error(error)),
            Ok(None) => Ok(None),
            Err(_) => Err(FirstTokenTimeoutError {
                timeout_ms: self.timeout_ms,
                model: self.model.clone(),
            }
            .into()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_timeout_when_duration_is_none() {
        let gate = FirstTokenGate::new(None, "model");
        assert!(gate.deadline.is_none());
        assert_eq!(gate.timeout_ms, 0);
    }

    #[test]
    fn no_timeout_when_duration_is_zero() {
        let gate = FirstTokenGate::new(Some(std::time::Duration::ZERO), "model");
        assert!(gate.deadline.is_none());
    }

    #[test]
    fn mark_content_sets_received() {
        let mut gate = FirstTokenGate::new(Some(std::time::Duration::from_secs(10)), "model");
        assert!(!gate.received);
        gate.mark_content();
        assert!(gate.received);
    }
}
