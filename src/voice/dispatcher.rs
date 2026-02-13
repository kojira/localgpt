//! Dispatcher — routes audio to per-user pipeline workers.
//!
//! Manages worker lifecycle, spawning new workers for unknown users
//! and forwarding PCM chunks via unbounded channels.
//! Supports barge-in by tracking CancellationTokens and is_playing
//! flags per user.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info};

use super::agent_bridge::AgentBridge;
use super::provider::{SttProvider, TtsProvider};
use super::transcript::TranscriptEntry;
use super::worker::PipelineWorker;

/// Per-user worker state held by the dispatcher.
struct WorkerState {
    /// Channel to send audio to the worker.
    audio_tx: mpsc::UnboundedSender<Vec<f32>>,
    /// Shared flag: true while the worker is playing back TTS audio.
    is_playing: Arc<AtomicBool>,
    /// Token to cancel the current LLM/TTS pipeline on barge-in.
    cancel: CancellationToken,
}

/// Routes incoming audio to the correct [`PipelineWorker`].
pub struct Dispatcher {
    workers: HashMap<u64, WorkerState>,
    stt_provider: Arc<dyn SttProvider>,
    tts_provider: Arc<dyn TtsProvider>,
    agent_bridge: Arc<dyn AgentBridge>,
    audio_output_tx: mpsc::UnboundedSender<(u64, Vec<f32>)>,
    transcript_tx: Option<mpsc::UnboundedSender<TranscriptEntry>>,
    bot_name: String,
    idle_timeout_sec: u64,
    interrupt_enabled: bool,
}

impl Dispatcher {
    pub fn new(
        stt_provider: Arc<dyn SttProvider>,
        tts_provider: Arc<dyn TtsProvider>,
        agent_bridge: Arc<dyn AgentBridge>,
        audio_output_tx: mpsc::UnboundedSender<(u64, Vec<f32>)>,
        transcript_tx: Option<mpsc::UnboundedSender<TranscriptEntry>>,
        bot_name: String,
        idle_timeout_sec: u64,
        interrupt_enabled: bool,
    ) -> Self {
        Self {
            workers: HashMap::new(),
            stt_provider,
            tts_provider,
            agent_bridge,
            audio_output_tx,
            transcript_tx,
            bot_name,
            idle_timeout_sec,
            interrupt_enabled,
        }
    }

    /// Dispatch audio to the worker for the given user.
    ///
    /// If no worker exists yet, a new one is spawned in a background task.
    pub fn dispatch(&mut self, user_id: u64, user_name: String, audio: Vec<f32>) {
        let state = self.workers.entry(user_id).or_insert_with(|| {
            let (tx, rx) = mpsc::unbounded_channel();
            let stt = self.stt_provider.clone();
            let tts = self.tts_provider.clone();
            let bridge = self.agent_bridge.clone();
            let output_tx = self.audio_output_tx.clone();
            let transcript_tx = self.transcript_tx.clone();
            let bot_name = self.bot_name.clone();
            let uname = user_name.clone();
            let is_playing = Arc::new(AtomicBool::new(false));
            let cancel = CancellationToken::new();
            let idle_timeout_sec = self.idle_timeout_sec;

            let is_playing_clone = is_playing.clone();
            let cancel_clone = cancel.clone();

            tokio::spawn(async move {
                let mut worker = PipelineWorker::new(
                    user_id,
                    uname,
                    bot_name,
                    stt,
                    tts,
                    bridge,
                    rx,
                    output_tx,
                    transcript_tx,
                    is_playing_clone,
                    cancel_clone,
                    idle_timeout_sec,
                );
                match worker.run().await {
                    Ok(reason) => {
                        info!(user_id, ?reason, "Worker finished");
                    }
                    Err(e) => {
                        error!(user_id, "Worker error: {}", e);
                    }
                }
            });
            WorkerState {
                audio_tx: tx,
                is_playing,
                cancel,
            }
        });

        if state.audio_tx.send(audio).is_err() {
            // Worker task has exited; remove stale entry.
            self.workers.remove(&user_id);
        }
    }

    /// Handle a barge-in interrupt for a user.
    ///
    /// Cancels the current LLM/TTS pipeline and prepares a new
    /// CancellationToken for the next response.
    pub fn handle_interrupt(&mut self, user_id: u64) {
        if !self.interrupt_enabled {
            debug!(user_id, "Interrupt disabled in config, ignoring");
            return;
        }

        if let Some(state) = self.workers.get_mut(&user_id) {
            if state.is_playing.load(Ordering::Acquire) {
                info!(user_id, "Interrupt: cancelling LLM stream and TTS playback");

                // Cancel the current pipeline.
                state.cancel.cancel();

                // Create a new token for the next response.
                let new_cancel = CancellationToken::new();
                state.cancel = new_cancel;

                // Clear playing flag.
                state.is_playing.store(false, Ordering::Release);
            }
        }
    }

    /// Check if a user's worker is currently playing audio.
    pub fn is_user_playing(&self, user_id: u64) -> bool {
        self.workers
            .get(&user_id)
            .is_some_and(|s| s.is_playing.load(Ordering::Acquire))
    }

    /// Start the dispatch loop (stub — to be wired to AudioChunk receiver).
    pub async fn run(&self) -> Result<()> {
        info!("Dispatcher::run (stub)");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voice::agent_bridge::MockAgentBridge;
    use crate::voice::provider::stt::mock::{MockSttConfig, MockSttProvider, MockUtterance};
    use crate::voice::provider::tts::mock::MockTtsProvider;
    use std::time::Duration;

    fn make_dispatcher() -> (Dispatcher, mpsc::UnboundedReceiver<(u64, Vec<f32>)>) {
        let stt: Arc<dyn SttProvider> = Arc::new(MockSttProvider::new(MockSttConfig {
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
        }));
        let tts: Arc<dyn TtsProvider> = Arc::new(MockTtsProvider::silent());
        let bridge: Arc<dyn AgentBridge> = Arc::new(MockAgentBridge::new());
        let (out_tx, out_rx) = mpsc::unbounded_channel();
        (
            Dispatcher::new(stt, tts, bridge, out_tx, None, "Bot".to_string(), 300, true),
            out_rx,
        )
    }

    #[test]
    fn dispatcher_new() {
        let (_d, _rx) = make_dispatcher();
    }

    #[tokio::test]
    async fn dispatcher_dispatch_spawns_worker() {
        let (mut d, mut out_rx) = make_dispatcher();

        // Dispatch audio for user 1.
        d.dispatch(1, "User1".to_string(), vec![0.1f32; 400]);

        // Should receive TTS output from the spawned worker.
        let (uid, audio) = tokio::time::timeout(Duration::from_secs(5), out_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(uid, 1);
        assert!(!audio.is_empty());
    }

    #[tokio::test]
    async fn dispatcher_run_stub_succeeds() {
        let (d, _rx) = make_dispatcher();
        assert!(d.run().await.is_ok());
    }

    #[tokio::test]
    async fn dispatcher_with_transcript() {
        let stt: Arc<dyn SttProvider> = Arc::new(MockSttProvider::new(MockSttConfig {
            utterances: vec![MockUtterance {
                text: "hi".to_string(),
                language: "en".to_string(),
                delay_before_start: Duration::ZERO,
                partial_interval: Duration::ZERO,
                delay_to_final: Duration::ZERO,
                confidence: 0.9,
            }],
            close_after_all: true,
            latency_multiplier: 1.0,
        }));
        let tts: Arc<dyn TtsProvider> = Arc::new(MockTtsProvider::silent());
        let bridge: Arc<dyn AgentBridge> = Arc::new(MockAgentBridge::new());
        let (out_tx, _out_rx) = mpsc::unbounded_channel();
        let (transcript_tx, mut transcript_rx) = mpsc::unbounded_channel();

        let mut d = Dispatcher::new(
            stt,
            tts,
            bridge,
            out_tx,
            Some(transcript_tx),
            "TestBot".to_string(),
            300,
            true,
        );

        d.dispatch(1, "Alice".to_string(), vec![0.1f32; 400]);

        // User speech entry.
        let entry = tokio::time::timeout(Duration::from_secs(5), transcript_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(entry, TranscriptEntry::UserSpeech { .. }));

        // Bot response entry.
        let entry = tokio::time::timeout(Duration::from_secs(5), transcript_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(entry, TranscriptEntry::BotResponse { .. }));
    }

    #[test]
    fn handle_interrupt_cancels_playing() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let (mut d, _out_rx) = make_dispatcher();

            // Spawn a worker first.
            d.dispatch(1, "User1".to_string(), vec![0.1f32; 400]);

            // Give worker time to start.
            tokio::time::sleep(Duration::from_millis(10)).await;

            // Manually set is_playing to true.
            if let Some(state) = d.workers.get(&1) {
                state.is_playing.store(true, Ordering::Release);
            }

            assert!(d.is_user_playing(1));

            // Handle interrupt.
            d.handle_interrupt(1);

            // Should no longer be playing.
            assert!(!d.is_user_playing(1));
        });
    }

    #[test]
    fn handle_interrupt_noop_when_not_playing() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let (mut d, _out_rx) = make_dispatcher();

            d.dispatch(1, "User1".to_string(), vec![0.1f32; 400]);
            tokio::time::sleep(Duration::from_millis(10)).await;

            // Not playing — interrupt should be a no-op.
            d.handle_interrupt(1);
            assert!(!d.is_user_playing(1));
        });
    }

    #[test]
    fn handle_interrupt_disabled_in_config() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let stt: Arc<dyn SttProvider> = Arc::new(MockSttProvider::new(MockSttConfig {
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
            }));
            let tts: Arc<dyn TtsProvider> = Arc::new(MockTtsProvider::silent());
            let bridge: Arc<dyn AgentBridge> = Arc::new(MockAgentBridge::new());
            let (out_tx, _out_rx) = mpsc::unbounded_channel();

            // interrupt_enabled = false
            let mut d = Dispatcher::new(
                stt,
                tts,
                bridge,
                out_tx,
                None,
                "Bot".to_string(),
                300,
                false,
            );

            d.dispatch(1, "User1".to_string(), vec![0.1f32; 400]);
            tokio::time::sleep(Duration::from_millis(10)).await;

            if let Some(state) = d.workers.get(&1) {
                state.is_playing.store(true, Ordering::Release);
            }

            // Should remain playing because interrupt is disabled.
            d.handle_interrupt(1);
            assert!(d.is_user_playing(1));
        });
    }

    #[test]
    fn is_user_playing_unknown_user() {
        let (_d, _rx) = make_dispatcher();
        let (d, _rx) = make_dispatcher();
        assert!(!d.is_user_playing(999));
    }
}
