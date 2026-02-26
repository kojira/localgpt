//! Pipeline worker — per-user STT → Agent → TTS processing.
//!
//! Each worker owns an STT session, an agent bridge reference,
//! and a TTS provider reference.  It receives PCM chunks from
//! the dispatcher and produces response audio sent back to the
//! main thread for playback via songbird.
//!
//! Supports barge-in (interrupt) via `CancellationToken` and
//! idle timeout via configurable silence duration.
//!
//! PCM is buffered to 100ms (1600 samples at 16 kHz) before sending to STT,
//! so that backends with frame-based VAD receive more stable input.

/// STT input sample rate (must match receiver resample output).
const STT_SAMPLE_RATE: u32 = 16000;
/// Buffer size in ms before sending to STT; many backends work better with ~100ms frames.
const STT_BUFFER_MS: u32 = 100;
/// Samples per buffer at STT_SAMPLE_RATE. Use 0 in tests for no buffering.
pub const STT_BUFFER_SAMPLES: usize =
    (STT_SAMPLE_RATE as usize * STT_BUFFER_MS as usize) / 1000;
/// RMS amplitude threshold above which a PCM chunk is considered "audible" (voice-present).
///
/// Used to anchor `speech_start_at` to the first loud chunk rather than the STT
/// `SpeechStart` event, which may arrive late or be missing entirely.
/// Typical background noise RMS is < 0.005; voiced speech is usually > 0.01.
const AUDIBLE_RMS_THRESHOLD: f32 = 0.01;

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::Result;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info};
use super::profiling::{ProfileSession, VoiceProfilerWriter};

use super::agent_bridge::{AgentBridge, RoomMessage};
use super::provider::{SttEvent, SttProvider, TtsProvider};
use super::splitter::SentenceSplitter;
use super::transcript::TranscriptEntry;
use super::tts_pipeline::TtsPipeline;

/// Per-user voice processing pipeline.
pub struct PipelineWorker {
    user_id: u64,
    user_name: String,
    bot_name: String,
    stt_provider: Arc<dyn SttProvider>,
    tts_provider: Arc<dyn TtsProvider>,
    agent_bridge: Arc<dyn AgentBridge>,
    audio_rx: mpsc::UnboundedReceiver<Vec<f32>>,
    audio_output_tx: mpsc::UnboundedSender<(u64, crate::voice::PlaybackAudio)>,
    transcript_tx: Option<mpsc::UnboundedSender<TranscriptEntry>>,
    /// Shared flag indicating whether the bot is currently playing audio.
    is_playing: Arc<AtomicBool>,
    /// Token cancelled by the dispatcher on barge-in to abort LLM/TTS.
    cancel: CancellationToken,
    /// Idle timeout duration (0 = disabled).
    idle_timeout: Duration,
    /// Min samples to buffer before sending to STT (0 = send every chunk).
    stt_buffer_samples: usize,
    /// Accumulates PCM until stt_buffer_samples, then sends to STT.
    pcm_buffer: Vec<f32>,
    /// When Some, STT finals are sent here for room batching instead of per-user LLM+TTS.
    room_tx: Option<mpsc::UnboundedSender<RoomMessage>>,
    profiler_writer: VoiceProfilerWriter,
    /// Timestamp of the first audible PCM chunk (RMS > AUDIBLE_RMS_THRESHOLD) in the current
    /// utterance.  Used as the `speech_start_at` anchor for `ProfileSession` so that
    /// `elapsed_from_speech_start` reflects actual voice onset rather than the (potentially
    /// late or missing) STT `SpeechStart` event.  Reset to `None` each time a new
    /// `ProfileSession` is created.
    first_audible_at: Option<std::time::Instant>,
    /// Timestamp of the first non-empty PCM chunk received since the last STT Final.
    ///
    /// Used as the **ultimate fallback** for `speech_start_at` when:
    ///   - No `SpeechStart` event was received (→ `current_prof_session` is `None`), AND
    ///   - No audible (RMS > threshold) chunk was detected (→ `first_audible_at` is `None`).
    ///
    /// Some STT servers skip `SpeechStart` for very short or quiet utterances and jump
    /// straight to `Final`.  Without this anchor, the fallback `Instant::now()` is set
    /// just before `log_stt_final()`, making `elapsed_from_speech_start = 0ms`.
    pcm_session_start_at: Option<std::time::Instant>,
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
        audio_output_tx: mpsc::UnboundedSender<(u64, crate::voice::PlaybackAudio)>,
        transcript_tx: Option<mpsc::UnboundedSender<TranscriptEntry>>,
        is_playing: Arc<AtomicBool>,
        cancel: CancellationToken,
        idle_timeout_sec: u64,
        stt_buffer_samples: usize,
        room_tx: Option<mpsc::UnboundedSender<RoomMessage>>,
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
            stt_buffer_samples,
            pcm_buffer: Vec::with_capacity(if stt_buffer_samples == 0 {
                0
            } else {
                stt_buffer_samples * 2
            }),
            room_tx,
            // In tests use a null (no-op) writer so unit test runs never create or
            // pollute real profiling log files under ~/.localgpt/logs/.
            #[cfg(not(test))]
            profiler_writer: VoiceProfilerWriter::open(),
            #[cfg(test)]
            profiler_writer: VoiceProfilerWriter::null(),
            first_audible_at: None,
            pcm_session_start_at: None,
        }
    }

    /// Run the worker loop.
    ///
    /// Receives PCM chunks, forwards to STT, handles recognition events,
    /// calls the agent bridge for final transcriptions, synthesizes TTS,
    /// and emits transcript entries.
    ///
    /// The loop exits when:
    /// - The audio input channel closes.
    /// - The cancellation token is cancelled (shutdown).
    /// - The idle timeout fires (no speech for `idle_timeout` duration).
    pub async fn run(&mut self) -> Result<WorkerExitReason> {
        info!(user_id = self.user_id, "PipelineWorker started");

        let (mut stt_sender, mut stt_receiver) = self.stt_provider.connect().await?;
        let mut last_speech_at = Instant::now();
        // Profiling session: created on SpeechStart, consumed on process_text.
        let mut current_prof_session: Option<ProfileSession> = None;
        // Speech-start anchor for the current utterance, mirroring the one used for
        // current_prof_session.  Carried into RoomMessage so room_collector can build
        // an accurate ProfileSession without relying on the (late) batch arrival time.
        let mut current_speech_start: Option<std::time::Instant> = None;

        loop {
            let idle_deadline = last_speech_at + self.idle_timeout;

            tokio::select! {
                biased;

                // External cancellation (shutdown).
                _ = self.cancel.cancelled() => {
                    info!(user_id = self.user_id, "PipelineWorker cancelled");
                    if !self.pcm_buffer.is_empty() {
                        let chunk = std::mem::take(&mut self.pcm_buffer);
                        let _ = stt_sender.send_audio(&chunk).await;
                    }
                    let _ = stt_sender.send_end_of_stream().await;
                    stt_sender.close().await?;
                    return Ok(WorkerExitReason::Cancelled);
                }

                // Idle timeout.
                _ = tokio::time::sleep_until(idle_deadline) => {
                    info!(
                        user_id = self.user_id,
                        timeout_secs = self.idle_timeout.as_secs(),
                        "Idle timeout reached, stopping worker"
                    );
                    if !self.pcm_buffer.is_empty() {
                        let chunk = std::mem::take(&mut self.pcm_buffer);
                        let _ = stt_sender.send_audio(&chunk).await;
                    }
                    let _ = stt_sender.send_end_of_stream().await;
                    stt_sender.close().await?;
                    return Ok(WorkerExitReason::IdleTimeout);
                }

                // Audio input.
                pcm = self.audio_rx.recv() => {
                    let Some(pcm) = pcm else {
                        // Channel closed — flush remaining buffered PCM to STT.
                        if !self.pcm_buffer.is_empty() {
                            let chunk = std::mem::take(&mut self.pcm_buffer);
                            stt_sender.send_audio(&chunk).await?;
                        }
                        break;
                    };

                    // Ultimate fallback anchor: capture when the first non-empty PCM
                    // chunk of this utterance arrived.  Used when neither SpeechStart
                    // nor an audible (RMS > threshold) chunk is available at Final time.
                    if self.pcm_session_start_at.is_none() && !pcm.is_empty() {
                        self.pcm_session_start_at = Some(std::time::Instant::now());
                    }

                    // Track the first audible chunk for speech-start anchoring.
                    // This captures voice onset more accurately than the STT
                    // `SpeechStart` event, which can arrive late or be missing.
                    if self.first_audible_at.is_none() && !pcm.is_empty() {
                        let sum_sq: f32 = pcm.iter().map(|s| s * s).sum();
                        let rms = (sum_sq / pcm.len() as f32).sqrt();
                        if rms > AUDIBLE_RMS_THRESHOLD {
                            self.first_audible_at = Some(std::time::Instant::now());
                            debug!(
                                user_id = self.user_id,
                                rms,
                                "first audible chunk detected (speech-start anchor)"
                            );
                        }
                    }

                    if self.stt_buffer_samples == 0 {
                        stt_sender.send_audio(&pcm).await?;
                    } else {
                        self.pcm_buffer.extend_from_slice(&pcm);
                        while self.pcm_buffer.len() >= self.stt_buffer_samples {
                            let n = self.stt_buffer_samples;
                            let chunk: Vec<f32> =
                                self.pcm_buffer.drain(..n).collect();
                            stt_sender.send_audio(&chunk).await?;
                        }
                    }
                }

                // STT event reception.
                event_result = stt_receiver.recv_event() => {
                    match event_result? {
                        Some(SttEvent::SpeechStart { .. }) => {
                            last_speech_at = Instant::now();
                            debug!(user_id = self.user_id, "Speech start (timer reset)");
                            // Anchor to the first audible PCM chunk if available.
                            // Priority: first_audible_at (RMS>threshold, best) >
                            //           pcm_session_start_at (any PCM, fallback) >
                            //           Instant::now() (last resort).
                            let speech_start = self
                                .first_audible_at
                                .take()
                                .or_else(|| self.pcm_session_start_at.take())
                                .unwrap_or_else(std::time::Instant::now);
                            self.pcm_session_start_at = None;
                            info!(
                                user_id = self.user_id,
                                speech_start_age_ms = speech_start.elapsed().as_millis() as u64,
                                "STT SpeechStart: profiling anchor set"
                            );
                            current_speech_start = Some(speech_start);
                            current_prof_session = Some(ProfileSession::new_with_start(
                                self.profiler_writer.clone(),
                                speech_start,
                            ));

                            // Barge-in: if bot is playing, signal interrupt.
                            if self.is_playing.load(Ordering::Acquire) {
                                info!(
                                    user_id = self.user_id,
                                    "Barge-in detected, cancelling playback"
                                );
                                // The dispatcher watches is_playing and will
                                // handle the actual cancellation/token rotation.
                                // We notify via a special audio output message.
                                let _ = self.audio_output_tx.send((
                                    self.user_id,
                                    crate::voice::PlaybackAudio::Pcm(vec![]),
                                ));
                            }
                        }
                        Some(SttEvent::Final { ref text, .. }) => {
                            last_speech_at = Instant::now();
                            if text.trim().is_empty() {
                                continue;
                            }
                            info!(user_id = self.user_id, text, "STT final");
                            let has_room_tx = self.room_tx.is_some();
                            let _ = std::fs::OpenOptions::new().append(true).create(true)
                                .open("/Users/kojira/.openclaw/workspace/projects/localgpt/.cursor/debug.log")
                                .and_then(|mut f| {
                                    use std::io::Write;
                                    let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis();
                                    writeln!(f, r#"{{"id":"stt_final","timestamp":{},"location":"voice/worker.rs","message":"STT Final","data":{{"user_id":{},"has_room_tx":{},"text_len":{}}},"hypothesisId":"B,C"}}"#, ts, self.user_id, has_room_tx, text.len())
                                });

                            // Log user speech transcript.
                            self.send_transcript(TranscriptEntry::UserSpeech {
                                user_id: self.user_id,
                                user_name: self.user_name.clone(),
                                text: text.clone(),
                            });

                            // Room mode: send to collector for batched LLM; otherwise per-user LLM+TTS.
                            if let Some(ref room_tx) = self.room_tx {
                                // Log STT_FINAL for room mode (room_collector handles LLM/TTS timing).
                                if let Some(ref mut s) = current_prof_session {
                                    s.log_stt_final(text);
                                }
                                // Reset all PCM-timing anchors so the next utterance starts fresh.
                                // (session is not consumed in room mode — only borrowed above.)
                                self.first_audible_at = None;
                                self.pcm_session_start_at = None;
                                // Take the speech_start anchor and carry it in the RoomMessage so
                                // room_collector's ProfileSession uses the true voice-onset time.
                                // Fallback chain mirrors SpeechStart handler above.
                                let speech_start = current_speech_start
                                    .take()
                                    .unwrap_or_else(std::time::Instant::now);
                                let send_ok = room_tx.send((
                                    self.user_id,
                                    self.user_name.clone(),
                                    text.clone(),
                                    speech_start,
                                )).is_ok();
                                let _ = std::fs::OpenOptions::new().append(true).create(true)
                                    .open("/Users/kojira/.openclaw/workspace/projects/localgpt/.cursor/debug.log")
                                    .and_then(|mut f| {
                                        use std::io::Write;
                                        let ts = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_millis();
                                        writeln!(f, r#"{{"id":"room_send","timestamp":{},"location":"voice/worker.rs","message":"room_tx.send","data":{{"user_id":{},"send_ok":{}}},"hypothesisId":"C"}}"#, ts, self.user_id, send_ok)
                                    });
                                if !send_ok {
                                    debug!(user_id = self.user_id, "Room collector channel closed");
                                }
                                continue;
                            }

                            // Take profiling session (or create one if SpeechStart was missed).
                            // Anchor priority (best → worst):
                            //   1. current_prof_session  — already anchored at SpeechStart
                            //   2. first_audible_at      — first RMS-loud PCM chunk
                            //   3. pcm_session_start_at  — first any PCM chunk (quiet speech)
                            //   4. Instant::now()        — last resort; gives near-0ms elapsed
                            let mut prof_session = current_prof_session.take().unwrap_or_else(|| {
                                let start = self
                                    .first_audible_at
                                    .take()
                                    .or_else(|| self.pcm_session_start_at.take())
                                    .unwrap_or_else(std::time::Instant::now);
                                ProfileSession::new_with_start(self.profiler_writer.clone(), start)
                            });
                            // Reset all per-utterance anchors for the next turn.
                            self.first_audible_at = None;
                            self.pcm_session_start_at = None;
                            current_speech_start = None;
                            prof_session.log_stt_final(text);

                            // Process text through agent + TTS with cancellation support.
                            self.process_text(text, prof_session).await?;
                        }
                        Some(SttEvent::Partial { ref text }) => {
                            if let Some(ref mut s) = current_prof_session {
                                s.log_stt_partial(text);
                            }
                        }
                        Some(event) => {
                            debug!(user_id = self.user_id, ?event, "STT event");
                        }
                        None => {
                            // STT session closed normally.
                            break;
                        }
                    }
                }
            }
        }

        if !self.pcm_buffer.is_empty() {
            let chunk = std::mem::take(&mut self.pcm_buffer);
            let _ = stt_sender.send_audio(&chunk).await;
        }
        let _ = stt_sender.send_end_of_stream().await;
        stt_sender.close().await?;
        info!(user_id = self.user_id, "PipelineWorker stopped");
        Ok(WorkerExitReason::ChannelClosed)
    }

    /// Generate agent response via streaming LLM, split into sentences,
    /// synthesize TTS in parallel per segment, and play in sequence order.
    ///
    /// Flow:
    ///   `generate_stream()` → `SentenceSplitter` → `TtsPipeline` (parallel TTS)
    ///   → `BTreeMap` ordered buffer → `audio_output_tx` (in-order playback)
    ///
    /// This allows audio playback to begin as soon as the first sentence is
    /// synthesised, instead of waiting for the full LLM response.
    ///
    /// If the cancellation token fires at any point, partial playback is
    /// recorded in the transcript and the function returns early.
    async fn process_text(&self, text: &str, mut prof_session: ProfileSession) -> Result<()> {
        // Child token: barge-in can cancel this response without killing the worker.
        let response_cancel = self.cancel.child_token();

        // ── Phase 1: Start LLM streaming ─────────────────────────────────────
        prof_session.log_llm_start();
        let token_stream = tokio::select! {
            biased;
            _ = response_cancel.cancelled() => {
                debug!(user_id = self.user_id, "LLM stream cancelled before start");
                return Ok(());
            }
            result = self.agent_bridge.generate_stream(self.user_id, text) => result?
        };

        // ── Phase 2: Sentence splitting ──────────────────────────────────────
        // SentenceSplitter accumulates tokens and emits one segment per sentence.
        let sentence_stream = SentenceSplitter::default().split(token_stream);

        // ── Phase 3: Parallel TTS pipeline ───────────────────────────────────
        // TtsPipeline dispatches TTS for each segment concurrently (up to 3 at once).
        // Segments may complete out of order; we reorder below.
        let pipeline = TtsPipeline::with_defaults(Arc::clone(&self.tts_provider));
        let mut tts_rx = pipeline.process(sentence_stream);

        // Signal that the bot has started producing audio.
        self.is_playing.store(true, Ordering::Release);

        // ── Phase 4: Ordered playback loop ───────────────────────────────────
        // Buffer completed TTS segments by sequence index, then drain in order.
        let mut pending: BTreeMap<usize, super::tts_pipeline::TtsSegment> = BTreeMap::new();
        let mut next_play_idx: usize = 0;
        // Text accumulated in playback order (for transcript).
        let mut played_text = String::new();
        let mut first_segment_logged = false;
        let mut audio_play_logged = false;

        loop {
            tokio::select! {
                biased;

                // Barge-in / cancellation during playback.
                _ = response_cancel.cancelled() => {
                    self.is_playing.store(false, Ordering::Release);
                    info!(
                        user_id = self.user_id,
                        played_segments = next_play_idx,
                        "Streaming TTS cancelled mid-playback"
                    );
                    self.send_transcript(TranscriptEntry::BotResponseInterrupted {
                        bot_name: self.bot_name.clone(),
                        played_text: played_text.clone(),
                    });
                    return Ok(());
                }

                // Next completed TTS segment from the pipeline.
                item = tts_rx.recv() => {
                    let Some(result) = item else {
                        // All segments received — exit loop and flush any remainder.
                        break;
                    };
                    match result {
                        Ok(tts_seg) => {
                            // Log first-segment latency (key streaming metric).
                            if !first_segment_logged {
                                prof_session.log_llm_first_segment();
                                first_segment_logged = true;
                            }

                            // Log LLM segment text + TTS duration at INFO.
                            // Full text without truncation so SentenceSplitter boundaries are visible.
                            info!(
                                user_id = self.user_id,
                                segment = tts_seg.index,
                                text = %tts_seg.text,
                                tts_duration_ms = tts_seg.tts_duration_ms,
                                "TTS input[{}]: \"{}\"",
                                tts_seg.index,
                                tts_seg.text
                            );
                            prof_session.log_llm_segment(tts_seg.index, &tts_seg.text, tts_seg.synthesis_started_at);
                            prof_session.log_tts_segment_done(tts_seg.index, tts_seg.tts_duration_ms, tts_seg.synthesis_started_at);

                            pending.insert(tts_seg.index, tts_seg);

                            // Drain pending in strict sequence order.
                            while let Some(seg) = pending.remove(&next_play_idx) {
                                let playback = Self::tts_result_to_playback(
                                    &seg.tts_result,
                                    self.user_id,
                                    next_play_idx,
                                    &seg.text,
                                );

                                // Log AUDIO_PLAY once (first segment sent to queue).
                                if !audio_play_logged {
                                    prof_session.log_audio_play();
                                    audio_play_logged = true;
                                }

                                played_text.push_str(&seg.text);
                                if self.audio_output_tx.send((self.user_id, playback)).is_err() {
                                    error!(user_id = self.user_id, "Audio output channel closed");
                                }
                                next_play_idx += 1;
                            }
                        }
                        Err(e) => {
                            error!(
                                user_id = self.user_id,
                                error = %e,
                                "TTS pipeline segment error (skipping)"
                            );
                        }
                    }
                }
            }
        }

        // Flush any segments that arrived but were waiting on a gap
        // (e.g. segment 0 errored so 1, 2 were buffered behind it).
        // BTreeMap iterates in key order, so playback order is preserved.
        for (_idx, seg) in pending {
            let playback = Self::tts_result_to_playback(
                &seg.tts_result,
                self.user_id,
                _idx,
                &seg.text,
            );
            played_text.push_str(&seg.text);
            if self.audio_output_tx.send((self.user_id, playback)).is_err() {
                error!(user_id = self.user_id, "Audio output channel closed during flush");
            }
        }

        // Log full response transcript.
        self.send_transcript(TranscriptEntry::BotResponse {
            bot_name: self.bot_name.clone(),
            text: played_text,
        });

        self.is_playing.store(false, Ordering::Release);
        Ok(())
    }

    /// Convert a [`TtsResult`] to a [`PlaybackAudio`], logging details.
    fn tts_result_to_playback(
        tts_result: &crate::voice::provider::TtsResult,
        user_id: u64,
        segment_idx: usize,
        text: &str,
    ) -> crate::voice::PlaybackAudio {
        let preview = text.chars().take(40).collect::<String>();
        match tts_result {
            crate::voice::provider::TtsResult::Pcm { audio, .. } => {
                info!(
                    user_id,
                    segment = segment_idx,
                    samples = audio.len(),
                    preview = %preview,
                    "TTS segment ready, sending to playback"
                );
                crate::voice::PlaybackAudio::Pcm(audio.clone())
            }
            crate::voice::provider::TtsResult::EncodedOpus { data, .. } => {
                info!(
                    user_id,
                    segment = segment_idx,
                    bytes = data.len(),
                    preview = %preview,
                    "TTS Opus segment ready, sending to playback"
                );
                crate::voice::PlaybackAudio::Opus(data.clone())
            }
        }
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
        audio_output_tx: mpsc::UnboundedSender<(u64, crate::voice::PlaybackAudio)>,
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
            0,
            None,
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
            0,
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
