//! AivisSpeech TTS provider using the `/voice` endpoint.
//!
//! Flow:
//! 1. POST `{endpoint}/voice?model=M&text=T&speed=S&format=wav` → WAV bytes
//! 2. Parse WAV header to detect sample rate
//! 3. Convert i16 PCM → f32, apply volume scale
//! 4. Resample to 48 kHz for Discord playback

use anyhow::{Context, Result};
use async_trait::async_trait;
use std::io::Cursor;
use tracing::debug;

use crate::config::VoiceTtsAivisSpeechConfig;
use crate::voice::audio::{pcm_i16_to_f32, resample_mono};
use crate::voice::provider::{TtsProvider, TtsResult};

/// AivisSpeech TTS provider using the `/voice` REST endpoint.
pub struct AivisSpeechProvider {
    config: VoiceTtsAivisSpeechConfig,
    client: reqwest::Client,
}

impl AivisSpeechProvider {
    pub fn new(config: VoiceTtsAivisSpeechConfig) -> Self {
        Self {
            config,
            client: reqwest::Client::new(),
        }
    }

    /// Parse WAV bytes into i16 PCM samples and the WAV sample rate.
    fn parse_wav(wav_bytes: &[u8]) -> Result<(Vec<i16>, u32)> {
        let reader =
            hound::WavReader::new(Cursor::new(wav_bytes)).context("failed to parse WAV response")?;
        let sample_rate = reader.spec().sample_rate;
        let samples: Vec<i16> = reader
            .into_samples::<i16>()
            .collect::<Result<Vec<_>, _>>()
            .context("failed to read WAV samples")?;
        Ok((samples, sample_rate))
    }
}

#[async_trait]
impl TtsProvider for AivisSpeechProvider {
    async fn synthesize(&self, text: &str) -> Result<TtsResult> {
        let base = self.config.endpoint.trim_end_matches('/');

        // POST /voice with query params
        let wav_bytes = self
            .client
            .post(format!("{}/voice", base))
            .query(&[
                ("model", self.config.model.as_str()),
                ("text", text),
                ("speed", &self.config.speed_scale.to_string()),
                ("format", "wav"),
            ])
            .send()
            .await
            .context("voice request failed")?
            .error_for_status()
            .context("voice endpoint returned error status")?
            .bytes()
            .await
            .context("failed to read voice response body")?;

        // Parse WAV → i16 samples + detect sample rate
        let (samples_i16, wav_sr) = Self::parse_wav(&wav_bytes)?;

        debug!(
            wav_sample_rate = wav_sr,
            samples = samples_i16.len(),
            model = %self.config.model,
            speed = self.config.speed_scale,
            "AivisSpeech WAV received"
        );

        // Convert i16 → f32
        let mut samples_f32 = pcm_i16_to_f32(&samples_i16);

        // Apply volume scale
        if (self.config.volume_scale - 1.0).abs() > f64::EPSILON {
            let vol = self.config.volume_scale as f32;
            for s in &mut samples_f32 {
                *s *= vol;
            }
        }

        // Resample to 48 kHz for Discord playback
        let resampled = resample_mono(&samples_f32, wav_sr, 48000)
            .map_err(|e| anyhow::anyhow!("resampling failed: {}", e))?;

        let duration_ms = resampled.len() as f64 / 48000.0 * 1000.0;

        debug!(
            samples = resampled.len(),
            duration_ms, "AivisSpeech synthesis complete"
        );

        Ok(TtsResult {
            audio: resampled,
            sample_rate: 48000,
            duration_ms,
        })
    }

    fn name(&self) -> &str {
        "aivis-speech"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::VoiceTtsAivisSpeechConfig;

    fn default_provider() -> AivisSpeechProvider {
        AivisSpeechProvider::new(VoiceTtsAivisSpeechConfig::default())
    }

    #[test]
    fn provider_name() {
        assert_eq!(default_provider().name(), "aivis-speech");
    }

    #[test]
    fn parse_wav_valid_mono_i16() {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 24000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut buf = Vec::new();
        {
            let cursor = Cursor::new(&mut buf);
            let mut writer = hound::WavWriter::new(cursor, spec).unwrap();
            for i in 0..240 {
                let t = i as f32 / 240.0;
                let sample = (t * std::f32::consts::TAU).sin();
                writer.write_sample((sample * 16000.0) as i16).unwrap();
            }
            writer.finalize().unwrap();
        }

        let (samples, sample_rate) = AivisSpeechProvider::parse_wav(&buf).unwrap();
        assert_eq!(sample_rate, 24000);
        assert_eq!(samples.len(), 240);
    }

    #[test]
    fn parse_wav_empty_audio() {
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 24000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut buf = Vec::new();
        {
            let cursor = Cursor::new(&mut buf);
            let writer = hound::WavWriter::new(cursor, spec).unwrap();
            writer.finalize().unwrap();
        }

        let (samples, sample_rate) = AivisSpeechProvider::parse_wav(&buf).unwrap();
        assert_eq!(sample_rate, 24000);
        assert!(samples.is_empty());
    }

    #[test]
    fn parse_wav_invalid_data() {
        let result = AivisSpeechProvider::parse_wav(b"not a wav file");
        assert!(result.is_err());
    }

    #[test]
    fn parse_wav_detects_sample_rate() {
        // Create a WAV at 44100 Hz to verify we detect it correctly
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 44100,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut buf = Vec::new();
        {
            let cursor = Cursor::new(&mut buf);
            let mut writer = hound::WavWriter::new(cursor, spec).unwrap();
            for _ in 0..100 {
                writer.write_sample(0i16).unwrap();
            }
            writer.finalize().unwrap();
        }

        let (samples, sample_rate) = AivisSpeechProvider::parse_wav(&buf).unwrap();
        assert_eq!(sample_rate, 44100);
        assert_eq!(samples.len(), 100);
    }

    #[test]
    fn full_pipeline_wav_to_resampled() {
        // Simulate the post-HTTP portion of synthesize:
        // parse WAV → i16→f32 → resample to 48k
        let spec = hound::WavSpec {
            channels: 1,
            sample_rate: 24000,
            bits_per_sample: 16,
            sample_format: hound::SampleFormat::Int,
        };
        let mut buf = Vec::new();
        {
            let cursor = Cursor::new(&mut buf);
            let mut writer = hound::WavWriter::new(cursor, spec).unwrap();
            // 480 samples @ 24kHz = 20ms of a 440 Hz sine
            for i in 0..480 {
                let t = i as f32 / 24000.0;
                let sample = (t * 440.0 * std::f32::consts::TAU).sin();
                writer.write_sample((sample * 16000.0) as i16).unwrap();
            }
            writer.finalize().unwrap();
        }

        let (samples_i16, sr) = AivisSpeechProvider::parse_wav(&buf).unwrap();
        assert_eq!(sr, 24000);
        assert_eq!(samples_i16.len(), 480);

        let samples_f32 = pcm_i16_to_f32(&samples_i16);
        assert_eq!(samples_f32.len(), 480);
        // Values should be in -1.0..1.0 range
        assert!(samples_f32.iter().all(|&s| s >= -1.0 && s <= 1.0));

        let resampled = resample_mono(&samples_f32, 24000, 48000).unwrap();
        // Should roughly double (rubato may produce slightly fewer due to filter delay)
        assert!(
            resampled.len() > 400 && resampled.len() <= 1100,
            "expected ~960 samples, got {}",
            resampled.len()
        );

        let duration_ms = resampled.len() as f64 / 48000.0 * 1000.0;
        assert!(duration_ms > 0.0);
    }

    #[test]
    fn constructor_stores_config() {
        let config = VoiceTtsAivisSpeechConfig {
            endpoint: "http://localhost:9999".to_string(),
            model: "testmodel".to_string(),
            speed_scale: 2.0,
            volume_scale: 0.7,
        };
        let provider = AivisSpeechProvider::new(config);
        assert_eq!(provider.config.endpoint, "http://localhost:9999");
        assert_eq!(provider.config.model, "testmodel");
        assert_eq!(provider.config.speed_scale, 2.0);
        assert_eq!(provider.config.volume_scale, 0.7);
    }
}
