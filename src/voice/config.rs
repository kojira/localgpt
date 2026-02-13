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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn from_voice_config_preserves_enabled() {
        let mut vc = VoiceConfig::default();
        vc.enabled = true;
        let mgr = VoiceManagerConfig::from_voice_config(vc);
        assert!(mgr.voice.enabled);
    }

    #[test]
    fn default_voice_config_is_disabled() {
        let vc = VoiceConfig::default();
        let mgr = VoiceManagerConfig::from_voice_config(vc);
        assert!(!mgr.voice.enabled);
    }

    #[test]
    fn default_stt_provider_is_ws() {
        let vc = VoiceConfig::default();
        assert_eq!(vc.stt.provider, "ws");
    }

    #[test]
    fn default_tts_provider_is_aivis_speech() {
        let vc = VoiceConfig::default();
        assert_eq!(vc.tts.provider, "aivis-speech");
    }

    #[test]
    fn default_audio_sample_rates() {
        let vc = VoiceConfig::default();
        assert_eq!(vc.audio.input_sample_rate, 48000);
        assert_eq!(vc.audio.stt_sample_rate, 16000);
    }

    #[test]
    fn default_pipeline_config() {
        let vc = VoiceConfig::default();
        assert!(vc.pipeline.interrupt_enabled);
        assert_eq!(vc.pipeline.idle_timeout_sec, 300);
    }

    #[test]
    fn default_stt_ws_config() {
        let ws = VoiceSttWsConfig::default();
        assert_eq!(ws.endpoint, "ws://127.0.0.1:8766/ws");
        assert_eq!(ws.reconnect_interval_ms, 1000);
        assert_eq!(ws.max_reconnect_attempts, 10);
    }

    #[test]
    fn default_tts_aivis_config() {
        let aivis = VoiceTtsAivisSpeechConfig::default();
        assert_eq!(aivis.endpoint, "http://127.0.0.1:10101");
        assert_eq!(aivis.style_id, 888753760);
        assert!((aivis.speed_scale - 1.0).abs() < f64::EPSILON);
        assert!((aivis.volume_scale - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn default_transcript_is_disabled() {
        let vc = VoiceConfig::default();
        assert!(!vc.transcript.enabled);
        assert!(vc.transcript.channel_id.is_none());
    }
}
