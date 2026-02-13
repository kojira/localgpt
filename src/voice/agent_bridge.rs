//! Voice ↔ Agent bridge.
//!
//! Provides direct access to [`crate::agent::Agent`] and
//! [`crate::memory`] without going through the HTTP API,
//! eliminating network round-trip latency.

use anyhow::Result;

/// Bridges voice pipeline workers to the LLM agent.
pub struct VoiceAgentBridge {
    // Will hold Arc<Agent>, Arc<MemoryStore>
}

impl VoiceAgentBridge {
    pub fn new() -> Self {
        Self {}
    }

    /// Generate a text response for a voice user (stub).
    pub async fn generate(&self, _user_id: u64, _text: &str) -> Result<String> {
        Ok("(stub response)".to_string())
    }

    /// Reset the conversation context for a user (stub).
    pub async fn reset_context(&self, _user_id: u64) -> Result<()> {
        Ok(())
    }
}
