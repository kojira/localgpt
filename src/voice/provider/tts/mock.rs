//! Mock TTS provider for testing.
//!
//! Generates silence or sine-wave audio with deterministic duration
//! based on input text length.  Useful for unit-testing the pipeline
//! without an external TTS server.

use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use tokio::time::sleep;

use crate::voice::provider::{TtsProvider, TtsResult};

// ── Configuration ────────────────────────────────────────────────

/// Waveform type for mock audio generation.
#[derive(Debug, Clone)]
pub enum MockWaveform {
    Silence,
    Sine {
        frequency_hz: f32,
        amplitude: f32,
    },
}

/// Configuration for [`MockTtsProvider`].
#[derive(Debug, Clone)]
pub struct MockTtsConfig {
    pub sample_rate: u32,
    pub ms_per_char: f64,
    pub min_duration_ms: f64,
    pub max_duration_ms: f64,
    pub waveform: MockWaveform,
    pub latency_ms: u64,
}

impl Default for MockTtsConfig {
    fn default() -> Self {
        Self {
            sample_rate: 24000,
            ms_per_char: 150.0,
            min_duration_ms: 200.0,
            max_duration_ms: 30000.0,
            waveform: MockWaveform::Silence,
            latency_ms: 0,
        }
    }
}

// ── Provider ─────────────────────────────────────────────────────

/// Mock TTS provider that generates deterministic audio.
pub struct MockTtsProvider {
    config: MockTtsConfig,
}

impl MockTtsProvider {
    pub fn new(config: MockTtsConfig) -> Self {
        Self { config }
    }

    /// Create a silent mock TTS provider with default settings.
    pub fn silent() -> Self {
        Self::new(MockTtsConfig::default())
    }

    /// Create a sine-wave mock TTS provider at the given frequency.
    pub fn sine(frequency_hz: f32) -> Self {
        Self::new(MockTtsConfig {
            waveform: MockWaveform::Sine {
                frequency_hz,
                amplitude: 0.8,
            },
            ..Default::default()
        })
    }

    /// Set simulated synthesis latency.
    pub fn with_latency(mut self, ms: u64) -> Self {
        self.config.latency_ms = ms;
        self
    }
}

#[async_trait]
impl TtsProvider for MockTtsProvider {
    async fn synthesize(&self, text: &str) -> Result<TtsResult> {
        if self.config.latency_ms > 0 {
            sleep(Duration::from_millis(self.config.latency_ms)).await;
        }

        let char_count = text.chars().count() as f64;
        let duration_ms = (char_count * self.config.ms_per_char)
            .clamp(self.config.min_duration_ms, self.config.max_duration_ms);

        let sample_count = (self.config.sample_rate as f64 * duration_ms / 1000.0) as usize;

        let audio = match &self.config.waveform {
            MockWaveform::Silence => vec![0.0f32; sample_count],
            MockWaveform::Sine {
                frequency_hz,
                amplitude,
            } => (0..sample_count)
                .map(|i| {
                    let t = i as f32 / self.config.sample_rate as f32;
                    amplitude * (2.0 * std::f32::consts::PI * frequency_hz * t).sin()
                })
                .collect(),
        };

        Ok(TtsResult {
            audio,
            sample_rate: self.config.sample_rate,
            duration_ms,
        })
    }

    fn name(&self) -> &str {
        "mock"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn duration_calculation() {
        let provider = MockTtsProvider::silent();
        let result = provider.synthesize("hello").await.unwrap();
        // 5 chars * 150 ms/char = 750 ms
        assert!((result.duration_ms - 750.0).abs() < f64::EPSILON);
        // 24000 Hz * 0.75 s = 18000 samples
        assert_eq!(result.audio.len(), 18000);
    }

    #[tokio::test]
    async fn min_duration() {
        let provider = MockTtsProvider::silent();
        let result = provider.synthesize("a").await.unwrap();
        // 1 char * 150 ms = 150 ms, clamped to min 200 ms
        assert!((result.duration_ms - 200.0).abs() < f64::EPSILON);
    }

    #[tokio::test]
    async fn sine_wave() {
        let provider = MockTtsProvider::sine(440.0);
        let result = provider.synthesize("hello").await.unwrap();
        assert!(!result.audio.is_empty());
        // Sine wave with amplitude 0.8 should exceed 0.4.
        let max_amp = result
            .audio
            .iter()
            .map(|s| s.abs())
            .fold(0.0f32, f32::max);
        assert!(max_amp > 0.4, "max amplitude was {}", max_amp);
    }

    #[tokio::test]
    async fn silent_all_zero() {
        let provider = MockTtsProvider::silent();
        let result = provider.synthesize("test").await.unwrap();
        assert!(result.audio.iter().all(|&s| s == 0.0));
    }
}
