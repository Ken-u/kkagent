//! Streaming UI helpers — cursor blink + delta coalescing.

use std::time::{Duration, Instant};

#[derive(Debug)]
pub struct StreamingCursor {
    started: Instant,
    pub visible: bool,
}

impl Default for StreamingCursor {
    fn default() -> Self {
        Self {
            started: Instant::now(),
            visible: true,
        }
    }
}

impl StreamingCursor {
    pub fn tick(&mut self) {
        // ~530ms blink
        self.visible = (self.started.elapsed().as_millis() / 530).is_multiple_of(2);
    }

    pub fn glyph(&self) -> &'static str {
        if self.visible {
            "▌"
        } else {
            " "
        }
    }
}

#[derive(Debug, Default)]
pub struct DeltaBuffer {
    buf: String,
    last_flush: Option<Instant>,
}

impl DeltaBuffer {
    pub fn push(&mut self, text: &str) {
        self.buf.push_str(text);
    }

    pub fn should_flush(&self) -> bool {
        if self.buf.is_empty() {
            return false;
        }
        match self.last_flush {
            None => true,
            Some(t) => t.elapsed() >= Duration::from_millis(16) || self.buf.len() > 64,
        }
    }

    pub fn take(&mut self) -> String {
        self.last_flush = Some(Instant::now());
        std::mem::take(&mut self.buf)
    }
}
