//! Voice receive handler.
//!
//! Receives Opus packets from songbird, decodes to PCM,
//! resamples 48 kHz stereo → 16 kHz mono, and forwards
//! [`AudioChunk`]s to the dispatcher via an mpsc channel.

use anyhow::Result;
use opus::{Channels, Decoder};
use rubato::{
    Resampler, SincFixedIn, SincInterpolationParameters,
    SincInterpolationType, WindowFunction,
};
use songbird::events::{Event, EventContext, EventHandler as VoiceEventHandler};
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{debug, warn};

/// A chunk of decoded, resampled audio from a single speaker.
#[derive(Debug, Clone)]
pub struct AudioChunk {
    /// Synchronization source — identifies the speaker.
    pub ssrc: u32,
    /// 16 kHz mono f32 PCM samples.
    pub pcm: Vec<f32>,
}

/// songbird `EventHandler` implementation for receiving voice packets.
pub struct VoiceReceiveHandler {
    /// Channel to send audio chunks to the dispatcher
    audio_tx: mpsc::UnboundedSender<AudioChunk>,
    /// Opus decoder (48 kHz stereo)
    opus_decoder: Arc<std::sync::Mutex<Decoder>>,
    /// Resampler (48 kHz → 16 kHz) - using concrete type
    resampler: Arc<std::sync::Mutex<SincFixedIn<f32>>>,
}

impl VoiceReceiveHandler {
    /// Create a new receive handler
    pub fn new(audio_tx: mpsc::UnboundedSender<AudioChunk>) -> Result<Self> {
        // Opus decoder: 48 kHz stereo
        let opus_decoder = Decoder::new(48000, Channels::Stereo)?;

        // Resampler: 48 kHz stereo → 16 kHz mono
        // Chunk size: 20ms @ 48kHz = 960 samples per channel
        let params = SincInterpolationParameters {
            sinc_len: 256,
            f_cutoff: 0.95,
            interpolation: SincInterpolationType::Linear,
            oversampling_factor: 256,
            window: WindowFunction::BlackmanHarris2,
        };

        let resampler = SincFixedIn::<f32>::new(
            16000.0 / 48000.0, // ratio (1/3)
            2.0,               // max resample ratio delta
            params,
            960,  // chunk size (20ms @ 48kHz)
            2,    // channels (stereo input)
        )?;

        Ok(Self {
            audio_tx,
            opus_decoder: Arc::new(std::sync::Mutex::new(opus_decoder)),
            resampler: Arc::new(std::sync::Mutex::new(resampler)),
        })
    }

    /// Process a single Opus packet: decode → resample → send
    fn process_packet(&self, ssrc: u32, opus_data: &[u8]) -> Result<()> {
        // Decode Opus → 48 kHz stereo PCM (i16)
        let mut pcm_i16 = vec![0i16; 960 * 2]; // 20ms @ 48kHz stereo
        let samples = {
            let mut decoder = self.opus_decoder.lock().unwrap();
            decoder.decode(opus_data, &mut pcm_i16, false)?
        };

        if samples == 0 {
            return Ok(());
        }

        // Convert i16 → f32 and separate channels
        let mut left = Vec::with_capacity(samples);
        let mut right = Vec::with_capacity(samples);
        for i in 0..samples {
            let l = pcm_i16[i * 2] as f32 / 32768.0;
            let r = pcm_i16[i * 2 + 1] as f32 / 32768.0;
            left.push(l);
            right.push(r);
        }

        // Resample 48 kHz → 16 kHz
        let input = vec![left, right];
        let output = {
            let mut resampler = self.resampler.lock().unwrap();
            resampler.process(&input, None)?
        };

        // Mix stereo → mono (average L and R)
        let left_16k = &output[0];
        let right_16k = &output[1];
        let mono: Vec<f32> = left_16k
            .iter()
            .zip(right_16k.iter())
            .map(|(l, r)| (l + r) / 2.0)
            .collect();

        // Calculate RMS for logging
        let rms = calculate_rms(&mono);

        debug!(
            ssrc,
            samples_48k = samples,
            samples_16k = mono.len(),
            rms = format!("{:.4}", rms),
            "Received audio"
        );

        // Send to dispatcher
        let chunk = AudioChunk { ssrc, pcm: mono };
        if let Err(e) = self.audio_tx.send(chunk) {
            warn!("Failed to send audio chunk: {}", e);
        }

        Ok(())
    }
}

#[async_trait::async_trait]
impl VoiceEventHandler for VoiceReceiveHandler {
    async fn act(&self, ctx: &EventContext<'_>) -> Option<Event> {
        match ctx {
            EventContext::VoiceTick(tick) => {
                // Process all speaking users in this tick
                for (ssrc, data) in tick.speaking.iter() {
                    if let Some(packet) = data.packet.as_ref() {
                        let opus_data = &packet.packet
                            [packet.payload_offset..packet.packet.len() - packet.payload_end_pad];
                        if let Err(e) = self.process_packet(*ssrc, opus_data) {
                            warn!(ssrc, error = %e, "Failed to process audio packet");
                        }
                    }
                }
            }
            _ => {}
        }
        None
    }
}

/// Calculate RMS (root mean square) of audio samples
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
        assert!(handler.is_ok());
    }

    #[test]
    fn process_packet_invalid_opus() {
        let (tx, _rx) = mpsc::unbounded_channel();
        let handler = VoiceReceiveHandler::new(tx).unwrap();

        // Invalid Opus data should error gracefully
        let invalid_data = vec![0u8; 10];
        let result = handler.process_packet(123, &invalid_data);
        // Opus decoder will reject invalid data
        assert!(result.is_err());
    }

    #[test]
    fn resample_ratio() {
        // 48 kHz → 16 kHz should be exactly 1/3
        let ratio: f64 = 16000.0 / 48000.0;
        assert!((ratio - 1.0 / 3.0).abs() < 0.0001);

        // 960 samples @ 48kHz = 20ms
        // After resampling to 16kHz: 320 samples
        let expected_output = (960.0 * ratio) as usize;
        assert_eq!(expected_output, 320);
    }
}
