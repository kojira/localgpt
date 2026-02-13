//! Discord Voice Gateway integration via songbird standalone driver.
//!
//! Manages the songbird `Call` lifecycle without serenity,
//! using `ConnectionInfo` built from raw Gateway events forwarded
//! by the existing text-gateway in `src/discord/`.

use anyhow::Result;

/// Wraps `Arc<Songbird>` and manages VC join/leave.
pub struct VoiceGateway {
    // Will hold Arc<Songbird> and bot_user_id
}

impl VoiceGateway {
    /// Create a new gateway (stub).
    pub fn new() -> Self {
        Self {}
    }

    /// Join a voice channel.
    pub async fn join(&self, _guild_id: u64, _channel_id: u64) -> Result<()> {
        tracing::info!("VoiceGateway::join (stub)");
        Ok(())
    }

    /// Leave a voice channel.
    pub async fn leave(&self, _guild_id: u64) -> Result<()> {
        tracing::info!("VoiceGateway::leave (stub)");
        Ok(())
    }
}
