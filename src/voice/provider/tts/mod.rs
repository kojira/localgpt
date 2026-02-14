//! TTS provider implementations.

pub mod aivis_speech;
pub mod mock;

use std::sync::Arc;

use anyhow::{bail, Result};

use crate::config::VoiceTtsConfig;
use crate::voice::provider::TtsProvider;

/// Create a [`TtsProvider`] from configuration.
///
/// Supported `provider` values:
/// - `"aivis-speech"` — AivisSpeech REST API (VOICEVOX-compatible).
/// - `"mock"` — Mock provider for testing (generates silence).
pub fn create_tts_provider(config: &VoiceTtsConfig) -> Result<Arc<dyn TtsProvider>> {
    match config.provider.as_str() {
        "aivis-speech" => Ok(Arc::new(aivis_speech::AivisSpeechProvider::new(
            config.aivis_speech.clone(),
        ))),
        "mock" => Ok(Arc::new(mock::MockTtsProvider::silent())),
        other => bail!("unknown TTS provider: {other:?} (expected \"aivis-speech\" or \"mock\")"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_aivis_provider() {
        let config = VoiceTtsConfig::default();
        let provider = create_tts_provider(&config).unwrap();
        assert_eq!(provider.name(), "aivis-speech");
    }

    #[test]
    fn create_mock_tts_provider() {
        let mut config = VoiceTtsConfig::default();
        config.provider = "mock".to_string();
        let provider = create_tts_provider(&config).unwrap();
        assert_eq!(provider.name(), "mock");
    }

    #[test]
    fn unknown_tts_provider_is_error() {
        let mut config = VoiceTtsConfig::default();
        config.provider = "nonexistent".to_string();
        assert!(create_tts_provider(&config).is_err());
    }
}
