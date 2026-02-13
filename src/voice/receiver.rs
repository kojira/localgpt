//! Voice receive handler.
//!
//! Receives Opus packets from songbird, decodes to PCM,
//! resamples 48 kHz stereo → 16 kHz mono, and forwards
//! [`AudioChunk`]s to the dispatcher via an mpsc channel.

/// A chunk of decoded, resampled audio from a single speaker.
#[derive(Debug, Clone)]
pub struct AudioChunk {
    /// Synchronization source — identifies the speaker.
    pub ssrc: u32,
    /// 16 kHz mono f32 PCM samples.
    pub pcm: Vec<f32>,
}

/// songbird `EventHandler` implementation (stub).
pub struct VoiceReceiveHandler {
    // Will hold mpsc::Sender<AudioChunk>
}

impl VoiceReceiveHandler {
    pub fn new() -> Self {
        Self {}
    }
}
