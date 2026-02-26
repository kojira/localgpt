//! Parallel TTS pipeline with concurrency control.
//!
//! Receives [`SentenceSegment`]s from the splitter, dispatches TTS
//! synthesis requests in parallel (bounded by a semaphore), and
//! produces sequence-numbered [`TtsSegment`]s for ordered playback.

use std::sync::Arc;

use anyhow::Result;
use futures::{Stream, StreamExt};
use tokio::sync::{mpsc, Semaphore};
use tracing::{debug, error, info};

use super::provider::{TtsProvider, TtsResult};
use super::splitter::SentenceSegment;

/// Returns `true` if `c` is an emoji or emoji-related character.
fn is_emoji_char_tts(c: char) -> bool {
    let cp = c as u32;
    if cp >= 0x1F000 {
        return true;
    }
    matches!(cp,
        0x00A9 | 0x00AE |
        0x203C | 0x2049 |
        0x2122 | 0x2139 |
        0x2194..=0x2199 |
        0x21A9..=0x21AA |
        0x231A..=0x231B |
        0x2328 | 0x23CF |
        0x23E9..=0x23F3 |
        0x23F8..=0x23FA |
        0x24C2 |
        0x25AA..=0x25AB |
        0x25B6 | 0x25C0 |
        0x25FB..=0x25FE |
        0x2600..=0x27BF |
        0x2934..=0x2935 |
        0x2B05..=0x2B07 |
        0x2B1B..=0x2B1C |
        0x2B50 | 0x2B55 |
        0x3030 | 0x303D |
        0x3297 | 0x3299 |
        0xFE00..=0xFE0F |
        0x200D | 0x20E3
    )
}

/// Returns `true` if a TTS segment should be skipped (not synthesized).
///
/// Skips:
/// * Empty text
/// * `NO_REPLY` marker (OpenClaw agent signal)
/// * Emoji-only lines (would be read aloud awkwardly by TTS)
fn should_skip_tts_segment(text: &str) -> bool {
    let t = text.trim();
    if t.is_empty() {
        return true;
    }
    if t == "NO_REPLY" {
        return true;
    }
    // Emoji-only: all chars are emoji or spaces
    t.chars().all(|c| is_emoji_char_tts(c) || c == ' ')
}

/// Default maximum number of concurrent TTS requests.
const DEFAULT_MAX_CONCURRENT: usize = 3;

/// A completed TTS segment ready for playback.
#[derive(Debug, Clone)]
pub struct TtsSegment {
    /// Sequence index (mirrors `SentenceSegment::index`).
    pub index: usize,
    /// Original text that was synthesized.
    pub text: String,
    /// TTS synthesis result (PCM audio).
    pub tts_result: TtsResult,
    /// When TTS synthesis started (= when the LLM segment was ready to be synthesized).
    /// Carry this to the playback loop so profiling can compute accurate elapsed-from-speech-start.
    pub synthesis_started_at: std::time::Instant,
    /// Duration of TTS synthesis in milliseconds.
    pub tts_duration_ms: u64,
}

/// Parallel TTS pipeline that respects a concurrency limit.
pub struct TtsPipeline {
    tts_provider: Arc<dyn TtsProvider>,
    semaphore: Arc<Semaphore>,
}

impl TtsPipeline {
    /// Create a new pipeline with the given concurrency limit.
    pub fn new(tts_provider: Arc<dyn TtsProvider>, max_concurrent: usize) -> Self {
        Self {
            tts_provider,
            semaphore: Arc::new(Semaphore::new(max_concurrent)),
        }
    }

    /// Create a pipeline with the default concurrency (3).
    pub fn with_defaults(tts_provider: Arc<dyn TtsProvider>) -> Self {
        Self::new(tts_provider, DEFAULT_MAX_CONCURRENT)
    }

    /// Consume a sentence stream and produce a TTS segment stream.
    ///
    /// Each sentence is dispatched to a `tokio::spawn` task.  The semaphore
    /// limits the number of in-flight TTS requests.  Results arrive in
    /// arbitrary order — the downstream [`SequencedPlaybackQueue`] is
    /// responsible for reordering.
    pub fn process(
        &self,
        sentence_stream: impl Stream<Item = Result<SentenceSegment>> + Send + 'static,
    ) -> mpsc::Receiver<Result<TtsSegment>> {
        let (tx, rx) = mpsc::channel::<Result<TtsSegment>>(32);
        let tts = Arc::clone(&self.tts_provider);
        let sem = Arc::clone(&self.semaphore);

        tokio::spawn(async move {
            let mut stream = Box::pin(sentence_stream);

            while let Some(item) = stream.next().await {
                let seg = match item {
                    Ok(s) => s,
                    Err(e) => {
                        let _ = tx.send(Err(e)).await;
                        continue;
                    }
                };

                // Skip segments that should not be synthesized.
                if should_skip_tts_segment(&seg.text) {
                    debug!(index = seg.index, text = %seg.text, "TTS skipped segment[{}]", seg.index);
                    continue;
                }

                let permit = match sem.clone().acquire_owned().await {
                    Ok(p) => p,
                    Err(_) => break, // semaphore closed
                };

                let tts_clone = Arc::clone(&tts);
                let tx_clone = tx.clone();

                tokio::spawn(async move {
                    let _permit = permit; // held until this task completes

                    // Log: LLM produced this segment (SentenceSplitter confirmed the boundary).
                    // Full text without truncation so split quality is visible in logs.
                    info!(
                        segment = seg.index,
                        text = %seg.text,
                        "LLM segment[{}]: \"{}\"",
                        seg.index,
                        seg.text
                    );

                    // Capture synthesis start time — this is also the "LLM segment ready" time.
                    let synthesis_started_at = std::time::Instant::now();
                    debug!(index = seg.index, text = %seg.text, "TTS synthesis started");

                    match tts_clone.synthesize(&seg.text).await {
                        Ok(tts_result) => {
                            let tts_duration_ms = synthesis_started_at.elapsed().as_millis() as u64;
                            info!(
                                segment = seg.index,
                                tts_duration_ms,
                                "TTS segment[{}] done in {}ms",
                                seg.index,
                                tts_duration_ms
                            );
                            let tts_seg = TtsSegment {
                                index: seg.index,
                                text: seg.text,
                                tts_result,
                                synthesis_started_at,
                                tts_duration_ms,
                            };
                            let _ = tx_clone.send(Ok(tts_seg)).await;
                        }
                        Err(e) => {
                            error!(index = seg.index, error = %e, "TTS synthesis failed");
                            let _ = tx_clone
                                .send(Err(anyhow::anyhow!(
                                    "TTS failed for segment {}: {}",
                                    seg.index,
                                    e
                                )))
                                .await;
                        }
                    }
                });
            }
            // All sentences dispatched.  Sender drops naturally when all
            // spawned tasks finish.
        });

        rx
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::voice::provider::tts::mock::MockTtsProvider;
    use futures::stream;

    fn mock_segments(texts: &[&str]) -> Vec<Result<SentenceSegment>> {
        texts
            .iter()
            .enumerate()
            .map(|(i, t)| {
                Ok(SentenceSegment {
                    index: i,
                    text: t.to_string(),
                })
            })
            .collect()
    }

    #[tokio::test]
    async fn pipeline_produces_all_segments() {
        let tts: Arc<dyn TtsProvider> = Arc::new(MockTtsProvider::silent());
        let pipeline = TtsPipeline::with_defaults(tts);

        let input = stream::iter(mock_segments(&["Hello!", "World!"]));
        let mut rx = pipeline.process(input);

        let mut results = Vec::new();
        while let Some(item) = rx.recv().await {
            results.push(item.unwrap());
        }

        assert_eq!(results.len(), 2);
        // Both indices should be present (order may vary due to parallelism).
        let mut indices: Vec<usize> = results.iter().map(|s| s.index).collect();
        indices.sort();
        assert_eq!(indices, vec![0, 1]);
    }

    #[tokio::test]
    async fn pipeline_respects_concurrency() {
        // Use concurrency of 1 — segments must be processed serially.
        let tts: Arc<dyn TtsProvider> = Arc::new(MockTtsProvider::silent());
        let pipeline = TtsPipeline::new(tts, 1);

        let input = stream::iter(mock_segments(&["A!", "B!", "C!"]));
        let mut rx = pipeline.process(input);

        let mut results = Vec::new();
        while let Some(item) = rx.recv().await {
            results.push(item.unwrap());
        }

        assert_eq!(results.len(), 3);
    }

    #[tokio::test]
    async fn pipeline_preserves_text() {
        let tts: Arc<dyn TtsProvider> = Arc::new(MockTtsProvider::silent());
        let pipeline = TtsPipeline::with_defaults(tts);

        let input = stream::iter(mock_segments(&["こんにちは。"]));
        let mut rx = pipeline.process(input);

        let seg = rx.recv().await.unwrap().unwrap();
        assert_eq!(seg.text, "こんにちは。");
        assert_eq!(seg.index, 0);
    }

    #[tokio::test]
    async fn pipeline_audio_is_non_empty() {
        let tts: Arc<dyn TtsProvider> = Arc::new(MockTtsProvider::silent());
        let pipeline = TtsPipeline::with_defaults(tts);

        let input = stream::iter(mock_segments(&["test"]));
        let mut rx = pipeline.process(input);

        let seg = rx.recv().await.unwrap().unwrap();
        // MockTtsProvider::silent() generates silence samples based on text length.
        match &seg.tts_result {
            crate::voice::provider::TtsResult::Pcm { audio, .. } => assert!(!audio.is_empty()),
            crate::voice::provider::TtsResult::EncodedOpus { data, .. } => assert!(!data.is_empty()),
        }
    }

    #[tokio::test]
    async fn pipeline_skips_empty_segments() {
        let tts: Arc<dyn TtsProvider> = Arc::new(MockTtsProvider::silent());
        let pipeline = TtsPipeline::with_defaults(tts);

        let input = stream::iter(vec![
            Ok(SentenceSegment { index: 0, text: "".to_string() }),
            Ok(SentenceSegment { index: 1, text: "Hello!".to_string() }),
        ]);
        let mut rx = pipeline.process(input);

        let mut results = Vec::new();
        while let Some(item) = rx.recv().await {
            results.push(item.unwrap());
        }
        // Only the non-empty segment should be processed
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].text, "Hello!");
    }

    #[tokio::test]
    async fn pipeline_skips_no_reply() {
        let tts: Arc<dyn TtsProvider> = Arc::new(MockTtsProvider::silent());
        let pipeline = TtsPipeline::with_defaults(tts);

        let input = stream::iter(vec![
            Ok(SentenceSegment { index: 0, text: "NO_REPLY".to_string() }),
            Ok(SentenceSegment { index: 1, text: "Hello!".to_string() }),
        ]);
        let mut rx = pipeline.process(input);

        let mut results = Vec::new();
        while let Some(item) = rx.recv().await {
            results.push(item.unwrap());
        }
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].text, "Hello!");
    }

    #[tokio::test]
    async fn pipeline_skips_emoji_only() {
        let tts: Arc<dyn TtsProvider> = Arc::new(MockTtsProvider::silent());
        let pipeline = TtsPipeline::with_defaults(tts);

        let input = stream::iter(vec![
            Ok(SentenceSegment { index: 0, text: "🎉".to_string() }),
            Ok(SentenceSegment { index: 1, text: "Hello!".to_string() }),
        ]);
        let mut rx = pipeline.process(input);

        let mut results = Vec::new();
        while let Some(item) = rx.recv().await {
            results.push(item.unwrap());
        }
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].text, "Hello!");
    }

    #[tokio::test]
    async fn empty_input() {
        let tts: Arc<dyn TtsProvider> = Arc::new(MockTtsProvider::silent());
        let pipeline = TtsPipeline::with_defaults(tts);

        let input = stream::iter(Vec::<Result<SentenceSegment>>::new());
        let mut rx = pipeline.process(input);

        assert!(rx.recv().await.is_none());
    }
}
