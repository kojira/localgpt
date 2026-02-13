//! Discord voice chat module
//!
//! Provides a real-time voice conversation pipeline:
//! Discord VC → STT → LLM Agent → TTS → Discord VC

pub mod agent_bridge;
pub mod audio;
pub mod config;
pub mod dispatcher;
pub mod gateway;
pub mod provider;
pub mod receiver;
pub mod worker;

pub use config::VoiceManagerConfig;
pub use gateway::VoiceGateway;

use anyhow::Result;

/// Top-level voice subsystem manager.
/// Owns the gateway, dispatcher, and worker lifecycle.
pub struct VoiceManager {
    _config: VoiceManagerConfig,
}

impl VoiceManager {
    pub fn new(config: VoiceManagerConfig) -> Self {
        Self { _config: config }
    }

    /// Start the voice subsystem (call from daemon).
    pub async fn start(&self) -> Result<()> {
        tracing::info!("Voice manager started (stub)");
        Ok(())
    }

    /// Gracefully shut down all voice resources.
    pub async fn shutdown(&self) -> Result<()> {
        tracing::info!("Voice manager shutting down (stub)");
        Ok(())
    }
}
