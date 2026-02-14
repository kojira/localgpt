//! Mock STT provider for testing.
//!
//! Simulates a speech-to-text provider with configurable utterances,
//! partial results, and timing delays.  Useful for unit-testing the
//! pipeline without an external STT server.

use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use tokio::sync::Mutex;
use tokio::time::sleep;

use crate::voice::provider::{SttEvent, SttProvider, SttReceiver, SttSender};

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
    async fn connect(&self) -> Result<(Box<dyn SttSender>, Box<dyn SttReceiver>)> {
        let config = self.config.clone();
        let shared_state = Arc::new(MockSttSharedState {
            state: Mutex::new(MockSttState::WaitingForAudio),
            utterance_index: Mutex::new(0),
            audio_sample_count: Mutex::new(0),
        });

        let sender = Box::new(MockSttSender {
            config: config.clone(),
            shared: shared_state.clone(),
        }) as Box<dyn SttSender>;

        let receiver = Box::new(MockSttReceiver {
            config,
            shared: shared_state,
        }) as Box<dyn SttReceiver>;

        Ok((sender, receiver))
    }

    fn name(&self) -> &str {
        "mock"
    }
}

// ── Session ──────────────────────────────────────────────────────

#[derive(Debug, Clone)]
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

struct MockSttSharedState {
    state: Mutex<MockSttState>,
    utterance_index: Mutex<usize>,
    audio_sample_count: Mutex<usize>,
}

struct MockSttSender {
    config: Arc<MockSttConfig>,
    shared: Arc<MockSttSharedState>,
}

struct MockSttReceiver {
    config: Arc<MockSttConfig>,
    shared: Arc<MockSttSharedState>,
}

#[async_trait]
impl SttSender for MockSttSender {
    async fn send_audio(&mut self, audio: &[f32]) -> Result<()> {
        let mut state = self.shared.state.lock().await;
        let utt_idx = self.shared.utterance_index.lock().await;
        let mut audio_count = self.shared.audio_sample_count.lock().await;

        if matches!(*state, MockSttState::WaitingForAudio)
            && *utt_idx < self.config.utterances.len()
        {
            *audio_count += audio.len();
            if *audio_count > AUDIO_TRIGGER_THRESHOLD {
                *state = MockSttState::AudioReceived;
            }
        }
        Ok(())
    }

    async fn close(&mut self) -> Result<()> {
        let mut state = self.shared.state.lock().await;
        *state = MockSttState::Closed;
        Ok(())
    }
}

#[async_trait]
impl SttReceiver for MockSttReceiver {
    async fn recv_event(&mut self) -> Result<Option<SttEvent>> {
        loop {
            let mut state = self.shared.state.lock().await;
            let utt_idx = self.shared.utterance_index.lock().await;
            let audio_count = self.shared.audio_sample_count.lock().await;

            match &*state {
                MockSttState::WaitingForAudio => {
                    // Release locks and yield so send_audio / close can make
                    // progress.  Short sleep keeps CPU usage low while letting
                    // tokio::select! poll other branches (cancel, idle timeout).
                    drop((state, utt_idx, audio_count));
                    sleep(Duration::from_millis(5)).await;
                    continue;
                }

                MockSttState::AudioReceived => {
                    let delay = self.config.utterances[*utt_idx].delay_before_start;
                    let ts = ((*audio_count) as u64 * 1000) / 16000;
                    drop((state, utt_idx, audio_count)); // Release locks before sleep
                    let scaled = delay.mul_f64(self.config.latency_multiplier);
                    if !scaled.is_zero() {
                        sleep(scaled).await;
                    }
                    let mut state = self.shared.state.lock().await;
                    *state = MockSttState::Partial(0);
                    return Ok(Some(SttEvent::SpeechStart { timestamp_ms: ts }));
                }

                MockSttState::Partial(n) => {
                    let n = *n;
                    let utt = self.config.utterances[*utt_idx].clone();
                    if n < NUM_PARTIALS {
                        let interval = utt.partial_interval;
                        drop((state, utt_idx, audio_count)); // Release locks before sleep
                        let scaled = interval.mul_f64(self.config.latency_multiplier);
                        if !scaled.is_zero() {
                            sleep(scaled).await;
                        }
                        let mut state = self.shared.state.lock().await;
                        let end = ((n + 1) * utt.text.len()) / NUM_PARTIALS;
                        let partial_text = utt.text[..end].to_string();
                        *state = MockSttState::Partial(n + 1);
                        return Ok(Some(SttEvent::Partial { text: partial_text }));
                    } else {
                        // All partials done — emit Final.
                        let delay_to_final = utt.delay_to_final;
                        drop((state, utt_idx, audio_count)); // Release locks before sleep
                        let scaled = delay_to_final.mul_f64(self.config.latency_multiplier);
                        if !scaled.is_zero() {
                            sleep(scaled).await;
                        }
                        let mut state = self.shared.state.lock().await;
                        let duration_ms = utt.text.len() as f64 * 100.0;
                        *state = MockSttState::SpeechEndReady;
                        return Ok(Some(SttEvent::Final {
                            text: utt.text.clone(),
                            language: utt.language.clone(),
                            confidence: utt.confidence,
                            duration_ms,
                        }));
                    }
                }

                MockSttState::SpeechEndReady => {
                    let duration_ms = self.config.utterances[*utt_idx].text.len() as f64 * 100.0;
                    let ts = ((*audio_count) as u64 * 1000) / 16000;
                    *state = MockSttState::NextUtterance;
                    return Ok(Some(SttEvent::SpeechEnd {
                        timestamp_ms: ts,
                        duration_ms,
                    }));
                }

                MockSttState::NextUtterance => {
                    let next_idx = *utt_idx + 1;
                    // Drop read-only guards before re-acquiring for write.
                    drop((utt_idx, audio_count));
                    {
                        let mut idx = self.shared.utterance_index.lock().await;
                        *idx = next_idx;
                        let mut cnt = self.shared.audio_sample_count.lock().await;
                        *cnt = 0;
                    }
                    if next_idx < self.config.utterances.len() {
                        *state = MockSttState::WaitingForAudio;
                        drop(state);
                        continue;
                    } else if self.config.close_after_all {
                        *state = MockSttState::Closed;
                        return Ok(None);
                    } else {
                        *state = MockSttState::WaitingForAudio;
                        drop(state);
                        continue;
                    }
                }

                MockSttState::Closed => return Ok(None),
            }
        }
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

        let (mut sender, mut receiver) = provider.connect().await.unwrap();

        // Send enough audio to trigger (recv_event blocks until audio arrives).
        sender.send_audio(&vec![0.1f32; 400]).await.unwrap();

        // SpeechStart
        let event = receiver.recv_event().await.unwrap().unwrap();
        assert!(matches!(event, SttEvent::SpeechStart { .. }));

        // 3 Partials with progressive text slicing.
        for i in 0..NUM_PARTIALS {
            let event = receiver.recv_event().await.unwrap().unwrap();
            match &event {
                SttEvent::Partial { text } => {
                    let expected_end = ((i + 1) * "hello world".len()) / NUM_PARTIALS;
                    assert_eq!(text, &"hello world"[..expected_end]);
                }
                _ => panic!("expected Partial, got {:?}", event),
            }
        }

        // Final
        let event = receiver.recv_event().await.unwrap().unwrap();
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
        let event = receiver.recv_event().await.unwrap().unwrap();
        assert!(matches!(event, SttEvent::SpeechEnd { .. }));

        // Session ends (close_after_all).
        assert!(receiver.recv_event().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn multiple_utterances() {
        let provider = MockSttProvider::new(MockSttConfig {
            utterances: vec![simple_utterance("hello"), simple_utterance("world")],
            close_after_all: true,
            latency_multiplier: 1.0,
        });

        let (mut sender, mut receiver) = provider.connect().await.unwrap();

        // First utterance: SpeechStart + 3 Partial + Final + SpeechEnd = 6 events
        sender.send_audio(&vec![0.1f32; 400]).await.unwrap();
        let mut events = Vec::new();
        for _ in 0..6 {
            let event = receiver.recv_event().await.unwrap().unwrap();
            events.push(event);
        }
        assert_eq!(events.len(), 6);

        // Second utterance: After the first 6 events, state is NextUtterance.
        // recv_event will transition NextUtterance → WaitingForAudio, then poll.
        // We must send audio concurrently so it arrives while WaitingForAudio.
        let recv_handle = tokio::spawn(async move {
            let mut events = Vec::new();
            for _ in 0..6 {
                let event = receiver.recv_event().await.unwrap().unwrap();
                events.push(event);
            }
            (events, receiver)
        });

        // Small delay so recv_event reaches WaitingForAudio before we send.
        sleep(Duration::from_millis(20)).await;
        sender.send_audio(&vec![0.1f32; 400]).await.unwrap();

        let (events, mut receiver) = recv_handle.await.unwrap();
        assert_eq!(events.len(), 6);

        // Verify second utterance produced "world".
        match &events[4] {
            SttEvent::Final { text, .. } => assert_eq!(text, "world"),
            _ => panic!("expected Final"),
        }

        // After all utterances with close_after_all=true, session ends.
        assert!(receiver.recv_event().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn close_after_all() {
        let provider = MockSttProvider::new(MockSttConfig {
            utterances: vec![simple_utterance("test")],
            close_after_all: true,
            latency_multiplier: 1.0,
        });

        let (mut sender, mut receiver) = provider.connect().await.unwrap();
        sender.send_audio(&vec![0.1f32; 400]).await.unwrap();

        // Drain all events.
        while receiver.recv_event().await.unwrap().is_some() {}

        // Subsequent calls keep returning None.
        assert!(receiver.recv_event().await.unwrap().is_none());
        assert!(receiver.recv_event().await.unwrap().is_none());
    }
}
