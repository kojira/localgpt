//! STT / TTS provider traits and implementations.

pub mod stt;
pub mod tts;

use anyhow::Result;
use async_trait::async_trait;
use serde::Deserialize;

// ── STT ──────────────────────────────────────────────────────────

/// Events received from an STT server over WebSocket.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "type")]
pub enum SttEvent {
    /// Speech onset detected (server-side VAD).
    #[serde(rename = "speech_start")]
    SpeechStart { timestamp_ms: u64 },

    /// Interim (unstable) recognition result.
    #[serde(rename = "partial")]
    Partial { text: String },

    /// Final (stable) recognition result for one utterance.
    #[serde(rename = "final")]
    Final {
        text: String,
        language: String,
        confidence: f32,
        duration_ms: f64,
    },

    /// Speech offset detected (server-side VAD).
    #[serde(rename = "speech_end")]
    SpeechEnd {
        timestamp_ms: u64,
        duration_ms: f64,
    },
}

/// A single streaming STT session (one WebSocket connection).
#[async_trait]
pub trait SttSession: Send {
    /// Send a PCM audio chunk (16 kHz mono f32 LE).
    async fn send_audio(&mut self, audio: &[f32]) -> Result<()>;

    /// Receive the next event. Returns `None` when the session ends.
    async fn recv_event(&mut self) -> Result<Option<SttEvent>>;

    /// Close the session.
    async fn close(&mut self) -> Result<()>;
}

/// Factory for STT streaming sessions.
#[async_trait]
pub trait SttProvider: Send + Sync {
    /// Open a new streaming session (WebSocket connection).
    async fn connect(&self) -> Result<Box<dyn SttSession>>;

    /// Human-readable provider name.
    fn name(&self) -> &str;

    /// Release resources.
    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }
}

// ── TTS ──────────────────────────────────────────────────────────

/// Result of a TTS synthesis call.
#[derive(Debug, Clone)]
pub struct TtsResult {
    /// PCM f32 audio samples.
    pub audio: Vec<f32>,
    /// Sample rate of `audio` (e.g. 24000, 44100).
    pub sample_rate: u32,
    /// Duration in milliseconds.
    pub duration_ms: f64,
}

/// Text-to-speech provider.
#[async_trait]
pub trait TtsProvider: Send + Sync {
    /// Synthesize text into audio.
    async fn synthesize(&self, text: &str) -> Result<TtsResult>;

    /// Human-readable provider name.
    fn name(&self) -> &str;

    /// Release resources.
    async fn shutdown(&self) -> Result<()> {
        Ok(())
    }
}
