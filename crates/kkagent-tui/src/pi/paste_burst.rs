//! Debounce large pastes into a single editor insert.

use std::time::{Duration, Instant};

#[derive(Debug, Default)]
pub struct PasteBurst {
    buf: String,
    last: Option<Instant>,
    window: Duration,
}

impl PasteBurst {
    pub fn new() -> Self {
        Self {
            buf: String::new(),
            last: None,
            window: Duration::from_millis(40),
        }
    }

    pub fn push(&mut self, chunk: &str) {
        self.buf.push_str(chunk);
        self.last = Some(Instant::now());
    }

    pub fn ready(&self) -> bool {
        if self.buf.is_empty() {
            return false;
        }
        match self.last {
            None => true,
            Some(t) => t.elapsed() >= self.window || self.buf.len() > 8_192,
        }
    }

    pub fn take(&mut self) -> Option<String> {
        if self.buf.is_empty() {
            return None;
        }
        if !self.ready() {
            return None;
        }
        Some(std::mem::take(&mut self.buf))
    }

    pub fn force_take(&mut self) -> String {
        std::mem::take(&mut self.buf)
    }
}
