//! Mock STT provider for testing.
//!
//! Simulates a speech-to-text provider with configurable utterances,
//! partial results, and timing delays.  Useful for unit-testing the
//! pipeline without an external STT server.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use tokio::time::sleep;

use crate::voice::provider::{SttEvent, SttProvider, SttSession};

/// Number of partial results emitted per utterance.
const NUM_PARTIALS: usize = 3;

/// Audio sample count threshold to trigger speech detection.
const AUDIO_TRIGGER_THRESHOLD: usize = 320;

// ── Configuration ────────────────────────────────────────────────

/// A single scripted utterance for the mock STT.
#[derive(Debug, Clone)]
pub struct MockUtterance {
    pub text: String,
    pub language: String,
    pub delay_before_start: Duration,
    pub partial_interval: Duration,
    pub delay_to_final: Duration,
    pub confidence: f32,
}

/// Configuration for [`MockSttProvider`].
#[derive(Debug, Clone)]
pub struct MockSttConfig {
    pub utterances: Vec<MockUtterance>,
    /// Return `None` from `recv_event` after all utterances are consumed.
    pub close_after_all: bool,
    /// Multiplier applied to all delay durations (0.0 = instant).
    pub latency_multiplier: f64,
}

// ── Provider ─────────────────────────────────────────────────────

/// Mock STT provider that replays scripted utterances.
pub struct MockSttProvider {
    config: Arc<MockSttConfig>,
}

impl MockSttProvider {
    pub fn new(config: MockSttConfig) -> Self {
        Self {
            config: Arc::new(config),
        }
    }
}

#[async_trait]
impl SttProvider for MockSttProvider {
    async fn connect(&self) -> Result<Box<dyn SttSession>> {
        Ok(Box::new(MockSttSession {
            config: self.config.clone(),
            state: MockSttState::WaitingForAudio,
            utterance_index: 0,
            audio_sample_count: 0,
        }))
    }

    fn name(&self) -> &str {
        "mock"
    }
}

// ── Session ──────────────────────────────────────────────────────

#[derive(Debug)]
enum MockSttState {
    /// Waiting for enough audio to trigger speech detection.
    WaitingForAudio,
    /// Enough audio received; ready to emit SpeechStart.
    AudioReceived,
    /// Emitting partial results (index 0..NUM_PARTIALS).
    /// When index == NUM_PARTIALS, emits Final instead.
    Partial(usize),
    /// Ready to emit SpeechEnd.
    SpeechEndReady,
    /// Transitioning to next utterance.
    NextUtterance,
    /// Session closed.
    Closed,
}

struct MockSttSession {
    config: Arc<MockSttConfig>,
    state: MockSttState,
    utterance_index: usize,
    audio_sample_count: usize,
}

impl MockSttSession {
    fn current_utterance(&self) -> &MockUtterance {
        &self.config.utterances[self.utterance_index]
    }

    fn timestamp_ms(&self) -> u64 {
        // Approximate timestamp based on audio received (assuming 16 kHz).
        (self.audio_sample_count as u64 * 1000) / 16000
    }

    async fn maybe_sleep(&self, duration: Duration) {
        let scaled = duration.mul_f64(self.config.latency_multiplier);
        if !scaled.is_zero() {
            sleep(scaled).await;
        }
    }
}

#[async_trait]
impl SttSession for MockSttSession {
    async fn send_audio(&mut self, audio: &[f32]) -> Result<()> {
        if matches!(self.state, MockSttState::WaitingForAudio)
            && self.utterance_index < self.config.utterances.len()
        {
            self.audio_sample_count += audio.len();
            if self.audio_sample_count > AUDIO_TRIGGER_THRESHOLD {
                self.state = MockSttState::AudioReceived;
            }
        }
        Ok(())
    }

    async fn recv_event(&mut self) -> Result<Option<SttEvent>> {
        loop {
            match self.state {
                MockSttState::WaitingForAudio => return Ok(None),

                MockSttState::AudioReceived => {
                    let delay = self.current_utterance().delay_before_start;
                    self.maybe_sleep(delay).await;
                    let ts = self.timestamp_ms();
                    self.state = MockSttState::Partial(0);
                    return Ok(Some(SttEvent::SpeechStart { timestamp_ms: ts }));
                }

                MockSttState::Partial(n) => {
                    let utt = self.current_utterance().clone();
                    if n < NUM_PARTIALS {
                        self.maybe_sleep(utt.partial_interval).await;
                        let end = ((n + 1) * utt.text.len()) / NUM_PARTIALS;
                        let partial_text = utt.text[..end].to_string();
                        self.state = MockSttState::Partial(n + 1);
                        return Ok(Some(SttEvent::Partial { text: partial_text }));
                    } else {
                        // All partials done — emit Final.
                        self.maybe_sleep(utt.delay_to_final).await;
                        let duration_ms = utt.text.len() as f64 * 100.0;
                        self.state = MockSttState::SpeechEndReady;
                        return Ok(Some(SttEvent::Final {
                            text: utt.text.clone(),
                            language: utt.language.clone(),
                            confidence: utt.confidence,
                            duration_ms,
                        }));
                    }
                }

                MockSttState::SpeechEndReady => {
                    let duration_ms = self.current_utterance().text.len() as f64 * 100.0;
                    let ts = self.timestamp_ms();
                    self.state = MockSttState::NextUtterance;
                    return Ok(Some(SttEvent::SpeechEnd {
                        timestamp_ms: ts,
                        duration_ms,
                    }));
                }

                MockSttState::NextUtterance => {
                    self.utterance_index += 1;
                    self.audio_sample_count = 0;
                    if self.utterance_index < self.config.utterances.len() {
                        self.state = MockSttState::WaitingForAudio;
                        continue;
                    } else if self.config.close_after_all {
                        self.state = MockSttState::Closed;
                        return Ok(None);
                    } else {
                        self.state = MockSttState::WaitingForAudio;
                        continue;
                    }
                }

                MockSttState::Closed => return Ok(None),
            }
        }
    }

    async fn close(&mut self) -> Result<()> {
        self.state = MockSttState::Closed;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn simple_utterance(text: &str) -> MockUtterance {
        MockUtterance {
            text: text.to_string(),
            language: "en".to_string(),
            delay_before_start: Duration::ZERO,
            partial_interval: Duration::ZERO,
            delay_to_final: Duration::ZERO,
            confidence: 0.95,
        }
    }

    #[tokio::test]
    async fn basic_utterance_event_order() {
        let provider = MockSttProvider::new(MockSttConfig {
            utterances: vec![simple_utterance("hello world")],
            close_after_all: true,
            latency_multiplier: 1.0,
        });

        let mut session = provider.connect().await.unwrap();

        // No events before audio.
        assert!(session.recv_event().await.unwrap().is_none());

        // Send enough audio to trigger.
        session.send_audio(&vec![0.1f32; 400]).await.unwrap();

        // SpeechStart
        let event = session.recv_event().await.unwrap().unwrap();
        assert!(matches!(event, SttEvent::SpeechStart { .. }));

        // 3 Partials with progressive text slicing.
        for i in 0..NUM_PARTIALS {
            let event = session.recv_event().await.unwrap().unwrap();
            match &event {
                SttEvent::Partial { text } => {
                    let expected_end = ((i + 1) * "hello world".len()) / NUM_PARTIALS;
                    assert_eq!(text, &"hello world"[..expected_end]);
                }
                _ => panic!("expected Partial, got {:?}", event),
            }
        }

        // Final
        let event = session.recv_event().await.unwrap().unwrap();
        match event {
            SttEvent::Final {
                text,
                language,
                confidence,
                ..
            } => {
                assert_eq!(text, "hello world");
                assert_eq!(language, "en");
                assert!((confidence - 0.95).abs() < f32::EPSILON);
            }
            _ => panic!("expected Final"),
        }

        // SpeechEnd
        let event = session.recv_event().await.unwrap().unwrap();
        assert!(matches!(event, SttEvent::SpeechEnd { .. }));

        // Session ends (close_after_all).
        assert!(session.recv_event().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn multiple_utterances() {
        let provider = MockSttProvider::new(MockSttConfig {
            utterances: vec![simple_utterance("hello"), simple_utterance("world")],
            close_after_all: true,
            latency_multiplier: 1.0,
        });

        let mut session = provider.connect().await.unwrap();

        // First utterance.
        session.send_audio(&vec![0.1f32; 400]).await.unwrap();
        let mut events = Vec::new();
        while let Some(event) = session.recv_event().await.unwrap() {
            events.push(event);
        }
        assert_eq!(events.len(), 6); // SpeechStart + 3 Partial + Final + SpeechEnd

        // Second utterance.
        session.send_audio(&vec![0.1f32; 400]).await.unwrap();
        let mut events = Vec::new();
        while let Some(event) = session.recv_event().await.unwrap() {
            events.push(event);
        }
        assert_eq!(events.len(), 6);

        // Verify second utterance produced "world".
        match &events[4] {
            SttEvent::Final { text, .. } => assert_eq!(text, "world"),
            _ => panic!("expected Final"),
        }
    }

    #[tokio::test]
    async fn close_after_all() {
        let provider = MockSttProvider::new(MockSttConfig {
            utterances: vec![simple_utterance("test")],
            close_after_all: true,
            latency_multiplier: 1.0,
        });

        let mut session = provider.connect().await.unwrap();
        session.send_audio(&vec![0.1f32; 400]).await.unwrap();

        // Drain all events.
        while session.recv_event().await.unwrap().is_some() {}

        // Subsequent calls keep returning None.
        assert!(session.recv_event().await.unwrap().is_none());
        assert!(session.recv_event().await.unwrap().is_none());
    }
}
