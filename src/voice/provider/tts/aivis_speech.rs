//! AivisSpeech (VOICEVOX-compatible) TTS provider.
//!
//! Communicates via REST API: `/audio_query` → `/synthesis`.
//!
//! Flow:
//! 1. POST `/audio_query?text=X&speaker=ID` → JSON query parameters
//! 2. Apply speed/pitch/intonation/volume scales from config
//! 3. POST `/synthesis?speaker=ID` with JSON body → WAV audio
//! 4. Parse WAV (i16) → f32 → resample 24 kHz → 48 kHz

use anyhow::{Context, Result};
use async_trait::async_trait;
use std::io::Cursor;
use tracing::debug;

use crate::config::VoiceTtsAivisSpeechConfig;
use crate::voice::audio::{pcm_i16_to_f32, resample_24k_to_48k};
use crate::voice::provider::{TtsProvider, TtsResult};

/// AivisSpeech TTS provider using VOICEVOX-compatible REST API.
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

    /// Apply configured voice parameters to the audio_query JSON.
    fn apply_scales(&self, query: &mut serde_json::Value) {
        if let Some(obj) = query.as_object_mut() {
            obj.insert(
                "speedScale".to_string(),
                serde_json::json!(self.config.speed_scale),
            );
            obj.insert(
                "pitchScale".to_string(),
                serde_json::json!(self.config.pitch_scale),
            );
            obj.insert(
                "intonationScale".to_string(),
                serde_json::json!(self.config.intonation_scale),
            );
            obj.insert(
                "volumeScale".to_string(),
                serde_json::json!(self.config.volume_scale),
            );
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

        // Step 1: POST /audio_query to get synthesis parameters
        let mut query: serde_json::Value = self
            .client
            .post(format!("{}/audio_query", base))
            .query(&[
                ("text", text),
                ("speaker", &self.config.style_id.to_string()),
            ])
            .send()
            .await
            .context("audio_query request failed")?
            .error_for_status()
            .context("audio_query returned error status")?
            .json()
            .await
            .context("failed to parse audio_query response as JSON")?;

        // Step 2: Apply voice parameter overrides from config
        self.apply_scales(&mut query);

        debug!(
            speed = self.config.speed_scale,
            pitch = self.config.pitch_scale,
            intonation = self.config.intonation_scale,
            volume = self.config.volume_scale,
            "applied voice scales to audio_query"
        );

        // Step 3: POST /synthesis with modified query → WAV audio
        let wav_bytes = self
            .client
            .post(format!("{}/synthesis", base))
            .query(&[("speaker", &self.config.style_id.to_string())])
            .json(&query)
            .send()
            .await
            .context("synthesis request failed")?
            .error_for_status()
            .context("synthesis returned error status")?
            .bytes()
            .await
            .context("failed to read synthesis response body")?;

        // Step 4: Parse WAV → i16 samples
        let (samples_i16, _wav_sr) = Self::parse_wav(&wav_bytes)?;

        // Step 5: Convert i16 → f32
        let samples_f32 = pcm_i16_to_f32(&samples_i16);

        // Step 6: Resample 24 kHz → 48 kHz for Discord playback
        let resampled = resample_24k_to_48k(&samples_f32)
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
    fn apply_scales_default_config() {
        let provider = default_provider();
        let mut query = serde_json::json!({
            "accent_phrases": [],
            "speedScale": 1.0,
            "pitchScale": 0.0,
            "intonationScale": 1.0,
            "volumeScale": 1.0,
        });
        provider.apply_scales(&mut query);
        assert_eq!(query["speedScale"], 1.0);
        assert_eq!(query["pitchScale"], 0.0);
        assert_eq!(query["intonationScale"], 1.0);
        assert_eq!(query["volumeScale"], 1.0);
    }

    #[test]
    fn apply_scales_custom_values() {
        let provider = AivisSpeechProvider::new(VoiceTtsAivisSpeechConfig {
            speed_scale: 1.5,
            pitch_scale: 0.1,
            intonation_scale: 1.2,
            volume_scale: 0.8,
            ..Default::default()
        });
        let mut query = serde_json::json!({
            "accent_phrases": [],
            "speedScale": 1.0,
            "pitchScale": 0.0,
            "intonationScale": 1.0,
            "volumeScale": 1.0,
        });
        provider.apply_scales(&mut query);
        assert_eq!(query["speedScale"], 1.5);
        assert_eq!(query["pitchScale"], 0.1);
        assert_eq!(query["intonationScale"], 1.2);
        assert_eq!(query["volumeScale"], 0.8);
    }

    #[test]
    fn apply_scales_adds_missing_keys() {
        let provider = AivisSpeechProvider::new(VoiceTtsAivisSpeechConfig {
            speed_scale: 2.0,
            ..Default::default()
        });
        let mut query = serde_json::json!({"accent_phrases": []});
        provider.apply_scales(&mut query);
        assert_eq!(query["speedScale"], 2.0);
        assert_eq!(query["pitchScale"], 0.0);
    }

    #[test]
    fn apply_scales_non_object_is_noop() {
        let provider = default_provider();
        let mut query = serde_json::json!("not an object");
        provider.apply_scales(&mut query);
        assert_eq!(query, serde_json::json!("not an object"));
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
    fn full_pipeline_wav_to_resampled() {
        // Simulate the post-HTTP portion of synthesize:
        // parse WAV → i16→f32 → resample 24k→48k
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

        let resampled = resample_24k_to_48k(&samples_f32).unwrap();
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
            style_id: 42,
            speed_scale: 2.0,
            pitch_scale: 0.5,
            intonation_scale: 1.5,
            volume_scale: 0.7,
        };
        let provider = AivisSpeechProvider::new(config);
        assert_eq!(provider.config.endpoint, "http://localhost:9999");
        assert_eq!(provider.config.style_id, 42);
        assert_eq!(provider.config.speed_scale, 2.0);
    }
}
