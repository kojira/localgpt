//! Room-level transcript collector.
//!
//! Buffers STT finals from multiple users within a time window (e.g. 0.2s),
//! then sends one batched message to the LLM so it can reply to the group
//! with awareness of who said what.

use std::time::Duration;

use anyhow::Result;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tracing::{debug, info};

use super::agent_bridge::{AgentBridge, RoomMessage};
use super::transcript::TranscriptEntry;
use super::PlaybackAudio;
use crate::voice::provider::{TtsProvider, TtsResult};

/// Runs the room collector loop: receive (user_id, user_name, text), buffer,
/// flush after `context_window` with no new message, then one LLM call + TTS + playback.
pub async fn run_room_collector(
    room_id: u64,
    mut rx: mpsc::UnboundedReceiver<RoomMessage>,
    agent_bridge: std::sync::Arc<dyn AgentBridge>,
    tts_provider: std::sync::Arc<dyn TtsProvider>,
    audio_output_tx: mpsc::UnboundedSender<(u64, PlaybackAudio)>,
    transcript_tx: Option<mpsc::UnboundedSender<TranscriptEntry>>,
    bot_name: String,
    context_window_ms: u64,
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
                    Some((user_id, user_name, text)) => {
                        if text.trim().is_empty() {
                            continue;
                        }
                        info!(room_id, user_id, %user_name, %text, "Room collector: buffered utterance");
                        send_transcript(&transcript_tx, TranscriptEntry::UserSpeech {
                            user_id,
                            user_name: user_name.clone(),
                            text: text.clone(),
                        });
                        buffer.push((user_id, user_name, text));
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
    agent_bridge: &std::sync::Arc<dyn AgentBridge>,
    tts_provider: &std::sync::Arc<dyn TtsProvider>,
    audio_output_tx: &mpsc::UnboundedSender<(u64, PlaybackAudio)>,
    transcript_tx: &Option<mpsc::UnboundedSender<TranscriptEntry>>,
    bot_name: &str,
) -> Result<()> {
    let batch = std::mem::take(buffer);
    if batch.is_empty() {
        return Ok(());
    }
    debug!(room_id, count = batch.len(), "Room collector: flushing batch to LLM");

    let response = agent_bridge.generate_room(room_id, &batch).await?;

    let playback = match tts_provider.synthesize(&response).await? {
        TtsResult::Pcm { audio, .. } => PlaybackAudio::Pcm(audio),
        TtsResult::EncodedOpus { data, .. } => PlaybackAudio::Opus(data),
    };

    send_transcript(transcript_tx, TranscriptEntry::BotResponse {
        bot_name: bot_name.to_string(),
        text: response.clone(),
    });

    if audio_output_tx.send((room_id, playback)).is_err() {
        tracing::warn!(room_id, "Room collector: audio output channel closed");
    }
    Ok(())
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
