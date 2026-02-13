//! AivisSpeech (VOICEVOX-compatible) TTS provider.
//!
//! Communicates via REST API: `/audio_query` → `/synthesis`.

use anyhow::Result;
use async_trait::async_trait;

use crate::config::VoiceTtsAivisSpeechConfig;
use crate::voice::provider::{TtsProvider, TtsResult};

/// AivisSpeech TTS provider (stub).
pub struct AivisSpeechProvider {
    _config: VoiceTtsAivisSpeechConfig,
}

impl AivisSpeechProvider {
    pub fn new(config: VoiceTtsAivisSpeechConfig) -> Self {
        Self { _config: config }
    }
}

#[async_trait]
impl TtsProvider for AivisSpeechProvider {
    async fn synthesize(&self, _text: &str) -> Result<TtsResult> {
        // Stub — will call AivisSpeech REST API
        Ok(TtsResult {
            audio: Vec::new(),
            sample_rate: 24000,
            duration_ms: 0.0,
        })
    }

    fn name(&self) -> &str {
        "aivis-speech"
    }
}
