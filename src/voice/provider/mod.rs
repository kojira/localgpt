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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stt_event_deserialize_speech_start() {
        let json = r#"{"type":"speech_start","timestamp_ms":1234}"#;
        let event: SttEvent = serde_json::from_str(json).unwrap();
        match event {
            SttEvent::SpeechStart { timestamp_ms } => assert_eq!(timestamp_ms, 1234),
            _ => panic!("expected SpeechStart"),
        }
    }

    #[test]
    fn stt_event_deserialize_partial() {
        let json = r#"{"type":"partial","text":"hello"}"#;
        let event: SttEvent = serde_json::from_str(json).unwrap();
        match event {
            SttEvent::Partial { text } => assert_eq!(text, "hello"),
            _ => panic!("expected Partial"),
        }
    }

    #[test]
    fn stt_event_deserialize_final() {
        let json = r#"{"type":"final","text":"hello world","language":"en","confidence":0.95,"duration_ms":1500.0}"#;
        let event: SttEvent = serde_json::from_str(json).unwrap();
        match event {
            SttEvent::Final {
                text,
                language,
                confidence,
                duration_ms,
            } => {
                assert_eq!(text, "hello world");
                assert_eq!(language, "en");
                assert!((confidence - 0.95).abs() < f32::EPSILON);
                assert!((duration_ms - 1500.0).abs() < f64::EPSILON);
            }
            _ => panic!("expected Final"),
        }
    }

    #[test]
    fn stt_event_deserialize_speech_end() {
        let json = r#"{"type":"speech_end","timestamp_ms":5678,"duration_ms":2000.0}"#;
        let event: SttEvent = serde_json::from_str(json).unwrap();
        match event {
            SttEvent::SpeechEnd {
                timestamp_ms,
                duration_ms,
            } => {
                assert_eq!(timestamp_ms, 5678);
                assert!((duration_ms - 2000.0).abs() < f64::EPSILON);
            }
            _ => panic!("expected SpeechEnd"),
        }
    }

    #[test]
    fn stt_event_unknown_type_is_err() {
        let json = r#"{"type":"unknown","data":123}"#;
        assert!(serde_json::from_str::<SttEvent>(json).is_err());
    }

    #[test]
    fn tts_result_construction() {
        let result = TtsResult {
            audio: vec![0.1, 0.2, -0.3],
            sample_rate: 24000,
            duration_ms: 100.0,
        };
        assert_eq!(result.audio.len(), 3);
        assert_eq!(result.sample_rate, 24000);
        assert!((result.duration_ms - 100.0).abs() < f64::EPSILON);
    }

    #[test]
    fn tts_result_clone() {
        let result = TtsResult {
            audio: vec![0.5],
            sample_rate: 44100,
            duration_ms: 50.0,
        };
        let cloned = result.clone();
        assert_eq!(cloned.audio, result.audio);
        assert_eq!(cloned.sample_rate, result.sample_rate);
    }

    #[test]
    fn stt_event_clone() {
        let event = SttEvent::Partial {
            text: "test".to_string(),
        };
        let cloned = event.clone();
        match cloned {
            SttEvent::Partial { text } => assert_eq!(text, "test"),
            _ => panic!("clone should preserve variant"),
        }
    }
}
