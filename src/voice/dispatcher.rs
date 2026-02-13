//! Dispatcher — routes audio to per-user pipeline workers.
//!
//! Manages worker lifecycle, spawning new workers for unknown users
//! and forwarding PCM chunks via unbounded channels.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::mpsc;
use tracing::{error, info};

use super::agent_bridge::AgentBridge;
use super::provider::{SttProvider, TtsProvider};
use super::transcript::TranscriptEntry;
use super::worker::PipelineWorker;

/// Routes incoming audio to the correct [`PipelineWorker`].
pub struct Dispatcher {
    workers: HashMap<u64, mpsc::UnboundedSender<Vec<f32>>>,
    stt_provider: Arc<dyn SttProvider>,
    tts_provider: Arc<dyn TtsProvider>,
    agent_bridge: Arc<dyn AgentBridge>,
    audio_output_tx: mpsc::UnboundedSender<(u64, Vec<f32>)>,
    transcript_tx: Option<mpsc::UnboundedSender<TranscriptEntry>>,
    bot_name: String,
}

impl Dispatcher {
    pub fn new(
        stt_provider: Arc<dyn SttProvider>,
        tts_provider: Arc<dyn TtsProvider>,
        agent_bridge: Arc<dyn AgentBridge>,
        audio_output_tx: mpsc::UnboundedSender<(u64, Vec<f32>)>,
        transcript_tx: Option<mpsc::UnboundedSender<TranscriptEntry>>,
        bot_name: String,
    ) -> Self {
        Self {
            workers: HashMap::new(),
            stt_provider,
            tts_provider,
            agent_bridge,
            audio_output_tx,
            transcript_tx,
            bot_name,
        }
    }

    /// Dispatch audio to the worker for the given user.
    ///
    /// If no worker exists yet, a new one is spawned in a background task.
    pub fn dispatch(&mut self, user_id: u64, user_name: String, audio: Vec<f32>) {
        let tx = self.workers.entry(user_id).or_insert_with(|| {
            let (tx, rx) = mpsc::unbounded_channel();
            let stt = self.stt_provider.clone();
            let tts = self.tts_provider.clone();
            let bridge = self.agent_bridge.clone();
            let output_tx = self.audio_output_tx.clone();
            let transcript_tx = self.transcript_tx.clone();
            let bot_name = self.bot_name.clone();
            let uname = user_name.clone();
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
                );
                if let Err(e) = worker.run().await {
                    error!(user_id, "Worker error: {}", e);
                }
                info!(user_id, "Worker finished");
            });
            tx
        });

        if tx.send(audio).is_err() {
            // Worker task has exited; remove stale entry.
            self.workers.remove(&user_id);
        }
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
            Dispatcher::new(stt, tts, bridge, out_tx, None, "Bot".to_string()),
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
}
