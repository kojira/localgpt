//! Voice receive handler.
//!
//! Receives decoded PCM audio from songbird's VoiceTick events and
//! forwards [`AudioChunk`]s to the dispatcher via an mpsc channel.
//!
//! Songbird is configured with `DecodeMode::Decode`, `Channels::Mono`,
//! and `SampleRate::Hz16000`, so the audio arrives as 16 kHz mono i16
//! PCM — exactly what the STT pipeline needs. No manual Opus decoding
//! or resampling is required.

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

                    let rms = calculate_rms(&pcm_f32);

                    debug!(
                        ssrc,
                        samples = pcm_f32.len(),
                        rms = format!("{:.4}", rms),
                        "Received audio"
                    );

                    let chunk = AudioChunk {
                        ssrc,
                        pcm: pcm_f32,
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
    fn resample_ratio() {
        // With songbird configured to decode at 16kHz mono,
        // no resampling is needed. This test documents the sample counts.
        // 20ms @ 16kHz mono = 320 samples
        let samples_per_tick = 16000 * 20 / 1000;
        assert_eq!(samples_per_tick, 320);
    }
}
