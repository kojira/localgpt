//! Dispatcher — routes [`AudioChunk`]s to per-user pipeline workers.
//!
//! Runs in the main async context (lightweight, non-blocking).
//! Creates / retrieves workers keyed by SSRC → user ID mapping.

use anyhow::Result;

/// Routes incoming audio to the correct [`super::worker::PipelineWorker`].
pub struct Dispatcher {
    // Will hold worker map, SSRC→UserID mapping, etc.
}

impl Dispatcher {
    pub fn new() -> Self {
        Self {}
    }

    /// Start the dispatch loop (stub).
    pub async fn run(&self) -> Result<()> {
        tracing::info!("Dispatcher::run (stub)");
        Ok(())
    }
}
