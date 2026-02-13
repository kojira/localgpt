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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gateway_new() {
        let _gw = VoiceGateway::new();
    }

    #[tokio::test]
    async fn gateway_join_stub_succeeds() {
        let gw = VoiceGateway::new();
        assert!(gw.join(123, 456).await.is_ok());
    }

    #[tokio::test]
    async fn gateway_leave_stub_succeeds() {
        let gw = VoiceGateway::new();
        assert!(gw.leave(123).await.is_ok());
    }

    #[tokio::test]
    async fn gateway_join_then_leave() {
        let gw = VoiceGateway::new();
        gw.join(111, 222).await.unwrap();
        gw.leave(111).await.unwrap();
    }
}
