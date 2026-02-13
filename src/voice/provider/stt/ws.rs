//! WebSocket-based STT provider.
//!
//! Connects to an external STT server (e.g. Voxtral / mlx-whisper)
//! that performs VAD + speech recognition and returns [`SttEvent`]s.

use anyhow::Result;
use async_trait::async_trait;

use crate::config::VoiceSttWsConfig;
use crate::voice::provider::{SttEvent, SttProvider, SttSession};

/// WebSocket STT provider (stub).
pub struct WsSttProvider {
    _config: VoiceSttWsConfig,
}

impl WsSttProvider {
    pub fn new(config: VoiceSttWsConfig) -> Self {
        Self { _config: config }
    }
}

#[async_trait]
impl SttProvider for WsSttProvider {
    async fn connect(&self) -> Result<Box<dyn SttSession>> {
        Ok(Box::new(WsSttSession {}))
    }

    fn name(&self) -> &str {
        "ws"
    }
}

/// A single WebSocket STT session (stub).
struct WsSttSession {
    // Will hold WebSocket connection
}

#[async_trait]
impl SttSession for WsSttSession {
    async fn send_audio(&mut self, _audio: &[f32]) -> Result<()> {
        Ok(())
    }

    async fn recv_event(&mut self) -> Result<Option<SttEvent>> {
        Ok(None)
    }

    async fn close(&mut self) -> Result<()> {
        Ok(())
    }
}
