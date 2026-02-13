//! Voice ↔ Agent bridge.
//!
//! Provides direct access to [`crate::agent::Agent`] and
//! [`crate::memory`] without going through the HTTP API,
//! eliminating network round-trip latency.

use anyhow::Result;
use async_trait::async_trait;

/// Bridges voice pipeline workers to the LLM agent.
#[async_trait]
pub trait AgentBridge: Send + Sync {
    /// Generate a text response for a voice user.
    async fn generate(&self, user_id: u64, text: &str) -> Result<String>;

    /// Reset the conversation context for a user.
    async fn reset_context(&self, user_id: u64) -> Result<()>;
}

/// Mock agent bridge that echoes user input back.
///
/// Used for testing the pipeline without a real LLM.
pub struct MockAgentBridge;

impl MockAgentBridge {
    pub fn new() -> Self {
        Self
    }
}

#[async_trait]
impl AgentBridge for MockAgentBridge {
    async fn generate(&self, _user_id: u64, text: &str) -> Result<String> {
        Ok(format!("echo: {}", text))
    }

    async fn reset_context(&self, _user_id: u64) -> Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn mock_bridge_echoes() {
        let bridge = MockAgentBridge::new();
        let result = bridge.generate(1, "hello").await.unwrap();
        assert_eq!(result, "echo: hello");
    }

    #[tokio::test]
    async fn mock_bridge_reset_ok() {
        let bridge = MockAgentBridge::new();
        assert!(bridge.reset_context(42).await.is_ok());
    }

    #[tokio::test]
    async fn mock_bridge_is_send_sync() {
        let bridge: std::sync::Arc<dyn AgentBridge> = std::sync::Arc::new(MockAgentBridge::new());
        let b = bridge.clone();
        let handle = tokio::spawn(async move { b.generate(1, "test").await });
        let result = handle.await.unwrap().unwrap();
        assert_eq!(result, "echo: test");
    }
}
