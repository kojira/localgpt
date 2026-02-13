//! Pipeline worker — per-user STT → Agent → TTS processing.
//!
//! Each worker owns an STT session, an agent bridge reference,
//! and a TTS provider reference.  It receives PCM chunks from
//! the dispatcher and produces response audio sent back to the
//! main thread for playback via songbird.

use anyhow::Result;

/// Per-user voice processing pipeline.
pub struct PipelineWorker {
    _user_id: u64,
    // Will hold SttSession, AgentBridge, TtsProvider refs, channels
}

impl PipelineWorker {
    pub fn new(user_id: u64) -> Self {
        Self { _user_id: user_id }
    }

    /// Run the worker loop (stub).
    pub async fn run(&self) -> Result<()> {
        tracing::info!(user_id = self._user_id, "PipelineWorker::run (stub)");
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn worker_new_stores_user_id() {
        let w = PipelineWorker::new(42);
        // Verify construction succeeds with any user_id
        assert_eq!(w._user_id, 42);
    }

    #[test]
    fn worker_new_different_users() {
        let w1 = PipelineWorker::new(1);
        let w2 = PipelineWorker::new(u64::MAX);
        assert_ne!(w1._user_id, w2._user_id);
    }

    #[tokio::test]
    async fn worker_run_stub_succeeds() {
        let w = PipelineWorker::new(99);
        assert!(w.run().await.is_ok());
    }
}
