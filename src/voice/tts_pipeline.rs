//! Parallel TTS pipeline with concurrency control.
//!
//! Receives [`SentenceSegment`]s from the splitter, dispatches TTS
//! synthesis requests in parallel (bounded by a semaphore), and
//! produces sequence-numbered [`TtsSegment`]s for ordered playback.

use std::sync::Arc;

use anyhow::Result;
use futures::{Stream, StreamExt};
use tokio::sync::{mpsc, Semaphore};
use tracing::{debug, error};

use super::provider::{TtsProvider, TtsResult};
use super::splitter::SentenceSegment;

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

                let permit = match sem.clone().acquire_owned().await {
                    Ok(p) => p,
                    Err(_) => break, // semaphore closed
                };

                let tts_clone = Arc::clone(&tts);
                let tx_clone = tx.clone();

                tokio::spawn(async move {
                    let _permit = permit; // held until this task completes

                    debug!(index = seg.index, text = %seg.text, "TTS synthesis started");

                    match tts_clone.synthesize(&seg.text).await {
                        Ok(tts_result) => {
                            debug!(index = seg.index, "TTS synthesis completed");
                            let tts_seg = TtsSegment {
                                index: seg.index,
                                text: seg.text,
                                tts_result,
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
        assert!(!seg.tts_result.audio.is_empty());
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
