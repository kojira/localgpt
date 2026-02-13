//! Dispatcher — routes audio to per-user pipeline workers.
//!
//! Manages worker lifecycle, spawning new workers for unknown users
//! and forwarding PCM chunks via unbounded channels.

use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;
use tokio::sync::mpsc;
use tracing::{error, info};

use super::provider::{SttProvider, TtsProvider};
use super::worker::PipelineWorker;

/// Routes incoming audio to the correct [`PipelineWorker`].
pub struct Dispatcher {
    workers: HashMap<u64, mpsc::UnboundedSender<Vec<f32>>>,
    stt_provider: Arc<dyn SttProvider>,
    tts_provider: Arc<dyn TtsProvider>,
    audio_output_tx: mpsc::UnboundedSender<(u64, Vec<f32>)>,
}

impl Dispatcher {
    pub fn new(
        stt_provider: Arc<dyn SttProvider>,
        tts_provider: Arc<dyn TtsProvider>,
        audio_output_tx: mpsc::UnboundedSender<(u64, Vec<f32>)>,
    ) -> Self {
        Self {
            workers: HashMap::new(),
            stt_provider,
            tts_provider,
            audio_output_tx,
        }
    }

    /// Dispatch audio to the worker for the given user.
    ///
    /// If no worker exists yet, a new one is spawned in a background task.
    pub fn dispatch(&mut self, user_id: u64, audio: Vec<f32>) {
        let tx = self.workers.entry(user_id).or_insert_with(|| {
            let (tx, rx) = mpsc::unbounded_channel();
            let stt = self.stt_provider.clone();
            let tts = self.tts_provider.clone();
            let output_tx = self.audio_output_tx.clone();
            tokio::spawn(async move {
                let mut worker = PipelineWorker::new(user_id, stt, tts, rx, output_tx);
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
        let (out_tx, out_rx) = mpsc::unbounded_channel();
        (Dispatcher::new(stt, tts, out_tx), out_rx)
    }

    #[test]
    fn dispatcher_new() {
        let (_d, _rx) = make_dispatcher();
    }

    #[tokio::test]
    async fn dispatcher_dispatch_spawns_worker() {
        let (mut d, mut out_rx) = make_dispatcher();

        // Dispatch audio for user 1.
        d.dispatch(1, vec![0.1f32; 400]);

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
}
