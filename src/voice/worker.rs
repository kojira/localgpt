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
