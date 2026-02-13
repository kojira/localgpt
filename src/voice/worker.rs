//! Pipeline worker — per-user STT → Agent → TTS processing.
//!
//! Each worker owns an STT session, an agent bridge reference,
//! and a TTS provider reference.  It receives PCM chunks from
//! the dispatcher and produces response audio sent back to the
//! main thread for playback via songbird.

use std::sync::Arc;

use anyhow::Result;
use tokio::sync::mpsc;
use tracing::{debug, error, info};

use super::provider::{SttEvent, SttProvider, TtsProvider};

/// Per-user voice processing pipeline.
pub struct PipelineWorker {
    user_id: u64,
    stt_provider: Arc<dyn SttProvider>,
    tts_provider: Arc<dyn TtsProvider>,
    audio_rx: mpsc::UnboundedReceiver<Vec<f32>>,
    audio_output_tx: mpsc::UnboundedSender<(u64, Vec<f32>)>,
}

impl PipelineWorker {
    pub fn new(
        user_id: u64,
        stt_provider: Arc<dyn SttProvider>,
        tts_provider: Arc<dyn TtsProvider>,
        audio_rx: mpsc::UnboundedReceiver<Vec<f32>>,
        audio_output_tx: mpsc::UnboundedSender<(u64, Vec<f32>)>,
    ) -> Self {
        Self {
            user_id,
            stt_provider,
            tts_provider,
            audio_rx,
            audio_output_tx,
        }
    }

    /// Run the worker loop.
    ///
    /// Receives PCM chunks, forwards to STT, drains recognition events,
    /// and synthesizes TTS audio for any final transcriptions.
    pub async fn run(&mut self) -> Result<()> {
        info!(user_id = self.user_id, "PipelineWorker started");

        let mut stt_session = self.stt_provider.connect().await?;

        while let Some(pcm) = self.audio_rx.recv().await {
            stt_session.send_audio(&pcm).await?;

            // Drain all available events after sending audio.
            loop {
                match stt_session.recv_event().await? {
                    Some(SttEvent::Final { ref text, .. }) => {
                        debug!(user_id = self.user_id, text, "STT final");
                        let tts_result = self.tts_provider.synthesize(text).await?;
                        if self
                            .audio_output_tx
                            .send((self.user_id, tts_result.audio))
                            .is_err()
                        {
                            error!(user_id = self.user_id, "Audio output channel closed");
                            break;
                        }
                    }
                    Some(event) => {
                        debug!(user_id = self.user_id, ?event, "STT event");
                    }
                    None => break,
                }
            }
        }

        stt_session.close().await?;
        info!(user_id = self.user_id, "PipelineWorker stopped");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voice::provider::stt::mock::{MockSttConfig, MockSttProvider, MockUtterance};
    use crate::voice::provider::tts::mock::MockTtsProvider;
    use std::time::Duration;

    #[test]
    fn worker_new() {
        let stt: Arc<dyn SttProvider> = Arc::new(MockSttProvider::new(MockSttConfig {
            utterances: vec![],
            close_after_all: true,
            latency_multiplier: 1.0,
        }));
        let tts: Arc<dyn TtsProvider> = Arc::new(MockTtsProvider::silent());
        let (_tx, rx) = mpsc::unbounded_channel();
        let (out_tx, _out_rx) = mpsc::unbounded_channel();
        let w = PipelineWorker::new(42, stt, tts, rx, out_tx);
        assert_eq!(w.user_id, 42);
    }

    #[tokio::test]
    async fn pipeline_stt_to_tts() {
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

        let (in_tx, in_rx) = mpsc::unbounded_channel();
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();

        let mut worker = PipelineWorker::new(1, stt, tts, in_rx, out_tx);
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
    }
}
