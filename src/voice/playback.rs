//! Sequenced playback queue for ordered audio delivery.
//!
//! TTS segments may arrive out of order (due to parallel synthesis).
//! [`SequencedPlaybackQueue`] buffers them and emits audio strictly in
//! sequence-number order so the listener hears a coherent response.

use std::collections::HashMap;

use tokio::sync::mpsc;
use tracing::debug;

use super::tts_pipeline::TtsSegment;

/// A sequenced playback queue that reorders TTS segments.
///
/// Segments are submitted via [`submit`].  The queue buffers out-of-order
/// arrivals and yields them through the receiver returned by [`take_rx`]
/// strictly in monotonically increasing index order.
pub struct SequencedPlaybackQueue {
    /// Next expected sequence index.
    next_index: usize,
    /// Buffer for segments that arrived ahead of `next_index`.
    buffer: HashMap<usize, TtsSegment>,
    /// Channel for emitting ordered segments.
    tx: mpsc::UnboundedSender<TtsSegment>,
    /// Receiver end — taken once by the consumer.
    rx: Option<mpsc::UnboundedReceiver<TtsSegment>>,
    /// Total number of segments expected (set when the stream is fully received).
    total_segments: Option<usize>,
}

impl SequencedPlaybackQueue {
    pub fn new() -> Self {
        let (tx, rx) = mpsc::unbounded_channel();
        Self {
            next_index: 0,
            buffer: HashMap::new(),
            tx,
            rx: Some(rx),
            total_segments: None,
        }
    }

    /// Take the receiver.  Can only be called once.
    pub fn take_rx(&mut self) -> Option<mpsc::UnboundedReceiver<TtsSegment>> {
        self.rx.take()
    }

    /// Declare the total number of segments.
    ///
    /// Once all segments up to `total - 1` have been emitted, the sender
    /// is dropped and the receiver will see `None`.
    pub fn set_total(&mut self, total: usize) {
        self.total_segments = Some(total);
    }

    /// Submit a segment for ordered delivery.
    ///
    /// If the segment's index matches `next_index`, it (and any
    /// consecutively buffered successors) are sent immediately.
    /// Otherwise the segment is buffered.
    pub fn submit(&mut self, segment: TtsSegment) {
        debug!(
            index = segment.index,
            next = self.next_index,
            buffered = self.buffer.len(),
            "Playback queue: segment submitted"
        );

        if segment.index == self.next_index {
            // Send this segment and drain any consecutive buffered ones.
            if self.tx.send(segment).is_err() {
                return; // receiver dropped
            }
            self.next_index += 1;

            while let Some(next_seg) = self.buffer.remove(&self.next_index) {
                if self.tx.send(next_seg).is_err() {
                    return;
                }
                self.next_index += 1;
            }

            self.maybe_close();
        } else {
            self.buffer.insert(segment.index, segment);
        }
    }

    /// Reset the queue for a new response.
    pub fn reset(&mut self) {
        self.next_index = 0;
        self.buffer.clear();
        self.total_segments = None;
        let (tx, rx) = mpsc::unbounded_channel();
        self.tx = tx;
        self.rx = Some(rx);
    }

    /// If all expected segments have been emitted, close the channel.
    fn maybe_close(&mut self) {
        if let Some(total) = self.total_segments {
            if self.next_index >= total {
                debug!(total, "Playback queue: all segments emitted, closing");
                // Dropping the sender is not needed explicitly — it will
                // happen when SequencedPlaybackQueue is dropped or reset.
            }
        }
    }
}

impl Default for SequencedPlaybackQueue {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voice::provider::TtsResult;

    fn make_segment(index: usize, text: &str) -> TtsSegment {
        TtsSegment {
            index,
            text: text.to_string(),
            tts_result: TtsResult {
                audio: vec![0.0; 100],
                sample_rate: 48000,
                duration_ms: 10.0,
            },
        }
    }

    #[test]
    fn in_order_delivery() {
        let mut queue = SequencedPlaybackQueue::new();
        let mut rx = queue.take_rx().unwrap();

        queue.submit(make_segment(0, "first"));
        queue.submit(make_segment(1, "second"));
        queue.submit(make_segment(2, "third"));

        assert_eq!(rx.try_recv().unwrap().text, "first");
        assert_eq!(rx.try_recv().unwrap().text, "second");
        assert_eq!(rx.try_recv().unwrap().text, "third");
    }

    #[test]
    fn out_of_order_waits() {
        let mut queue = SequencedPlaybackQueue::new();
        let mut rx = queue.take_rx().unwrap();

        // Segment 2 arrives first — should be buffered.
        queue.submit(make_segment(2, "third"));
        assert!(rx.try_recv().is_err()); // nothing yet

        // Segment 1 arrives — still buffered (waiting for 0).
        queue.submit(make_segment(1, "second"));
        assert!(rx.try_recv().is_err());

        // Segment 0 arrives — all three should flush.
        queue.submit(make_segment(0, "first"));

        assert_eq!(rx.try_recv().unwrap().text, "first");
        assert_eq!(rx.try_recv().unwrap().text, "second");
        assert_eq!(rx.try_recv().unwrap().text, "third");
    }

    #[test]
    fn partial_out_of_order() {
        let mut queue = SequencedPlaybackQueue::new();
        let mut rx = queue.take_rx().unwrap();

        // seg0 arrives — emitted immediately.
        queue.submit(make_segment(0, "a"));
        assert_eq!(rx.try_recv().unwrap().text, "a");

        // seg2 arrives before seg1 — buffered.
        queue.submit(make_segment(2, "c"));
        assert!(rx.try_recv().is_err());

        // seg1 arrives — both seg1 and seg2 flush.
        queue.submit(make_segment(1, "b"));
        assert_eq!(rx.try_recv().unwrap().text, "b");
        assert_eq!(rx.try_recv().unwrap().text, "c");
    }

    #[test]
    fn reset_clears_state() {
        let mut queue = SequencedPlaybackQueue::new();
        let _rx = queue.take_rx().unwrap();

        queue.submit(make_segment(0, "old"));

        queue.reset();

        let mut rx2 = queue.take_rx().unwrap();
        queue.submit(make_segment(0, "new"));
        assert_eq!(rx2.try_recv().unwrap().text, "new");
    }

    #[test]
    fn take_rx_only_once() {
        let mut queue = SequencedPlaybackQueue::new();
        assert!(queue.take_rx().is_some());
        assert!(queue.take_rx().is_none());
    }

    #[test]
    fn set_total_does_not_affect_delivery() {
        let mut queue = SequencedPlaybackQueue::new();
        let mut rx = queue.take_rx().unwrap();
        queue.set_total(2);

        queue.submit(make_segment(0, "x"));
        queue.submit(make_segment(1, "y"));

        assert_eq!(rx.try_recv().unwrap().text, "x");
        assert_eq!(rx.try_recv().unwrap().text, "y");
    }

    #[test]
    fn large_gap_out_of_order() {
        let mut queue = SequencedPlaybackQueue::new();
        let mut rx = queue.take_rx().unwrap();

        // Submit segments 4, 3, 2, 1, 0 (reverse order).
        for i in (0..5).rev() {
            queue.submit(make_segment(i, &format!("seg{}", i)));
        }

        // All five should be delivered in order.
        for i in 0..5 {
            let seg = rx.try_recv().unwrap();
            assert_eq!(seg.index, i);
            assert_eq!(seg.text, format!("seg{}", i));
        }
    }
}
