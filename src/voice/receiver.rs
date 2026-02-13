//! Voice receive handler.
//!
//! Receives raw Opus packets from songbird's VoiceTick events
//! (configured with `DecodeMode::Pass`) and decodes them manually
//! via audiopus. The decoded 48 kHz stereo PCM is then downmixed
//! to mono and resampled to 16 kHz before forwarding [`AudioChunk`]s
//! to the dispatcher via an mpsc channel.

use audiopus::packet::Packet as OpusPacket;
use audiopus::MutSignals;
use songbird::events::{Event, EventContext, EventHandler as VoiceEventHandler};
use std::sync::Mutex;
use tokio::sync::mpsc;
use tracing::{debug, warn};

/// Maximum Opus frame size: 120 ms @ 48 kHz stereo = 5760 samples × 2 channels.
const MAX_OPUS_FRAME_SAMPLES: usize = 5760 * 2;

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
/// Each tick (every 20 ms) delivers raw Opus packets for all speaking users.
/// This handler decodes Opus → i16 PCM (48 kHz stereo) via audiopus,
/// converts to f32, downmixes stereo → mono, and resamples 48 kHz → 16 kHz.
pub struct VoiceReceiveHandler {
    /// Channel to send audio chunks to the dispatcher
    audio_tx: mpsc::UnboundedSender<AudioChunk>,
    /// Opus decoder (48 kHz stereo). Mutex-wrapped because `act()` takes `&self`
    /// but audiopus::coder::Decoder requires `&mut self` to decode.
    opus_decoder: Mutex<audiopus::coder::Decoder>,
}

impl VoiceReceiveHandler {
    /// Create a new receive handler with an Opus decoder configured for
    /// 48 kHz stereo (matching Discord's native Opus format).
    pub fn new(audio_tx: mpsc::UnboundedSender<AudioChunk>) -> Self {
        let decoder = audiopus::coder::Decoder::new(
            audiopus::SampleRate::Hz48000,
            audiopus::Channels::Stereo,
        )
        .expect("Failed to create Opus decoder");

        Self {
            audio_tx,
            opus_decoder: Mutex::new(decoder),
        }
    }
}

#[async_trait::async_trait]
impl VoiceEventHandler for VoiceReceiveHandler {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        debug!("VoiceReceiveHandler::act called, ctx variant: {:?}", std::mem::discriminant(ctx));
        if let EventContext::VoiceTick(tick) = ctx {
            debug!("VoiceTick: speaking={} silent={}", tick.speaking.len(), tick.silent.len());
            for (&ssrc, data) in &tick.speaking {
                // In DecodeMode::Pass, raw RTP data is in `data.packet`
                let rtp_data = match &data.packet {
                    Some(rtp) => rtp,
                    None => continue,
                };

                // Extract Opus payload from the RTP packet
                let raw = &rtp_data.packet;
                let end = raw.len().saturating_sub(rtp_data.payload_end_pad);
                if rtp_data.payload_offset >= end {
                    continue;
                }
                let opus_payload = &raw[rtp_data.payload_offset..end];
                debug!("SSRC {} opus_payload size: {} bytes", ssrc, opus_payload.len());
                if opus_payload.is_empty() {
                    continue;
                }

                // Wrap as audiopus Packet
                let opus_pkt = match OpusPacket::try_from(opus_payload) {
                    Ok(p) => p,
                    Err(e) => {
                        warn!(ssrc, "Invalid Opus packet: {}", e);
                        continue;
                    }
                };

                // Decode Opus → interleaved i16 PCM (48 kHz stereo)
                let pcm_i16 = {
                    let mut decoder = match self.opus_decoder.lock() {
                        Ok(d) => d,
                        Err(e) => {
                            warn!(ssrc, "Opus decoder lock poisoned: {}", e);
                            continue;
                        }
                    };
                    let mut buf = vec![0i16; MAX_OPUS_FRAME_SAMPLES];
                    let mut_signals = match MutSignals::try_from(buf.as_mut_slice()) {
                        Ok(s) => s,
                        Err(e) => {
                            warn!(ssrc, "MutSignals creation failed: {}", e);
                            continue;
                        }
                    };
                    match decoder.decode(Some(opus_pkt), mut_signals, false) {
                        Ok(decoded_samples) => {
                            // decoded_samples is per-channel; stereo = samples * 2 interleaved
                            buf.truncate(decoded_samples * 2);
                            buf
                        }
                        Err(e) => {
                            warn!(ssrc, "Opus decode failed: {}", e);
                            continue;
                        }
                    }
                };

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
                    opus_bytes = opus_payload.len(),
                    decoded_samples = pcm_i16.len(),
                    out_samples = resampled.len(),
                    rms = format!("{:.4}", rms),
                    "Decoded Opus → 48kHz stereo → 16kHz mono"
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
        // Verify construction succeeds (Opus decoder created)
        let _ = handler;
    }

    #[test]
    fn opus_decoder_plc() {
        // Packet loss concealment: passing None should produce silence samples.
        let mut decoder = audiopus::coder::Decoder::new(
            audiopus::SampleRate::Hz48000,
            audiopus::Channels::Stereo,
        )
        .expect("decoder creation");

        let mut buf = vec![0i16; MAX_OPUS_FRAME_SAMPLES];
        let signals = MutSignals::try_from(buf.as_mut_slice()).unwrap();
        let result = decoder.decode(None::<OpusPacket<'_>>, signals, false);
        assert!(result.is_ok());
        let samples = result.unwrap();
        assert!(samples > 0);
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
