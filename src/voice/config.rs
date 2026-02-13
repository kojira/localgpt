//! Voice-specific configuration helpers.
//!
//! The deserialization structs live in [`crate::config`] so that
//! `config.toml` can always be parsed regardless of the `voice` feature.
//! This module re-exports them and adds voice-specific convenience methods.

pub use crate::config::{
    VoiceAudioConfig, VoiceConfig, VoiceDiscordConfig, VoicePipelineConfig, VoiceSttConfig,
    VoiceSttWsConfig, VoiceTranscriptConfig, VoiceTtsAivisSpeechConfig, VoiceTtsConfig,
};

/// Runtime configuration for [`super::VoiceManager`].
#[derive(Debug, Clone)]
pub struct VoiceManagerConfig {
    pub voice: VoiceConfig,
}

impl VoiceManagerConfig {
    pub fn from_voice_config(voice: VoiceConfig) -> Self {
        Self { voice }
    }
}
