//! Voice receive handler.
//!
//! Receives decoded PCM audio from songbird's VoiceTick events and
//! forwards [`AudioChunk`]s to the dispatcher via an mpsc channel.
//!
//! Songbird is configured with `DecodeMode::Decode`, `Channels::Stereo`,
//! and `SampleRate::Hz48000` (matching Discord's native Opus format).
//! This handler downmixes stereo → mono and resamples 48 kHz → 16 kHz
//! before forwarding to the STT pipeline.

use songbird::events::{Event, EventContext, EventHandler as VoiceEventHandler};
use tokio::sync::mpsc;
use tracing::{debug, warn};

/// A chunk of decoded audio from a single speaker.
#[derive(Debug, Clone)]
pub struct AudioChunk {
    /// Synchronization source — identifies the speaker.
    pub ssrc: u32,
    /// 16 kHz mono f32 PCM samples.
    pub pcm: Vec<f32>,
}

/// songbird `EventHandler` implementation for receiving voice packets.
///
/// Registered on the Call/Driver as a `CoreEvent::VoiceTick` handler.
/// Each tick (every 20 ms) delivers decoded audio for all speaking users.
pub struct VoiceReceiveHandler {
    /// Channel to send audio chunks to the dispatcher
    audio_tx: mpsc::UnboundedSender<AudioChunk>,
}

impl VoiceReceiveHandler {
    /// Create a new receive handler.
    pub fn new(audio_tx: mpsc::UnboundedSender<AudioChunk>) -> Self {
        Self { audio_tx }
    }
}

#[async_trait::async_trait]
impl VoiceEventHandler for VoiceReceiveHandler {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        if let EventContext::VoiceTick(tick) = ctx {
            for (&ssrc, data) in &tick.speaking {
                if let Some(pcm_i16) = &data.decoded_voice {
                    if pcm_i16.is_empty() {
                        continue;
                    }

                    // Convert i16 → f32 (range −1.0 … 1.0)
                    let pcm_f32: Vec<f32> =
                        pcm_i16.iter().map(|&s| s as f32 / 32768.0).collect();

                    // Downmix stereo → mono (average L and R channels)
                    let mono = stereo_to_mono(&pcm_f32);

                    // Resample 48 kHz → 16 kHz
                    let resampled = match super::audio::resample_mono(&mono, 48000, 16000) {
                        Ok(v) => v,
                        Err(e) => {
                            warn!(ssrc, "Resample failed: {}", e);
                            continue;
                        }
                    };

                    let rms = calculate_rms(&resampled);

                    debug!(
                        ssrc,
                        raw_samples = pcm_f32.len(),
                        out_samples = resampled.len(),
                        rms = format!("{:.4}", rms),
                        "Received audio (48kHz stereo → 16kHz mono)"
                    );

                    let chunk = AudioChunk {
                        ssrc,
                        pcm: resampled,
                    };
                    if let Err(e) = self.audio_tx.send(chunk) {
                        warn!("Failed to send audio chunk: {}", e);
                    }
                }
            }
        }
        None
    }
}

/// Downmix interleaved stereo to mono by averaging L and R channels.
fn stereo_to_mono(interleaved: &[f32]) -> Vec<f32> {
    interleaved
        .chunks_exact(2)
        .map(|pair| (pair[0] + pair[1]) * 0.5)
        .collect()
}

/// Calculate RMS (root mean square) of audio samples.
fn calculate_rms(samples: &[f32]) -> f32 {
    if samples.is_empty() {
        return 0.0;
    }
    let sum: f32 = samples.iter().map(|s| s * s).sum();
    (sum / samples.len() as f32).sqrt()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_chunk_fields() {
        let chunk = AudioChunk {
            ssrc: 12345,
            pcm: vec![0.1, 0.2, 0.3],
        };
        assert_eq!(chunk.ssrc, 12345);
        assert_eq!(chunk.pcm.len(), 3);
    }

    #[test]
    fn calculate_rms_zero() {
        let samples = vec![0.0; 100];
        let rms = calculate_rms(&samples);
        assert!(rms < 0.001);
    }

    #[test]
    fn calculate_rms_nonzero() {
        let samples = vec![0.5; 100];
        let rms = calculate_rms(&samples);
        assert!((rms - 0.5).abs() < 0.001);
    }

    #[test]
    fn calculate_rms_empty() {
        let samples: Vec<f32> = vec![];
        let rms = calculate_rms(&samples);
        assert_eq!(rms, 0.0);
    }

    #[test]
    fn voice_receive_handler_new() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let handler = VoiceReceiveHandler::new(tx);
        // Verify construction succeeds
        let _ = handler;
    }

    #[test]
    fn i16_to_f32_conversion() {
        // Max positive i16 → ~1.0
        let max_val = i16::MAX as f32 / 32768.0;
        assert!((max_val - 0.999969).abs() < 0.001);

        // Min i16 → −1.0
        let min_val = i16::MIN as f32 / 32768.0;
        assert!((min_val - (-1.0)).abs() < 0.001);

        // Zero
        let zero_val = 0i16 as f32 / 32768.0;
        assert!((zero_val - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn stereo_to_mono_downmix() {
        // L=1.0, R=0.0 → 0.5
        let stereo = vec![1.0f32, 0.0, 0.6, 0.4, -0.5, 0.5];
        let mono = stereo_to_mono(&stereo);
        assert_eq!(mono.len(), 3);
        assert!((mono[0] - 0.5).abs() < f32::EPSILON);
        assert!((mono[1] - 0.5).abs() < f32::EPSILON);
        assert!((mono[2] - 0.0).abs() < f32::EPSILON);
    }

    #[test]
    fn stereo_to_mono_empty() {
        let mono = stereo_to_mono(&[]);
        assert!(mono.is_empty());
    }

    #[test]
    fn sample_counts_per_tick() {
        // 20ms @ 48kHz stereo = 960 samples/ch × 2 = 1920 interleaved
        let interleaved_per_tick = 48000 * 20 / 1000 * 2;
        assert_eq!(interleaved_per_tick, 1920);

        // After stereo→mono: 960 mono samples
        let mono_per_tick = interleaved_per_tick / 2;
        assert_eq!(mono_per_tick, 960);

        // After 48→16 kHz resample: ~320 samples (16000 * 20 / 1000)
        let expected_output = 16000 * 20 / 1000;
        assert_eq!(expected_output, 320);
    }
}
