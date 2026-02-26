//! Room-level transcript collector.
//!
//! Buffers STT finals from multiple users within a time window (e.g. 0.2s),
//! then sends one batched message to the LLM so it can reply to the group
//! with awareness of who said what.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;
use std::time::Instant as StdInstant;

use anyhow::Result;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tracing::{debug, error, info};

use super::agent_bridge::{AgentBridge, RoomMessage};
use super::profiling::{ProfileSession, VoiceProfilerWriter};
use super::splitter::SentenceSplitter;
use super::transcript::TranscriptEntry;
use super::tts_pipeline::{TtsPipeline, TtsSegment};
use super::PlaybackAudio;
use crate::voice::provider::{TtsProvider, TtsResult};

/// Runs the room collector loop: receive (user_id, user_name, text), buffer,
/// flush after `context_window` with no new message, then one LLM call + TTS + playback.
pub async fn run_room_collector(
    room_id: u64,
    mut rx: mpsc::UnboundedReceiver<RoomMessage>,
    agent_bridge: Arc<dyn AgentBridge>,
    tts_provider: Arc<dyn TtsProvider>,
    audio_output_tx: mpsc::UnboundedSender<(u64, PlaybackAudio)>,
    transcript_tx: Option<mpsc::UnboundedSender<TranscriptEntry>>,
    bot_name: String,
    context_window_ms: u64,
    profiler_writer: VoiceProfilerWriter,
) {
    let context_window = Duration::from_millis(context_window_ms);
    let mut buffer: Vec<RoomMessage> = Vec::new();
    let mut flush_deadline: Option<Instant> = None;

    loop {
        let sleep_duration = if buffer.is_empty() {
            Duration::from_secs(86400 * 365)
        } else {
            flush_deadline
                .map(|d| {
                    let now = Instant::now();
                    if d > now {
                        d.duration_since(now)
                    } else {
                        Duration::ZERO
                    }
                })
                .unwrap_or(Duration::from_secs(86400 * 365))
        };
        let sleep = tokio::time::sleep(sleep_duration);
        tokio::pin!(sleep);

        tokio::select! {
            msg = rx.recv() => {
                match msg {
                    Some((user_id, user_name, text, speech_start_at)) => {
                        if text.trim().is_empty() {
                            continue;
                        }
                        info!(room_id, user_id, %user_name, %text, "Room collector: buffered utterance");
                        send_transcript(&transcript_tx, TranscriptEntry::UserSpeech {
                            user_id,
                            user_name: user_name.clone(),
                            text: text.clone(),
                        });
                        buffer.push((user_id, user_name, text, speech_start_at));
                        flush_deadline = Some(Instant::now() + context_window);
                    }
                    None => {
                        if !buffer.is_empty() {
                            if let Err(e) = flush(
                                room_id,
                                &mut buffer,
                                &agent_bridge,
                                &tts_provider,
                                &audio_output_tx,
                                &transcript_tx,
                                &bot_name,
                                &profiler_writer,
                            ).await {
                                tracing::error!(room_id, "Room flush error: {}", e);
                            }
                        }
                        info!(room_id, "Room collector channel closed");
                        break;
                    }
                }
            }
            _ = sleep.as_mut() => {
                flush_deadline = None;
                if buffer.is_empty() {
                    continue;
                }
                if let Err(e) = flush(
                    room_id,
                    &mut buffer,
                    &agent_bridge,
                    &tts_provider,
                    &audio_output_tx,
                    &transcript_tx,
                    &bot_name,
                    &profiler_writer,
                ).await {
                    tracing::error!(room_id, "Room flush error: {}", e);
                }
            }
        }
    }
}

async fn flush(
    room_id: u64,
    buffer: &mut Vec<RoomMessage>,
    agent_bridge: &Arc<dyn AgentBridge>,
    tts_provider: &Arc<dyn TtsProvider>,
    audio_output_tx: &mpsc::UnboundedSender<(u64, PlaybackAudio)>,
    transcript_tx: &Option<mpsc::UnboundedSender<TranscriptEntry>>,
    bot_name: &str,
    profiler_writer: &VoiceProfilerWriter,
) -> Result<()> {
    let batch = std::mem::take(buffer);
    if batch.is_empty() {
        return Ok(());
    }
    debug!(room_id, count = batch.len(), "Room collector: flushing batch to LLM (streaming)");

    // Use speech_start carried in the first RoomMessage.
    // This was captured in PipelineWorker at the moment of the first audible PCM chunk
    // (or STT SpeechStart), giving an accurate latency anchor for the entire pipeline.
    let speech_start_at = batch
        .first()
        .map(|(_, _, _, start)| *start)
        .unwrap_or_else(StdInstant::now);

    let mut prof_session = ProfileSession::new_with_start(profiler_writer.clone(), speech_start_at);
    if let Some((_, _, text, _)) = batch.first() {
        prof_session.log_stt_final(text);
    }
    prof_session.log_llm_start();

    // ── Phase 1: Start streaming LLM response ────────────────────────────────
    let token_stream = agent_bridge.generate_room_stream(room_id, &batch).await?;

    // ── Phase 2: Sentence splitting ──────────────────────────────────────────
    let sentence_stream = SentenceSplitter::default().split(token_stream);

    // ── Phase 3: Parallel TTS pipeline ───────────────────────────────────────
    let pipeline = TtsPipeline::with_defaults(Arc::clone(tts_provider));
    let mut tts_rx = pipeline.process(sentence_stream);

    // ── Phase 4: Ordered playback loop ───────────────────────────────────────
    let mut pending: BTreeMap<usize, TtsSegment> = BTreeMap::new();
    let mut next_play_idx: usize = 0;
    let mut played_text = String::new();
    let mut first_segment_logged = false;
    let mut audio_play_logged = false;

    loop {
        match tts_rx.recv().await {
            Some(Ok(tts_seg)) => {
                // Log first-segment latency (key streaming metric).
                if !first_segment_logged {
                    prof_session.log_llm_first_segment();
                    first_segment_logged = true;
                }

                // Log: which LLM segment this is + TTS timing.
                // LLM segment text is logged at INFO so split boundaries are visible.
                info!(
                    room_id,
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
                    let playback = seg_to_playback(&seg);

                    // Log AUDIO_PLAY once (first segment sent to queue).
                    if !audio_play_logged {
                        prof_session.log_audio_play();
                        audio_play_logged = true;
                    }

                    info!(
                        room_id,
                        segment = seg.index,
                        text = %seg.text,
                        "AUDIO_PLAY segment[{}]: \"{}\"",
                        seg.index,
                        seg.text
                    );
                    played_text.push_str(&seg.text);
                    if audio_output_tx.send((room_id, playback)).is_err() {
                        tracing::warn!(room_id, "Room collector: audio output channel closed");
                    }
                    next_play_idx += 1;
                }
            }
            Some(Err(e)) => {
                error!(room_id, error = %e, "Room TTS pipeline segment error (skipping)");
            }
            None => {
                // All TTS segments received.
                break;
            }
        }
    }

    // Flush any out-of-order segments buffered in pending.
    for (_, seg) in pending {
        let playback = seg_to_playback(&seg);
        info!(
            room_id,
            segment = seg.index,
            text = %seg.text,
            "AUDIO_PLAY segment[{}] (flush): \"{}\"",
            seg.index,
            seg.text
        );
        played_text.push_str(&seg.text);
        if audio_output_tx.send((room_id, playback)).is_err() {
            tracing::warn!(room_id, "Room collector: audio output channel closed during flush");
        }
    }

    send_transcript(
        transcript_tx,
        TranscriptEntry::BotResponse {
            bot_name: bot_name.to_string(),
            text: played_text,
        },
    );

    Ok(())
}

/// Convert a completed [`TtsSegment`] to a [`PlaybackAudio`] value.
fn seg_to_playback(seg: &TtsSegment) -> PlaybackAudio {
    match &seg.tts_result {
        TtsResult::Pcm { audio, .. } => PlaybackAudio::Pcm(audio.clone()),
        TtsResult::EncodedOpus { data, .. } => PlaybackAudio::Opus(data.clone()),
    }
}

fn send_transcript(
    tx: &Option<mpsc::UnboundedSender<TranscriptEntry>>,
    entry: TranscriptEntry,
) {
    if let Some(t) = tx {
        if t.send(entry).is_err() {
            debug!("Transcript channel closed");
        }
    }
}
