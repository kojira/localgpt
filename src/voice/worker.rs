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
}

impl PipelineWorker {
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
        }
    }

    /// Run the worker loop.
    ///
    /// Receives PCM chunks, forwards to STT, drains recognition events,
    /// calls the agent bridge for final transcriptions, synthesizes TTS,
    /// and emits transcript entries.
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

                        // Log user speech transcript.
                        self.send_transcript(TranscriptEntry::UserSpeech {
                            user_id: self.user_id,
                            user_name: self.user_name.clone(),
                            text: text.clone(),
                        });

                        // Generate agent response.
                        let response = self.agent_bridge.generate(self.user_id, text).await?;

                        // Log bot response transcript.
                        self.send_transcript(TranscriptEntry::BotResponse {
                            bot_name: self.bot_name.clone(),
                            text: response.clone(),
                        });

                        // Synthesize TTS from agent response.
                        let tts_result = self.tts_provider.synthesize(&response).await?;
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

    /// Send a transcript entry if the transcript channel is configured.
    fn send_transcript(&self, entry: TranscriptEntry) {
        if let Some(ref tx) = self.transcript_tx {
            if tx.send(entry).is_err() {
                debug!(user_id = self.user_id, "Transcript channel closed");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voice::agent_bridge::MockAgentBridge;
    use crate::voice::provider::stt::mock::{MockSttConfig, MockSttProvider, MockUtterance};
    use crate::voice::provider::tts::mock::MockTtsProvider;
    use std::time::Duration;

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

        let mut worker = PipelineWorker::new(
            1,
            "User1".to_string(),
            "Bot".to_string(),
            stt,
            tts,
            bridge,
            in_rx,
            out_tx,
            None,
        );
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

    #[tokio::test]
    async fn pipeline_emits_transcript() {
        let stt = default_stt();
        let tts: Arc<dyn TtsProvider> = Arc::new(MockTtsProvider::silent());
        let bridge: Arc<dyn AgentBridge> = Arc::new(MockAgentBridge::new());

        let (in_tx, in_rx) = mpsc::unbounded_channel();
        let (out_tx, _out_rx) = mpsc::unbounded_channel();
        let (transcript_tx, mut transcript_rx) = mpsc::unbounded_channel();

        let mut worker = PipelineWorker::new(
            1,
            "Alice".to_string(),
            "TestBot".to_string(),
            stt,
            tts,
            bridge,
            in_rx,
            out_tx,
            Some(transcript_tx),
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
                user_name: "Alice".to_string(),
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
                bot_name: "TestBot".to_string(),
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

        let mut worker = PipelineWorker::new(
            1,
            "User".to_string(),
            "Bot".to_string(),
            stt,
            tts,
            bridge,
            in_rx,
            out_tx,
            None, // No transcript channel.
        );
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
}
