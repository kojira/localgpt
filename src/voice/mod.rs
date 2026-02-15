//! Discord voice chat module
//!
//! Provides a real-time voice conversation pipeline:
//! Discord VC → STT → LLM Agent → TTS → Discord VC

pub mod agent_bridge;
pub mod audio;
pub mod config;
pub mod context_window;
pub mod dispatcher;
pub mod gateway;
pub mod lrs;
pub mod playback;
pub mod provider;
pub mod receiver;
pub mod room_collector;
pub mod splitter;
pub mod ssrc_map;
pub mod transcript;
pub mod tts_cache;
pub mod tts_pipeline;
pub mod worker;
#[cfg(test)]
mod e2e_test;

pub use config::VoiceManagerConfig;
pub use gateway::{VoiceGateway, VoiceServerData, VoiceStateData};
pub use receiver::AudioChunk;

use anyhow::Result;
use std::sync::Arc;
use tokio::sync::mpsc;
use tracing::{info, warn};

/// Audio payload for TTS playback: PCM (converted to WAV by gateway) or pre-encoded Opus.
#[derive(Debug, Clone)]
pub enum PlaybackAudio {
    Pcm(Vec<f32>),
    Opus(Vec<u8>),
}

impl PlaybackAudio {
    /// True if the payload is empty (e.g. interrupt sent as Pcm(vec![])).
    pub fn is_empty(&self) -> bool {
        match self {
            PlaybackAudio::Pcm(v) => v.is_empty(),
            PlaybackAudio::Opus(v) => v.is_empty(),
        }
    }

    /// Length in samples (Pcm) or bytes (Opus).
    pub fn len(&self) -> usize {
        match self {
            PlaybackAudio::Pcm(v) => v.len(),
            PlaybackAudio::Opus(v) => v.len(),
        }
    }

    /// Reference to PCM samples if this is Pcm variant (for tests).
    pub fn pcm_samples(&self) -> Option<&[f32]> {
        match self {
            PlaybackAudio::Pcm(v) => Some(v.as_slice()),
            PlaybackAudio::Opus(_) => None,
        }
    }
}

use agent_bridge::{AgentBridge, MockAgentBridge, RoomMessage};
use dispatcher::Dispatcher;
use transcript::TranscriptEntry;

/// Top-level voice subsystem manager.
/// Owns the gateway, dispatcher, and worker lifecycle.
pub struct VoiceManager {
    config: VoiceManagerConfig,
    gateway: Option<Arc<VoiceGateway>>,
    /// Receive end of the audio channel (consumed by the dispatcher).
    audio_rx: Option<mpsc::UnboundedReceiver<AudioChunk>>,
}

impl VoiceManager {
    pub fn new(config: VoiceManagerConfig) -> Self {
        Self {
            config,
            gateway: None,
            audio_rx: None,
        }
    }

    /// Initialize the voice gateway with bot user ID.
    ///
    /// Creates the audio channel and songbird standalone driver config.
    pub fn init_gateway(&mut self, bot_user_id: u64) {
        // Install the rustls ring crypto provider before songbird uses TLS.
        // The call is idempotent — .ok() ignores AlreadyInstalled errors.
        rustls::crypto::ring::default_provider()
            .install_default()
            .ok();

        let (audio_tx, audio_rx) = mpsc::unbounded_channel();
        let gateway = VoiceGateway::new(bot_user_id, audio_tx);
        self.gateway = Some(Arc::new(gateway));
        self.audio_rx = Some(audio_rx);
        info!(bot_user_id, "Voice gateway initialized");
    }

    /// Start the voice pipeline: spawn a dispatcher task that reads from audio_rx
    /// and routes audio chunks to per-user workers via STT → LLM → TTS.
    ///
    /// When `transcript_tx` is `Some`, workers will send transcript entries (user speech / bot response)
    /// so the caller can post them to a Discord text channel or log.
    /// When `agent_bridge` is `Some`, that bridge is used for LLM; otherwise a mock echo bridge is used.
    pub async fn start_pipeline(
        &mut self,
        transcript_tx: Option<mpsc::UnboundedSender<TranscriptEntry>>,
        agent_bridge: Option<Arc<dyn AgentBridge>>,
    ) -> Result<()> {
        let audio_rx = self
            .audio_rx
            .take()
            .ok_or_else(|| anyhow::anyhow!("Audio receiver not available (already consumed?)"))?;

        // Create STT provider from config
        let stt_provider = provider::stt::create_stt_provider(&self.config.voice.stt)?;
        info!("STT provider created: {}", stt_provider.name());

        // Create TTS provider from config
        let tts_provider = provider::tts::create_tts_provider(&self.config.voice.tts)?;
        info!("TTS provider created: {}", tts_provider.name());

        let agent_bridge = agent_bridge.unwrap_or_else(|| Arc::new(MockAgentBridge::new()));
        info!("Agent bridge created");

        // Create audio output channel for TTS playback (PCM or Opus)
        let (audio_output_tx, mut audio_output_rx) = mpsc::unbounded_channel::<(u64, PlaybackAudio)>();

        let context_window_ms = self.config.voice.pipeline.context_window_ms;
        let use_room = self.config.voice.pipeline.context_window_auto && context_window_ms > 0;
        let (room_tx, room_rx) = if use_room {
            let (tx, rx) = mpsc::unbounded_channel::<RoomMessage>();
            (Some(tx), Some(rx))
        } else {
            (None, None)
        };

        if use_room {
            let room_id = 0u64;
            let bridge = agent_bridge.clone();
            let tts = tts_provider.clone();
            let out_tx = audio_output_tx.clone();
            let tr_tx = transcript_tx.clone();
            tokio::spawn(async move {
                room_collector::run_room_collector(
                    room_id,
                    room_rx.unwrap(),
                    bridge,
                    tts,
                    out_tx,
                    tr_tx,
                    "LocalGPT".to_string(),
                    context_window_ms,
                )
                .await;
            });
            info!(context_window_ms, "Room collector started (multi-user batch)");
        }

        // Create dispatcher (transcript_tx = None → no transcript; caller can pass channel for VC-linked text channel)
        let mut dispatcher = Dispatcher::new(
            stt_provider,
            tts_provider,
            agent_bridge,
            audio_output_tx,
            transcript_tx,
            "LocalGPT".to_string(),
            self.config.voice.pipeline.idle_timeout_sec,
            self.config.voice.pipeline.interrupt_enabled,
            None,
            room_tx,
        );
        info!("Dispatcher created");

        // Spawn task to play TTS audio output via songbird
        let gateway_for_playback = self.gateway.clone();
        let playback_guild_id = self
            .config
            .voice
            .discord
            .auto_join
            .first()
            .and_then(|aj| aj.guild_id.parse::<u64>().ok());
        tokio::spawn(async move {
            while let Some((_user_id, playback)) = audio_output_rx.recv().await {
                if let (Some(gw), Some(gid)) = (&gateway_for_playback, playback_guild_id) {
                    match playback {
                        PlaybackAudio::Pcm(audio) => {
                            if audio.is_empty() {
                                continue;
                            }
                            let n = audio.len();
                            if let Err(e) = gw.play_audio(gid, audio).await {
                                warn!("Failed to play TTS audio: {}", e);
                            } else {
                                info!(guild_id = gid, samples = n, "Playing TTS audio");
                            }
                        }
                        PlaybackAudio::Opus(data) => {
                            if data.is_empty() {
                                continue;
                            }
                            let n = data.len();
                            if let Err(e) = gw.play_audio_opus(gid, data).await {
                                warn!("Failed to play TTS Opus: {}", e);
                            } else {
                                info!(guild_id = gid, bytes = n, "Playing TTS Opus");
                            }
                        }
                    }
                } else {
                    warn!("No gateway or guild_id for TTS playback");
                }
            }
        });

        // Spawn main dispatch loop
        tokio::spawn(async move {
            let mut audio_rx = audio_rx;
            let mut dispatch_count: u64 = 0;
            while let Some(chunk) = audio_rx.recv().await {
                // Use the resolved user_id/user_name from the SSRC map
                // (populated by SpeakingStateUpdate events in the receiver).
                // Fall back to SSRC-based placeholder if not yet mapped.
                let user_id = chunk.user_id.unwrap_or(chunk.ssrc as u64);
                let user_name = chunk
                    .user_name
                    .unwrap_or_else(|| format!("user_{}", chunk.ssrc));

                // Diagnostic: log first 20 dispatches to confirm data flow
                if dispatch_count < 20 {
                    info!(
                        user_id,
                        user_name,
                        ssrc = chunk.ssrc,
                        samples = chunk.pcm.len(),
                        dispatch_count,
                        "[B] Dispatching audio to worker"
                    );
                }
                dispatch_count += 1;

                dispatcher.dispatch(user_id, user_name, chunk.pcm);
            }
            info!("Audio dispatcher loop ended");
        });

        info!("Voice pipeline started");
        Ok(())
    }

    /// Start the voice subsystem (call from daemon).
    pub async fn start(&self) -> Result<()> {
        if !self.config.voice.enabled {
            info!("Voice subsystem disabled in config");
            return Ok(());
        }

        info!("Voice manager started");
        Ok(())
    }

    /// Gracefully shut down all voice resources.
    pub async fn shutdown(&self) -> Result<()> {
        if let Some(ref gateway) = self.gateway {
            gateway.shutdown().await;
        }
        info!("Voice manager shut down");
        Ok(())
    }

    /// Join a voice channel.
    pub async fn join(
        &self,
        guild_id: u64,
        channel_id: u64,
        gateway_tx: &mpsc::Sender<serde_json::Value>,
    ) -> Result<()> {
        let gateway = self
            .gateway
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Voice gateway not initialized"))?;

        gateway.join(guild_id, channel_id, gateway_tx).await
    }

    /// Leave a voice channel.
    pub async fn leave(&self, guild_id: u64) -> Result<()> {
        let gateway = self
            .gateway
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Voice gateway not initialized"))?;

        gateway.leave(guild_id).await
    }

    /// Handle Voice State Update from Discord Gateway.
    pub async fn handle_voice_state_update(&self, data: VoiceStateData) {
        if let Some(ref gateway) = self.gateway {
            gateway.handle_voice_state_update(data).await;
        }
    }

    /// Handle Voice Server Update from Discord Gateway.
    pub async fn handle_voice_server_update(&self, data: VoiceServerData) {
        if let Some(ref gateway) = self.gateway {
            gateway.handle_voice_server_update(data).await;
        }
    }

    /// Take the audio receiver (consumed once by the dispatcher).
    pub fn take_audio_rx(&mut self) -> Option<mpsc::UnboundedReceiver<AudioChunk>> {
        self.audio_rx.take()
    }

    /// Get the voice gateway (for advanced usage).
    pub fn gateway(&self) -> Option<Arc<VoiceGateway>> {
        self.gateway.clone()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voice_manager_new() {
        let config = VoiceManagerConfig::from_voice_config(crate::config::VoiceConfig::default());
        let manager = VoiceManager::new(config);
        assert!(manager.gateway.is_none());
        assert!(manager.audio_rx.is_none());
    }

    #[test]
    fn voice_manager_init_gateway() {
        let config = VoiceManagerConfig::from_voice_config(crate::config::VoiceConfig::default());
        let mut manager = VoiceManager::new(config);

        manager.init_gateway(12345);
        assert!(manager.gateway.is_some());
        assert!(manager.audio_rx.is_some());
    }

    #[test]
    fn voice_manager_take_audio_rx() {
        let config = VoiceManagerConfig::from_voice_config(crate::config::VoiceConfig::default());
        let mut manager = VoiceManager::new(config);

        manager.init_gateway(12345);
        let rx = manager.take_audio_rx();
        assert!(rx.is_some());
        // Second take returns None
        let rx2 = manager.take_audio_rx();
        assert!(rx2.is_none());
    }

    #[tokio::test]
    async fn voice_manager_start_disabled() {
        let config = VoiceManagerConfig::from_voice_config(crate::config::VoiceConfig::default());
        let manager = VoiceManager::new(config);

        // Should succeed even without gateway init when disabled
        let result = manager.start().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn voice_manager_shutdown() {
        let config = VoiceManagerConfig::from_voice_config(crate::config::VoiceConfig::default());
        let manager = VoiceManager::new(config);

        let result = manager.shutdown().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn join_without_gateway_fails() {
        let config = VoiceManagerConfig::from_voice_config(crate::config::VoiceConfig::default());
        let manager = VoiceManager::new(config);

        let (tx, _rx) = mpsc::channel(1);
        let result = manager.join(123, 456, &tx).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn leave_without_gateway_fails() {
        let config = VoiceManagerConfig::from_voice_config(crate::config::VoiceConfig::default());
        let manager = VoiceManager::new(config);

        let result = manager.leave(123).await;
        assert!(result.is_err());
    }
}
