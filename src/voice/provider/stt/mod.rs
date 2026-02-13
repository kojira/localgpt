//! STT provider implementations.

pub mod mock;
pub mod ws;

use std::sync::Arc;

use anyhow::{bail, Result};

use crate::config::VoiceSttConfig;
use crate::voice::provider::SttProvider;

/// Create an [`SttProvider`] from configuration.
///
/// Supported `provider` values:
/// - `"ws"` — WebSocket-based (connects to an external STT server).
/// - `"mock"` — Mock provider for testing (always returns empty sessions).
pub fn create_stt_provider(config: &VoiceSttConfig) -> Result<Arc<dyn SttProvider>> {
    match config.provider.as_str() {
        "ws" => Ok(Arc::new(ws::WsSttProvider::new(config.ws.clone()))),
        "mock" => Ok(Arc::new(mock::MockSttProvider::new(
            mock::MockSttConfig {
                utterances: vec![],
                close_after_all: false,
                latency_multiplier: 1.0,
            },
        ))),
        other => bail!("unknown STT provider: {other:?} (expected \"ws\" or \"mock\")"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn create_ws_provider() {
        let config = VoiceSttConfig::default();
        let provider = create_stt_provider(&config).unwrap();
        assert_eq!(provider.name(), "ws");
    }

    #[test]
    fn create_mock_provider() {
        let mut config = VoiceSttConfig::default();
        config.provider = "mock".to_string();
        let provider = create_stt_provider(&config).unwrap();
        assert_eq!(provider.name(), "mock");
    }

    #[test]
    fn unknown_provider_is_error() {
        let mut config = VoiceSttConfig::default();
        config.provider = "nonexistent".to_string();
        assert!(create_stt_provider(&config).is_err());
    }
}
