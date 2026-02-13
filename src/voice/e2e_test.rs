//! End-to-end integration tests for the voice pipeline.
//!
//! Tests the complete flow: Audio → STT → Agent → TTS → Audio Output
//! using mock providers. Covers all steps (2-8) of the voice implementation.

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;
    use std::time::Duration;

    use tokio::sync::mpsc;
    use tokio_util::sync::CancellationToken;

    use crate::voice::agent_bridge::{AgentBridge, MockAgentBridge};
    use crate::voice::dispatcher::Dispatcher;
    use crate::voice::provider::stt::mock::{MockSttConfig, MockSttProvider, MockUtterance};
    use crate::voice::provider::tts::mock::MockTtsProvider;
    use crate::voice::provider::{SttProvider, TtsProvider};
    use crate::voice::transcript::TranscriptEntry;
    use crate::voice::worker::{PipelineWorker, WorkerExitReason};

    // ── Test Helpers ─────────────────────────────────────────────

    /// Generate a sine wave PCM buffer (16 kHz mono f32).
    fn generate_sine_pcm(frequency_hz: f32, duration_ms: u64, amplitude: f32) -> Vec<f32> {
        let sample_rate = 16000.0f32;
        let num_samples = (sample_rate * duration_ms as f32 / 1000.0) as usize;
        (0..num_samples)
            .map(|i| {
                let t = i as f32 / sample_rate;
                amplitude * (2.0 * std::f32::consts::PI * frequency_hz * t).sin()
            })
            .collect()
    }

    /// Generate enough audio to trigger mock STT (> 320 samples).
    fn trigger_audio() -> Vec<f32> {
        generate_sine_pcm(440.0, 30, 0.5) // ~480 samples at 16kHz
    }

    /// Create a simple mock utterance with zero latency.
    fn utterance(text: &str) -> MockUtterance {
        MockUtterance {
            text: text.to_string(),
            language: "ja".to_string(),
            delay_before_start: Duration::ZERO,
            partial_interval: Duration::ZERO,
            delay_to_final: Duration::ZERO,
            confidence: 0.95,
        }
    }

    /// A slow agent bridge that simulates LLM processing time.
    struct SlowAgentBridge {
        delay: Duration,
        response: String,
    }

    impl SlowAgentBridge {
        fn new(delay: Duration, response: &str) -> Self {
            Self {
                delay,
                response: response.to_string(),
            }
        }
    }

    #[async_trait::async_trait]
    impl AgentBridge for SlowAgentBridge {
        async fn generate(&self, _user_id: u64, _text: &str) -> anyhow::Result<String> {
            tokio::time::sleep(self.delay).await;
            Ok(self.response.clone())
        }
        async fn reset_context(&self, _user_id: u64) -> anyhow::Result<()> {
            Ok(())
        }
    }

    /// A configurable agent bridge that returns different responses per call.
    struct ScriptedAgentBridge {
        responses: std::sync::Mutex<Vec<String>>,
    }

    impl ScriptedAgentBridge {
        fn new(responses: Vec<String>) -> Self {
            Self {
                responses: std::sync::Mutex::new(responses),
            }
        }
    }

    #[async_trait::async_trait]
    impl AgentBridge for ScriptedAgentBridge {
        async fn generate(&self, _user_id: u64, _text: &str) -> anyhow::Result<String> {
            let mut responses = self.responses.lock().unwrap();
            if responses.is_empty() {
                Ok("default response".to_string())
            } else {
                Ok(responses.remove(0))
            }
        }
        async fn reset_context(&self, _user_id: u64) -> anyhow::Result<()> {
            Ok(())
        }
    }

    /// Build a full pipeline worker with mocks.
    fn build_worker(
        stt: Arc<dyn SttProvider>,
        tts: Arc<dyn TtsProvider>,
        bridge: Arc<dyn AgentBridge>,
    ) -> (
        PipelineWorker,
        mpsc::UnboundedSender<Vec<f32>>,
        mpsc::UnboundedReceiver<(u64, Vec<f32>)>,
        Arc<AtomicBool>,
        CancellationToken,
    ) {
        let (in_tx, in_rx) = mpsc::unbounded_channel();
        let (out_tx, out_rx) = mpsc::unbounded_channel();
        let is_playing = Arc::new(AtomicBool::new(false));
        let cancel = CancellationToken::new();
        let worker = PipelineWorker::new(
            1,
            "TestUser".to_string(),
            "TestBot".to_string(),
            stt,
            tts,
            bridge,
            in_rx,
            out_tx,
            None,
            is_playing.clone(),
            cancel.clone(),
            300,
        );
        (worker, in_tx, out_rx, is_playing, cancel)
    }

    /// Build a worker with transcript channel.
    fn build_worker_with_transcript(
        stt: Arc<dyn SttProvider>,
        tts: Arc<dyn TtsProvider>,
        bridge: Arc<dyn AgentBridge>,
    ) -> (
        PipelineWorker,
        mpsc::UnboundedSender<Vec<f32>>,
        mpsc::UnboundedReceiver<(u64, Vec<f32>)>,
        mpsc::UnboundedReceiver<TranscriptEntry>,
        Arc<AtomicBool>,
        CancellationToken,
    ) {
        let (in_tx, in_rx) = mpsc::unbounded_channel();
        let (out_tx, out_rx) = mpsc::unbounded_channel();
        let (transcript_tx, transcript_rx) = mpsc::unbounded_channel();
        let is_playing = Arc::new(AtomicBool::new(false));
        let cancel = CancellationToken::new();
        let worker = PipelineWorker::new(
            1,
            "TestUser".to_string(),
            "TestBot".to_string(),
            stt,
            tts,
            bridge,
            in_rx,
            out_tx,
            Some(transcript_tx),
            is_playing.clone(),
            cancel.clone(),
            300,
        );
        (worker, in_tx, out_rx, transcript_rx, is_playing, cancel)
    }

    // ── Step 2 E2E: VC Join/Leave (Worker Lifecycle) ─────────────

    /// Test: Worker starts and stops cleanly when input channel closes.
    #[tokio::test]
    async fn e2e_step2_worker_lifecycle_channel_close() {
        let stt: Arc<dyn SttProvider> = Arc::new(MockSttProvider::new(MockSttConfig {
            utterances: vec![],
            close_after_all: false,
            latency_multiplier: 1.0,
        }));
        let tts: Arc<dyn TtsProvider> = Arc::new(MockTtsProvider::silent());
        let bridge: Arc<dyn AgentBridge> = Arc::new(MockAgentBridge::new());

        let (mut worker, in_tx, _out_rx, _is_playing, _cancel) =
            build_worker(stt, tts, bridge);

        let handle = tokio::spawn(async move { worker.run().await });

        // Simulate VC leave by closing the audio channel.
        drop(in_tx);

        let result = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.unwrap(), WorkerExitReason::ChannelClosed);
    }

    /// Test: Worker stops via cancellation token (graceful shutdown).
    #[tokio::test]
    async fn e2e_step2_worker_lifecycle_cancellation() {
        let stt: Arc<dyn SttProvider> = Arc::new(MockSttProvider::new(MockSttConfig {
            utterances: vec![],
            close_after_all: false,
            latency_multiplier: 1.0,
        }));
        let tts: Arc<dyn TtsProvider> = Arc::new(MockTtsProvider::silent());
        let bridge: Arc<dyn AgentBridge> = Arc::new(MockAgentBridge::new());

        let (mut worker, _in_tx, _out_rx, _is_playing, cancel) =
            build_worker(stt, tts, bridge);

        let handle = tokio::spawn(async move { worker.run().await });

        tokio::time::sleep(Duration::from_millis(50)).await;
        cancel.cancel();

        let result = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.unwrap(), WorkerExitReason::Cancelled);
    }

    /// Test: Worker exits on idle timeout.
    #[tokio::test]
    async fn e2e_step2_worker_lifecycle_idle_timeout() {
        let _stt: Arc<dyn SttProvider> = Arc::new(MockSttProvider::new(MockSttConfig {
            utterances: vec![],
            close_after_all: false,
            latency_multiplier: 1.0,
        }));
        let _tts: Arc<dyn TtsProvider> = Arc::new(MockTtsProvider::silent());
        let _bridge: Arc<dyn AgentBridge> = Arc::new(MockAgentBridge::new());

        let (in_tx, in_rx) = mpsc::unbounded_channel();
        let (out_tx, _out_rx) = mpsc::unbounded_channel();
        let is_playing = Arc::new(AtomicBool::new(false));
        let cancel = CancellationToken::new();
        // 1 second idle timeout
        let mut worker = PipelineWorker::new(
            1, "User".to_string(), "Bot".to_string(),
            Arc::new(MockSttProvider::new(MockSttConfig {
                utterances: vec![],
                close_after_all: false,
                latency_multiplier: 1.0,
            })),
            Arc::new(MockTtsProvider::silent()),
            Arc::new(MockAgentBridge::new()),
            in_rx, out_tx, None, is_playing, cancel, 1,
        );
        let _ = in_tx; // keep channel open

        let handle = tokio::spawn(async move { worker.run().await });
        let result = tokio::time::timeout(Duration::from_secs(5), handle)
            .await
            .unwrap()
            .unwrap();
        assert_eq!(result.unwrap(), WorkerExitReason::IdleTimeout);
    }

    // ── Step 3 E2E: Mock Pipeline Response ───────────────────────

    /// Test: Full pipeline produces audio output from mock STT→Agent→TTS.
    #[tokio::test]
    async fn e2e_step3_mock_pipeline_produces_audio() {
        let stt: Arc<dyn SttProvider> = Arc::new(MockSttProvider::new(MockSttConfig {
            utterances: vec![utterance("hello")],
            close_after_all: true,
            latency_multiplier: 1.0,
        }));
        let tts: Arc<dyn TtsProvider> = Arc::new(MockTtsProvider::sine(440.0));
        let bridge: Arc<dyn AgentBridge> = Arc::new(MockAgentBridge::new());

        let (mut worker, in_tx, mut out_rx, _is_playing, _cancel) =
            build_worker(stt, tts, bridge);

        let handle = tokio::spawn(async move { worker.run().await });

        in_tx.send(trigger_audio()).unwrap();

        // Should receive non-empty audio output (sine wave from TTS).
        let (uid, audio) = tokio::time::timeout(Duration::from_secs(5), out_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert_eq!(uid, 1);
        assert!(!audio.is_empty());

        // Verify it's actually a sine wave (has positive and negative values).
        let has_positive = audio.iter().any(|&s| s > 0.1);
        let has_negative = audio.iter().any(|&s| s < -0.1);
        assert!(has_positive && has_negative, "Expected sine wave audio");

        drop(in_tx);
        handle.await.unwrap().unwrap();
    }

    // ── Step 4 E2E: Speech → Text Response ───────────────────────

    /// Test: User speech is transcribed and agent generates text response.
    #[tokio::test]
    async fn e2e_step4_speech_to_text_response() {
        let stt: Arc<dyn SttProvider> = Arc::new(MockSttProvider::new(MockSttConfig {
            utterances: vec![utterance("how is the weather today?")],
            close_after_all: true,
            latency_multiplier: 1.0,
        }));
        let tts: Arc<dyn TtsProvider> = Arc::new(MockTtsProvider::silent());
        let bridge: Arc<dyn AgentBridge> = Arc::new(MockAgentBridge::new());

        let (mut worker, in_tx, _out_rx, mut transcript_rx, _is_playing, _cancel) =
            build_worker_with_transcript(stt, tts, bridge);

        let handle = tokio::spawn(async move { worker.run().await });

        in_tx.send(trigger_audio()).unwrap();

        // Verify user speech transcript.
        let entry = tokio::time::timeout(Duration::from_secs(5), transcript_rx.recv())
            .await.unwrap().unwrap();
        match entry {
            TranscriptEntry::UserSpeech { user_id, user_name, text } => {
                assert_eq!(user_id, 1);
                assert_eq!(user_name, "TestUser");
                assert_eq!(text, "how is the weather today?");
            }
            other => panic!("Expected UserSpeech, got {:?}", other),
        }

        // Verify bot response transcript (MockAgentBridge echoes).
        let entry = tokio::time::timeout(Duration::from_secs(5), transcript_rx.recv())
            .await.unwrap().unwrap();
        match entry {
            TranscriptEntry::BotResponse { bot_name, text } => {
                assert_eq!(bot_name, "TestBot");
                assert_eq!(text, "echo: how is the weather today?");
            }
            other => panic!("Expected BotResponse, got {:?}", other),
        }

        drop(in_tx);
        handle.await.unwrap().unwrap();
    }

    // ── Step 5 E2E: Speech → Audio Response ──────────────────────

    /// Test: Complete pipeline from speech input to audio output.
    #[tokio::test]
    async fn e2e_step5_speech_to_audio_response() {
        let stt: Arc<dyn SttProvider> = Arc::new(MockSttProvider::new(MockSttConfig {
            utterances: vec![utterance("hello")],
            close_after_all: true,
            latency_multiplier: 1.0,
        }));
        let tts: Arc<dyn TtsProvider> = Arc::new(MockTtsProvider::sine(440.0));
        let bridge: Arc<dyn AgentBridge> = Arc::new(MockAgentBridge::new());

        let (mut worker, in_tx, mut out_rx, mut transcript_rx, _is_playing, _cancel) =
            build_worker_with_transcript(stt, tts, bridge);

        let handle = tokio::spawn(async move { worker.run().await });

        in_tx.send(trigger_audio()).unwrap();

        // Verify transcript chain: UserSpeech → BotResponse.
        let entry = tokio::time::timeout(Duration::from_secs(5), transcript_rx.recv())
            .await.unwrap().unwrap();
        assert!(matches!(entry, TranscriptEntry::UserSpeech { .. }));

        let entry = tokio::time::timeout(Duration::from_secs(5), transcript_rx.recv())
            .await.unwrap().unwrap();
        assert!(matches!(entry, TranscriptEntry::BotResponse { .. }));

        // Verify audio output exists and is non-trivial.
        let (uid, audio) = tokio::time::timeout(Duration::from_secs(5), out_rx.recv())
            .await.unwrap().unwrap();
        assert_eq!(uid, 1);
        assert!(audio.len() > 1000, "Expected substantial audio output, got {} samples", audio.len());

        drop(in_tx);
        handle.await.unwrap().unwrap();
    }

    // ── Step 6 E2E: Long Response Sequential Playback ────────────

    /// Test: Long response generates proportionally longer audio.
    #[tokio::test]
    async fn e2e_step6_long_response_produces_longer_audio() {
        let stt: Arc<dyn SttProvider> = Arc::new(MockSttProvider::new(MockSttConfig {
            utterances: vec![utterance("short")],
            close_after_all: true,
            latency_multiplier: 1.0,
        }));
        let tts: Arc<dyn TtsProvider> = Arc::new(MockTtsProvider::silent());
        // Agent returns a long response.
        let long_text = "This is a very long response. ".repeat(10);
        let bridge: Arc<dyn AgentBridge> = Arc::new(ScriptedAgentBridge::new(vec![long_text.clone()]));

        let (mut worker, in_tx, mut out_rx, _is_playing, _cancel) =
            build_worker(stt, tts, bridge);

        let handle = tokio::spawn(async move { worker.run().await });

        in_tx.send(trigger_audio()).unwrap();

        let (uid, audio) = tokio::time::timeout(Duration::from_secs(5), out_rx.recv())
            .await.unwrap().unwrap();
        assert_eq!(uid, 1);
        // MockTtsProvider: 150ms per char. Long text = many chars = many samples.
        // "echo: 短い" (short) would be ~10 chars * 150ms = 1500ms = 36000 samples
        // Long text is much longer.
        assert!(audio.len() > 100_000, "Long response should produce lots of audio, got {} samples", audio.len());

        drop(in_tx);
        handle.await.unwrap().unwrap();
    }

    /// Test: Multiple sequential utterances each produce separate audio outputs.
    #[tokio::test]
    async fn e2e_step6_sequential_utterances() {
        let stt: Arc<dyn SttProvider> = Arc::new(MockSttProvider::new(MockSttConfig {
            utterances: vec![
                utterance("first one"),
                utterance("second one"),
            ],
            close_after_all: true,
            latency_multiplier: 1.0,
        }));
        let tts: Arc<dyn TtsProvider> = Arc::new(MockTtsProvider::silent());
        let bridge: Arc<dyn AgentBridge> = Arc::new(MockAgentBridge::new());

        let (mut worker, in_tx, mut out_rx, _is_playing, _cancel) =
            build_worker(stt, tts, bridge);

        let handle = tokio::spawn(async move { worker.run().await });

        // First utterance.
        in_tx.send(trigger_audio()).unwrap();
        let (uid, audio1) = tokio::time::timeout(Duration::from_secs(5), out_rx.recv())
            .await.unwrap().unwrap();
        assert_eq!(uid, 1);
        assert!(!audio1.is_empty());

        // Second utterance (need fresh audio to trigger).
        in_tx.send(trigger_audio()).unwrap();
        let (uid, audio2) = tokio::time::timeout(Duration::from_secs(5), out_rx.recv())
            .await.unwrap().unwrap();
        assert_eq!(uid, 1);
        assert!(!audio2.is_empty());

        drop(in_tx);
        handle.await.unwrap().unwrap();
    }

    // ── Step 7 E2E: Barge-in (Interrupt) ─────────────────────────

    /// Test: Barge-in during playback sends empty audio signal.
    #[tokio::test]
    async fn e2e_step7_barge_in_sends_interrupt_signal() {
        let stt: Arc<dyn SttProvider> = Arc::new(MockSttProvider::new(MockSttConfig {
            utterances: vec![utterance("stop now")],
            close_after_all: true,
            latency_multiplier: 1.0,
        }));
        let tts: Arc<dyn TtsProvider> = Arc::new(MockTtsProvider::silent());
        let bridge: Arc<dyn AgentBridge> = Arc::new(MockAgentBridge::new());

        let (mut worker, in_tx, mut out_rx, is_playing, _cancel) =
            build_worker(stt, tts, bridge);

        // Simulate bot is currently playing audio.
        is_playing.store(true, Ordering::Release);

        let handle = tokio::spawn(async move { worker.run().await });

        in_tx.send(trigger_audio()).unwrap();

        // First output should be the barge-in signal (empty audio).
        let (uid, audio) = tokio::time::timeout(Duration::from_secs(5), out_rx.recv())
            .await.unwrap().unwrap();
        assert_eq!(uid, 1);
        assert!(audio.is_empty(), "Barge-in signal should be empty audio");

        // Then the actual TTS response follows.
        let (uid, audio) = tokio::time::timeout(Duration::from_secs(5), out_rx.recv())
            .await.unwrap().unwrap();
        assert_eq!(uid, 1);
        assert!(!audio.is_empty());

        drop(in_tx);
        handle.await.unwrap().unwrap();
    }

    /// Test: Dispatcher handle_interrupt cancels playback.
    #[tokio::test]
    async fn e2e_step7_dispatcher_interrupt_cancels_playback() {
        let stt: Arc<dyn SttProvider> = Arc::new(MockSttProvider::new(MockSttConfig {
            utterances: vec![utterance("hello")],
            close_after_all: true,
            latency_multiplier: 1.0,
        }));
        let tts: Arc<dyn TtsProvider> = Arc::new(MockTtsProvider::silent());
        let bridge: Arc<dyn AgentBridge> = Arc::new(MockAgentBridge::new());
        let (out_tx, _out_rx) = mpsc::unbounded_channel();

        let mut dispatcher = Dispatcher::new(
            stt, tts, bridge, out_tx, None,
            "Bot".to_string(), 300, true,
        );

        // Spawn worker.
        dispatcher.dispatch(1, "User1".to_string(), trigger_audio());
        tokio::time::sleep(Duration::from_millis(50)).await;

        // User is not playing initially — interrupt is no-op.
        dispatcher.handle_interrupt(1);
        assert!(!dispatcher.is_user_playing(1));
    }

    // ── Step 8 E2E: Multi-user Simultaneous Speech ───────────────

    /// Test: Two users speaking simultaneously get separate responses.
    #[tokio::test]
    async fn e2e_step8_multi_user_simultaneous_speech() {
        let stt: Arc<dyn SttProvider> = Arc::new(MockSttProvider::new(MockSttConfig {
            utterances: vec![utterance("hello")],
            close_after_all: true,
            latency_multiplier: 1.0,
        }));
        let tts: Arc<dyn TtsProvider> = Arc::new(MockTtsProvider::silent());
        let bridge: Arc<dyn AgentBridge> = Arc::new(MockAgentBridge::new());
        let (out_tx, mut out_rx) = mpsc::unbounded_channel();

        let mut dispatcher = Dispatcher::new(
            stt, tts, bridge, out_tx, None,
            "Bot".to_string(), 300, true,
        );

        // Two users speak at the same time.
        dispatcher.dispatch(1, "User1".to_string(), trigger_audio());
        dispatcher.dispatch(2, "User2".to_string(), trigger_audio());

        // Collect responses (should get one from each user).
        let mut user_ids = std::collections::HashSet::new();
        for _ in 0..2 {
            let (uid, audio) = tokio::time::timeout(Duration::from_secs(5), out_rx.recv())
                .await.unwrap().unwrap();
            assert!(!audio.is_empty());
            user_ids.insert(uid);
        }

        assert!(user_ids.contains(&1), "User 1 should get a response");
        assert!(user_ids.contains(&2), "User 2 should get a response");
    }

    /// Test: Multi-user with transcript tracking.
    #[tokio::test]
    async fn e2e_step8_multi_user_with_transcript() {
        let stt: Arc<dyn SttProvider> = Arc::new(MockSttProvider::new(MockSttConfig {
            utterances: vec![utterance("hi")],
            close_after_all: true,
            latency_multiplier: 1.0,
        }));
        let tts: Arc<dyn TtsProvider> = Arc::new(MockTtsProvider::silent());
        let bridge: Arc<dyn AgentBridge> = Arc::new(MockAgentBridge::new());
        let (out_tx, _out_rx) = mpsc::unbounded_channel();
        let (transcript_tx, mut transcript_rx) = mpsc::unbounded_channel();

        let mut dispatcher = Dispatcher::new(
            stt, tts, bridge, out_tx, Some(transcript_tx),
            "Bot".to_string(), 300, true,
        );

        dispatcher.dispatch(1, "Alice".to_string(), trigger_audio());
        dispatcher.dispatch(2, "Bob".to_string(), trigger_audio());

        // Collect all transcript entries (4 total: 2 UserSpeech + 2 BotResponse).
        let mut entries = Vec::new();
        for _ in 0..4 {
            let entry = tokio::time::timeout(Duration::from_secs(5), transcript_rx.recv())
                .await.unwrap().unwrap();
            entries.push(entry);
        }

        let user_speeches: Vec<_> = entries.iter().filter(|e| matches!(e, TranscriptEntry::UserSpeech { .. })).collect();
        let bot_responses: Vec<_> = entries.iter().filter(|e| matches!(e, TranscriptEntry::BotResponse { .. })).collect();

        assert_eq!(user_speeches.len(), 2, "Should have 2 user speech entries");
        assert_eq!(bot_responses.len(), 2, "Should have 2 bot response entries");
    }

    // ── Cross-step Integration ───────────────────────────────────

    /// Test: Full pipeline with sine wave audio generation helpers.
    #[tokio::test]
    async fn e2e_audio_helper_generates_valid_pcm() {
        let audio = generate_sine_pcm(440.0, 1000, 0.8);
        assert_eq!(audio.len(), 16000); // 1 second at 16kHz

        let max_amp = audio.iter().map(|s| s.abs()).fold(0.0f32, f32::max);
        assert!(max_amp > 0.7 && max_amp <= 0.8, "Amplitude should be ~0.8, got {}", max_amp);

        // Verify it's a proper sine wave (crosses zero multiple times).
        let zero_crossings = audio.windows(2)
            .filter(|w| (w[0] >= 0.0) != (w[1] >= 0.0))
            .count();
        // 440 Hz sine wave should cross zero ~880 times per second.
        assert!(zero_crossings > 800 && zero_crossings < 960,
            "Expected ~880 zero crossings, got {}", zero_crossings);
    }

    /// Test: Pipeline handles empty STT result gracefully.
    #[tokio::test]
    async fn e2e_empty_stt_no_response() {
        let stt: Arc<dyn SttProvider> = Arc::new(MockSttProvider::new(MockSttConfig {
            utterances: vec![MockUtterance {
                text: "   ".to_string(), // whitespace-only
                language: "ja".to_string(),
                delay_before_start: Duration::ZERO,
                partial_interval: Duration::ZERO,
                delay_to_final: Duration::ZERO,
                confidence: 0.1,
            }],
            close_after_all: true,
            latency_multiplier: 1.0,
        }));
        let tts: Arc<dyn TtsProvider> = Arc::new(MockTtsProvider::silent());
        let bridge: Arc<dyn AgentBridge> = Arc::new(MockAgentBridge::new());

        let (mut worker, in_tx, mut out_rx, _is_playing, _cancel) =
            build_worker(stt, tts, bridge);

        let handle = tokio::spawn(async move { worker.run().await });

        in_tx.send(trigger_audio()).unwrap();

        // Empty STT text should not produce audio output.
        let result = tokio::time::timeout(Duration::from_millis(500), out_rx.recv()).await;
        assert!(result.is_err(), "Empty STT text should not trigger TTS");

        drop(in_tx);
        handle.await.unwrap().unwrap();
    }

    /// Test: Pipeline with slow agent still completes.
    #[tokio::test]
    async fn e2e_slow_agent_completes() {
        let stt: Arc<dyn SttProvider> = Arc::new(MockSttProvider::new(MockSttConfig {
            utterances: vec![utterance("test")],
            close_after_all: true,
            latency_multiplier: 1.0,
        }));
        let tts: Arc<dyn TtsProvider> = Arc::new(MockTtsProvider::silent());
        let bridge: Arc<dyn AgentBridge> = Arc::new(SlowAgentBridge::new(
            Duration::from_millis(100),
            "delayed response",
        ));

        let (mut worker, in_tx, mut out_rx, _is_playing, _cancel) =
            build_worker(stt, tts, bridge);

        let handle = tokio::spawn(async move { worker.run().await });

        in_tx.send(trigger_audio()).unwrap();

        let (uid, audio) = tokio::time::timeout(Duration::from_secs(5), out_rx.recv())
            .await.unwrap().unwrap();
        assert_eq!(uid, 1);
        assert!(!audio.is_empty());

        drop(in_tx);
        handle.await.unwrap().unwrap();
    }
}
