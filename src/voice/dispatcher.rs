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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dispatcher_new() {
        let _d = Dispatcher::new();
    }

    #[tokio::test]
    async fn dispatcher_run_stub_succeeds() {
        let d = Dispatcher::new();
        assert!(d.run().await.is_ok());
    }
}
