//! Pipeline worker — per-user STT → Agent → TTS processing.
//!
//! Each worker owns an STT session, an agent bridge reference,
//! and a TTS provider reference.  It receives PCM chunks from
//! the dispatcher and produces response audio sent back to the
//! main thread for playback via songbird.
//!
//! Supports barge-in (interrupt) via `CancellationToken` and
//! idle timeout via configurable silence duration.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info};

use super::agent_bridge::AgentBridge;
use super::provider::{SttEvent, SttProvider, TtsProvider};
use super::transcript::TranscriptEntry;

/// Per-user voice processing pipeline.
pub struct PipelineWorker {
    user_id: u64,
    user_name: String,
    bot_name: String,
    stt_provider: Arc<dyn SttProvider>,
    tts_provider: Arc<dyn TtsProvider>,
    agent_bridge: Arc<dyn AgentBridge>,
    audio_rx: mpsc::UnboundedReceiver<Vec<f32>>,
    audio_output_tx: mpsc::UnboundedSender<(u64, Vec<f32>)>,
    transcript_tx: Option<mpsc::UnboundedSender<TranscriptEntry>>,
    /// Shared flag indicating whether the bot is currently playing audio.
    is_playing: Arc<AtomicBool>,
    /// Token cancelled by the dispatcher on barge-in to abort LLM/TTS.
    cancel: CancellationToken,
    /// Idle timeout duration (0 = disabled).
    idle_timeout: Duration,
}

impl PipelineWorker {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        user_id: u64,
        user_name: String,
        bot_name: String,
        stt_provider: Arc<dyn SttProvider>,
        tts_provider: Arc<dyn TtsProvider>,
        agent_bridge: Arc<dyn AgentBridge>,
        audio_rx: mpsc::UnboundedReceiver<Vec<f32>>,
        audio_output_tx: mpsc::UnboundedSender<(u64, Vec<f32>)>,
        transcript_tx: Option<mpsc::UnboundedSender<TranscriptEntry>>,
        is_playing: Arc<AtomicBool>,
        cancel: CancellationToken,
        idle_timeout_sec: u64,
    ) -> Self {
        Self {
            user_id,
            user_name,
            bot_name,
            stt_provider,
            tts_provider,
            agent_bridge,
            audio_rx,
            audio_output_tx,
            transcript_tx,
            is_playing,
            cancel,
            idle_timeout: if idle_timeout_sec == 0 {
                // Effectively infinite — will never fire within a process lifetime.
                Duration::from_secs(86400 * 365) // ~1 year
            } else {
                Duration::from_secs(idle_timeout_sec)
            },
        }
    }

    /// Run the worker loop.
    ///
    /// Receives PCM chunks, forwards to STT, drains recognition events,
    /// calls the agent bridge for final transcriptions, synthesizes TTS,
    /// and emits transcript entries.
    ///
    /// The loop exits when:
    /// - The audio input channel closes.
    /// - The cancellation token is cancelled (shutdown).
    /// - The idle timeout fires (no speech for `idle_timeout` duration).
    pub async fn run(&mut self) -> Result<WorkerExitReason> {
        info!(user_id = self.user_id, "PipelineWorker started");

        let mut stt_session = self.stt_provider.connect().await?;
        let mut last_speech_at = Instant::now();

        loop {
            let idle_deadline = last_speech_at + self.idle_timeout;

            tokio::select! {
                biased;

                // External cancellation (shutdown).
                _ = self.cancel.cancelled() => {
                    info!(user_id = self.user_id, "PipelineWorker cancelled");
                    stt_session.close().await?;
                    return Ok(WorkerExitReason::Cancelled);
                }

                // Idle timeout.
                _ = tokio::time::sleep_until(idle_deadline) => {
                    info!(
                        user_id = self.user_id,
                        timeout_secs = self.idle_timeout.as_secs(),
                        "Idle timeout reached, stopping worker"
                    );
                    stt_session.close().await?;
                    return Ok(WorkerExitReason::IdleTimeout);
                }

                // Audio input.
                pcm = self.audio_rx.recv() => {
                    let Some(pcm) = pcm else {
                        // Channel closed — dispatcher removed us.
                        break;
                    };

                    stt_session.send_audio(&pcm).await?;

                    // Drain all available events after sending audio.
                    loop {
                        match stt_session.recv_event().await? {
                            Some(SttEvent::SpeechStart { .. }) => {
                                last_speech_at = Instant::now();
                                debug!(user_id = self.user_id, "Speech start (timer reset)");

                                // Barge-in: if bot is playing, signal interrupt.
                                if self.is_playing.load(Ordering::Acquire) {
                                    info!(
                                        user_id = self.user_id,
                                        "Barge-in detected, cancelling playback"
                                    );
                                    // The dispatcher watches is_playing and will
                                    // handle the actual cancellation/token rotation.
                                    // We notify via a special audio output message.
                                    let _ = self.audio_output_tx.send((self.user_id, vec![]));
                                }
                            }
                            Some(SttEvent::Final { ref text, .. }) => {
                                last_speech_at = Instant::now();
                                if text.trim().is_empty() {
                                    continue;
                                }
                                debug!(user_id = self.user_id, text, "STT final");

                                // Log user speech transcript.
                                self.send_transcript(TranscriptEntry::UserSpeech {
                                    user_id: self.user_id,
                                    user_name: self.user_name.clone(),
                                    text: text.clone(),
                                });

                                // Process text through agent + TTS with cancellation support.
                                self.process_text(text).await?;
                            }
                            Some(event) => {
                                debug!(user_id = self.user_id, ?event, "STT event");
                            }
                            None => break,
                        }
                    }
                }
            }
        }

        stt_session.close().await?;
        info!(user_id = self.user_id, "PipelineWorker stopped");
        Ok(WorkerExitReason::ChannelClosed)
    }

    /// Generate agent response and synthesize TTS, with cancellation support.
    ///
    /// If the cancellation token fires during LLM generation or TTS synthesis,
    /// we record the partial transcript and return early.
    async fn process_text(&self, text: &str) -> Result<()> {
        // Create a child token so that barge-in during this specific
        // response can be detected without killing the whole worker.
        let response_cancel = self.cancel.child_token();

        // Generate agent response — cancellable.
        let response = tokio::select! {
            biased;
            _ = response_cancel.cancelled() => {
                debug!(user_id = self.user_id, "LLM generation cancelled by interrupt");
                return Ok(());
            }
            result = self.agent_bridge.generate(self.user_id, text) => {
                result?
            }
        };

        // Check cancellation before starting TTS.
        if response_cancel.is_cancelled() {
            debug!(user_id = self.user_id, "Cancelled before TTS");
            return Ok(());
        }

        // Mark as playing before TTS synthesis + playback.
        self.is_playing.store(true, Ordering::Release);

        // Synthesize TTS — cancellable.
        let tts_result = tokio::select! {
            biased;
            _ = response_cancel.cancelled() => {
                self.is_playing.store(false, Ordering::Release);
                debug!(user_id = self.user_id, "TTS synthesis cancelled by interrupt");
                // Record interrupted transcript (nothing played yet).
                self.send_transcript(TranscriptEntry::BotResponseInterrupted {
                    bot_name: self.bot_name.clone(),
                    played_text: String::new(),
                });
                return Ok(());
            }
            result = self.tts_provider.synthesize(&response) => {
                result?
            }
        };

        // Check cancellation before sending audio.
        if response_cancel.is_cancelled() {
            self.is_playing.store(false, Ordering::Release);
            self.send_transcript(TranscriptEntry::BotResponseInterrupted {
                bot_name: self.bot_name.clone(),
                played_text: String::new(),
            });
            return Ok(());
        }

        // Log bot response transcript.
        self.send_transcript(TranscriptEntry::BotResponse {
            bot_name: self.bot_name.clone(),
            text: response.clone(),
        });

        // Send audio for playback.
        if self
            .audio_output_tx
            .send((self.user_id, tts_result.audio))
            .is_err()
        {
            error!(user_id = self.user_id, "Audio output channel closed");
        }

        self.is_playing.store(false, Ordering::Release);
        Ok(())
    }

    /// Send a transcript entry if the transcript channel is configured.
    fn send_transcript(&self, entry: TranscriptEntry) {
        if let Some(ref tx) = self.transcript_tx {
            if tx.send(entry).is_err() {
                debug!(user_id = self.user_id, "Transcript channel closed");
            }
        }
    }

    /// Returns a reference to the shared is_playing flag.
    pub fn is_playing(&self) -> &Arc<AtomicBool> {
        &self.is_playing
    }
}

/// Reason the worker exited its run loop.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkerExitReason {
    /// Audio input channel was closed.
    ChannelClosed,
    /// Idle timeout (no speech for configured duration).
    IdleTimeout,
    /// Cancelled externally (shutdown or barge-in at worker level).
    Cancelled,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voice::agent_bridge::MockAgentBridge;
    use crate::voice::provider::stt::mock::{MockSttConfig, MockSttProvider, MockUtterance};
    use crate::voice::provider::tts::mock::MockTtsProvider;

    /// Default idle timeout for tests (5 minutes).
    const DEFAULT_IDLE_TIMEOUT_SEC: u64 = 300;

    fn default_stt() -> Arc<dyn SttProvider> {
        Arc::new(MockSttProvider::new(MockSttConfig {
            utterances: vec![MockUtterance {
                text: "hello".to_string(),
                language: "en".to_string(),
                delay_before_start: Duration::ZERO,
                partial_interval: Duration::ZERO,
                delay_to_final: Duration::ZERO,
                confidence: 0.95,
            }],
            close_after_all: true,
            latency_multiplier: 1.0,
        }))
    }

    fn make_worker(
        stt: Arc<dyn SttProvider>,
        tts: Arc<dyn TtsProvider>,
        bridge: Arc<dyn AgentBridge>,
        audio_rx: mpsc::UnboundedReceiver<Vec<f32>>,
        audio_output_tx: mpsc::UnboundedSender<(u64, Vec<f32>)>,
        transcript_tx: Option<mpsc::UnboundedSender<TranscriptEntry>>,
        idle_timeout_sec: u64,
    ) -> (PipelineWorker, Arc<AtomicBool>, CancellationToken) {
        let is_playing = Arc::new(AtomicBool::new(false));
        let cancel = CancellationToken::new();
        let worker = PipelineWorker::new(
            1,
            "User1".to_string(),
            "Bot".to_string(),
            stt,
            tts,
            bridge,
            audio_rx,
            audio_output_tx,
            transcript_tx,
            is_playing.clone(),
            cancel.clone(),
            idle_timeout_sec,
        );
        (worker, is_playing, cancel)
    }

    #[test]
    fn worker_new() {
        let stt: Arc<dyn SttProvider> = Arc::new(MockSttProvider::new(MockSttConfig {
            utterances: vec![],
            close_after_all: true,
            latency_multiplier: 1.0,
        }));
        let tts: Arc<dyn TtsProvider> = Arc::new(MockTtsProvider::silent());
        let bridge: Arc<dyn AgentBridge> = Arc::new(MockAgentBridge::new());
        let (_tx, rx) = mpsc::unbounded_channel();
        let (out_tx, _out_rx) = mpsc::unbounded_channel();
        let is_playing = Arc::new(AtomicBool::new(false));
        let cancel = CancellationToken::new();
        let w = PipelineWorker::new(
            42,
            "User42".to_string(),
            "Bot".to_string(),
            stt,
            tts,
            bridge,
            rx,
            out_tx,
            None,
            is_playing,
            cancel,
            DEFAULT_IDLE_TIMEOUT_SEC,
        );
        assert_eq!(w.user_id, 42);
    }

    #[tokio::test]
    async fn pipeline_stt_to_tts() {
        let stt = default_stt();
        let tts: Arc<dyn TtsProvider> = Arc::new(MockTtsProvider::silent());
        let bridge: Arc<dyn AgentBridge> = Arc::new(MockAgentBridge::new());

        let (in_tx, in_rx) = mpsc::unbounded_channel();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();

        let (mut worker, _is_playing, _cancel) =
            make_worker(stt, tts, bridge, in_rx, out_tx, None, DEFAULT_IDLE_TIMEOUT_SEC);
        let handle = tokio::spawn(async move { worker.run().await });

        // Send enough audio to trigger STT (> 320 samples).
        in_tx.send(vec![0.1f32; 400]).unwrap();

        // Receive TTS output.
        let (uid, audio) = tokio::time::timeout(Duration::from_secs(5), out_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(uid, 1);
        assert!(!audio.is_empty());

        // Close input channel so the worker loop exits.
        drop(in_tx);
        let result = handle.await.unwrap();
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), WorkerExitReason::ChannelClosed);
    }

    #[tokio::test]
    async fn pipeline_emits_transcript() {
        let stt = default_stt();
        let tts: Arc<dyn TtsProvider> = Arc::new(MockTtsProvider::silent());
        let bridge: Arc<dyn AgentBridge> = Arc::new(MockAgentBridge::new());

        let (in_tx, in_rx) = mpsc::unbounded_channel();
        let (out_tx, _out_rx) = mpsc::unbounded_channel();
        let (transcript_tx, mut transcript_rx) = mpsc::unbounded_channel();

        let (mut worker, _is_playing, _cancel) = make_worker(
            stt,
            tts,
            bridge,
            in_rx,
            out_tx,
            Some(transcript_tx),
            DEFAULT_IDLE_TIMEOUT_SEC,
        );
        let handle = tokio::spawn(async move { worker.run().await });

        in_tx.send(vec![0.1f32; 400]).unwrap();

        // Should receive user speech transcript.
        let entry = tokio::time::timeout(Duration::from_secs(5), transcript_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            entry,
            TranscriptEntry::UserSpeech {
                user_id: 1,
                user_name: "User1".to_string(),
                text: "hello".to_string(),
            }
        );

        // Should receive bot response transcript.
        let entry = tokio::time::timeout(Duration::from_secs(5), transcript_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(
            entry,
            TranscriptEntry::BotResponse {
                bot_name: "Bot".to_string(),
                text: "echo: hello".to_string(),
            }
        );

        drop(in_tx);
        let result = handle.await.unwrap();
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn pipeline_works_without_transcript() {
        let stt = default_stt();
        let tts: Arc<dyn TtsProvider> = Arc::new(MockTtsProvider::silent());
        let bridge: Arc<dyn AgentBridge> = Arc::new(MockAgentBridge::new());

        let (in_tx, in_rx) = mpsc::unbounded_channel();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();

        let (mut worker, _is_playing, _cancel) =
            make_worker(stt, tts, bridge, in_rx, out_tx, None, DEFAULT_IDLE_TIMEOUT_SEC);
        let handle = tokio::spawn(async move { worker.run().await });

        in_tx.send(vec![0.1f32; 400]).unwrap();

        let (uid, audio) = tokio::time::timeout(Duration::from_secs(5), out_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(uid, 1);
        assert!(!audio.is_empty());

        drop(in_tx);
        assert!(handle.await.unwrap().is_ok());
    }

    #[tokio::test]
    async fn cancellation_stops_worker() {
        let stt: Arc<dyn SttProvider> = Arc::new(MockSttProvider::new(MockSttConfig {
            utterances: vec![],
            close_after_all: false, // Keep session open.
            latency_multiplier: 1.0,
        }));
        let tts: Arc<dyn TtsProvider> = Arc::new(MockTtsProvider::silent());
        let bridge: Arc<dyn AgentBridge> = Arc::new(MockAgentBridge::new());

        let (_in_tx, in_rx) = mpsc::unbounded_channel();
        let (out_tx, _out_rx) = mpsc::unbounded_channel();

        let (mut worker, _is_playing, cancel) =
            make_worker(stt, tts, bridge, in_rx, out_tx, None, 0);
        let handle = tokio::spawn(async move { worker.run().await });

        // Cancel after a short delay.
        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel.cancel();

        let result = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .unwrap()
            .unwrap();
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), WorkerExitReason::Cancelled);
    }

    #[tokio::test]
    async fn idle_timeout_stops_worker() {
        let stt: Arc<dyn SttProvider> = Arc::new(MockSttProvider::new(MockSttConfig {
            utterances: vec![],
            close_after_all: false, // Keep session open.
            latency_multiplier: 1.0,
        }));
        let tts: Arc<dyn TtsProvider> = Arc::new(MockTtsProvider::silent());
        let bridge: Arc<dyn AgentBridge> = Arc::new(MockAgentBridge::new());

        let (_in_tx, in_rx) = mpsc::unbounded_channel();
        let (out_tx, _out_rx) = mpsc::unbounded_channel();

        // Use a very short idle timeout (1 second).
        let (mut worker, _is_playing, _cancel) =
            make_worker(stt, tts, bridge, in_rx, out_tx, None, 1);
        let handle = tokio::spawn(async move { worker.run().await });

        // Wait for idle timeout to fire.
        let result = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .unwrap()
            .unwrap();
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), WorkerExitReason::IdleTimeout);
    }

    #[tokio::test]
    async fn barge_in_sends_empty_audio_signal() {
        // Use an STT that emits SpeechStart first.
        let stt: Arc<dyn SttProvider> = Arc::new(MockSttProvider::new(MockSttConfig {
            utterances: vec![MockUtterance {
                text: "stop".to_string(),
                language: "en".to_string(),
                delay_before_start: Duration::ZERO,
                partial_interval: Duration::ZERO,
                delay_to_final: Duration::from_millis(50),
                confidence: 0.9,
            }],
            close_after_all: true,
            latency_multiplier: 1.0,
        }));
        let tts: Arc<dyn TtsProvider> = Arc::new(MockTtsProvider::silent());
        let bridge: Arc<dyn AgentBridge> = Arc::new(MockAgentBridge::new());

        let (in_tx, in_rx) = mpsc::unbounded_channel();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();

        let (mut worker, is_playing, _cancel) =
            make_worker(stt, tts, bridge, in_rx, out_tx, None, DEFAULT_IDLE_TIMEOUT_SEC);

        // Simulate bot playing audio.
        is_playing.store(true, Ordering::Release);

        let handle = tokio::spawn(async move { worker.run().await });

        // Send audio — this triggers SpeechStart which should detect barge-in.
        in_tx.send(vec![0.1f32; 400]).unwrap();

        // First output should be the barge-in signal (empty audio).
        let (uid, audio) = tokio::time::timeout(Duration::from_secs(5), out_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(uid, 1);
        assert!(audio.is_empty(), "Barge-in should send empty audio signal");

        // Then the actual TTS response.
        let (uid, audio) = tokio::time::timeout(Duration::from_secs(5), out_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(uid, 1);
        assert!(!audio.is_empty());

        drop(in_tx);
        let result = handle.await.unwrap();
        assert!(result.is_ok());
    }
}
