//! Context-window batching for multi-user STT.
//!
//! Collects [`LabeledUtterance`]s from multiple speakers within a
//! configurable time window, then flushes them as a single labelled
//! block for the LLM.

use std::time::{Duration, Instant};

/// A single STT-confirmed utterance with speaker identity.
#[derive(Debug, Clone)]
pub struct LabeledUtterance {
    pub user_id: u64,
    pub username: String,
    pub text: String,
    pub timestamp: Instant,
}

/// Buffers utterances within a sliding time window.
///
/// The window starts when the first utterance arrives and closes after
/// `window_duration` elapses.  [`flush`](Self::flush) returns the
/// concatenated, speaker-labelled text and resets the buffer.
pub struct ContextWindowBuffer {
    utterances: Vec<LabeledUtterance>,
    window_start: Option<Instant>,
    window_duration: Duration,
}

impl ContextWindowBuffer {
    pub fn new(window_duration: Duration) -> Self {
        Self {
            utterances: Vec::new(),
            window_start: None,
            window_duration,
        }
    }

    /// Add a confirmed STT utterance. Starts the timer on first push.
    pub fn push(&mut self, utterance: LabeledUtterance) {
        if self.window_start.is_none() {
            self.window_start = Some(Instant::now());
        }
        self.utterances.push(utterance);
    }

    /// Returns `true` when the time window has elapsed (and there is
    /// at least one utterance buffered).
    pub fn is_ready(&self) -> bool {
        self.window_start
            .map(|start| start.elapsed() >= self.window_duration)
            .unwrap_or(false)
    }

    /// Drain the buffer and return speaker-labelled text, or `None` if
    /// the buffer is empty.
    pub fn flush(&mut self) -> Option<String> {
        if self.utterances.is_empty() {
            return None;
        }

        let text = self
            .utterances
            .iter()
            .map(|u| format!("{}さん: {}", u.username, u.text))
            .collect::<Vec<_>>()
            .join("\n");

        self.utterances.clear();
        self.window_start = None;
        Some(text)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn utterance(user_id: u64, name: &str, text: &str) -> LabeledUtterance {
        LabeledUtterance {
            user_id,
            username: name.into(),
            text: text.into(),
            timestamp: Instant::now(),
        }
    }

    #[test]
    fn empty_buffer_is_not_ready() {
        let buf = ContextWindowBuffer::new(Duration::from_secs(2));
        assert!(!buf.is_ready());
    }

    #[test]
    fn flush_empty_returns_none() {
        let mut buf = ContextWindowBuffer::new(Duration::from_secs(2));
        assert!(buf.flush().is_none());
    }

    #[test]
    fn push_starts_window() {
        let mut buf = ContextWindowBuffer::new(Duration::from_secs(2));
        buf.push(utterance(1, "Alice", "hello"));
        // Just pushed — window should NOT be ready yet (2 s hasn't passed)
        assert!(!buf.is_ready());
    }

    #[test]
    fn flush_formats_labels() {
        let mut buf = ContextWindowBuffer::new(Duration::from_secs(2));
        buf.push(utterance(1, "Alice", "今日の天気は？"));
        buf.push(utterance(2, "Bob", "それと明日も教えて"));
        buf.push(utterance(1, "Alice", "東京で"));

        let text = buf.flush().unwrap();
        assert_eq!(
            text,
            "Aliceさん: 今日の天気は？\nBobさん: それと明日も教えて\nAliceさん: 東京で"
        );
    }

    #[test]
    fn flush_clears_buffer() {
        let mut buf = ContextWindowBuffer::new(Duration::from_secs(2));
        buf.push(utterance(1, "Alice", "hello"));
        let _ = buf.flush();

        assert!(buf.flush().is_none());
        assert!(!buf.is_ready());
    }

    #[test]
    fn is_ready_after_window_elapsed() {
        let mut buf = ContextWindowBuffer::new(Duration::from_millis(0));
        buf.push(utterance(1, "Alice", "hello"));
        // 0ms window → immediately ready
        assert!(buf.is_ready());
    }

    #[test]
    fn multiple_flush_cycles() {
        let mut buf = ContextWindowBuffer::new(Duration::from_millis(0));

        buf.push(utterance(1, "Alice", "first"));
        assert!(buf.flush().is_some());

        buf.push(utterance(2, "Bob", "second"));
        let text = buf.flush().unwrap();
        assert!(text.contains("Bob"));
        assert!(!text.contains("Alice"));
    }
}
